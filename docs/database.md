# Database schema

Fork-owned notes on how the explorer's schema evolves, what the startup check
refuses, and how to run the Postgres tests. The design is
`docs/superpowers/specs/2026-10-02-schema-two-dialects-design.md`.

## The schema model

`init_db` (`src/db.rs`) is idempotent DDL, run on every start:

- `CREATE TABLE IF NOT EXISTS` and `CREATE INDEX IF NOT EXISTS` for the
  current shape;
- `DROP TABLE IF EXISTS` and `DROP INDEX IF EXISTS` for what was retired.

There is no migration runner and no version number. Most schema changes add a
whole table, which the indexer refills from the chain or from other local
tables, often against a `kv` watermark. Derived data may be dropped and
re-derived; rows are never migrated in place.

What idempotent DDL cannot do is change a table that already exists. When
upstream changes an existing table's columns or keys (as `f6a5c27` added
`UNIQUE (block_number, log_index)` to `transfer_events`), `CREATE TABLE IF NOT
EXISTS` leaves every deployed database on the old shape. The startup check
exists to make that loud.

## The startup shape check

`db::open` runs `init_db`, then `src/db/schema_check.rs` compares the opened
database with what `init_db` builds in an empty in-memory database:

- every column's name, declared type, `NOT NULL` and default, and the column
  order;
- the primary key, `AUTOINCREMENT` and every `UNIQUE` constraint;
- every index written as `CREATE INDEX`: its key columns and direction,
  uniqueness, and its SQL (case and whitespace ignored), which alone shows a
  partial predicate or an expression.

A table only in the database is ignored: upstream retires tables with `DROP
TABLE IF EXISTS`, which has already run.

On any difference the explorer exits before it binds the port. On Fly (one
machine, replaced in place) and under systemd (`Restart=always`) the explorer
is then down until an operator either redeploys the previous image
(`fly deploy --image …:sha-<commit>`) or applies the recovery below and
restarts. There is no switch to skip the check: starting on a drifted schema
is the failure it prevents.

### Reading the error

```text
the database's tables differ from what this build creates, so it was not opened (see docs/database.md):
blocks: column finalized: missing
blocks: index idx_blocks_timestamp: definition differs
  recovery: no watermark exists, and the backfill only walks below MIN(blocks.number): stop the explorer and re-index from an empty database file.
  recovery for idx_blocks_timestamp: `DROP INDEX idx_blocks_timestamp;` and restart; init_db rebuilds it from the existing rows.
```

Lines labelled `column …` or `table: …` are **table drift**: only re-deriving
the table fixes them. Lines labelled `index …` are **index drift**: drop the
index and restart, and `init_db` rebuilds it from the rows already there.

### Recovery per table

The map lives in `recovery()` in `src/db/schema_check.rs`; this is a copy.

| Table                                                               | Recovery                                                                                                                             |
|---------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------|
| `blocks`, `transactions`, `transfer_events`, `token_metadata`, `kv` | No watermark exists, and backfill only walks below `MIN(blocks.number)`. Stop the explorer and re-index from an empty database file. |
| `token_balances`                                                    | Drop it. `repair_derived_tables` rebuilds it from `transfer_events` and `genesis_balances` on the next start.                        |
| `counters`                                                          | Drop it. `seed_counters` recounts it on open.                                                                                        |
| `anchoring_events`                                                  | Drop it and delete the `kv` key `anchoring_backfilled_to`.                                                                           |
| `genesis_balances`                                                  | Drop it and `token_balances`, and delete the `kv` key `genesis_balances_cursor`.                                                     |
| `selector_names`                                                    | Drop it. It is a cache of directory lookups.                                                                                         |
| any other table                                                     | No known recovery. Work it out before deploying, and add it to the map.                                                              |

Back up the database file before dropping anything.

### When `init_db` itself fails

The check runs after `init_db`. If an upstream change adds an index over a
column an old table lacks, `init_db`'s `CREATE INDEX` fails first, with
SQLite's own `no such column` error and no recovery hint. The fixture test
(below) catches that case on the sync PR too, because it opens copies of the
fixtures the same way.

## Upstream merges

Sync upstream through a pull request, not GitHub's "Sync fork" button, which
pushes straight to `main`, where `docker.yml` publishes `:latest` whether or
not CI passes. The full recipe is in
`docs/superpowers/specs/2026-10-01-db-boundary-design.md`. Two checks matter
for the schema:

- **`every_baseline_fixture_opens`** (in `cargo test --lib`) runs `db::open`
  on a copy of each `fixtures/baseline/*.db`. A merge that only adds tables or
  changes indexes passes, because `init_db` applies those to the copy as it
  would in production. A merge that changes an existing table fails it, and
  every deployed database would refuse to start the same way: pick the
  table's recovery (add one to the map if it has none) before merging, then
  regenerate the affected fixtures with `build_baseline`.
- **`schema_pg_matches_init_db`** (in `tests/postgres.rs`) fails when
  `init_db` changed and `src/db/schema_pg.sql` did not. Port the change using
  the type rules at the top of `schema_pg.sql`.

## Postgres

Phase 2 has no Postgres backend: only `src/db/schema_pg.sql` and the tests
that hold it to `init_db`. `tests/postgres.rs` checks that applying the file
twice changes nothing, that it matches `init_db` (with the intended
differences listed in `ALLOWED`), and that every baseline fixture's rows copy
into it and read back unchanged.

Run it locally:

```text
docker compose up -d --wait
PG_TEST_URL=postgres://explorer:explorer@localhost:5432/explorer \
    cargo test --test postgres -- --include-ignored
```

`PG_PORT=5433 docker compose up -d --wait` uses another host port; change the
URL to match. The tests are `#[ignore]`d so plain `cargo test` needs no
server, and with `--include-ignored` but no `PG_TEST_URL` they fail rather than
skip. Each test works in its own schema, `t_<test>_<pid>`, dropped only when
it passes, so a failed run leaves its tables to inspect.

CI runs the same command in `.github/workflows/postgres.yml`, against a
`postgres:18.6` service container. The image tag appears in that file and in
`docker-compose.yml`; change both together.
