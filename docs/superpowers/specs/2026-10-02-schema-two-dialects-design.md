# Phase 2: one schema, two dialects

Status: approved 2026-10-02; implemented
Date: 2026-10-02
Follows: `2026-10-01-db-boundary-design.md` (phase 1, merged as bb83764)

## Why

Phase 1 planned three steps:

1. Seal the boundary.
2. "Versioned schema migrations, on SQLite, designed for two dialects."
3. A Postgres backend.

This spec is step 2. It replaces "versioned migrations" with something that
fits how upstream actually evolves the schema.

### What upstream does

**It dropped its migration runner.** Upstream had a `PRAGMA user_version`
runner with `ALTER TABLE` steps and removed it in `a7ad1ae` (2026-08-08). All
three local databases (`fixtures/baseline/*.db` and `explorer.db`) report
`user_version = 0`.

**Since then, `init_db` is idempotent DDL:** `CREATE … IF NOT EXISTS` plus
`DROP INDEX/TABLE IF EXISTS`, with one comment per change. It changed in 21
upstream commits since 2026-08-01.

**Most schema changes arrive as whole tables**, refilled from the chain, often
against a `kv` watermark:

- `anchoring_events` (`0923c88`)
- `genesis_balances` (`593f401`)
- `counters` (`83bd4ea`)

Old tables are retired with `DROP TABLE IF EXISTS`. Index changes are a
`DROP INDEX IF EXISTS` of the old name and a `CREATE INDEX IF NOT EXISTS`
under a new one.

**Existing tables have still changed in place four times since `a7ad1ae`:**

| Commit    | Change                                                        | What happened to old databases                                            |
|-----------|---------------------------------------------------------------|---------------------------------------------------------------------------|
| `f6a5c27` | `UNIQUE (block_number, log_index)` added to `transfer_events` | Stayed on the old shape, with no warning.                                 |
| `d843f5b` | `registries` re-keyed                                         | A `pragma_table_info` probe dropped the table so it re-indexed.           |
| `e7ed388` | columns removed from `registries`                             | Same probe.                                                               |
| `2c088d2` | `anchored_events` re-keyed                                    | A probe (`start_over`) dropped the table and `blocks` so they re-indexed. |

Upstream deleted those probes in `d760898`. The next in-place change will
again leave old databases silently on the old shape, because `CREATE TABLE IF
NOT EXISTS` never changes an existing table.

### Decision

Derived data may be dropped and re-derived when the schema changes, either
from the chain or from other local tables. Existing rows do not need to be
preserved in place. That makes a numbered runner pointless:

- each upstream DDL change would have to be hand-translated into a step;
- replacing `init_db`'s body would turn every upstream schema commit into a
  merge conflict.

Instead:

1. SQLite keeps upstream's `init_db`, byte-identical.
2. Postgres gets its own idempotent schema file.
3. A **parity test** against a real Postgres server fails whenever the two
   schemas drift, for example after an upstream merge.
4. A **startup shape check** refuses to open a SQLite database whose existing
   tables no longer match what `init_db` would create. This catches the next
   `f6a5c27`.
5. A **baseline round-trip** test proves that the Postgres column types hold
   the real canary data exactly.

Production behaviour does not change, except that `db::open` can now refuse
to start. No runtime dependency is added.

## Constraints carried over from phase 1

Phase 1's upstream-merge rules still apply:

- new code goes in new files;
- no reordering, renames or SQL text changes in `db.rs`;
- `init_db` is off limits;
- seals are enforced by tests.

This phase changes `db.rs` in exactly two places:

- the body of `open`, a function the fork owns (added in phase 1);
- one `mod schema_check;` line next to the existing `mod indexer_jobs;`.

`db.rs` already imports `anyhow::Context`.

Upstream-owned files this phase touches, kept to the minimum:

- **`Cargo.toml`:** one dev-dependency line.
- **`README.md`:** one sentence replaced (section 5).
- **`AGENTS.md`:** one line added (section 5).

`ci.yml` does not change. The CI job goes in a new workflow file.

## Design

### 1. Postgres schema: `src/db/schema_pg.sql`

The file is new and fork-owned. It describes today's end state of `init_db`:
every table and named index, and no legacy `DROP`s, because no older Postgres
database exists. Every statement is `CREATE … IF NOT EXISTS`, so applying the
file twice is a no-op.

In phase 2 only `tests/postgres.rs` loads it, through
`include_str!("../src/db/schema_pg.sql")`. Phase 3's backend will load it the
same way.

**Type rules.** One rule per SQLite declared type, plus one exception. The
rules were checked against the stored value types in both baseline fixtures.
`canary-blocks.db` only has rows in `blocks`, `kv` and `counters`;
`canary-rich.db` has rows in every table except `selector_names`.

| SQLite declared                                            | Postgres                                              | Notes                                                                                                                               |
|------------------------------------------------------------|-------------------------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------|
| `INTEGER`                                                  | `BIGINT`                                              | `timestamp_ms` exceeds `int4`. Every binding in the code is `i64`.                                                                  |
| `TEXT`                                                     | `TEXT COLLATE "C"`                                    | `"C"` matches SQLite's default `BINARY` comparison, so ordering and equality agree whatever the server's locale.                    |
| `BLOB`                                                     | `BYTEA`                                               | Every `BLOB` column holds blobs (or `NULL`) in the fixtures…                                                                        |
| `BLOB` in `token_balances.token_addr` and `.holder_addr`   | `TEXT COLLATE "C"`                                    | …except these two, which are declared `BLOB` but hold `0x` text (29 of 29 rows). The declaration is wrong, not the data.            |
| `INTEGER PRIMARY KEY AUTOINCREMENT` (`transfer_events.id`) | `BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY` | `BY DEFAULT` lets a copy keep its ids (the round-trip test). A real import in phase 3 must also `setval` the sequence to `max(id)`. |
| `INTEGER PRIMARY KEY` (`blocks.number`)                    | `BIGINT PRIMARY KEY`                                  | No identity: the indexer always supplies the number.                                                                                |

**Defaults keep their values.** `DEFAULT X''` becomes `DEFAULT '\x'::bytea`.
Text defaults stay text (`'0'`, `''`, `'0x'`, `'[]'`), and integer defaults
stay integers.

**Constraints are copied as declared.** That covers `NOT NULL`,
`PRIMARY KEY`, the composite primary keys and every `UNIQUE (…)`, with the
same column lists. Postgres implies `NOT NULL` on primary-key columns, so the
file does not repeat it.

**Indexes keep their names and column lists**, `DESC` included.

**The partial index keeps its exact predicate.** `idx_tb_holding` uses the
`HOLDING` predicate (`balance NOT LIKE '-%'`) and the same
`LENGTH(balance) DESC, balance DESC` keys, so Postgres can still prove that
the read queries' filter implies it. A probe on Postgres 18.6 confirmed the
holders query uses the index.

**`receipt_data` and `trace_data` stay `TEXT`, not `JSONB`.** JSONB would
re-serialize the JSON, and pages would no longer receive the bytes that were
written.

**Out of scope:** pragmas, and the server settings that replace them. Those
belong to phase 3's connection setup.

### 2. Startup shape check: `src/db/schema_check.rs`

New file. `open` becomes:

```rust
/// Open (creating if needed) the database at `path` and bring its schema up to date.
pub fn open(path: &str) -> Result<Db> {
    let conn = init_db(path).map_err(|e| schema_check::explain(path, e))?;
    schema_check::verify(&conn).with_context(|| format!("schema of {path}"))?;
    Ok(Db(Arc::new(Mutex::new(conn))))
}
```

`init_db` itself does not change. Callers that use `init_db` directly (tests)
skip the check, as they do today.

**`pub(super) fn explain(path: &str, err: anyhow::Error) -> anyhow::Error`**
(added after review of PR #4). An old table can fail `init_db` before
`verify` runs: an index over a column it lacks fails `CREATE INDEX`, and a
`counters` table without `n` fails `seed_counters`. `explain` reopens the
file read-only, compares only the tables both it and the expected shape have,
and leaves indexes out, since `init_db` may have stopped before creating or
dropping them. With drift, it puts the same report and recoveries as `verify`
in front of `init_db`'s error; without, the error is unchanged; if the file
cannot be read, it logs a warning and returns the error unchanged.

It is a diagnosis after the failure, not a check before `init_db`: a check
first would refuse a database that an in-place fix in `init_db` (a guarded
`ALTER TABLE … ADD COLUMN`, as upstream has used before) was about to repair.

**`pub(super) fn verify(conn: &Connection) -> Result<()>`**

1. **Build the expected shape** by running `init_db(":memory:")`. If an
   in-memory database rejects one of `init_db`'s pragmas, use a file in a
   temporary directory instead. The first unit test settles which one works.
2. **Read the shape of both databases** from SQLite's own catalog:
   - **Tables:** `sqlite_master` with `type = 'table'`, excluding `sqlite_%`.
   - **Columns:** `pragma_table_info` gives each column's name, declared
     type, `NOT NULL`, default text and primary-key position, in column order.
   - **Indexes:** `pragma_index_list` and `pragma_index_xinfo` give each
     index's origin (`c` created, `u` unique, `pk`), whether it is unique,
     whether it is partial, and its key columns (a name or an expression, with
     direction).
     - A **created** index is also compared by its `sqlite_master.sql` text
       with case and whitespace dropped outside string literals, because the
       pragmas do not show a partial predicate or an expression. Dropping
       case and all whitespace, not just collapsing it, keeps a reformatted
       `init_db` statement from refusing every deployed database.
     - The table's `sqlite_master.sql` is checked for `AUTOINCREMENT`, which
       `pragma_table_info` does not show.
     - An **automatic** index (`sqlite_autoindex_*`, which backs a `UNIQUE` or
       a non-integer primary key) is compared by origin and columns, not by
       name. Its key columns carry direction and collation (collation names
       upper-cased), so a primary key's index stands in for
       `pragma_table_info`'s key list, which shows neither; a rowid-alias
       `INTEGER PRIMARY KEY` has no such index and keeps that list.
     - The collation of a column outside every key is not compared:
       `pragma_table_info` does not report it, and no index carries it.
3. **Compare** the two shapes:
   - **A table that is only in the file:** ignored. Upstream retires tables
     with `DROP TABLE IF EXISTS`, which `init_db` has already run.
   - **A table that is only in the expected shape:** reported as
     `<table>: missing`. Through `open` this cannot happen, because `init_db`
     just created the table, but `verify` must not panic on it.
   - **A table in both:** any difference in columns, primary key or automatic
     indexes is a **table drift**. A named index that differs, or a created
     index that exists only in the file, is an **index drift**. Retired
     indexes are already gone at this point, because `init_db` ran their
     `DROP INDEX IF EXISTS` first.
4. **On drift, return one error** that lists every difference, grouped by
   table and labelled by kind. For example:
   - `transfer_events: table: missing unique (block_number, log_index)`
   - `blocks: column epoch: default 1, expected 0`
   - `transfer_events: index idx_transfer_token_block: definition differs`

   Below each table, print that table's recovery hint from a static map in
   `schema_check.rs`:

   | Table                                                               | Recovery                                                                                                                             |
   |---------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------|
   | `blocks`, `transactions`, `transfer_events`, `token_metadata`, `kv` | No watermark exists, and backfill only walks below `MIN(blocks.number)`. Stop the explorer and re-index from an empty database file. |
   | `token_balances`                                                    | Drop it. `repair_derived_tables` rebuilds it from `transfer_events` and `genesis_balances` on the next start.                        |
   | `counters`                                                          | Drop it. `seed_counters` recounts it on open.                                                                                        |
   | `anchoring_events`                                                  | Drop it and delete the `kv` key `anchoring_backfilled_to`.                                                                           |
   | `genesis_balances`                                                  | Drop it and `token_balances`, and delete the `kv` key `genesis_balances_cursor`.                                                     |
   | `selector_names`                                                    | Drop it. It is a cache of directory lookups.                                                                                         |
   | any other table                                                     | No known recovery. Work it out before deploying, and add it to this map.                                                             |

   An **index drift** gets its own remedy, because it never needs data
   re-derived: `DROP INDEX <name>` and restart, and `init_db` rebuilds the
   index from the existing rows. The error may print that `DROP INDEX`
   statement, because it is safe, with the name double-quoted and any `"` in
   it doubled. It never prints a `DROP TABLE` statement.

**At deploy time.** The process exits with this error before it binds the
port, and nothing stays on the old release:

- `fly.toml` runs one machine with a mounted volume, which Fly replaces in
  place on deploy.
- `deploy/nvnmchain-explorer.service` (`Restart=always`) goes into a crash
  loop.

Either way the explorer is down until an operator does one of these:

- redeploys the previous image (`fly deploy --image …` with its
  `sha-<commit>` tag);
- applies the recovery and restarts.

There is no environment variable to skip the check: starting on a drifted
schema is the failure the check exists to prevent. Step 0 of the post-merge
recipe (section 5) keeps a drifting merge off `main` in the first place.

**Cost.** One in-memory `init_db` and a handful of pragma queries per `open`:
milliseconds, once per process.

### 3. Tests

#### 3a. Network-free (`cargo test --lib`)

These live in `#[cfg(test)] mod tests` inside `schema_check.rs`:

- A fresh `db::open` on a temp file passes.
- Each kind of drift is reported with its table named, its kind labelled and
  the right remedy:
  - a table recreated without one column;
  - a column whose default differs;
  - a table recreated without its `UNIQUE` (the `f6a5c27` case);
  - a named index recreated with a different column;
  - a partial index whose predicate differs.
- A table that exists only in the file (e.g. a leftover `contract_labels`)
  passes.
- **Both baseline fixtures pass `db::open`.**
  - Each `fixtures/baseline/<name>.db` is copied (the main file only) into a
    temp directory, and `db::open` runs on the copy. That is the production
    path: `init_db`, then `verify`. The committed fixture is never opened in
    place.
  - `canary-blocks.db` is a deployed explorer's database (`tests/baseline.rs:11`).
  - A merge that only adds tables, or adds or drops indexes, does not fail
    this test, because `init_db` applies those changes to the copy just as it
    would in production.
  - A merge that changes an existing table does fail it, and so does every
    deployed database. That failure has to show up on the sync PR (recipe
    step 0).

#### 3b. Against Postgres: `tests/postgres.rs`

**Opt-in, like the repo's other opt-in tests.** Every test is marked
`#[ignore = "needs PG_TEST_URL; see AGENTS.md"]`, following
`tests/baseline.rs`'s `build_baseline` and `tests/write_scale.rs`. Plain
`cargo test` therefore stays green without a server.

**How to run.** Run with `-- --include-ignored`, with `PG_TEST_URL` set:
in CI, or locally after `docker compose up -d --wait`. Each test reads
`PG_TEST_URL` through one helper, which panics with a message naming the
variable when it is unset. A job that runs the tests without the variable
therefore fails, and cannot pass by skipping. A run that reports
`0 passed; N ignored` is not a pass.

**Client.** `sqlx` 0.9 with only its `postgres` and `runtime-tokio` features
(no TLS, no macros), as a dev-dependency only. Each test holds one
`PgConnection`, not a pool, because `SET search_path` belongs to the session.
Multi-statement SQL (the schema file, the scratch setup) goes through
`sqlx::raw_sql`; SQL built at run time is wrapped in `AssertSqlSafe`, since
it interpolates only the test's own constants and catalog or fixture names.

**Isolation.**

- Each test works in its own schema, `t_<test>_<pid>`. The round-trip test
  uses one schema per fixture: `t_roundtrip_<fixture-stem>_<pid>`.
- Setup runs `DROP SCHEMA IF EXISTS <name> CASCADE; CREATE SCHEMA <name>;
  SET search_path TO <name>`, so a panicked earlier run cannot block a later
  one.
- The schema is dropped only when the test passes, so a failure leaves it for
  inspection.

The tests:

1. **Idempotence.** Apply `schema_pg.sql` twice. The second run succeeds and
   changes nothing in the catalog.
2. **Parity.** Build a fresh SQLite schema (`init_db` on a temp file) and a
   fresh Postgres schema, normalize both, and compare.
   - **Tables:** the set of table names.
   - **Columns:**
     - type class (`int`, `text` or `bytes`);
     - nullability;
     - the default as a typed value (casts stripped, quotes removed, `X''`
       and `'\x'::bytea` both becoming empty bytes);
     - for `text` columns, the collation, which must be `"C"`.
   - **Nullability rule:** a column is `NOT NULL` if it is declared so or is
     part of the primary key. On SQLite that is `notnull = 1 OR pk > 0`; on
     Postgres it is `is_nullable = 'NO'`. SQLite does not enforce `NOT NULL`
     on non-integer keys, but the code never writes a `NULL` key and neither
     fixture holds one, so this is not an intended difference.
   - **Auto ids:**
     - On SQLite, a column is `auto` when it is its table's only
       primary-key column, its declared type is `INTEGER`, and the table's
       `sqlite_master.sql` contains `AUTOINCREMENT`.
     - On Postgres, it is `auto` when `is_identity = 'YES'`.
     - A plain rowid alias such as `blocks.number` is not `auto`.
   - **Keys:** the primary key and every unique constraint, as ordered column
     lists.
   - **Named indexes:** an index created by `CREATE INDEX`. On SQLite that is
     origin `c`; on Postgres it is an index in the test schema that no
     `pg_constraint.conindid` points at. Each is compared by name, table,
     whether it is unique, and its key columns with direction.
     - Expression and partial indexes are spelled differently by each engine,
       so each is pinned in `TRANSLATED` (added after review of PR #4): its
       `sqlite_master.sql` from `init_db` and its `pg_get_indexdef` from
       `schema_pg.sql`, without the scratch schema's prefix. Compared
       ignoring case and whitespace outside quotes, each side must equal its
       pin, so a change to either fails until it is ported and the entry
       updated. A missing or stale entry fails too.
     - Indexes that back a key (`sqlite_autoindex_*`, `*_pkey`, `*_key`) are
       compared only as keys.
   - **Postgres catalog sources:** `information_schema.columns`,
     `pg_constraint` (`contype` `p` and `u`) and `pg_index`/`pg_class`, all
     filtered to the test schema.
   - **`ALLOWED`:** the intended differences.
     - Each entry is an exact tuple (table, column, field, SQLite value,
       Postgres value) with a reason. It suppresses only that one difference.
     - Today there are two entries, `(token_balances, token_addr, type, bytes,
       text)` and `(token_balances, holder_addr, type, bytes, text)`, because
       those columns are declared `BLOB` but hold `0x` text.
     - The collation rule cannot be exempted.
     - A stale entry, one that no longer matches a real difference, fails the
       test.
   - **On failure:** every difference is printed, e.g.
     `anchoring_events.record_id: missing from schema_pg.sql`.
3. **Baseline round-trip.** For each `fixtures/baseline/*.db`, in its own
   schema:
   - Apply `schema_pg.sql`.
   - Copy every row of every table except `sqlite_sequence`, one transaction
     per table, with a generic `INSERT`.
     - Each parameter's Rust type comes from the prepared statement's
       parameter types (`Statement::parameters()`): `Option<i64>` for
       `INT8`, `Option<String>` for `TEXT` and `Option<Vec<u8>>` for `BYTEA`.
     - A SQLite `NULL` is bound as `None` of that column's own type, because
       a bound `Option<T>` carries `T`'s Postgres type even when it is
       `None`.
     - A SQLite value whose storage class does not fit the column fails the
       test, naming the table, column and row key. This is the
       dynamic-typing trap the test exists to catch.
   - Read every table back and compare it row by row with the SQLite source.
     - Both sides are converted with `tests/baseline.rs`'s `to_json` rules:
       integers as numbers, text as strings, bytes as `0x` hex, and
       `receipt_data` parsed as JSON.
     - Rows are keyed by the `SPECS` keys, or by the primary key for tables
       outside `SPECS` (`kv`, `selector_names`).
     - No column is skipped, because the rows were copied, not re-indexed.

   A probe against Postgres 18.6 copied both fixtures this way (4654 and 2273
   rows) with zero differences.

**Shared code.** `Spec`, `SPECS`, `to_json` and the row-diff part of
`compare` move out of `tests/baseline.rs`:

- `tests/common/mod.rs` contains only `pub mod baseline;`. Use this layout,
  not `tests/common.rs`, which Cargo would build as its own test target.
- `tests/common/baseline.rs` holds the moved items.
- `tests/baseline.rs` and `tests/postgres.rs` each declare
  `#[allow(dead_code)] mod common;`. Each test crate compiles the module
  separately and uses a different subset of it (`postgres.rs` never reads
  `Spec::skip`), so without the attribute `clippy --all-targets -D warnings`
  fails. Use `allow`, not `expect`, which fails in the crate that uses every
  item.

`tests/baseline.rs` is fork-owned (added in #3), so this move costs nothing at
merge time, and the live baseline test's behaviour does not change. In phase
3, the same workflow adds the live re-index-into-Postgres baseline.

### 4. Infra

**`docker-compose.yml`** (repo root, new). Its header comment says it is for
development and tests only.

```yaml
services:
  postgres:
    image: postgres:18.<minor>          # pinned; same tag as .github/workflows/postgres.yml
    environment:
      POSTGRES_USER: explorer
      POSTGRES_PASSWORD: explorer
      POSTGRES_DB: explorer
      POSTGRES_INITDB_ARGS: --locale=C --encoding=UTF8
    ports: ["${PG_PORT:-5432}:5432"]
    volumes: [pgdata:/var/lib/postgresql]   # 18+ volume path; data in 18/docker
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U explorer -d explorer"]
      interval: 2s
      timeout: 3s
      retries: 30
volumes:
  pgdata:
```

The minor version is filled in with the current `18.x` tag when the file is
written. The tag appears in exactly two files, this one and `postgres.yml`,
and each carries a comment pointing at the other.

**CI: a new fork-owned workflow, `.github/workflows/postgres.yml`.** It uses a
GitHub service container, as in
<https://docs.github.com/en/actions/tutorials/use-containerized-services/create-postgresql-service-containers>:

```yaml
name: Postgres

on:
  push:
    branches: ["main"]
  pull_request:
    branches: ["main"]

env:
  RUSTFLAGS: "-D warnings"

jobs:
  postgres:
    name: Test (Postgres)
    runs-on: ubuntu-latest
    services:
      postgres:
        image: postgres:18.<minor>      # same tag as docker-compose.yml
        env:
          POSTGRES_USER: explorer
          POSTGRES_PASSWORD: explorer
          POSTGRES_DB: explorer
          POSTGRES_INITDB_ARGS: --locale=C --encoding=UTF8
        ports: ["5432:5432"]
        options: >-
          --health-cmd "pg_isready -U explorer -d explorer"
          --health-interval 2s --health-timeout 3s --health-retries 30
    steps:
      - uses: actions/checkout@v5
        with:
          submodules: true
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - name: Schema parity and baseline round-trip
        run: cargo test --test postgres -- --include-ignored
        env:
          PG_TEST_URL: postgres://explorer:explorer@localhost:5432/explorer
```

**Why a new file.** `ci.yml` is upstream-owned, and upstream extends its
`test` job at the end of the file. A job appended there would conflict on the
next such change. `ci.yml`'s `lint` job already compiles `tests/postgres.rs`
through `clippy --all-targets`.

**Server flags.** Phase 2 needs none. If phase 3 wants test-only flags:

- `fsync`, `full_page_writes` and `synchronous_commit` take effect on reload,
  so a `psql` step running `ALTER SYSTEM …; SELECT pg_reload_conf()` sets
  them.
- `max_connections` only takes effect on restart, so it goes into
  `POSTGRES_INITDB_ARGS` as `-c max_connections=…` (initdb `--set`, available
  since Postgres 16).

**Dependency.** `[dev-dependencies] sqlx = { version = "0.9",
default-features = false, features = ["runtime-tokio", "postgres"] }`.
Phase 3 promotes it to a normal dependency and adds a rustls TLS feature.
The `sqlite` feature stays off, because the SQLite backend stays on
`rusqlite`.

### 5. Docs

**A new fork-owned file, `docs/database.md`,** holds the full explanation:

- the schema model: idempotent DDL, new tables, re-derived data;
- the startup shape check and what its error means;
- the per-table recovery map;
- running Postgres locally: `docker compose up -d --wait`, then
  `PG_TEST_URL=postgres://explorer:explorer@localhost:5432/explorer cargo test --test postgres -- --include-ignored`.

**README, "Persistence & schema migrations"** (upstream-owned): replace only
the stale part of the "Migrations run automatically" bullet. Today it reads
"applies pending schema migrations tracked by SQLite's `PRAGMA user_version`
… (additive `ALTER TABLE` steps, logged at startup);". Replace that with:

> applies idempotent DDL (`CREATE … IF NOT EXISTS`, `DROP … IF EXISTS`); a
> changed column or key on an existing table makes the explorer refuse to
> start and name the table (see `docs/database.md`);

Keep the rest of the bullet word for word, including the token-balance
rebuild and the anchoring read-back, which are still true.

**AGENTS.md, "Tests"** (upstream-owned): add one line:

> Against Postgres: `docker compose up -d --wait`, then
> `PG_TEST_URL=postgres://explorer:explorer@localhost:5432/explorer cargo test --test postgres -- --include-ignored`

**Phase 1 spec, post-merge recipe.** Add these steps:

- **Step 0 (before step 1).** Sync upstream through a pull request:
  1. `git fetch upstream`.
  2. Merge `upstream/main` into a `sync/<date>` branch.
  3. Open a PR to `main`, and run steps 1-5 on that branch.
  4. Merge only when CI is green, including the fixture shape test and the
     `Postgres` workflow.

  Do not use GitHub's "Sync fork" button, which every past sync used
  (`4123460`, `84392e0`, `c3d4a1e`, `7ce6608`). It pushes straight to `main`,
  where `docker.yml` publishes `:latest` whether or not CI passes.
- **Step 4.** If `cargo test --test postgres -- --include-ignored` fails on
  parity, port the `init_db` change to `schema_pg.sql`, using this spec's
  type rules.
- **Step 5.** If the fixture shape test fails, the merge changed the columns
  or keys of an existing table, and every deployed database will refuse to
  start. Before merging the sync PR:
  1. Pick that table's recovery from the map in `schema_check.rs`. Add an
     entry if the table has none.
  2. Regenerate each affected fixture over its own block range.
     `build_baseline` never overwrites a file and defaults to `RICH_RANGES`,
     so delete the old file first:

     ```text
     rm fixtures/baseline/canary-rich.db
     BASELINE_BUILD=fixtures/baseline/canary-rich.db \
         cargo test --test baseline build_baseline -- --ignored --nocapture
     rm fixtures/baseline/canary-blocks.db
     BASELINE_BUILD=fixtures/baseline/canary-blocks.db BASELINE_RANGES=1579026-1583674 \
         cargo test --test baseline build_baseline -- --ignored --nocapture
     ```

  3. Update `canary-blocks.db`'s line in `tests/baseline.rs`'s doc comment. It
     is now rebuilt by `build_baseline` at `<commit>`, no longer the deployed
     copy, unless a fresh copy of the deployed database, taken after the
     recovery, replaces it.

## Recorded for phase 3 (decided, not built here)

### Sync vs async

**Keep the public `db::*` API synchronous**, and write the Postgres backend as
async code:

- `sqlx` 0.9 (`postgres`, `runtime-tokio`), with its own `PgPool`;
- rustls through sqlx's `tls-rustls-ring-webpki` feature;
- a private multi-thread runtime.

The two meet in one bridge, `run(fut)`, which checks `Handle::try_current()`:

| Caller                                    | `run` does                                  |
|-------------------------------------------|---------------------------------------------|
| multi-thread runtime (production)         | `block_in_place(\|\| handle.block_on(fut))` |
| current-thread runtime (`#[tokio::test]`) | `handle.block_on` on a scoped thread        |
| no runtime                                | `handle.block_on(fut)`                      |

Why:

- **A plain sync call that blocks on a channel freezes the runtime.** In a
  local probe on a 1-worker runtime (Fly runs `cpus = 1`), it stalled the
  runtime for up to 215 ms. It deadlocked when a timer from the caller's
  runtime crossed into the database future.
- **The bridge removes the stall.** The worst delay was 3 ms, at a cost of
  0.1–9 µs per call.
- **An async public API would add hand work to most upstream merges.** Of the
  non-merge upstream commits up to `593f401`, 29 of the 41 that touch `web.rs`
  and 22 of the 33 that touch `indexer.rs` add or remove a line containing
  `db::`. To recount:

  ```text
  git log --no-merges -G'db::' 593f401 -- src/web.rs | grep -c ^commit
  ```

  and compare with the same command without `-G`.
- **Async would not fix the real cost on a remote database, which is round
  trips.** They come from per-row loops in `save_block_bundles`,
  `adjust_balance`'s read-then-write, and the Tera `address_label` N+1. Those
  need set-based SQL under either model.
- **The decision can be revisited.** The Postgres core is async, so moving to
  an async API later means deleting the bridge, not rewriting the backend.

**Bridge rules.**

- Every tokio resource is created inside the database future.
- A thread-local guard panics on a re-entrant `db::*` call.
- No `LocalSet`.
- The seal test's forbidden names gain `sqlx` and `block_in_place`.

### Also decided for phase 3

**Layout.**

- `git mv src/db.rs src/db/sqlite.rs`. A merge probe merged two rounds of
  upstream-style edits into the moved file with 0 conflicts.
- `src/db/mod.rs` holds `pub struct Db(Backend)`, `open` choosing the backend
  by a `postgres://` or `postgresql://` prefix, and one thin wrapper per
  public fn.
- New files: `src/db/bridge.rs` and `src/db/pg/*.rs`.
- Stages:
  - **3a.** The move and dispatch, SQLite only. The baseline stays identical.
  - **3b.** The Postgres backend, plus CI.
  - **3c.** Performance.

**One writer per database.** A writer pool of size 1, plus session level
`pg_try_advisory_lock` keyed by database and schema. Readers get a read-only
pool, about 8 connections, with `statement_timeout`.

**Set-based writes.** The target is about 12 statements per batch in
`save_block_bundles`; today it loops per row.

- Deduplicate blocks by number.
- Net the balance deltas in Rust.
- Upsert token metadata before balances move.

**Writer retries** on SQLSTATE `08xxx`, `57P0x`, `40001` and `40P01`.

**Error handling.**

- No read error may become a default value inside a write: `adjust_balance`,
  `bigint()`, and `save_anchoring_window`'s `stamp`, where a failed stamp
  fails the window and leaves the watermark alone.
- The `let _ =` swallows at `db.rs:613` and `web.rs:1567` are fixed, as
  AGENTS.md requires.
- The database gets a readiness signal, because Fly's health check on `/`
  keeps passing when the database is gone.

**SQL dialect.**

- `CAST(NULL AS BYTEA)` in `TX_LIST_COLS`.
- Read `EXISTS` as `bool`.
- `CAST(SUM(…) AS BIGINT)`.
- Alias every derived table.
- Qualify `counters.n` in the upsert.
- `lower()` or `ILIKE` for search, because SQLite's `LIKE` is
  case-insensitive.
- Tie-break sort keys.
- Never stream rows while writing on the same connection.
- `= ANY($1)` instead of variable-length `IN` lists.

**A Postgres version of the shape check.** On open, compare the live schema
against `schema_pg.sql` applied to a scratch schema. This replaces the version
row the sync-vs-async analysis suggested.

**Credentials.** A `DbUrl` newtype with redacting `Display` and `Debug`,
covering `main.rs`'s startup log, `Settings`' `Debug` and `init_db`'s
`open db {path}` context.

**TLS.** `sslmode=require` for non-local hosts.

**Fix the Tera N+1.** `block.html` uses `previous_block`, and `address_label`
reads a cache the writer invalidates.

**Tests.**

- Tests that open a raw connection (`db::lock`, `init_db`) stay SQLite-only.
- The `Postgres` workflow gains:
  - the network-free suite on Postgres;
  - a network-free replay of the baseline fixtures on both backends;
  - the live re-index baseline into Postgres;
  - statement-count budgets.

### Open questions for phase 3

- Where Postgres runs relative to Fly `sin`, and the expected round-trip time.
- Whether a pooler sits in front. PgBouncer in transaction mode breaks named
  prepared statements.
- How many explorer processes share one database.
- Whether existing SQLite data is imported or re-indexed.
- The minimum Postgres version to support.
- What a page does when Postgres is down: an empty page with a warning, or a
  503.
- Whether `synchronous_commit=off` on the writer is acceptable.

## Out of scope

- Any Postgres code in `src/` beyond the schema file: no backend, no bridge,
  no `Db` enum.
- A numbered migration runner or version table.
- Changing `init_db`, any SQL in `db.rs`, or the `BLOB` declaration of
  `token_balances` on SQLite.
- Server tuning, pooling and TLS.
- Gating `docker.yml` on CI. That workflow is upstream's; step 0 of the recipe
  covers the risk instead.

## Acceptance

1. `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` pass,
   with `tests/baseline.rs` and `tests/postgres.rs` both compiling the shared
   `tests/common/` module.
2. `cargo test --lib --test decoder --test anchoring --test pages` passes with
   no network. That includes the shape-check unit tests and the fixture
   `db::open` test.
3. Plain `cargo test --test postgres` reports every test as ignored and fails
   none.
4. With `docker compose up -d --wait`,
   `cargo test --test postgres -- --include-ignored` passes locally:
   - idempotence;
   - parity, with an `ALLOWED` list of exactly two entries;
   - the round-trip of both baselines, with zero differences.

   The same command run without `PG_TEST_URL` fails, naming the variable.
   The `Postgres` workflow passes on the PR.
5. `cargo test --test live_rpc --test baseline` still passes where the RPC is
   reachable, after the move into `tests/common/`.
6. `git diff main -- src/db.rs` shows only the body of `open` and one added
   `mod schema_check;` line, and `git diff main -- .github/workflows/ci.yml`
   is empty.
7. Both deliberate-drift checks catch their change, on a scratch edit adding a
   column to `blocks` in `init_db`:
   - the parity test fails, naming the column;
   - `db::open` on a copy of `canary-blocks.db` refuses to start, names
     `blocks` and prints its recovery hint, and the fixture `db::open` test
     fails.

   Separately, a scratch edit that only adds a new index leaves both
   fixtures passing.
8. The README no longer mentions `PRAGMA user_version`, and still describes
   the token-balance rebuild and the anchoring read-back.
