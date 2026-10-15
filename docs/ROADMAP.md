# Roadmap: dual-backend database (SQLite & PostgreSQL) + React SPA

## Goal

1. Replace `rusqlite` with an async driver so the same code serves **both** SQLite and PostgreSQL, with no `spawn_blocking` anywhere.
2. Make `ROLE=INDEXER` / `ROLE=WEB` real, so indexer and web can run as separate processes.
3. Replace the server-rendered Tera frontend with a React single-page app that talks to a JSON API, switched over in one release.
4. After the cutover, move the remaining `?format=json` consumers (the wallet) to `/api/v1` and retire the legacy JSON layer.

## Why sqlx, not SeaORM or Diesel

The DB layer is about 58 handwritten SQL statements: range scans tuned to specific indexes, batched upserts, counter arithmetic, and per-backend details such as `COLLATE NOCASE` and `UNNEST`. An ORM would not remove that SQL; most of it would go through the ORM's raw-SQL escape hatch anyway.
- **Diesel** is synchronous at its core. `diesel-async` covers PostgreSQL, but for SQLite it only offers `SyncConnectionWrapper`, which runs each query on `spawn_blocking`. That is the thing this roadmap removes. Its typed query DSL is also checked against one backend's `schema.rs`, so two backends need `MultiConnection` and code written to what both support.
- **SeaORM** is built on sqlx, so it adds an entity layer on top of the same driver. That layer is designed for CRUD, which this codebase barely does. It means entity definitions to generate and keep in sync with two schemas, and one more layer to debug on the hot write path, without making the SQL any shorter.
- **sqlx** runs plain SQL asynchronously on both backends, natively, and the existing statements port almost unchanged.

## Why async, and why dropping Tera makes it possible

The aim is not just "no `spawn_blocking`". Without Tera, the server only returns JSON, so every request can be async 
from the handler down to the database. With nothing synchronous left in that path, `spawn_blocking` is never needed.

**Today the blocking happens directly on the async runtime.** The code has no
`spawn_blocking`. Instead:
- Every handler calls the synchronous `db::*` API while holding `Arc<std::sync::Mutex<rusqlite::Connection>>`. The indexer locks the same connection to write blocks.
- The web server, `web::feed` and the indexer are separate tokio tasks (`tokio::spawn` in `../src/main.rs`). They still share one runtime and its worker threads. Tokio can only switch a thread to another task at an `.await`.
- A rusqlite call has no `.await`. While a query or a block write runs, the worker thread running it can do nothing else, whichever task made the call.
- `std::sync::Mutex::lock()` blocks the thread as well. While the indexer holds the connection for a write, a handler calling `db::lock()` blocks its own worker thread until the write finishes.
- On a machine with several cores, the other worker threads keep running, so this costs one thread. Fly runs the app on one shared CPU, so tokio starts a single worker thread,
  and all three tasks share it. One slow query or block write therefore holds up every other request, the SSE keepalives, and the indexer's RPC calls.
- All reads queue behind writes on that one connection. SQLite runs in WAL mode (`src/db.rs:152`), which allows readers to run during a write, but the mutex rules that out.

**The usual fix is `spawn_blocking`, and it isn't worth doing.** It means wrapping every DB call so it runs on tokio's separate thread pool for blocking work:
- About 58 call sites would each become a `'static` closure: clone the `Db`, take ownership of every argument, and handle the `JoinError`.
- It adds no concurrency. The closures still queue on the same mutex; the waiting just moves to the blocking-thread pool.
- It does nothing for PostgreSQL. A Postgres driver is async anyway, so we would end up with two styles of database code.

**What keeps the request path synchronous is Tera.**
- Tera runs template functions as synchronous `Fn` closures during `render`.
- Two of them query the database inside those closures: `get_block_url` and `address_label` (`src/web.rs:2296-2328`).
- A synchronous closure cannot `.await`. So as long as templates query the database, those queries need a blocking connection, through `spawn_blocking` or a `block_on` inside the render.
- Rendering HTML on the server is the one step of a request that is synchronous by nature.

**With the SPA, nothing synchronous is left.**
- The browser renders the pages, and the server only returns JSON. Each route becomes: async handler → `.await` the database → serialize. No synchronous callback sits between the handler and the database.
- The display formatting Tera does today (`comma_num`, time-ago, truncated hashes, URLs) moves to the client; see "API contract" below.
- An async pool (sqlx) then gives three things:
  - SQLite readers run concurrently over WAL.
  - PostgreSQL uses the same code shape.
  - A worker never sits blocked on a query.

## Why this order

The DB layer doesn't have to wait for the SPA to go async. Task 0.4 already takes the database out of Tera's closures: handlers look up those two values before rendering.   
After that:
- Tera only formats data it has been handed, which is cheap, in-memory work that can safely run on the async worker.
- Every DB caller is a plain `async fn` handler or an indexer task.

So the DB layer goes async first (Stages 1–2) while the Tera UI keeps serving users, with no `spawn_blocking` stopgap. The SPA (Stage 5) then removes the last server-side rendering step.

The frontend comes last:
- Stage 4 builds the JSON API on top of the async DB layer (each endpoint once its reads are ported), so no endpoint is written twice.
- Stage 5 builds the SPA and replaces Tera in a single cutover.

## Decisions

| Decision              | Choice                                                                                                                         | Notes                                                                                                                                                                                                                                                                         |
|-----------------------|--------------------------------------------------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| DB library            | `sqlx` with **both** native drivers (`sqlite`, `postgres`). Not `AnyPool`: it has no array binds, so no `UNNEST`.              | rusqlite is blocking; see "Why async" above.                                                                                                                                                                                                                                  |
| Query style           | Runtime `sqlx::query_as` + `FromRow`, no `query!` macros                                                                       | `query!` binds one database per crate. Revisit for PG-only paths later.                                                                                                                                                                                                       |
| Backend dispatch      | `enum Db { Sqlite(SqliteDb), Pg(PgDb) }` with async methods that `match`                                                       | Callers never see a driver type.                                                                                                                                                                                                                                              |
| API contract          | New `/api/v1/*` returning raw data (numbers, hex, unix timestamps). The client does the formatting.                            | The current `?format=json` responses carry display strings (`comma_num`, time-ago, truncated hashes, URLs).                                                                                                                                                                   |
| Legacy `?format=json` | **Kept, frozen**: same paths, same shape, after the cutover                                                                    | Covers the wallet (cross-origin, `CorsLayer::permissive()`) without needing a list of the paths it calls. Stage 6 moves consumers to `/api/v1`, then removes it.                                                                                         |
| API types             | `utoipa` OpenAPI spec → `openapi-typescript` generated TS types; interactive docs via `utoipa-scalar` at `/api/v1/docs`        | Keeps Rust DTOs and TS types from drifting apart.                                                                                                                                                                                                                             |
| Live updates          | SSE stays the live feed. `/api/events` and `/api/v1/events` serve the same stream; the SPA consumes it with `EventSource`      | Events gain `id:` = block number, and `Last-Event-ID` is honoured, so a reconnecting browser gets the blocks it missed (task 4.9).                                                                                                                                            |
| Tera                  | **Not kept.** Deleted in the cutover release.                                                                                  | Fly.io deploys the SPA from the same `../Dockerfile`; nothing on Fly depends on Tera.                                                                                                                                                                                         |
| SPA toolchain         | **Vite+** (`vp` CLI: `vp dev` / `vp check` / `vp test` / `vp build`) with pnpm as the package manager (`packageManager` field) | `vp check` runs Oxfmt + Oxlint + type checks; `vp test` runs Vitest. Vite+ is pre-1.0, so pin its version.                                                                                                                                                                    |
| SPA libraries         | React + TypeScript, TanStack Router (file-based, typed routes) + TanStack Query, in `frontend/`                                | Router search params hold the URL state (`page`, `tab`, `from`, `q`); TanStack Query holds the server state, including what the SSE feed pushes.                                                                                                                              |
| Styling               | **Tailwind CSS v4** (`@tailwindcss/vite`), compiled at build time                                                              | Tera uses the v3 Play CDN. The 5.4 pixel test holds the SPA to it, so v4 runs with a v3-compatibility layer (task 5.1). Fall back to v3.4 only if a difference can't be closed.                                                                                               |
| UI components         | **shadcn/ui** (Radix primitives, source copied into `frontend/src/components/ui/`)                                             | Used for behaviour and accessibility (tabs, search popover, tooltips, copy feedback), restyled to Tera's look. Its default theme is not used, since the pixel test compares against Tera.                                                                                     |
| Client state          | **Jotai, only if needed.** Not installed up front.                                                                             | URL state lives in the router and server state in TanStack Query, which leaves almost no global client state. Add Jotai when shared client-only state appears that a small React context handles badly (e.g. the live/reconnecting feed status read by both header and home). |
| Serving the SPA       | Axum `ServeDir` over the built `dist`, falling back to `index.html` for client routes                                          | Same origin, no CORS for the app itself.                                                                                                                                                                                                                                      |
| Cutover               | One release: the SPA replaces Tera at `/`. No side-by-side `/app` period.                                                      | Until the cutover the SPA runs only in development (`vp dev` proxying `/api` to the Rust server) and on a staging Fly app (task 5.13). See "Transition safety" below.                                                                                                         |

## Transition safety (Tera → SPA)

Users should notice a new UI and nothing else. Every item below has a task behind it.

| Risk                                                                      | Mitigation                                                                                                                                                                                     | Task |
|---------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|------|
| Health check tied to the UI: Fly probes `GET /`, which renders Tera today | `/startupz` + `/livez` + `/readyz` (3.3), with the Fly and Render checks moved to them long before the cutover                                                                                 | 3.3  |
| Bookmarked or shared links break                                          | Every Tera path and query param (`page`, `tab`, `from`, `q`) has a matching SPA route, enforced by a test                                                                                      | 5.12 |
| The wallet's `?format=json` calls break                                   | Legacy JSON layer, same paths and shape                                                                                                                                                        | 5.14 |
| Tabs opened before the deploy still run the old Tera JS                   | `/api/events`, `/api/search`, `/api/anchoring/search` keep working as unversioned aliases                                                                                                      | 4.8  |
| Live feed drops blocks on reconnect                                       | SSE `id:` + `Last-Event-ID` resume                                                                                                                                                             | 4.9  |
| Pages missing data the Tera page showed                                   | Field-parity checklist per page, checked against the same fixture DB as `../tests/pages.rs`                                                                                                    | 5.12 |
| The SPA looks different from the Tera page                                | Pixel comparison of every route, tab and viewport against Tera in the same CI run; a required check from the first page on, and it gates deleting Tera                                         | 5.4  |
| Surprises only real chain data shows                                      | Staging Fly app running the cutover build against live data before production                                                                                                                  | 5.13 |
| Stale JS after deploy (old `index.html` asking for deleted chunks)        | `index.html` served `no-cache`, hashed `/assets/*` served `immutable`, and a missing asset returns 404 instead of the `index.html` fallback                                                    | 5.14 |
| Need to roll back                                                         | The cutover release contains **no DB migration**, so the previous GHCR image (`sha-<commit>`) runs against the same volume: `fly deploy --image ghcr.io/<owner>/nvnmchain-explorer:sha-<prev>` | 5.14 |

**PR size rule:** one task = one PR, aiming for under ~600 changed lines (pure moves, deletions and generated files excluded). Every PR keeps `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and the no-network test set green.

---

## Stage 0 — Groundwork

Small fixes and mechanical moves with no behavior change. They make every later diff smaller.

### 0.1 Fix swallowed error in `set_chain_head`
- `let _ = set_kv(...)` at `src/db.rs:600` violates AGENTS.md. Log it with `tracing::warn!` or return `Result`.
- **Files:** `../src/db.rs`, plus the callers in `src/indexer.rs` if it now returns `Result`.

### 0.2 Split `../src/web.rs` into a module directory (pure move)
- **Why:**
  - `../src/web.rs` is about 2400 lines covering nine concerns: routing/`AppState`, Tera rendering, SSE, and the home, block, tx, address, token, anchoring and search pages.
  - Almost every later stage edits it one resource at a time: 0.4 (DB out of templates), 1.3–1.5 (`.await` the DB), 4.2–4.8 (shared builders for the API), 5.14–5.15 (cutover, delete Tera). In one file, tasks marked "reviewable in parallel" (e.g. 4.2 blocks vs 4.5 addresses) would conflict with each other, and their diffs would hide what they actually touch. Split, each PR touches one or two files.
  - It gathers the Tera-only code (`render_html`, `html_or_json`, `page_ctx*`, `build_tera`) into `src/web/render.rs`, so 5.15 deletes one file instead of picking lines out of ~2400.
  - A pure-move PR is the cheapest kind to review (`git diff -M`, tests untouched). Done separately, it keeps every later PR's diff to real changes only, instead of "moved and modified" mixed together.
- **Costs:**
  - `git blame` crosses the split only with `git blame -C -C` / `git log --follow`. Add the commit to `.git-blame-ignore-revs`.
  - Open branches that edit `../src/web.rs` will need painful rebases, so land this when no other PR touches `src/web.rs`.
- Move code only, no logic changes. Re-export the current `pub` items (`tx_extras`, `replay_tx_calls`, `address_label`, `format_time_ago`, `app`, `AppState`, `feed`, `build_tera`) from `web/mod.rs` so external paths keep working.
- **Files:**
  - `../src/web.rs`, which becomes:
    - `src/web/mod.rs` (`AppState`, `app()`)
    - `src/web/render.rs` (`render_html`, `html_or_json`, `page_ctx*`, `build_tera`, format helpers)
    - `src/web/feed.rs` (`feed`, `events`, SSE)
    - `src/web/home.rs`
    - `src/web/block.rs`
    - `src/web/tx.rs` (tx/receipt pages, traces, calls, fees)
    - `src/web/address.rs`
    - `src/web/token.rs`
    - `src/web/anchoring.rs`
    - `src/web/search.rs`
  - `../src/lib.rs` (check only)
- **Done when:** `git diff --stat -M` shows renames, and all tests pass untouched.

### 0.3 Extract shared test fixtures
- Move `temp_db`, `serve`, `serve_beside_indexer`, `get_json`, the stub nodes and the fixture builders out of `../tests/pages.rs` into `tests/common/mod.rs`. The DB tests in Stage 1 and the API tests in Stage 4 reuse them.
- **Files:** `../tests/pages.rs`, `tests/common/mod.rs` (new)

### 0.4 Take the database out of Tera rendering
- Delete the DB-touching template functions:
  - `get_block_url`: resolve block hash → number in the handler.
  - `address_label`: collect the addresses a page shows, look their labels up once, and pass a `labels` map in the context.
- Templates read the precomputed values. `build_tera` no longer takes a `Db`.
- **Files:**
  - `src/web/render.rs` (`build_tera`)
  - `src/web/address.rs` (`address_label`)
  - the handlers that render blocks and addresses: `src/web/{home,block,tx,address,token}.rs`
  - `../templates/_macros.html`, `templates/block.html` (the only templates calling `get_block_url` / `address_label`)
  - `../src/main.rs`
  - `../tests/pages.rs`
- **Done when:** `grep -n "db::" src/web/render.rs` is empty.

---

## Stage 1 — Async DB layer on sqlx (SQLite first)

### 1.1 `Db` handle and pools (SQLite only)
- `pub enum Db { Sqlite(SqliteDb) }`, where `SqliteDb` holds a single-connection writer pool and a reader pool.
- PRAGMAs via `SqliteConnectOptions`, carrying over everything `init_db` sets (`src/db.rs:150-170`): WAL, `synchronous=NORMAL`, `busy_timeout=5000`, `foreign_keys=ON`, `cache_size` from `DB_CACHE_KIB` (default 512 MiB), `mmap_size` 1 GiB, `temp_store=MEMORY`.
- `cache_size` is per connection. Today there is one connection. A reader pool of N connections with the same setting could use N × 512 MiB on the 1 GB Fly machine. Give the writer the large cache and the readers a small one (or cap the reader pool), and leave `DB_CACHE_KIB` meaning the writer's cache.
- `Db::open(&Settings, ReadMode)`, where `ReadMode` is `ReadWrite` or `ReadOnly` (the latter for `ROLE=WEB` in Stage 3).
- rusqlite stays alongside for the moment.
- Check `cargo tree -i libsqlite3-sys` when adding sqlx. rusqlite and sqlx-sqlite live side by side until 1.8, and only one crate may link `sqlite3`, so they must agree on one `libsqlite3-sys` version (today rusqlite 0.32 pulls 0.30.1). Bump rusqlite first if needed.
- **Files:**
  - `../Cargo.toml` (`sqlx` with `runtime-tokio`, `sqlite`)
  - new: `src/db/mod.rs`, `src/db/sqlite/mod.rs`
  - `../src/db.rs` → `src/db/legacy.rs` (move)
  - `../src/main.rs`, `src/web/mod.rs` (`AppState` holds both handles during the port)

### 1.2 SQLite migrations
- Extract the `init_db` DDL into `migrations/sqlite/0001_init.sql` and apply it with `sqlx::migrate!`. Use `CREATE ... IF NOT EXISTS` so existing `explorer.db` files adopt it.
- Keep the `DROP INDEX IF EXISTS` / `DROP TABLE IF EXISTS` statements in 0001 too (`src/db.rs:214-286`). They upgrade older databases in place (retired indexes, `contract_labels`, the old anchoring tables), and `../tests/decoder.rs` checks that they do.
- Keep the counter seeding (`seed_counters`).
- `migrate!` embeds the files at compile time:
  - The Dockerfile copies the whole `migrations/` directory before `cargo build`.
  - A `build.rs` emits `cargo:rerun-if-changed=migrations`, so adding a migration file triggers a rebuild.
- **Files:** `migrations/sqlite/0001_init.sql` (new), `build.rs` (new), `../Dockerfile`, `src/db/sqlite/mod.rs`, `src/db/legacy.rs`
- **Tests:** `../tests/decoder.rs`, which introspects the schema through `sqlite_master` / pragmas:
  - `fresh_schema_has_all_columns`
  - the upgrade tests `a_database_with_the_anchoring_tables_loses_them`, `a_database_with_single_column_{address,transfer}_indexes_gets_the_composite_ones`
  - `counters_are_seeded_from_the_tables_of_an_older_database`
  - the `pragma_index_list` check in `duplicate_bundle_is_idempotent`

### 1.3 Port reads: blocks and transactions
- Async methods on `Db`; the page handlers `.await` them.
- Write portable SQL where possible, so Stage 2 copies rather than rewrites: `ON CONFLICT DO NOTHING` instead of `INSERT OR IGNORE`, table-qualified `excluded`, EXISTS read as bool.
- Also the indexer's own block reads: `get_latest_block` in `wait_for_seed` and the forward loop, and `get_min_block_number` in the backfill loop.
- **Files:** new: `src/db/sqlite/blocks.rs`, `src/db/sqlite/txs.rs`; `src/db/mod.rs`, `src/web/{home,block,tx,render}.rs` (`page_ctx` becomes async), `src/web/feed.rs`, `src/indexer.rs` (block reads), `../tests/pages.rs`

### 1.4 Port reads: addresses, tokens, holders, search
- Also the token-metadata reads in the block and tx handlers (`token_display_map`, `enrich_balance_changes`, `gas_and_fee_ctx`, `block_page`), and the indexer's `get_all_token_addresses` (it seeds the known tokens at start).
- **Files:** new: `src/db/sqlite/addresses.rs`, `src/db/sqlite/tokens.rs`; `src/db/mod.rs`, `src/web/{address,token,search,block,tx}.rs`, `src/indexer.rs` (`get_all_token_addresses`), `../tests/pages.rs`

### 1.5 Port anchoring reads, and kv and counters
- kv reads (`get_kv`, including the indexer's anchoring backfill watermark read) and writes: `set_kv`, and `set_chain_head`, which the indexer calls on every new head (0.1 fixes its swallowed error first).
- `counter` reads. This task defines `get_kv`, `set_kv` and `counter`; 1.7 only calls them.
- The stats load in `main.rs` and the stats read in `web::feed`.
- **Files:** new: `src/db/sqlite/anchoring.rs`, `src/db/sqlite/kv.rs`; `src/db/mod.rs`, `src/web/anchoring.rs`, `src/web/home.rs`, `src/web/feed.rs` (`get_kv("stats")`), `src/indexer.rs` (`set_chain_head`, backfill watermark `get_kv`), `src/main.rs` (stats load at `:61`), `tests/anchoring.rs`

### 1.6 Port the block write path
- `save_block_bundles` / `write_block` in one `sqlx` transaction.
- `adjust_balance`, `apply_transfer_balances`, holder counts, `insert_transfer`, `insert_anchoring`. Keep the "was new" signal via `rows_affected()`.
- `insert_anchoring` takes the transaction it runs in, so 1.7's backfill can call it too. This task defines it; 1.7 only calls it.
- The indexer stays the **only writer**. Both the counter arithmetic in `write_block` (`src/db.rs:511-524`: it reads `stored` / `held`, then adds) and the SELECT-then-write in `adjust_balance` depend on it.
- **No dedicated indexer thread.** The indexer stays a tokio task (`tokio::spawn` in `../src/main.rs`); only its DB calls become `.await`s.
  - sqlx-sqlite runs each connection on its own background OS thread. Async code sends each statement to it over a channel and `.await`s the reply. That is a writer thread fed by a channel, which sqlx provides for every connection.
  - The PG driver needs no thread at all: its network I/O is natively async.
- **What this gains over today:**
  - A block write no longer holds a tokio worker. The indexer task waits at an `.await`, and the worker serves requests, SSE keepalives and RPC calls in the meantime. This matters most on the one-CPU Fly machine, where today a write stalls the whole runtime.
  - Readers stop queueing behind writes. They use their own pool connections and read during a write under WAL, instead of waiting on the shared mutex.
- **Keep the transaction tight.** Fetch from the RPC and decode first, then open the transaction and write, as the code does today. Never `.await` an RPC call inside an open write transaction, or the single writer connection stays held for the whole network round trip.
- **Not covered:** CPU work outside the DB (decoding receipts, building bundles) still runs on the tokio worker. It is small per block. If it shows up as latency during a large backfill, move just that step to a thread, not the whole indexer.
- **Files:** `src/db/sqlite/write.rs` (new), `src/db/mod.rs`, `../src/indexer.rs` (the batch save and fallback around `:1049`), `tests/anchoring.rs`, `tests/write_scale.rs`

### 1.7 Port maintenance paths, then drop the legacy handle
- Starts after 1.3–1.6: it reuses their methods (`set_kv`, `counter`, `get_kv` from 1.5; `insert_anchoring` from 1.6; block and token reads from 1.3 / 1.4). So it lands last of 1.3–1.7 and is the task that drops the legacy handle.
- `rebuild_token_balances`, `sync_holder_counts`, `save_genesis_balances`, `holders_without_genesis_balance`.
- `repair_derived_tables` (runs at every start), including `table_has_rows`.
- `repair_token_metadata` (runs at every start): `get_all_token_metas` and `save_token_metadata`.
- The anchoring backfill transaction: `anchoring_event_from_log` (takes a raw `Connection` today) and its `get_block_timestamp` read, plus calls to 1.6's `insert_anchoring` and 1.5's `set_kv` for the watermark, all in one sqlx transaction.
- The stats write: `compute_and_store_stats` (which `stats_loop` in the indexer runs every interval), calling 1.5's `counter` and `set_kv`, and its `#[cfg(test)]` unit test.
- **Then drop the legacy handle.** Remove the legacy `Db` field from `AppState`, `Indexer`, `run_forever` and the feed state, and stop calling `init_db` in `main.rs`. The compiler then proves no running code uses rusqlite any more; only tests still do. This, not 1.8, is what lets PG run (Stage 2). It can be a separate small PR right after the maintenance port.
- **Files:** `src/db/sqlite/maintenance.rs` (new), `../src/indexer.rs` (raw `db::lock` sites at `:591`, `:680-698`, `:1137`; `repair_token_metadata`), `src/db/mod.rs`; for the handle drop: `src/main.rs` (`init_db`), `src/web/mod.rs` (`AppState`), `src/web/feed.rs` (feed state)

### 1.8 Remove rusqlite (cleanup)
- By now no running code uses rusqlite (the handle was dropped at the end of 1.7), so this is cleanup and nothing waits on it, PG included.
- Delete:
  - `src/db/legacy.rs`
  - `type Db = Arc<Mutex<Connection>>` and `lock()`
  - `OrWarn` (replace with explicit error logging, per AGENTS.md)
- Move the pure helpers out of `legacy.rs` first: `now_ts` (used by `src/parse.rs`, `src/signatures.rs`, `src/indexer.rs`, `src/web`), `page_offset`, `TxColumns`.
- Drop the `rusqlite` dependency and move all tests to `Db::open`.
- **Files:** `src/db/legacy.rs` (delete), `../Cargo.toml`, `src/parse.rs`, `src/signatures.rs`, `src/indexer.rs`, `src/web/*` (`now_ts`, `page_offset`, `TxColumns` imports), `tests/common/mod.rs`, `tests/decoder.rs`, `tests/baseline.rs`, `tests/anchoring.rs`, `tests/write_scale.rs`, `tests/live_rpc.rs`
- **Done when:** `grep -rn "rusqlite\|spawn_blocking" src tests` is empty.

---

## Stage 2 — PostgreSQL backend

Postgres doesn't wait for the whole SQLite port. It needs the `Db` enum (1.1), and each PG port follows the SQLite port that defines the same async methods:

| PG task | Needs                 | Pairs with the SQLite port of                         |
|---------|-----------------------|-------------------------------------------------------|
| 2.1     | 1.1                   | the `Db` enum and pools                               |
| 2.2     | 2.1, 1.2              | migrations (`build.rs`, Dockerfile `COPY migrations`) |
| 2.3     | 2.2, 0.3              | the shared test fixtures                              |
| 2.4     | 2.3, 1.3, 1.5         | blocks, txs (1.3) and kv, stats (1.5)                 |
| 2.5     | 2.3, 1.4, 1.5         | addresses, tokens, search (1.4) and anchoring (1.5)   |
| 2.6     | 2.3, 2.4, 2.5, 1.6, 1.7 | the write path (1.6) and maintenance (1.7); reuses 2.4's kv and 2.5's token PG arms |

So a PG port can land right after its SQLite counterpart, while other resources are still being ported. A small resource can also port both backends in one PR, within the PR size rule.

**PG runs before 1.8.** 1.8 only deletes code nothing runs any more. PG can run in production once:
1. **2.1 is set up correctly:** under a `postgres://` `DB_URI` the process never opens SQLite: no `init_db`, no SQLite file. Nothing needs an `Option` for this. While the legacy handle still exists, the startup guard stops a PG process before any database is opened, and by the time the guard comes off the handle is gone (condition 3). Never work around the guard with an in-memory placeholder connection: legacy calls go through `OrWarn`, which would turn a missed port into empty pages with only a `warn!` log.
2. **2.4–2.6 have landed** (each follows its SQLite port, so 1.3–1.7 are in too), with no `Db::Pg` arm still a `bail!`.
3. **The legacy handle is gone from running code** (end of 1.7), so the compiler, not a grep, proves nothing in PG mode can reach rusqlite.

Until then, PG code still lands early and is tested through the test matrix (2.3), which calls `Db` directly, and a `postgres://` `DB_URI` is a startup error ("the PostgreSQL backend is not complete yet"). The PR that completes the three conditions removes that guard; from then a deployment can run on PG (3.2).

### 2.1 Configuration and the `Pg` variant
- Add `DB_URI` to `Settings`. Its scheme selects the backend:
  - `postgres://localhost:5432` → PostgreSQL
  - `sqlite://explorer.db` (or `sqlite:///data/explorer.db` for an absolute path) → SQLite
  - unset → SQLite at `DB_PATH` (default `explorer.db`), exactly as today. Existing deployments (Dockerfile, `../fly.toml`, `render.yaml`, the systemd unit) keep working unchanged.
  - Any other scheme is a startup error, never a fallback.
- SQLite options set `create_if_missing(true)`. rusqlite creates the file today, but sqlx does not by default.
- PostgreSQL credentials and database name come from their own variables, never from `DB_URI`:
  - `PG_USER`, `PG_DATABASE`: plain config.
  - `PG_PASSWORD`: a secret. On k8s it is loaded from GCP Secret Manager into a Kubernetes Secret and injected as an env var, so it never sits in the URI, a ConfigMap or a log line.
  - The pool is built as `PgConnectOptions::from_str(DB_URI)?.username(PG_USER).password(PG_PASSWORD).database(PG_DATABASE)`.
  - With a `postgres://` `DB_URI`, a missing `PG_*` variable is a startup error that names it. A `DB_URI` that carries a user or password is also rejected, so each credential has exactly one source.
  - These are not libpq's `PGUSER` / `PGPASSWORD` / `PGDATABASE`, which `PgConnectOptions` reads from the environment by itself. Setting the three fields explicitly makes the `PG_*` values win.
  - `PG_PASSWORD` never prints: give it a redacting `Debug`, as `rpc_api_key` has since #61. The startup log (`src/main.rs:33-36`, which prints `cfg.db_path` today) shows the backend and host only.
- `Db::Pg(PgDb)` with a `PgPool`. Every not-yet-ported method `bail!`s explicitly; none falls back silently to a default.
- From here on, every 1.x PR that adds a `Db` method also adds its `Db::Pg` arm, as a `bail!` until the matching 2.x port lands.
- In PG mode, `main.rs` runs the guard before opening any database and never calls `init_db`. Once the guard comes off (legacy handle gone at the end of 1.7), it builds only the `Db::Pg` handle.
- Add the startup guard described above: a `postgres://` `DB_URI` refuses to start until the three conditions hold.
- **Files:** `../Cargo.toml` (sqlx `postgres`), `src/config.rs`, `.env.example`, `deploy/nvnmchain-explorer.env.example`, `README.md`, `src/db/mod.rs`, `src/db/pg/mod.rs` (new), `src/main.rs`

### 2.2 PG migrations
- Apply it at startup with `sqlx::migrate!("./migrations/postgres")`.
- docker-compose stops mounting the schema as an init script, so the app's migrator is the only path that creates tables.
- Freeze `0001_init.sql` once this lands. sqlx records each migration's checksum and refuses to start if an applied file changes, so later schema changes go in `0002_*.sql` and onward.
- A local volume created by the old init script has the tables but no `_sqlx_migrations` row. The migrator reruns 0001 there, which works because every statement is `IF NOT EXISTS`. The same goes for any PG database created before 2.2.
- The Dockerfile and `build.rs` already cover `migrations/postgres/` from 1.2.
- No counter seeding on PG: `seed_counters` exists for old SQLite files. A fresh PG database starts empty (it indexes from genesis), `add_to_counter` upserts, and `counter` reads a missing row as 0.
- **Files:** `src/db/pg/mod.rs`, `docker-compose.yml`

### 2.3 Test matrix
- Behavior tests take a `Db` and run on SQLite always, and on PG when `DATABASE_URL` is set. `#[sqlx::test]` gives each test its own database.
- `DATABASE_URL` is the variable `#[sqlx::test]` reads, with the credentials inline (e.g. the docker-compose defaults). It is test-only, set in CI and locally, and separate from the app's `DB_URI` + `PG_*`.
- CI gets a `postgres:18` service.
- SQLite-introspection tests stay SQLite-only.
- **Files:** `tests/common/mod.rs`, `../tests/pages.rs`, `tests/anchoring.rs`, `.github/workflows/ci.yml`, `AGENTS.md`

### 2.4 PG reads: blocks, txs, kv, stats
- Includes the kv reads and writes from 1.5 (`get_kv`, `set_kv`, `set_chain_head`, `counter`) and the indexer's block reads from 1.3.
- Dialect differences:
  - `$N` binds
  - typed NULLs in `TX_LIST_COLS`
  - `SUM(...)::bigint`
  - i64 `LIMIT`/`OFFSET`
- **Files:** new: `src/db/pg/blocks.rs`, `src/db/pg/txs.rs`, `src/db/pg/kv.rs`; `src/db/mod.rs`

### 2.5 PG reads: addresses, tokens, anchoring, search
- Case-insensitive token search: `lower(col) = lower($1 COLLATE "C")`, or `ILIKE` with `pg_trgm`, replacing `COLLATE NOCASE` / `LIKE`.
- `= ANY($1)` instead of the variable-length `IN` lists.
- Includes the token-metadata reads in the block and tx handlers and `get_all_token_addresses` (1.4).
- **Files:** new: `src/db/pg/addresses.rs`, `src/db/pg/tokens.rs`, `src/db/pg/anchoring.rs`; `src/db/mod.rs`

### 2.6 PG write path
- Batched `UNNEST(...) ON CONFLICT` upserts; balances pre-read in one query per block batch.
- `pg_advisory_xact_lock` as a guard against two indexers.
- Reuses the PG kv arms from 2.4 (`set_kv`, `counter`) and token arms from 2.5, as 1.7 reuses their SQLite counterparts.
- Maintenance paths, everything 1.7 lists: `rebuild_token_balances`, `sync_holder_counts`, genesis, `repair_derived_tables` (`table_has_rows`), `repair_token_metadata` (`save_token_metadata`), the anchoring backfill transaction, and `compute_and_store_stats`.
- `compute_and_store_stats` uses a derived table without an alias (`FROM blocks, (SELECT MAX(timestamp) - ?1 AS start FROM blocks)`), which PG accepts only from 16. CI runs `postgres:18`; give it an alias anyway so older servers work too.
- **Files:** new: `src/db/pg/write.rs`, `src/db/pg/maintenance.rs`; `src/db/mod.rs`, `../tests/write_scale.rs` (PG run)

### 2.7 Backend-agnostic baseline
- Make `../tests/baseline.rs` compare a logical dump rather than SQLite files (`canary.db`), so both backends are checked against the same chain data.
- **Files:** `../tests/baseline.rs`, `fixtures/baseline/*`

### 2.8 Remove `DB_PATH`
- `DB_URI` becomes the only database setting. Unset, it defaults to `sqlite://explorer.db`.
- Switch every deploy config from `DB_PATH=/x/explorer.db` to `DB_URI=sqlite:///x/explorer.db`:
  - `../Dockerfile`, `fly.toml`, `render.yaml`
  - `../deploy/nvnmchain-explorer.service`, `deploy/nvnmchain-explorer.env.example`
  - `../.env.example`, `README.md`
- If `DB_PATH` is still set at startup without `DB_URI` set, fail with a message saying to use `DB_URI`. If both `DB_PATH` & `DB_URI` are set, ignore `DB_PATH` and use `DB_URI` only. Log warning to remove `DB_PATH`.    
- Remove `Settings.db_path` and its use in `../tests/live_rpc.rs`.
- No SQLite → PG data copy: a PG deployment indexes from genesis.
- **Files:** `../src/config.rs`, `src/main.rs`, `Dockerfile`, `fly.toml`, `render.yaml`, `deploy/nvnmchain-explorer.service`, `deploy/nvnmchain-explorer.env.example`, `.env.example`, `README.md`, `tests/live_rpc.rs`

---

## Stage 3 — Make `ROLE` real and split the deployment

3.1 needs only task 1.1 (`ReadMode` on `Db`), so it runs in parallel with the rest of Stages 1–2. 3.3 also needs 1.2 (schema check). 3.2 splitting onto separate machines or pods needs PG to be runnable (Stage 2 intro).

### 3.1 Honour `ROLE`
- `WEB`: no indexer loops; DB opened `ReadOnly` (SQLite `read_only(true)`, PG `default_transaction_read_only = on`).
- `INDEXER`: no app routes. It still listens on `PORT`, serving only the probe and health endpoints (3.3).
- `ALL`: unchanged.
- SSE works across processes, because the live feed already follows the database (`../tests/pages.rs` `the_live_feed_follows_the_database`).
- Home stats work the same way. The indexer's `stats_loop` writes them to the `kv` table, and `web::feed` reads them from there.
- `ReadOnly` is safe for `WEB`: since #49 no page handler writes to the database.
- **Files:** `../src/main.rs`, `src/config.rs` (drop the "informational only" docs), `.env.example`, `README.md`, `tests/pages.rs`

### 3.2 Deployment split
- Run indexer and web as separate processes or services.
- **Fly.io constraint:** a Fly volume attaches to one machine, and each process group in `../fly.toml` runs on its own machines. With SQLite, Fly keeps `ROLE=ALL` on a single machine. Splitting `INDEXER` and `WEB` onto separate Fly machines requires PG to be runnable (Stage 2 intro: 2.4–2.6 landed and the legacy handle dropped at the end of 1.7; not 1.8), on Fly Managed Postgres or an external one.
- **Files:** `../fly.toml`, `render.yaml`, `deploy/entrypoint.sh`, `deploy/nvnmchain-explorer.service`, `deploy/install.sh`, `docker-compose.yml`, `README.md`

### 3.3 Kubernetes probes: liveness, readiness, startup
Following the [Kubernetes probe guide](https://kubernetes.io/docs/concepts/workloads/pods/probes/). All three roles are the same binary, so every role serves every endpoint below; what differs per role is what an endpoint waits for, and which probes the deployment configures. Each probe answers one question, so each gets its own endpoint, kept apart from `/healthz` (4.1), which is a detailed report for people and monitoring:

| Endpoint    | Question                   | 200 when                                                                                                                                                                                        | Never checks                          |
|-------------|----------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|---------------------------------------|
| `/startupz` | Has it finished starting?  | Migrations are applied and the listener is up. `INDEXER` / `ALL` also wait for the boot RPC check to pass once: `eth_chainId` answered and matches `CHAIN_ID`. Sticky: once true it stays true. | the indexed tip                       |
| `/livez`    | Is the process wedged?     | The runtime answers HTTP. For a role that runs the indexer: its loop has ticked within `INDEXER_STALL_SECONDS` (default 300).                                                                   | the DB, the RPC node, the indexed tip |
| `/readyz`   | Should it get traffic now? | The DB answers a `SELECT 1` within ~1 s, the schema is at the latest migration the binary embeds, and shutdown has not started.                                                                 | the RPC node, the indexed tip         |

- **Liveness only catches what a restart fixes.** The guide warns that a liveness probe tied to dependencies or load causes cascading restarts.
  - A database or RPC outage must not restart pods. A database outage makes `/readyz` fail, and pods come back on their own once it recovers.
  - The indexer heartbeat ticks on every loop iteration, including retries and backoff after RPC errors. So it fails only when the loop is truly stuck, not while the chain or the RPC node is slow.
- **The RPC node is checked at startup in the roles that index (`INDEXER`, `ALL`).** It is the system's key dependency, so an indexer that can't reach it, or reaches the wrong chain, should never start:
  - At boot, these roles call `eth_chainId` and compare it with `CHAIN_ID`. They retry with backoff until it succeeds; `/startupz` turns 200 only after that, together with migrations.
  - This catches a wrong `NVNM_RPC` URL, a missing or wrong `NVNM_RPC_API_KEY`, a node on the wrong network, or an RPC outage at deploy time. In a rolling update the new pods never pass startup, so the old pods keep running and `kubectl rollout status` fails the deploy.
  - **`WEB` is not gated on the RPC node**, so it keeps serving indexed data from the DB during an RPC outage, including on a pod that is rescheduled (node drain, scale-up, OOM) while the node is down. It still calls `eth_chainId` once in the background at boot and logs a `warn!` on failure or a chain-id mismatch, without blocking startup. Its `/startupz` doesn't wait for that call. A `WEB` deployment also doesn't need to configure a startup probe: it runs no migrations and starts in seconds, and readiness already keeps it out of rotation until the schema is current.
  - It runs only until it passes. After startup, an RPC outage doesn't affect any probe: liveness or readiness tied to the shared RPC node would restart or unroute every pod at once without fixing anything, so it shows up in `/healthz` and alerting (4.1) instead.
  - **Tradeoff for `ALL`:** it also serves the site, so during an RPC outage a rescheduled `ALL` pod can't come back, although it could serve pages from the DB. Where that matters, run `WEB` pods for serving and keep `ALL` for single-machine setups (Fly with SQLite).
  - **No probe calls the RPC node itself.** Probes run every few seconds on every pod, and the RPC is rate-limited (#61). The boot task makes the call once per pod start (plus retries), and the probe reads its result. `ChainRpc` also records the time of its last successful response and its last error, for `/healthz`.
- **A stale tip is not a probe failure.** If the indexer falls behind, every web pod sharing the database would go unready at once, taking the whole site down instead of serving slightly old data. Tip age stays in `/healthz` for alerting.
- **Readiness and the schema:**
  - `WEB` opens the DB read-only, so it can't run migrations. `INDEXER` (or `ALL`) runs them, and sqlx serialises concurrent migrators with an advisory lock on PG.
  - `/readyz` compares `_sqlx_migrations` with the newest migration compiled into the binary. In a rolling deploy, new web pods stay out of rotation until the indexer has applied the new schema.
- **Shutdown:** on SIGTERM, `/readyz` flips to 503 while the existing graceful shutdown drains connections and the indexer finishes its batch (`src/main.rs` waits up to 10 s). `terminationGracePeriodSeconds` must exceed that; the default 30 s does.
- **Startup:** the HTTP listener binds first and serves only the probe routes until startup completes. The startup probe hits `/startupz`, which also implies everything `/livez` checks. Its `failureThreshold × periodSeconds` must cover the slowest start, including RPC retries: SQLite's 0001 can rebuild indexes on an old, large database, and a large PG migration can take minutes. Liveness and readiness don't run until it passes.
- **Before Stage 1 lands,** a long block write blocks the single tokio worker, and `/livez` would time out mid-write. Probes go live after 1.6 / 1.7, or until then with a liveness `timeoutSeconds × failureThreshold` above the longest write.
- **Per role:**
  - `WEB`: configure liveness and readiness. A startup probe isn't needed, since `WEB` starts in seconds, but `/startupz` still answers. Readiness gates the Service. Run 2+ replicas with a `PodDisruptionBudget`, so a drain never takes them all at once.
  - `INDEXER`: startup (with the RPC check) and liveness. It gets no Service traffic, so readiness only matters as a signal. Run one replica, since the indexer is the only writer (`pg_advisory_xact_lock` from 2.6 guards against a second one).
  - `ALL`: all three, startup with the RPC check.
- Example container spec for `INDEXER` / `ALL`, documented in `README.md`. A `WEB` deployment can leave out `startupProbe`. Thresholds are a starting point, to be tuned against measured start and write times:
  ```yaml
  startupProbe:
    httpGet: { path: /startupz, port: 8080 }
    periodSeconds: 10
    failureThreshold: 60     # up to 10 min for migrations and RPC retries
  livenessProbe:
    httpGet: { path: /livez, port: 8080 }
    periodSeconds: 10
    timeoutSeconds: 2
    failureThreshold: 3
  readinessProbe:
    httpGet: { path: /readyz, port: 8080 }
    periodSeconds: 5
    timeoutSeconds: 2
    failureThreshold: 2
  ```
- Fly and Render point their health checks at `/readyz`. Fly also gets a second check on `/livez`.
- The probe routes sit outside `/api/v1`. They are not in the OpenAPI spec, and the SPA fallback (5.14) never shadows them.
- **Tests (`tests/api.rs`):**
  - `/livez` stays 200 with the DB gone.
  - `/readyz` turns 503 with the DB gone, with the schema one migration behind, and after shutdown starts.
  - A stuck indexer heartbeat fails `/livez`.
  - For `INDEXER` and `ALL`, `/startupz` stays 503 while the stub RPC node is down or reports a different chain id, and turns 200 once it answers with `CHAIN_ID`.
  - With the stub RPC node down, `WEB` starts, and both its `/startupz` and its `/readyz` return 200.
  - After startup, a stub RPC node that stops answering leaves `/livez`, `/readyz` and `/startupz` at 200, and only `/healthz` reports it.
- **Files:**
  - new: `src/probes.rs` (routes, indexer heartbeat, schema check, boot RPC check)
  - `src/rpc.rs` (record the last successful response and last error in `ChainRpc`)
  - `src/main.rs` (probe-only listener for `INDEXER`, flip readiness on shutdown), `src/indexer.rs` (tick the heartbeat), `src/config.rs` (`INDEXER_STALL_SECONDS`, `RPC_STALE_SECONDS`), `src/web/mod.rs`
  - `fly.toml`, `render.yaml`
  - `README.md` (Kubernetes section with the probe spec), `.env.example`
  - `tests/api.rs`

---

## Stage 4 — JSON API `/api/v1`

For each resource, pull the data-gathering logic out of the Tera page handler into an async function that returns a typed DTO. The page handler and the new endpoint both call it until the cutover.

### 4.1 API scaffolding
- `Router` nested at `/api/v1`.
- JSON error type: `{ "error": { "code", "message" } }` with proper status codes. 404s come back as JSON, not HTML.
- `Page<T>` pagination DTO (`items`, `page`, `per_page`, `total`).
- `utoipa` setup that serves `/api/v1/openapi.json`, with the `utoipa-scalar` UI at `/api/v1/docs`.
- `GET /healthz`: a JSON report for people and monitoring: role, DB reachable, schema version, indexed tip and its age, indexer heartbeat age, and the RPC node's state: last successful call, last error, and the chain head against the indexed tip (lag in blocks). It returns 503 when the DB is unreachable, the RPC has failed for longer than `RPC_STALE_SECONDS`, or the tip is older than a threshold, so alerting can fire on each. Probes and platform health checks use `/livez` and `/readyz` (3.3) instead, so a stale tip never takes the site down.
- New `tests/api.rs`, added to the no-network test command in CI and AGENTS.md.
- **Files:**
  - `../Cargo.toml` (`utoipa`, `utoipa-scalar` with the `axum` feature)
  - new: `src/api/mod.rs`, `src/api/error.rs`, `src/api/page.rs`, `src/api/health.rs`
  - `src/web/mod.rs` (nest the router), `../src/lib.rs`
  - `tests/api.rs` (new)
  - `../.github/workflows/ci.yml`, `AGENTS.md`, `Makefile`, `README.md` (API docs link)

### 4.2 Blocks
- `GET /api/v1/blocks?page=`
- `GET /api/v1/blocks/{number|hash}`
- `GET /api/v1/blocks/{id}/txs`
- **Files:** `src/api/blocks.rs` (new), `src/web/block.rs` (extract the shared builder), `src/api/mod.rs`, `tests/api.rs`

### 4.3 Transactions: list and core detail
- `GET /api/v1/txs?page=`
- `GET /api/v1/txs/{hash}`: overview, status, fee breakdown, summary (`build_summary`), failure reason.
- **Files:** `src/api/txs.rs` (new), `src/web/tx.rs`, `../src/summary.rs` (DTO derives only), `tests/api.rs`

### 4.4 Transactions: calls, logs, balance changes
- `GET /api/v1/txs/{hash}/calls`: trace fetch, revert decoding, named unknown calls.
- `GET /api/v1/txs/{hash}/logs`
- `GET /api/v1/txs/{hash}/balance-changes`
- Receipt passthrough.
- **Files:** `src/api/txs.rs`, `src/web/tx.rs` (`fetch_missing_trace`, `resolve_call_statuses`, `enrich_balance_changes`, `decode_call_reverts`), `tests/api.rs`

### 4.5 Addresses
- `GET /api/v1/addresses/{addr}`: label, contract code/interface, virtual-address parse.
- `GET /api/v1/addresses/{addr}/txs`, `GET /api/v1/addresses/{addr}/transfers`, `GET /api/v1/addresses/{addr}/holdings`
- `POST /api/v1/labels`: batch lookup of address labels, used by the SPA in place of the old `address_label` template function.
- **Files:** `src/api/addresses.rs` (new), `src/web/address.rs`, `tests/api.rs`

### 4.6 Tokens
- `GET /api/v1/tokens`
- `GET /api/v1/tokens/{addr}`, `GET /api/v1/tokens/{addr}/holders`, `GET /api/v1/tokens/{addr}/transfers`
- Amounts come back as raw integer strings plus `decimals`.
- **Files:** `src/api/tokens.rs` (new), `src/web/token.rs`, `../src/tokens.rs` (DTO derives), `tests/api.rs`

### 4.7 Anchoring
- `GET /api/v1/anchoring`, `GET /api/v1/anchoring/{registry}`, `GET /api/v1/anchoring/{registry}/{record}`
- **Files:** `src/api/anchoring.rs` (new), `src/web/anchoring.rs`, `../src/anchoring.rs`, `tests/api.rs`

### 4.8 Home, stats, search, live feed
- `GET /api/v1/stats`: from `AppState.stats`. Since #59/#60 the stats blob already holds raw per-minute series for the last hour (`tx_per_minute`, `gas_per_minute`), so they pass through as they are. Drop only the `*_display` strings `home` adds.
- `GET /api/v1/search?q=`: the resolve-to-destination logic from `resolve_search`.
- `/api/v1/search/suggest` and `/api/v1/anchoring/search`: aliases of the existing `/api/search` and `/api/anchoring/search`.
- `/api/v1/events`: the SSE stream, also kept at `/api/events`. It is documented in the OpenAPI spec as `text/event-stream` with its two named events, `block` (`models::block_event_json`) and `stats`.
- The unversioned `/api/events`, `/api/search` and `/api/anchoring/search` stay permanently. Tera pages still open in browser tabs at cutover time use them.
- **Files:** new: `src/api/home.rs`, `src/api/search.rs`; `src/web/home.rs`, `src/web/search.rs`, `src/web/feed.rs`, `../src/models.rs` (DTO for the block event), `tests/api.rs`

### 4.9 SSE resume on reconnect
- Each `block` event carries `id: <block number>`.
- On connect, `events` reads the `Last-Event-ID` header. If present, it replays `id+1 ..= tip` from the DB, through the existing lag-replay path and bounded by `SSE_MAX_REPLAY`, instead of sending only the current tip.
- `EventSource` sends that header automatically when it reconnects, so neither the Tera page nor the SPA misses blocks after a network blip or a server restart.
- **Files:** `src/web/feed.rs` (`events`, `sse_step`, `SseState`), `tests/api.rs` (reconnect with `Last-Event-ID` gets the gap; extends `the_live_feed_follows_the_database`)

---

## Stage 5 — React SPA and one-shot cutover

Tasks 5.1–5.12 add `frontend/` without changing what the Rust server serves. From 5.4 on, every page is checked pixel by pixel against its Tera page. 5.13 rehearses the cutover on a staging Fly app. Production users keep seeing the Tera UI until 5.14 and 5.15 ship together.

### 5.1 Scaffold `frontend/` with Vite+
- Create the app with `vp create`, configured for React + TS, TanStack Router (file-based routes) and TanStack Query.
- Tailwind CSS v4 through `@tailwindcss/vite`, with a v3-compatibility layer in `frontend/src/styles/tailwind.css` so its output matches Tera's v3 Play CDN:
  - `@theme` pins the palette colors the templates use to their v3 hex values: `blue-300`, `blue-400`, `red-400`, `green-400`, `violet-300`, `white`. v4 redefined the palette in OKLCH, which changes the pixels.
  - The v3 preflight defaults that v4 changed: border color, placeholder color, button cursor.
  - v4 renamed some scale steps (`shadow-sm` → `shadow-xs`, `rounded` → `rounded-sm`, …). Ported markup uses the v4 names, as `@tailwindcss/upgrade` maps them.
- `shadcn init` with the Vite + Tailwind v4 setup: `components.json`, the `@/` path alias, `frontend/src/lib/utils.ts` (`cn`). Add components on demand as pages need them, not up front.
- pnpm via the `packageManager` field; pin the Vite+ version.
- `vite.config.ts` dev proxy: `/api` → `http://127.0.0.1:8080`.
- CI job runs `vp install`, `vp check`, `vp test`, `vp build`.
- **Files:**
  - new: `frontend/package.json`, `frontend/pnpm-lock.yaml`, `frontend/vite.config.ts`, `frontend/tsconfig.json`, `frontend/index.html`, `frontend/src/main.tsx`, `frontend/src/routes/__root.tsx`, `frontend/src/styles/tailwind.css`, `frontend/components.json`, `frontend/src/lib/utils.ts`
  - `../.github/workflows/ci.yml` (new `frontend` job)
  - `../.gitignore` (`frontend/node_modules`, `frontend/dist`)
  - `Makefile`, `../README.md`, `AGENTS.md` (frontend commands)

### 5.2 Generated API client
- Script runs `openapi-typescript` against `/api/v1/openapi.json` and writes `frontend/src/api/schema.d.ts`.
- Thin fetch wrapper; TanStack Query hooks per resource.
- CI check that the generated file is up to date.
- **Files:** new: `frontend/src/api/schema.d.ts`, `frontend/src/api/client.ts`, `frontend/src/api/queries.ts`, `frontend/scripts/gen-api.sh`; `../.github/workflows/ci.yml`

### 5.3 Layout shell and formatting library
- Port `../templates/base.html`: nav, search box with suggest, theme, footer, plus the 404/500 views.
- Render the same pixels as Tera, which 5.4 checks:
  - Keep the templates' markup and Tailwind class names (with the v4 renames from 5.1).
  - shadcn components carry Tera's classes, not shadcn's default theme. Use them where Tera has behaviour: tabs (tx, address, token), the search suggest popover, the copy button's feedback, tooltips.
  - Copy the inline `<style>` block of `base.html` verbatim into `frontend/src/styles/base.css`.
  - Load the same fonts (Inter, JetBrains Mono, from the same Google Fonts URL).
  - Keep the whitespace between inline elements. JSX drops the whitespace text nodes the templates have, and a missing space is a visible difference.
- Port the formatters to TS, with `vp test` unit tests:
  - from `src/web/*`: `format_time_ago`, `comma_num`, `block_pct`, `format_block_time`
  - from `../src/decoder.rs`: `truncate_hash`
  - from `../src/tokens.rs`: `format_token_amount`, using `BigInt`
- **Files:** new: `frontend/src/styles/base.css`, `frontend/src/components/ui/*` (shadcn, restyled), `frontend/src/components/Layout.tsx`, `frontend/src/components/SearchBox.tsx`, `frontend/src/components/AddressLink.tsx` (with the batched label lookup), `frontend/src/lib/format.ts`, `frontend/src/lib/format.test.ts`, `frontend/src/routes/__root.tsx`
- **Reference:** `../templates/base.html`, `templates/_macros.html`, `templates/404.html`, `templates/500.html`

### 5.4 Visual parity test: SPA vs Tera, pixel by pixel
Before Tera is deleted, every SPA page must render pixel-identical to the Tera page it replaces. The harness lands here, before the pages, so each page task (5.5–5.11) is checked as it lands rather than all at the end.
- **How it runs:**
  - One Playwright job starts the Rust server with Tera at `/`, and the built SPA (`vp preview`, proxying `/api` to that same server).
  - Both read the fixture DB that `../tests/pages.rs` uses, with the indexer off so no data moves between screenshots.
  - For each case it screenshots the Tera page and the SPA page in the same run and browser, then compares the two with `pixelmatch`.
- **No stored golden images while Tera exists.** Tera itself is the reference, rendered fresh every run. A change on the Tera side needs no baseline update, and differences between OS or font versions on CI machines can't creep in.
- **Matrix:**
  - Every route and tab in the URL list of 5.12, plus the 404 page.
  - Two viewports, desktop 1440×900 and mobile 390×844, since the templates use Tailwind's `sm:` / `md:` / `lg:` breakpoints.
  - Full-page screenshots. Tera has a single (dark) theme, so there is no theme axis.
- **Making the two renders comparable:**
  - **External assets:** Tera loads Tailwind from the unversioned Play CDN and fonts from Google Fonts. The test routes Tera's requests (`page.route`) to a vendored, pinned copy of the v3 Play CDN script, and both pages' font requests to the same font files, so a CDN release can't fail the run.
  - **Time:** Tera renders "Xm ago" on the server from the real clock (`format_time_ago`), and the script in `home.html` updates times from `Date.now()`. Freeze the browser clock (`page.clock`) at the same instant for both pages. Fixture timestamps sit days in the past, so a server-rendered value can't cross a bucket between the two renders.
  - **Motion:** screenshots use `animations: 'disabled'` and a hidden caret. Each shot waits for network idle and `document.fonts.ready`. The SSE live indicator is in its steady connected state on both pages: the server is up and no blocks arrive.
- **Pass rule:**
  - Zero differing pixels, using pixelmatch's per-pixel `threshold: 0.1` and its anti-aliasing detection. There is no overall percentage tolerance.
  - An accepted difference goes in `frontend/e2e/visual-allowlist.ts` as a masked region, with the reason and the PR that accepted it. The list should stay close to empty, and every entry gets reviewed.
- **On failure:** CI uploads the Tera, SPA and diff PNGs for each failing case as artifacts.
- **Gate:**
  - A required CI check from this task on. Each page task (5.5–5.11) adds its routes to the matrix and must pass.
  - 5.15 (delete Tera) merges only when the full matrix passes on `main`.
- **After Tera is gone:** 5.15 commits the last passing SPA screenshots as Playwright `toHaveScreenshot` baselines, so the SPA stays checked against the look it matched.
- **Fixture server:** Playwright needs the Rust server running on the fixture DB.
  - Move the fixture builders from 0.3's `tests/common/mod.rs` into `src/fixtures.rs`, behind a `fixtures` cargo feature, so both the tests and an example can use them.
  - Add `examples/serve_fixture.rs`, which builds the fixture DB and serves `app()`. 5.12 reuses it.
- **Files:**
  - new: `frontend/playwright.config.ts`, `frontend/e2e/visual.spec.ts`, `frontend/e2e/visual-allowlist.ts`, `frontend/e2e/vendor/` (pinned Tailwind CDN script, font files), `examples/serve_fixture.rs`, `src/fixtures.rs`
  - `frontend/package.json` (`@playwright/test`, `pixelmatch`, `pngjs`)
  - `../Cargo.toml` (`fixtures` feature), `src/lib.rs`, `tests/common/mod.rs`
  - `../.github/workflows/ci.yml` (required `visual-parity` job, artifact upload)

### 5.5 Home page with live feed
- SSE live feed: one shared `EventSource` on `/api/v1/events`.
  - `addEventListener('block')` prepends to the latest-blocks and latest-txs panels in the TanStack Query cache.
  - `addEventListener('stats')` replaces the stats query data.
  - Reconnects are left to the browser; the server resumes from `Last-Event-ID` (task 4.9).
  - Connection state shows in the UI (live / reconnecting).
- Activity charts: port `drawChart` from the `<script>` in `../templates/home.html` (an SVG of `tx_per_minute` / `gas_per_minute` with a peak label). The `stats` event redraws them.
- Other pages that show the tip (the blocks list, page 1) subscribe to the same hook rather than polling.
- **Files:** `frontend/src/routes/index.tsx`, `frontend/src/hooks/useBlockFeed.ts` (new), `frontend/src/hooks/useBlockFeed.test.ts` (new, with a mocked `EventSource`)
- **Reference:** `../templates/home.html`

### 5.6 Blocks list and block detail
- **Files:** `frontend/src/routes/blocks.tsx`, `frontend/src/routes/block.$blockId.tsx` (new)
- **Reference:** `../templates/blocks.html`, `templates/block.html`

### 5.7 Transactions list and tx overview
- **Files:** `frontend/src/routes/txs.tsx`, `frontend/src/routes/tx.$txHash.tsx` (new)
- **Reference:** `../templates/txs.html`, `templates/tx.html` (overview section)

### 5.8 Tx tabs: calls, logs, balance changes
- **Files:** new: `frontend/src/components/tx/CallsTab.tsx`, `frontend/src/components/tx/LogsTab.tsx`, `frontend/src/components/tx/BalanceChangesTab.tsx`
- **Reference:** `../templates/tx.html` (tab sections)

### 5.9 Address page
- **Files:** `frontend/src/routes/address.$address.tsx` (new)
- **Reference:** `../templates/address.html`

### 5.10 Tokens list and token page
- **Files:** `frontend/src/routes/tokens.tsx`, `frontend/src/routes/token.$address.tsx` (new)
- **Reference:** `../templates/tokens.html`, `templates/token.html`

### 5.11 Anchoring pages and search page
- **Files:** new: `frontend/src/routes/anchoring.index.tsx`, `frontend/src/routes/anchoring.$registryId.index.tsx`, `frontend/src/routes/anchoring.$registryId.$recordId.tsx`, `frontend/src/routes/search.tsx`
- **Reference:** `templates/anchoring*.html`, `../templates/search.html`

### 5.12 End-to-end tests and parity
- Playwright visits every route against the fixture server from 5.4 (`examples/serve_fixture.rs`), plus a CI job.
- The visual parity job (5.4) runs the full matrix here: every route and tab, both viewports, with an allowlist that is empty or fully reviewed.
- **URL parity:** every path the Tera router serves has a matching SPA route, so existing links keep working:
  - `/block/{number|hash}`, `/blocks`, `/txs`, `/tx/{hash}`, `/receipt/{hash}`
  - `/address/{a}`, `/token/{a}`, `/tokens`
  - `/anchoring`, `/anchoring/{registry}`, `/anchoring/{registry}/{record}`
  - `/search`

  The query params they accept must also match: `page`, `tab` (tx, address and token tabs), `from` (blocks), `q`.
- **Content parity:** port the facts that `../tests/pages.rs` asserts on rendered HTML into Playwright assertions on the same fixtures, e.g. what a failed tx says, the fee breakdown, and an unknown log still appearing. A page passes when it shows everything the Tera page showed.
- **Live feed:** a test that indexes a new block into the fixture DB and sees it appear on the home page without a reload.
- **Files:** new: `frontend/e2e/*.spec.ts`; `frontend/playwright.config.ts`, `../.github/workflows/ci.yml`
- **Reference:** `../tests/pages.rs`, `src/web/mod.rs` (`app()` route list)

### 5.13 Staging rehearsal on Fly
- A second Fly app (e.g. `nvnmchain-explorer-staging`) with its own volume and indexer, deployed from the 5.14 + 5.15 branch, so it runs the SPA build against live chain data.
- Walk the transition checklist on it:
  - every page and tab
  - SSE across a `fly machine restart`
  - wallet `?format=json` paths
  - `/startupz`, `/livez`, `/readyz` and `/healthz`, and a pod or machine restart while the probes watch
  - a deployment while a tab is open, checking that no chunk fails to load
- Tear it down or keep it as a permanent staging app.
- **Files:** `fly.staging.toml` (new), `../README.md` (staging + rollback runbook)

### 5.14 Cutover: serve the SPA at `/`
- Turn on the `tower-http` `fs` feature.
- Routing:
  - `/api/*`, `/healthz`, `/startupz`, `/livez` and `/readyz` are unchanged.
  - `/assets/*` is served from `STATIC_DIR` with `Cache-Control: public, max-age=31536000, immutable`. A missing asset is a 404, never the `index.html` fallback.
  - Every other path gets `index.html` with `Cache-Control: no-cache`.
- **Legacy JSON layer:** on the old page paths, requests with `?format=json` or `Accept: application/json` still get the existing JSON contexts, frozen in shape. Everything else gets the SPA.
- Add a `STATIC_DIR` setting.
- Dockerfile: a Node + pnpm stage that runs `vp build` and copies `dist` into the runtime image. Its `COPY` sits after the Rust layers, so frontend changes don't rebuild Rust.
- **No DB migration in this release**, so rollback is just redeploying the previous image.
- **Files:**
  - `../Cargo.toml`, `src/config.rs`, `.env.example`
  - `src/web/mod.rs`, `src/web/legacy.rs` (new; the JSON branch of `html_or_json` per page), `src/web/spa.rs` (new; static serving + cache headers)
  - `../Dockerfile`, `.github/workflows/docker.yml`, `fly.toml`, `render.yaml`
  - `tests/api.rs` (legacy-shape assertions moved from `../tests/pages.rs`; asset 404 and cache-header assertions)

### 5.15 Delete Tera (same release as 5.14)
- Remove:
  - `../templates`
  - `build_tera`, `render_html`, `html_or_json`, `page_ctx*`
  - `AppState.tera`
  - the HTML branches of the page handlers
  - the `tera` dependency
- Delete the tests that read rendered HTML, which the Playwright suite now covers:
  - `a_block_links_only_to_neighbours_that_are_indexed`
  - `the_transactions_listing_holds_every_indexed_transaction` (its HTML half)
  - `the_activity_charts_draw_the_stats_series`
  - `every_transaction_tab_renders`
  - `addresses_are_copyable_and_labelled`
  - `every_address_tab_renders` (status only)

  Move the remaining tests into `tests/api.rs`.
- **Files:**
  - `templates/**` (delete)
  - `src/web/render.rs` (delete)
  - `src/web/*.rs`, `../src/main.rs`, `Cargo.toml`
  - `../tests/pages.rs` (delete), `tests/live_rpc.rs` (`web_api_serves_indexed_data`)
  - `frontend/e2e/visual.spec.ts` (switch to `toHaveScreenshot`), `frontend/e2e/visual.spec.ts-snapshots/` (new baselines), `frontend/e2e/vendor/` (drop the Tailwind CDN copy)
  - `../Dockerfile` (drop `COPY templates`)
  - `../.github/workflows/ci.yml`, `AGENTS.md`, `README.md`
- **Before merging:** the full visual parity matrix (5.4) passes on `main`, where Tera still serves the pages.
- Replace the Tera-vs-SPA comparison with committed `toHaveScreenshot` baselines taken from that passing run. Without Tera there is nothing left to render the reference.
- **Done when:** `grep -rni "tera" src Cargo.toml Dockerfile` is empty.

### Cutover release runbook
1. Merge 5.14 and 5.15 back to back. `docker.yml` publishes `ghcr.io/<owner>/nvnmchain-explorer:sha-<commit>`.
2. Note the currently deployed image tag (`fly releases --image`) as the rollback target.
3. Deploy that exact image: `fly deploy --image ghcr.io/<owner>/nvnmchain-explorer:sha-<commit>`.
4. Smoke-test production:
   - `/startupz`, `/readyz`, `/livez`, `/healthz`
   - home live feed
   - one tx, address and token page
   - one wallet `?format=json` path
   - `/api/v1/docs`
5. On trouble: `fly deploy --image ghcr.io/<owner>/nvnmchain-explorer:sha-<prev>`. The data volume is untouched, because the release has no migration.

---

## Stage 6 — Retire the legacy `?format=json` layer

Starts once the SPA is fully in production and Tera is deleted (5.15). After that, `?format=json` on an old page path is the only thing those paths answer with JSON; everything else on them is the SPA. The layer still runs its own copy of the old page-handler code (`page_ctx`, server-side `comma_num`, time-ago, truncated hashes) just to produce that shape. Stage 6 moves its consumers to `/api/v1` and then removes it, without breaking anyone along the way.

### 6.1 (If removal is far off) Rebuild the legacy responses on top of `/api/v1`
- Worth doing only if 6.4 is months away, e.g. the wallet can't move soon. Otherwise the adapters are deleted again shortly after; skip to 6.2 and keep only the golden files from the first step, which 6.2 and 6.4 test against.
- **First, pin the current output.** For every legacy path and tab, against the fixture server (`examples/serve_fixture.rs`), record the response as a golden file. Time-dependent fields (time-ago strings) are normalised before comparing.
- Rewrite each legacy response as an adapter: call the same builder the `/api/v1` endpoint uses, then map its DTO to the old shape, formatting display strings the way the old handler did.
- This deletes the duplicate data-gathering in `src/web/legacy.rs`, so `/api/v1` is the only code that reads the DB for a page.
- **Done when:** every golden file still matches, and `src/web/legacy.rs` calls no `db::` function directly.
- **Files:** `src/web/legacy.rs`, `src/api/*` (expose the builders), `tests/legacy.rs` (new), `tests/fixtures/legacy/*.json` (new)

### 6.2 Measure use and announce the successor
- Count legacy requests per path, with `Origin` / `User-Agent`, in logs and metrics, to find every consumer, not just the wallet.
- Add to every legacy response:
  - `Deprecation` ([RFC 9745](https://www.rfc-editor.org/rfc/rfc9745)), marking the path as deprecated.
  - `Sunset` ([RFC 8594](https://www.rfc-editor.org/rfc/rfc8594)), with the removal date once one is set.
  - `Link: </api/v1/...>; rel="successor-version"`, pointing to the matching `/api/v1` endpoint.
- Publish a mapping in `README.md`: each legacy path → its `/api/v1` endpoint, and each display field → the raw field plus the client-side formatter (the SPA's `frontend/src/lib/format.ts` is the reference implementation).
- **Files:** `src/web/legacy.rs`, `README.md`, `tests/legacy.rs`

### 6.3 Move the wallet to `/api/v1`
- Done in the wallet's repository: switch each call to the `/api/v1` endpoint from the 6.2 mapping, and format on the client.
- Confirm with the 6.2 counts that the wallet's traffic on legacy paths has dropped to zero.
- **Files:** the wallet repo; none here.

### 6.4 Remove the legacy layer
- **Starts when:** legacy traffic has been zero (or only from consumers that agreed to the date) for an agreed period, e.g. 4 weeks after the `Sunset` date.
- Delete `src/web/legacy.rs` and its tests.
- **Don't let JSON clients fall through to the SPA.** Without this, `?format=json` on an old path would get `index.html` with a 200, and a client would fail parsing it with an obscure error. Instead, a JSON request on an old page path gets `410 Gone` with a JSON body `{ "error": { "code": "gone", "message", "successor" } }` naming the `/api/v1` endpoint. The shim is a few lines and can stay indefinitely.
- `CorsLayer::permissive()` was there for the wallet's cross-origin page calls. Narrow it to `/api/*` (or to the known origins) now that page paths serve only the SPA.
- **Files:** `src/web/legacy.rs` (delete), `src/web/mod.rs`, `src/web/spa.rs` (the 410 shim), `tests/legacy.rs` → `tests/api.rs`, `README.md`

## Dependency graph

```
Stage 0/1   0.1 ─┐
            0.2 ─┴─► 1.1 ─► 1.2 ─┬───────────────────► 1.6 ─┐
            0.2 ─► 0.4 ──────────┼─► {1.3 1.4 1.5} ────────┴─► 1.7 (+ drop legacy handle) ─► 1.8 (cleanup)
            0.3 ─────────────────┘
Stage 2     1.1 ─► 2.1 ─► 2.2 ─► 2.3 ─► {2.4 2.5} ─► 2.6 ─► 2.7   (2.2 also needs 1.2; each of 2.4–2.6 also needs its SQLite port)
                    └──► 2.8                              (PG runnable once 2.4–2.6 land and the legacy handle is gone; 1.8 not needed)
Stage 3     1.1 ─► 3.1 ─┬─► 3.2          (separate machines/pods: also once PG is runnable)
                        └─► 3.3          (probes; live after 1.6 / 1.7)
Stage 4     1.3 ─► 4.1 ─► {4.2 … 4.9}  (each also needs its own read port)
Stage 5     5.1 ─► 5.2 ─► 5.3 ─► 5.4 ─► {5.5 … 5.11} ─► 5.12 ─► 5.13 ─► 5.14 + 5.15 (one release)
Stage 6     5.15 ─► {6.1 (optional) 6.2} ─► 6.3 (wallet repo) ─► 6.4 (after zero legacy traffic)
            (5.1 starts on day one; 5.2 needs 4.1; 5.4 is the pixel-parity gate; each page task needs its endpoint)
```

### When each task can start

"Starts after" is the hard dependency. "In parallel with" lists the tasks that can be in progress at the same time.

| Task          | Starts after                                                                         | In parallel with                                          |
|---------------|--------------------------------------------------------------------------------------|-----------------------------------------------------------|
| 0.1, 0.2, 0.3 | nothing (0.2: once no other PR touches `../src/web.rs`)                              | each other, 5.1                                           |
| 5.1           | nothing: it only adds `frontend/` and a CI job                                       | everything                                                |
| 0.4           | 0.2                                                                                  | 0.3, 1.1, 1.2                                             |
| 1.1           | 0.2 (`AppState` lives in `src/web/mod.rs`). Land 0.1 first: 1.1 moves `../src/db.rs` | 0.3, 0.4                                                  |
| 1.2           | 1.1                                                                                  | 0.4, 3.1                                                  |
| 1.3, 1.4, 1.5 | 1.2, 0.4 (handlers can `.await`), 0.3 (shared fixtures)                              | each other, 1.6, Stage 2, 3.1, Stage 4 as it unlocks      |
| 1.6           | 1.2                                                                                  | 1.3–1.5, Stage 2, 3.1                                     |
| 1.7           | 1.3–1.6 (reuses their methods); then drops the legacy handle                         | Stage 2, 3.1, Stage 4                                     |
| 1.8           | 1.3–1.7 (cleanup; nothing waits on it, PG included)                                  | everything after it                                       |
| 2.1           | 1.1 (`Db` enum)                                                                      | Stage 1, 3.x, Stage 4, 5.x                                |
| 2.2 → 2.3     | 2.1 + 1.2 (migrations setup), then 2.2 + 0.3 (fixtures)                              | Stage 1, 2.8, 3.x, Stage 4, 5.x                           |
| 2.4, 2.5      | 2.3 + their SQLite ports: 2.4 ← 1.3 + 1.5, 2.5 ← 1.4 + 1.5                           | each other, the rest of Stage 1, 2.8, 3.x, Stage 4, 5.x   |
| 2.6           | 2.4 + 2.5 + 1.6 + 1.7                                                                | the rest of Stage 1, 2.8, 3.x, Stage 4, 5.x               |
| 2.7           | 2.4–2.6 (the dump needs the PG reads and write path)                                 | 2.8, 3.x, Stage 4, 5.x                                    |
| 2.8           | 2.1 (`DB_URI` exists)                                                                | 2.2–2.7, 3.x, Stage 4, 5.x                                |
| 3.1           | 1.1 (`ReadMode`)                                                                     | the rest of Stages 1–2, Stage 4                           |
| 3.2           | 3.1. Separate machines or pods also need PG runnable: 2.4–2.6 and the legacy handle gone (not 1.8)                | Stage 4, 5.x                                              |
| 3.3           | 3.1 (per-role behaviour), 1.2 (schema check). Turn on in k8s after 1.6 / 1.7         | 3.2, Stage 2, Stage 4, 5.x                                |
| 4.1           | 1.3 (`/healthz` reads the tip through the async `Db`)                                | 1.4–1.8, Stage 2, 3.x                                     |
| 4.2, 4.3, 4.4 | 4.1 (+ 1.3)                                                                          | each other and the rest of Stage 4                        |
| 4.5, 4.6      | 4.1 + 1.4                                                                            | each other and the rest of Stage 4                        |
| 4.7           | 4.1 + 1.5                                                                            | the rest of Stage 4                                       |
| 4.8           | 4.1 + 1.3–1.5                                                                        | the rest of Stage 4                                       |
| 4.9           | 4.1 (+ 1.3)                                                                          | the rest of Stage 4                                       |
| 5.2           | 5.1, 4.1 (`/api/v1/openapi.json`)                                                    | Stage 4                                                   |
| 5.3           | 5.2; `AddressLink` also needs 4.5 (`POST /api/v1/labels`)                            | Stage 4                                                   |
| 5.4           | 5.3 (the shell is the first thing compared), 0.3 (fixture builders)                  | Stage 4                                                   |
| 5.5           | 5.4 + 4.8 + 4.9                                                                      | 5.6–5.11                                                  |
| 5.6           | 5.4 + 4.2                                                                            | 5.5, 5.7–5.11                                             |
| 5.7, 5.8      | 5.4 + 4.3, 5.4 + 4.4                                                                 | the other page tasks                                      |
| 5.9, 5.10     | 5.4 + 4.5, 5.4 + 4.6                                                                 | the other page tasks                                      |
| 5.11          | 5.4 + 4.7 + 4.8                                                                      | the other page tasks                                      |
| 5.14, 5.15    | built once 4.1 and 4.8 exist; **merged** only after 5.13                             | 5.5–5.12 (developed, not merged)                          |
| 5.12          | 5.5–5.11                                                                             | 5.14, 5.15 (on their branch)                              |
| 5.13          | 5.12 and the 5.14 + 5.15 branch                                                      | —                                                         |
| 6.1, 6.2      | 5.15 (Tera is gone, the legacy layer is the last copy of the page code)              | each other                                                |
| 6.3           | 6.2 (the path → endpoint mapping)                                                    | 6.1                                                       |
| 6.4           | 6.3, and legacy traffic at zero past the `Sunset` date                               | —                                                         |

### Waves

What can be in flight at once, as the work progresses:
1. **Start now:** 0.1, 0.2, 0.3, 5.1.
2. **After 0.2:** 0.4 and 1.1 → 1.2. 3.1 and 2.1 start as soon as 1.1 lands; 2.2 → 2.3 follow once 1.2 and 0.3 are in.
3. **After 1.2, 0.3 and 0.4:** 1.3–1.5 at once, with 1.6 (which needs only 1.2) alongside; then 1.7, which drops the legacy handle. Each is followed by its PG port as soon as it lands (2.4, 2.5, then 2.6). Stage 4 starts when 1.3 lands (4.1 first), and each endpoint follows its read port. 5.2 → 5.3 → 5.4 start as soon as 4.1 lands, so the pixel-parity gate is in place before the first page.
4. **After 2.4–2.6 and the legacy handle drop:** PG is runnable, then 2.7, with 2.8 alongside. 1.8 cleans up whenever convenient; nothing waits on it. Stages 4 and 5 keep going: SPA pages (5.5–5.11) land as their endpoints do, and the 5.14 + 5.15 branch is built next to them. 3.2 can split onto separate machines or pods once PG is runnable (2.4–2.6 landed and the legacy handle dropped).
5. **Finish:** 5.12 (full pixel-parity matrix passes) → 5.13 (staging) → 5.14 + 5.15 merged back to back and released.
6. **After the cutover:** Stage 6 retires the legacy JSON layer. 6.4 waits on real traffic dropping to zero, so it runs on a calendar, not on other tasks.

### Notes
- `{…}` groups are independent of each other and can be reviewed in parallel.
- 1.3–1.5 wait for 0.4, because a sync Tera closure cannot `.await` an async `Db`. 1.6 needs only 1.2. 1.7 comes last of 1.3–1.7 and drops the legacy handle.
- Stage 4 needs only the async reads each endpoint calls, not 1.8 and not Postgres, so Stages 2 and 4 run in parallel.
- Stage 2 runs alongside Stage 1: each PG port follows its SQLite port. PG becomes runnable once 2.4–2.6 have landed and the legacy handle is out of running code (end of 1.7), with 2.1 never opening SQLite in PG mode. 1.8 is cleanup and gates nothing.
- A Stage 5 page task needs only its matching Stage 4 endpoint, not the whole of Stage 4.
- 3.3 (probes), 4.1 (`/healthz`) and 4.9 (SSE resume) ship well before the cutover, so they are already proven in production with Tera.
- 5.13 runs the 5.14 + 5.15 branch on staging; those two then merge back to back and deploy as one release.
- 3.1 during the port: `WEB` is fully read-only only once the legacy handle is dropped (end of 1.7). Until then that handle's `init_db` still runs its DDL and `seed_counters` on open.
- **Same files, so parallel PRs need a rebase:**
  - 1.6 and 1.7 both edit `../src/indexer.rs` (different functions).
  - 2.1, 2.8 and 3.1 edit `../src/config.rs` and `src/main.rs`.
  - 2.8, 3.2, 3.3 and 5.14 edit `../fly.toml` / `render.yaml`.
  - Each 4.x task registers its routes in `src/api/mod.rs`.
