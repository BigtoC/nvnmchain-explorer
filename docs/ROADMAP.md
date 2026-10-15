# Roadmap: dual-backend database (SQLite & PostgreSQL) + React SPA

## Goal

1. Replace `rusqlite` with an async driver so the same code serves **both** SQLite and PostgreSQL, with no `spawn_blocking` anywhere.
2. Make `ROLE=INDEXER` / `ROLE=WEB` real, so indexer and web can run as separate processes.
3. Replace the server-rendered Tera frontend with a React single-page app that talks to a JSON API, switched over in one release.

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
| Legacy `?format=json` | **Kept, frozen**: same paths, same shape, after the cutover                                                                    | Covers the wallet (cross-origin, `CorsLayer::permissive()`) without needing a list of the paths it calls. Removing it is a later, separate decision once consumers move to `/api/v1`.                                                                                         |
| API types             | `utoipa` OpenAPI spec → `openapi-typescript` generated TS types; interactive docs via `utoipa-scalar` at `/api/v1/docs`        | Keeps Rust DTOs and TS types from drifting apart.                                                                                                                                                                                                                             |
| Live updates          | SSE stays the live feed. `/api/events` and `/api/v1/events` serve the same stream; the SPA consumes it with `EventSource`      | Events gain `id:` = block number, and `Last-Event-ID` is honoured, so a reconnecting browser gets the blocks it missed (task 4.9).                                                                                                                                            |
| Tera                  | **Not kept.** Deleted in the cutover release.                                                                                  | Fly.io deploys the SPA from the same `../Dockerfile`; nothing on Fly depends on Tera.                                                                                                                                                                                            |
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
| Health check tied to the UI: Fly probes `GET /`, which renders Tera today | `/healthz` (DB reachable, tip age) and the Fly check moved to it long before the cutover                                                                                                       | 4.1  |
| Bookmarked or shared links break                                          | Every Tera path and query param (`page`, `tab`, `from`, `q`) has a matching SPA route, enforced by a test                                                                                      | 5.12 |
| The wallet's `?format=json` calls break                                   | Legacy JSON layer, same paths and shape                                                                                                                                                        | 5.14 |
| Tabs opened before the deploy still run the old Tera JS                   | `/api/events`, `/api/search`, `/api/anchoring/search` keep working as unversioned aliases                                                                                                      | 4.8  |
| Live feed drops blocks on reconnect                                       | SSE `id:` + `Last-Event-ID` resume                                                                                                                                                             | 4.9  |
| Pages missing data the Tera page showed                                   | Field-parity checklist per page, checked against the same fixture DB as `../tests/pages.rs`                                                                                                       | 5.12 |
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
- **Files:** new: `src/db/sqlite/blocks.rs`, `src/db/sqlite/txs.rs`; `src/db/mod.rs`, `src/web/{home,block,tx,render}.rs` (`page_ctx` becomes async), `src/web/feed.rs`, `../tests/pages.rs`

### 1.4 Port reads: addresses, tokens, holders, search
- **Files:** new: `src/db/sqlite/addresses.rs`, `src/db/sqlite/tokens.rs`; `src/db/mod.rs`, `src/web/{address,token,search}.rs`, `../tests/pages.rs`

### 1.5 Port reads: anchoring, kv, stats, counters
- **Files:** new: `src/db/sqlite/anchoring.rs`, `src/db/sqlite/kv.rs`; `src/db/mod.rs`, `src/web/anchoring.rs`, `src/web/home.rs`, `../src/signatures.rs`, `src/main.rs` (stats load at `:61`), `tests/anchoring.rs`

### 1.6 Port the block write path
- `save_block_bundles` / `write_block` in one `sqlx` transaction.
- `adjust_balance`, `apply_transfer_balances`, holder counts, `insert_transfer`, `insert_anchoring`. Keep the "was new" signal via `rows_affected()`.
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

### 1.7 Port maintenance paths
- `rebuild_token_balances`, `sync_holder_counts`, `save_genesis_balances`, `holders_without_genesis_balance`, the anchoring backfill transaction, and the stats write (`compute_and_store_stats`, which `stats_loop` in the indexer runs every interval).
- **Files:** `src/db/sqlite/maintenance.rs` (new), `../src/indexer.rs` (raw `db::lock` sites at `:591`, `:680-698`, `:1137`), `src/db/mod.rs`

### 1.8 Remove rusqlite
- Delete:
  - `src/db/legacy.rs`
  - `type Db = Arc<Mutex<Connection>>` and `lock()`
  - `OrWarn` (replace with explicit error logging, per AGENTS.md)
- Drop the `rusqlite` dependency and move all tests to `Db::open`.
- **Files:** `src/db/legacy.rs` (delete), `../Cargo.toml`, `src/main.rs`, `src/web/mod.rs`, `tests/common/mod.rs`, `tests/decoder.rs`, `tests/baseline.rs`, `tests/anchoring.rs`, `tests/write_scale.rs`, `tests/live_rpc.rs`
- **Done when:** `grep -rn "rusqlite\|spawn_blocking" src tests` is empty.

---

## Stage 2 — PostgreSQL backend

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
- **Files:** `../Cargo.toml` (sqlx `postgres`), `src/config.rs`, `.env.example`, `deploy/nvnmchain-explorer.env.example`, `README.md`, `src/db/mod.rs`, `src/db/pg/mod.rs` (new), `src/main.rs`

### 2.2 PG migrations
- Apply it at startup with `sqlx::migrate!("./migrations/postgres")`.
- docker-compose stops mounting the schema as an init script, so the app's migrator is the only path that creates tables.
- Freeze `0001_init.sql` once this lands. sqlx records each migration's checksum and refuses to start if an applied file changes, so later schema changes go in `0002_*.sql` and onward.
- A local volume created by the old init script has the tables but no `_sqlx_migrations` row. The migrator reruns 0001 there, which works because every statement is `IF NOT EXISTS`. The same goes for any PG database created before 2.2.
- The Dockerfile and `build.rs` already cover `migrations/postgres/` from 1.2.
- **Files:** `src/db/pg/mod.rs`, `docker-compose.yml`

### 2.3 Test matrix
- Behavior tests take a `Db` and run on SQLite always, and on PG when `DATABASE_URL` is set. `#[sqlx::test]` gives each test its own database.
- `DATABASE_URL` is the variable `#[sqlx::test]` reads, with the credentials inline (e.g. the docker-compose defaults). It is test-only, set in CI and locally, and separate from the app's `DB_URI` + `PG_*`.
- CI gets a `postgres:18` service.
- SQLite-introspection tests stay SQLite-only.
- **Files:** `tests/common/mod.rs`, `../tests/pages.rs`, `tests/anchoring.rs`, `.github/workflows/ci.yml`, `AGENTS.md`

### 2.4 PG reads: blocks, txs, kv, stats
- Dialect differences:
  - `$N` binds
  - typed NULLs in `TX_LIST_COLS`
  - `SUM(...)::bigint`
  - i64 `LIMIT`/`OFFSET`
- **Files:** new: `src/db/pg/blocks.rs`, `src/db/pg/txs.rs`, `src/db/pg/kv.rs`; `src/db/mod.rs`

### 2.5 PG reads: addresses, tokens, anchoring, search
- Case-insensitive token search: `lower(col) = lower($1 COLLATE "C")`, or `ILIKE` with `pg_trgm`, replacing `COLLATE NOCASE` / `LIKE`.
- `= ANY($1)` instead of the variable-length `IN` lists.
- **Files:** new: `src/db/pg/addresses.rs`, `src/db/pg/tokens.rs`, `src/db/pg/anchoring.rs`; `src/db/mod.rs`

### 2.6 PG write path
- Batched `UNNEST(...) ON CONFLICT` upserts; balances pre-read in one query per block batch.
- `pg_advisory_xact_lock` as a guard against two indexers.
- Maintenance paths (`rebuild_token_balances`, `sync_holder_counts`, genesis).
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

Needs only task 1.1 (`ReadMode` on `Db`), so it can run in parallel with the rest of Stages 1–2.

### 3.1 Honour `ROLE`
- `WEB`: no indexer loops; DB opened `ReadOnly` (SQLite `read_only(true)`, PG `default_transaction_read_only = on`).
- `INDEXER`: no HTTP server.
- `ALL`: unchanged.
- SSE works across processes, because the live feed already follows the database (`../tests/pages.rs` `the_live_feed_follows_the_database`).
- Home stats work the same way. The indexer's `stats_loop` writes them to the `kv` table, and `web::feed` reads them from there.
- `ReadOnly` is safe for `WEB`: since #49 no page handler writes to the database.
- **Files:** `../src/main.rs`, `src/config.rs` (drop the "informational only" docs), `.env.example`, `README.md`, `tests/pages.rs`

### 3.2 Deployment split
- Run indexer and web as separate processes or services.
- **Fly.io constraint:** a Fly volume attaches to one machine, and each process group in `../fly.toml` runs on its own machines. With SQLite, Fly keeps `ROLE=ALL` on a single machine. Splitting `INDEXER` and `WEB` onto separate Fly machines requires the Postgres backend (Stage 2), either Fly Managed Postgres or an external one.
- **Files:** `../fly.toml`, `render.yaml`, `deploy/entrypoint.sh`, `deploy/nvnmchain-explorer.service`, `deploy/install.sh`, `docker-compose.yml`, `README.md`

---

## Stage 4 — JSON API `/api/v1`

For each resource, pull the data-gathering logic out of the Tera page handler into an async function that returns a typed DTO. The page handler and the new endpoint both call it until the cutover.

### 4.1 API scaffolding
- `Router` nested at `/api/v1`.
- JSON error type: `{ "error": { "code", "message" } }` with proper status codes. 404s come back as JSON, not HTML.
- `Page<T>` pagination DTO (`items`, `page`, `per_page`, `total`).
- `utoipa` setup that serves `/api/v1/openapi.json`, with the `utoipa-scalar` UI at `/api/v1/docs`.
- `GET /healthz`: 200 when the DB answers, and the indexed tip is no older than a threshold. Point the Fly and Render health checks at it, so the cutover no longer touches them.
- New `tests/api.rs`, added to the no-network test command in CI and AGENTS.md.
- **Files:**
  - `../Cargo.toml` (`utoipa`, `utoipa-scalar` with the `axum` feature)
  - new: `src/api/mod.rs`, `src/api/error.rs`, `src/api/page.rs`, `src/api/health.rs`
  - `src/web/mod.rs` (nest the router), `../src/lib.rs`
  - `../fly.toml` (`[[services.http_checks]] path = '/healthz'`), `render.yaml` (`healthCheckPath`)
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
  - `/healthz`
  - a deploy while a tab is open, checking that no chunk fails to load
- Tear it down or keep it as a permanent staging app.
- **Files:** `fly.staging.toml` (new), `../README.md` (staging + rollback runbook)

### 5.14 Cutover: serve the SPA at `/`
- Turn on the `tower-http` `fs` feature.
- Routing:
  - `/api/*` and `/healthz` are unchanged.
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
   - `/healthz`
   - home live feed
   - one tx, address and token page
   - one wallet `?format=json` path
   - `/api/v1/docs`
5. On trouble: `fly deploy --image ghcr.io/<owner>/nvnmchain-explorer:sha-<prev>`. The data volume is untouched, because the release has no migration.

---

## Dependency graph

```
Stage 0/1   0.1 ─┐
            0.2 ─┴─► 1.1 ─► 1.2 ─┐
                                 ├─► {1.3 … 1.7} ─► 1.8
            0.2 ─► 0.4 ──────────┤
            0.3 ─────────────────┘
Stage 2     1.8 ─► 2.1 ─► 2.2 ─► 2.3 ─► {2.4 2.5 2.6} ─► 2.7
                    └──► 2.8
Stage 3     1.1 ─► 3.1 ─► 3.2          (separate machines/pods: also after 2.4–2.6)
Stage 4     1.3 ─► 4.1 ─► {4.2 … 4.9}  (each also needs its own read port)
Stage 5     5.1 ─► 5.2 ─► 5.3 ─► 5.4 ─► {5.5 … 5.11} ─► 5.12 ─► 5.13 ─► 5.14 + 5.15 (one release)
            (5.1 starts on day one; 5.2 needs 4.1; 5.4 is the pixel-parity gate; each page task needs its endpoint)
```

### When each task can start

"Starts after" is the hard dependency. "In parallel with" lists the tasks that can be in progress at the same time.

| Task          | Starts after                                                                                             | In parallel with                                 |
|---------------|----------------------------------------------------------------------------------------------------------|--------------------------------------------------|
| 0.1, 0.2, 0.3 | nothing (0.2: once no other PR touches `../src/web.rs`)                                                     | each other, 5.1                                  |
| 5.1           | nothing: it only adds `frontend/` and a CI job                                                           | everything                                       |
| 0.4           | 0.2                                                                                                      | 0.3, 1.1, 1.2                                    |
| 1.1           | 0.2 (`AppState` lives in `src/web/mod.rs`). Land 0.1 first: 1.1 moves `../src/db.rs`                        | 0.3, 0.4                                         |
| 1.2           | 1.1                                                                                                      | 0.4, 3.1                                         |
| 1.3, 1.4, 1.5 | 1.2, 0.4 (handlers can `.await`), 0.3 (shared fixtures)                                                  | each other, 1.6, 1.7, 3.1, Stage 4 as it unlocks |
| 1.6, 1.7      | 1.2                                                                                                      | each other, 1.3–1.5, 3.1                         |
| 1.8           | 1.3–1.7                                                                                                  | 3.x, Stage 4, 5.x                                |
| 2.1           | 1.8                                                                                                      | 3.x, Stage 4, 5.x                                |
| 2.2 → 2.3     | 2.1, then 2.2                                                                                            | 2.8, 3.x, Stage 4, 5.x                           |
| 2.4, 2.5, 2.6 | 2.3                                                                                                      | each other, 2.8, 3.x, Stage 4, 5.x               |
| 2.7           | 2.4–2.6 (the dump needs the PG reads and write path)                                                     | 2.8, 3.x, Stage 4, 5.x                           |
| 2.8           | 2.1 (`DB_URI` exists)                                                                                    | 2.2–2.7, 3.x, Stage 4, 5.x                       |
| 3.1           | 1.1 (`ReadMode`)                                                                                         | the rest of Stages 1–2, Stage 4                  |
| 3.2           | 3.1. Separate machines or pods also need 2.4–2.6                                                         | Stage 4, 5.x                                     |
| 4.1           | 1.3 (`/healthz` reads the tip through the async `Db`)                                                    | 1.4–1.8, Stage 2, 3.x                            |
| 4.2, 4.3, 4.4 | 4.1 (+ 1.3)                                                                                              | each other and the rest of Stage 4               |
| 4.5, 4.6      | 4.1 + 1.4                                                                                                | each other and the rest of Stage 4               |
| 4.7           | 4.1 + 1.5                                                                                                | the rest of Stage 4                              |
| 4.8           | 4.1 + 1.3–1.5                                                                                            | the rest of Stage 4                              |
| 4.9           | 4.1 (+ 1.3)                                                                                              | the rest of Stage 4                              |
| 5.2           | 5.1, 4.1 (`/api/v1/openapi.json`)                                                                        | Stage 4                                          |
| 5.3           | 5.2; `AddressLink` also needs 4.5 (`POST /api/v1/labels`)                                                | Stage 4                                          |
| 5.4           | 5.3 (the shell is the first thing compared), 0.3 (fixture builders)                                      | Stage 4                                          |
| 5.5           | 5.4 + 4.8 + 4.9                                                                                          | 5.6–5.11                                         |
| 5.6           | 5.4 + 4.2                                                                                                | 5.5, 5.7–5.11                                    |
| 5.7, 5.8      | 5.4 + 4.3, 5.4 + 4.4                                                                                     | the other page tasks                             |
| 5.9, 5.10     | 5.4 + 4.5, 5.4 + 4.6                                                                                     | the other page tasks                             |
| 5.11          | 5.4 + 4.7 + 4.8                                                                                          | the other page tasks                             |
| 5.14, 5.15    | built once 4.1 and 4.8 exist; **merged** only after 5.13                                                 | 5.5–5.12 (developed, not merged)                 |
| 5.12          | 5.5–5.11                                                                                                 | 5.14, 5.15 (on their branch)                     |
| 5.13          | 5.12 and the 5.14 + 5.15 branch                                                                          | —                                                |

### Waves

What can be in flight at once, as the work progresses:
1. **Start now:** 0.1, 0.2, 0.3, 5.1.
2. **After 0.2:** 0.4 and 1.1 → 1.2. 3.1 starts as soon as 1.1 lands.
3. **After 1.2, 0.3 and 0.4:** 1.3–1.7 all at once. Stage 4 starts when 1.3 lands (4.1 first), and each endpoint follows its read port. 5.2 → 5.3 → 5.4 start as soon as 4.1 lands, so the pixel-parity gate is in place before the first page.
4. **After 1.8:** the Stage 2 chain, with 2.8 alongside it. Stages 4 and 5 keep going: SPA pages (5.5–5.11) land as their endpoints do, and the 5.14 + 5.15 branch is built next to them. 3.2 can split onto separate machines or pods once 2.4–2.6 land.
5. **Finish:** 5.12 (full pixel-parity matrix passes) → 5.13 (staging) → 5.14 + 5.15 merged back to back and released.

### Notes
- `{…}` groups are independent of each other and can be reviewed in parallel.
- 1.3 onward waits for 0.4, because a sync Tera closure cannot `.await` an async `Db`.
- Stage 4 needs only the async reads each endpoint calls, not 1.8 and not Postgres, so Stages 2 and 4 run in parallel.
- A Stage 5 page task needs only its matching Stage 4 endpoint, not the whole of Stage 4.
- 4.1 (with `/healthz`) and 4.9 (SSE resume) ship well before the cutover, so both are already proven in production with Tera.
- 5.13 runs the 5.14 + 5.15 branch on staging; those two then merge back to back and deploy as one release.
- 3.1 before 1.8: `WEB` is fully read-only only once 1.8 removes the rusqlite handle. Until then that handle's `init_db` still runs its DDL and `seed_counters` on open.
- **Same files, so parallel PRs need a rebase:**
  - 1.6 and 1.7 both edit `../src/indexer.rs` (different functions).
  - 2.1, 2.8 and 3.1 edit `../src/config.rs` and `src/main.rs`.
  - 2.8, 3.2, 4.1 and 5.14 edit `../fly.toml` / `render.yaml`.
  - Each 4.x task registers its routes in `src/api/mod.rs`.
