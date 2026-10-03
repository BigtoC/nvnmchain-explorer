//! The Postgres schema, `src/db/schema_pg.sql`, against a real server.
//!
//! - **Idempotence:** applying the file twice changes nothing.
//! - **Parity:** the file describes what `init_db` creates, except for the
//!   differences `ALLOWED` names.
//! - **Round-trip:** every baseline fixture's rows survive a copy into it
//!   unchanged.
//!
//! Every test is ignored unless run with `--include-ignored`, and then fails
//! unless `PG_TEST_URL` names a server; a run that ignores them all is not a
//! pass. Each test works in a schema of its own, dropped only when it passes.
//!
//! ```text
//! docker compose up -d --wait
//! PG_TEST_URL=postgres://explorer:explorer@localhost:5432/explorer \
//!     cargo test --test postgres -- --include-ignored
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use rusqlite::types::Value as Sql;
use rusqlite::{Connection, OpenFlags};
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{Client, NoTls};

#[allow(dead_code)]
mod common;
use common::baseline::{columns, diff_rows, row_key, rows, tables, to_json, Rows, Spec, SPECS};

const SCHEMA: &str = include_str!("../src/db/schema_pg.sql");

/// The differences between `init_db` and `schema_pg.sql` that are intended:
/// (table, column, field, SQLite, Postgres), and why. Each suppresses that one
/// difference; one that no longer occurs fails the parity test.
const ALLOWED: &[(&str, &str, &str, &str, &str, &str)] = &[
    (
        "token_balances",
        "token_addr",
        "type",
        "bytes",
        "text",
        "declared BLOB on SQLite, but every row holds 0x text",
    ),
    (
        "token_balances",
        "holder_addr",
        "type",
        "bytes",
        "text",
        "declared BLOB on SQLite, but every row holds 0x text",
    ),
];

/// Row keys for the tables `SPECS` leaves out, which the round-trip copies too.
const UNINDEXED: &[Spec] = &[
    Spec {
        table: "kv",
        key: &["key"],
        skip: &[],
    },
    Spec {
        table: "selector_names",
        key: &["selector"],
        skip: &[],
    },
];

/// The schema a test's statements resolve in, as a `pg_namespace` oid.
const HERE: &str = "(SELECT oid FROM pg_namespace WHERE nspname = current_schema())";

// ---------------------------------------------------------------------------
// A scratch schema per test
// ---------------------------------------------------------------------------

/// A connection whose `search_path` is a fresh schema of the test's own.
struct Scratch {
    client: Client,
    schema: String,
}

fn pg_error(e: &tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => format!("{} ({})", db.message(), db.code().code()),
        None => e.to_string(),
    }
}

/// Set up schema `t_<test>_<pid>`, dropping what an earlier failed run left.
async fn scratch(test: &str) -> Scratch {
    let url = std::env::var("PG_TEST_URL").unwrap_or_else(|_| {
        panic!(
            "PG_TEST_URL is not set; point it at a Postgres server, e.g. after \
             `docker compose up -d --wait` (see AGENTS.md)"
        )
    });
    let (client, connection) = tokio_postgres::connect(&url, NoTls)
        .await
        .unwrap_or_else(|e| panic!("connect to PG_TEST_URL: {}", pg_error(&e)));
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("postgres connection: {}", pg_error(&e));
        }
    });
    let schema = format!("t_{test}_{}", std::process::id());
    client
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema}; \
             SET search_path TO {schema}"
        ))
        .await
        .unwrap_or_else(|e| panic!("set up schema {schema}: {}", pg_error(&e)));
    Scratch { client, schema }
}

impl Scratch {
    async fn apply_schema(&self) {
        self.client
            .batch_execute(SCHEMA)
            .await
            .unwrap_or_else(|e| panic!("apply schema_pg.sql: {}", pg_error(&e)));
    }

    /// Drop the schema. Only a passing test calls this, so a failed one
    /// leaves its tables to inspect.
    async fn finish(self) {
        self.client
            .batch_execute(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .await
            .unwrap_or_else(|e| panic!("drop schema {}: {}", self.schema, pg_error(&e)));
    }
}

// ---------------------------------------------------------------------------
// Idempotence
// ---------------------------------------------------------------------------

/// Everything the schema holds, one line per column, constraint, relation and
/// index definition.
async fn catalog(client: &Client) -> Vec<String> {
    let sql = format!(
        "SELECT 'column ' || table_name || '.' || column_name || ' ' || data_type
                || ' ' || is_nullable || ' ' || coalesce(column_default, '-')
                || ' ' || is_identity || ' ' || coalesce(collation_name, '-')
           FROM information_schema.columns WHERE table_schema = current_schema()
         UNION ALL
         SELECT 'constraint ' || conname || ' ' || pg_get_constraintdef(oid)
           FROM pg_constraint WHERE connamespace = {HERE}
         UNION ALL
         SELECT 'relation ' || relname || ' ' || relkind::text
           FROM pg_class WHERE relnamespace = {HERE}
         UNION ALL
         SELECT 'index ' || indexdef FROM pg_indexes WHERE schemaname = current_schema()
         ORDER BY 1"
    );
    let rows = client.query(&sql, &[]).await.unwrap();
    rows.iter().map(|r| r.get(0)).collect()
}

#[tokio::test]
#[ignore = "needs PG_TEST_URL; see AGENTS.md"]
async fn applying_the_schema_twice_changes_nothing() {
    let pg = scratch("idempotence").await;
    pg.apply_schema().await;
    let once = catalog(&pg.client).await;
    assert!(!once.is_empty(), "schema_pg.sql created nothing");
    pg.apply_schema().await;
    assert_eq!(catalog(&pg.client).await, once);
    pg.finish().await;
}

// ---------------------------------------------------------------------------
// Parity
// ---------------------------------------------------------------------------

/// A column default as a value, whichever engine spelled it.
#[derive(Clone, Debug, PartialEq)]
enum Lit {
    Int(i64),
    Text(String),
    Bytes(String),
    Other(String),
}

impl fmt::Display for Lit {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Lit::Int(n) => write!(f, "{n}"),
            Lit::Text(s) => write!(f, "'{s}'"),
            Lit::Bytes(hex) => write!(f, "x'{hex}'"),
            Lit::Other(raw) => write!(f, "{raw}"),
        }
    }
}

fn unquote(s: &str) -> Option<String> {
    let inner = s.strip_prefix('\'')?.strip_suffix('\'')?;
    Some(inner.replace("''", "'"))
}

fn sqlite_lit(raw: &str) -> Lit {
    if let Some(hex) = raw
        .strip_prefix("X'")
        .or_else(|| raw.strip_prefix("x'"))
        .and_then(|s| s.strip_suffix('\''))
    {
        Lit::Bytes(hex.to_lowercase())
    } else if let Some(text) = unquote(raw) {
        Lit::Text(text)
    } else if let Ok(n) = raw.parse() {
        Lit::Int(n)
    } else {
        Lit::Other(raw.to_string())
    }
}

/// `'\x'::bytea`, `'0'::text` or `0`, with the casts stripped.
fn pg_lit(raw: &str) -> Lit {
    let mut s = raw.trim();
    let mut cast = None;
    while let Some(i) = s.rfind("::") {
        if s[i..].contains('\'') {
            break;
        }
        cast = Some(s[i + 2..].trim());
        s = s[..i].trim();
    }
    match (unquote(s), cast) {
        (Some(text), Some("bytea")) => match text.strip_prefix("\\x") {
            Some(hex) => Lit::Bytes(hex.to_lowercase()),
            None => Lit::Other(raw.to_string()),
        },
        (Some(text), _) => Lit::Text(text),
        (None, _) => s
            .parse()
            .map(Lit::Int)
            .unwrap_or_else(|_| Lit::Other(raw.to_string())),
    }
}

#[derive(Debug, PartialEq)]
struct Col {
    /// `int`, `text` or `bytes`; anything else by its own name.
    class: String,
    not_null: bool,
    default: Option<Lit>,
    /// Filled in by the database when left out: `AUTOINCREMENT` or an identity.
    auto: bool,
}

#[derive(Debug, PartialEq)]
struct Idx {
    table: String,
    unique: bool,
    partial: bool,
    /// Key columns with direction, or `None` for an expression or partial
    /// index, whose text differs between engines by design.
    keys: Option<Vec<String>>,
}

#[derive(Default)]
struct Shape {
    columns: BTreeMap<String, BTreeMap<String, Col>>,
    /// Per table: `primary key (a, b)` and `unique (a, b)`.
    keys: BTreeMap<String, BTreeSet<String>>,
    /// Indexes written as `CREATE INDEX`, by name.
    indexes: BTreeMap<String, Idx>,
}

fn key(kind: &str, cols: &[String]) -> String {
    format!("{kind} ({})", cols.join(", "))
}

fn sqlite_shape(conn: &Connection) -> Shape {
    let mut shape = Shape::default();
    for table in tables(conn)
        .into_iter()
        .filter(|t| !t.starts_with("sqlite_"))
    {
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [&table],
                |r| r.get(0),
            )
            .unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT name, type, \"notnull\", dflt_value, pk
                 FROM pragma_table_info(?1) ORDER BY cid",
            )
            .unwrap();
        let cols: Vec<(String, String, bool, Option<String>, i64)> = stmt
            .query_map([&table], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let mut primary: Vec<(i64, String)> = cols
            .iter()
            .filter(|c| c.4 > 0)
            .map(|c| (c.4, c.0.clone()))
            .collect();
        primary.sort();
        let sole_key = primary.len() == 1;
        let autoincrement = sql.to_uppercase().contains("AUTOINCREMENT");

        let table_cols = shape.columns.entry(table.clone()).or_default();
        for (name, decl, not_null, default, pk) in cols {
            let decl = decl.to_uppercase();
            let class = match decl.as_str() {
                "INTEGER" => "int".to_string(),
                "TEXT" => "text".to_string(),
                "BLOB" => "bytes".to_string(),
                other => other.to_lowercase(),
            };
            table_cols.insert(
                name,
                Col {
                    auto: autoincrement && sole_key && pk == 1 && class == "int",
                    class,
                    not_null: not_null || pk > 0,
                    default: default.as_deref().map(sqlite_lit),
                },
            );
        }

        let keys = shape.keys.entry(table.clone()).or_default();
        if !primary.is_empty() {
            let cols: Vec<String> = primary.into_iter().map(|(_, c)| c).collect();
            keys.insert(key("primary key", &cols));
        }
        let mut stmt = conn
            .prepare("SELECT name, \"unique\", origin, partial FROM pragma_index_list(?1)")
            .unwrap();
        let listed: Vec<(String, bool, String, bool)> = stmt
            .query_map([&table], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for (index, unique, origin, partial) in listed {
            let mut stmt = conn
                .prepare(
                    "SELECT cid, name, \"desc\" FROM pragma_index_xinfo(?1)
                     WHERE key = 1 ORDER BY seqno",
                )
                .unwrap();
            let parts: Vec<(i64, Option<String>, bool)> = stmt
                .query_map([&index], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            let expression = parts.iter().any(|(cid, _, _)| *cid < 0);
            let names: Vec<String> = parts
                .into_iter()
                .map(|(_, name, desc)| {
                    let name = name.unwrap_or_else(|| "<expr>".into());
                    if desc {
                        format!("{name} DESC")
                    } else {
                        name
                    }
                })
                .collect();
            match origin.as_str() {
                "u" => {
                    keys.insert(key("unique", &names));
                }
                "c" => {
                    shape.indexes.insert(
                        index,
                        Idx {
                            table: table.clone(),
                            unique,
                            partial,
                            keys: (!expression && !partial).then_some(names),
                        },
                    );
                }
                // A non-integer primary key's index: the key already says it.
                _ => {}
            }
        }
    }
    shape
}

async fn pg_shape(client: &Client) -> Shape {
    let mut shape = Shape::default();
    let cols = client
        .query(
            "SELECT table_name::text, column_name::text, data_type::text,
                    is_nullable = 'NO', column_default::text, is_identity = 'YES'
             FROM information_schema.columns WHERE table_schema = current_schema()
             ORDER BY table_name, ordinal_position",
            &[],
        )
        .await
        .unwrap();
    for row in cols {
        let class = match row.get::<_, String>(2).as_str() {
            "bigint" => "int".to_string(),
            "text" => "text".to_string(),
            "bytea" => "bytes".to_string(),
            other => other.to_string(),
        };
        shape.columns.entry(row.get(0)).or_default().insert(
            row.get(1),
            Col {
                class,
                not_null: row.get(3),
                default: row.get::<_, Option<String>>(4).as_deref().map(pg_lit),
                auto: row.get(5),
            },
        );
    }

    let constraints = client
        .query(
            &format!(
                "SELECT t.relname::text, c.contype = 'p',
                        ARRAY(SELECT a.attname::text
                              FROM unnest(c.conkey) WITH ORDINALITY k(attnum, ord)
                              JOIN pg_attribute a
                                ON a.attrelid = c.conrelid AND a.attnum = k.attnum
                              ORDER BY k.ord)
                 FROM pg_constraint c JOIN pg_class t ON t.oid = c.conrelid
                 WHERE t.relnamespace = {HERE} AND c.contype IN ('p', 'u')"
            ),
            &[],
        )
        .await
        .unwrap();
    for row in constraints {
        let kind = if row.get(1) { "primary key" } else { "unique" };
        let cols: Vec<String> = row.get(2);
        shape
            .keys
            .entry(row.get(0))
            .or_default()
            .insert(key(kind, &cols));
    }

    let indexes = client
        .query(
            &format!(
                "SELECT ic.relname::text, tc.relname::text, i.indisunique,
                        i.indpred IS NOT NULL, i.indexprs IS NOT NULL,
                        ARRAY(SELECT coalesce(a.attname::text, '<expr>')
                                     || CASE WHEN i.indoption[k] & 1 = 1
                                             THEN ' DESC' ELSE '' END
                              FROM generate_series(0, i.indnkeyatts - 1) k
                              LEFT JOIN pg_attribute a
                                ON a.attrelid = i.indrelid AND a.attnum = i.indkey[k]
                              ORDER BY k)
                 FROM pg_index i
                 JOIN pg_class ic ON ic.oid = i.indexrelid
                 JOIN pg_class tc ON tc.oid = i.indrelid
                 WHERE tc.relnamespace = {HERE}
                   AND NOT EXISTS (SELECT 1 FROM pg_constraint c
                                   WHERE c.conindid = i.indexrelid)"
            ),
            &[],
        )
        .await
        .unwrap();
    for row in indexes {
        let (partial, expression): (bool, bool) = (row.get(3), row.get(4));
        shape.indexes.insert(
            row.get(0),
            Idx {
                table: row.get(1),
                unique: row.get(2),
                partial,
                keys: (!expression && !partial).then(|| row.get(5)),
            },
        );
    }
    shape
}

/// The `text` columns of the schema not collated `"C"`, which SQLite's
/// `BINARY` comparison matches whatever the server's locale.
async fn uncollated(client: &Client) -> Vec<String> {
    let rows = client
        .query(
            "SELECT table_name::text || '.' || column_name::text || ': collation '
                    || coalesce(collation_name::text, 'default') || ', must be \"C\"'
             FROM information_schema.columns
             WHERE table_schema = current_schema() AND data_type = 'text'
               AND coalesce(collation_name::text, '') <> 'C'
             ORDER BY 1",
            &[],
        )
        .await
        .unwrap();
    rows.iter().map(|r| r.get(0)).collect()
}

/// One difference between the two schemas.
#[derive(Debug, PartialEq)]
struct Diff {
    table: String,
    /// A column, key or index; empty for the table itself.
    item: String,
    field: &'static str,
    sqlite: String,
    pg: String,
}

impl Diff {
    fn tuple(&self) -> (&str, &str, &str, &str, &str) {
        (&self.table, &self.item, self.field, &self.sqlite, &self.pg)
    }
}

impl fmt::Display for Diff {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let what = if self.item.is_empty() {
            self.table.clone()
        } else {
            format!("{}.{}", self.table, self.item)
        };
        match (self.field, self.pg.as_str()) {
            ("presence", "missing") => write!(f, "{what}: missing from schema_pg.sql"),
            ("presence", _) => write!(f, "{what}: only in schema_pg.sql"),
            _ => write!(
                f,
                "{what}: {}: init_db {}, schema_pg.sql {}",
                self.field, self.sqlite, self.pg
            ),
        }
    }
}

/// Push a presence difference for every name only one side has; return the
/// names both have.
fn present<'a, V>(
    out: &mut Vec<Diff>,
    table: &str,
    sqlite: &'a BTreeMap<String, V>,
    pg: &'a BTreeMap<String, V>,
    item: impl Fn(&str) -> (String, String),
) -> Vec<&'a str> {
    let mut both = Vec::new();
    for name in sqlite.keys().chain(pg.keys()).collect::<BTreeSet<_>>() {
        let (table_name, item_name) = item(name);
        let table_name = if table.is_empty() {
            table_name
        } else {
            table.to_string()
        };
        match (sqlite.contains_key(name), pg.contains_key(name)) {
            (true, true) => both.push(name.as_str()),
            (in_sqlite, _) => out.push(Diff {
                table: table_name,
                item: item_name,
                field: "presence",
                sqlite: if in_sqlite { "present" } else { "missing" }.into(),
                pg: if in_sqlite { "missing" } else { "present" }.into(),
            }),
        }
    }
    both
}

fn compare(sqlite: &Shape, pg: &Shape) -> Vec<Diff> {
    let mut out = Vec::new();
    let tables = present(&mut out, "", &sqlite.columns, &pg.columns, |t| {
        (t.to_string(), String::new())
    });
    for table in tables {
        let (s_cols, p_cols) = (&sqlite.columns[table], &pg.columns[table]);
        for col in present(&mut out, table, s_cols, p_cols, |c| {
            (String::new(), c.into())
        }) {
            let (s, p) = (&s_cols[col], &p_cols[col]);
            let show = |d: &Option<Lit>| d.as_ref().map_or("none".into(), Lit::to_string);
            for (field, a, b) in [
                ("type", s.class.clone(), p.class.clone()),
                ("not null", s.not_null.to_string(), p.not_null.to_string()),
                ("default", show(&s.default), show(&p.default)),
                ("auto", s.auto.to_string(), p.auto.to_string()),
            ] {
                if a != b {
                    out.push(Diff {
                        table: table.into(),
                        item: col.into(),
                        field,
                        sqlite: a,
                        pg: b,
                    });
                }
            }
        }
        let empty = BTreeSet::new();
        let as_map = |keys: Option<&BTreeSet<String>>| -> BTreeMap<String, ()> {
            keys.unwrap_or(&empty)
                .iter()
                .map(|k| (k.clone(), ()))
                .collect()
        };
        let (s_keys, p_keys) = (as_map(sqlite.keys.get(table)), as_map(pg.keys.get(table)));
        present(&mut out, table, &s_keys, &p_keys, |k| {
            (String::new(), k.into())
        });
    }
    for name in present(&mut out, "", &sqlite.indexes, &pg.indexes, |i| {
        let table = sqlite.indexes.get(i).or(pg.indexes.get(i)).unwrap();
        (table.table.clone(), format!("index {i}"))
    }) {
        let (s, p) = (&sqlite.indexes[name], &pg.indexes[name]);
        let show = |keys: &Option<Vec<String>>| {
            keys.as_ref().map_or("(expression or partial)".into(), |k| {
                format!("({})", k.join(", "))
            })
        };
        for (field, a, b) in [
            ("table", s.table.clone(), p.table.clone()),
            ("unique", s.unique.to_string(), p.unique.to_string()),
            ("partial", s.partial.to_string(), p.partial.to_string()),
            ("keys", show(&s.keys), show(&p.keys)),
        ] {
            if a != b {
                out.push(Diff {
                    table: s.table.clone(),
                    item: format!("index {name}"),
                    field,
                    sqlite: a,
                    pg: b,
                });
            }
        }
    }
    out
}

#[tokio::test]
#[ignore = "needs PG_TEST_URL; see AGENTS.md"]
async fn schema_pg_matches_init_db() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("parity.db");
    let sqlite = sqlite_shape(&nvnmchain_explorer::db::init_db(path.to_str().unwrap()).unwrap());

    let pg = scratch("parity").await;
    pg.apply_schema().await;
    let diffs = compare(&sqlite, &pg_shape(&pg.client).await);

    let mut problems: Vec<String> = diffs
        .iter()
        .filter(|d| {
            !ALLOWED
                .iter()
                .any(|a| (a.0, a.1, a.2, a.3, a.4) == d.tuple())
        })
        .map(ToString::to_string)
        .collect();
    for a in ALLOWED {
        if !diffs.iter().any(|d| d.tuple() == (a.0, a.1, a.2, a.3, a.4)) {
            problems.push(format!(
                "ALLOWED entry {:?} no longer matches a difference; remove it",
                (a.0, a.1, a.2, a.3, a.4)
            ));
        }
    }
    problems.extend(uncollated(&pg.client).await);
    assert!(
        problems.is_empty(),
        "schema_pg.sql and init_db differ; port the change using the type rules in \
         docs/superpowers/specs/2026-10-02-schema-two-dialects-design.md:\n{}",
        problems.join("\n")
    );
    pg.finish().await;
}

// ---------------------------------------------------------------------------
// Baseline round-trip
// ---------------------------------------------------------------------------

fn fixtures() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/baseline");
    let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "db"))
        .collect();
    found.sort();
    assert!(!found.is_empty(), "no baselines under {}", dir.display());
    found
}

/// A fixture, read-only and immutable, as `tests/baseline.rs` opens it.
fn open_fixture(path: &Path) -> Connection {
    let abs = std::fs::canonicalize(path).unwrap();
    Connection::open_with_flags(
        format!("file:{}?immutable=1", abs.display()),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .unwrap_or_else(|e| panic!("open {}: {e}", path.display()))
}

fn spec(table: &str) -> &'static Spec {
    SPECS
        .iter()
        .chain(UNINDEXED)
        .find(|s| s.table == table)
        .unwrap_or_else(|| panic!("{table}: no row key; add it to UNINDEXED in tests/postgres.rs"))
}

fn quoted(cols: &[String]) -> String {
    cols.iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A SQLite value as a parameter of Postgres type `ty`, typed by the column
/// even when it is `NULL`; `None` if the stored value does not fit.
fn bind(ty: &Type, value: &Sql) -> Option<Box<dyn ToSql + Sync + Send>> {
    let bound: Box<dyn ToSql + Sync + Send> = match value {
        Sql::Integer(i) if *ty == Type::INT8 => Box::new(Some(*i)),
        Sql::Null if *ty == Type::INT8 => Box::new(None::<i64>),
        Sql::Text(s) if *ty == Type::TEXT => Box::new(Some(s.clone())),
        Sql::Null if *ty == Type::TEXT => Box::new(None::<String>),
        Sql::Blob(b) if *ty == Type::BYTEA => Box::new(Some(b.clone())),
        Sql::Null if *ty == Type::BYTEA => Box::new(None::<Vec<u8>>),
        _ => return None,
    };
    Some(bound)
}

/// Copy every row of `table` in one transaction; returns how many.
async fn copy_table(
    client: &mut Client,
    sqlite: &Connection,
    table: &str,
    cols: &[String],
) -> usize {
    let list = quoted(cols);
    let source: Vec<Vec<Sql>> = {
        let mut stmt = sqlite
            .prepare(&format!("SELECT {list} FROM {table}"))
            .unwrap();
        stmt.query_map([], |r| (0..cols.len()).map(|i| r.get(i)).collect())
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let spec = spec(table);
    let placeholders = (1..=cols.len())
        .map(|i| format!("${i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let tx = client.transaction().await.unwrap();
    let insert = tx
        .prepare(&format!(
            "INSERT INTO {table} ({list}) VALUES ({placeholders})"
        ))
        .await
        .unwrap_or_else(|e| panic!("{table}: prepare insert: {}", pg_error(&e)));
    for row in &source {
        let row_id = || {
            let fields = cols
                .iter()
                .zip(row)
                .map(|(c, v)| (c.clone(), to_json(c, v.clone())))
                .collect();
            row_key(spec, &fields)
        };
        let params: Vec<Box<dyn ToSql + Sync + Send>> = cols
            .iter()
            .zip(insert.params())
            .zip(row)
            .map(|((col, ty), value)| {
                bind(ty, value).unwrap_or_else(|| {
                    panic!(
                        "{table} {}: {col} holds {value:?}, which a {ty} column cannot",
                        row_id()
                    )
                })
            })
            .collect();
        let refs: Vec<&(dyn ToSql + Sync)> = params
            .iter()
            .map(|p| p.as_ref() as &(dyn ToSql + Sync))
            .collect();
        tx.execute(&insert, &refs)
            .await
            .unwrap_or_else(|e| panic!("{table} {}: insert: {}", row_id(), pg_error(&e)));
    }
    tx.commit().await.unwrap();
    source.len()
}

/// A table's rows read back, converted with the same rules as the fixture's.
async fn pg_rows(client: &Client, spec: &Spec, cols: &[String]) -> Rows {
    let found = client
        .query(&format!("SELECT {} FROM {}", quoted(cols), spec.table), &[])
        .await
        .unwrap();
    let mut out = Rows::new();
    for row in &found {
        let mut fields = BTreeMap::new();
        for (i, col) in cols.iter().enumerate() {
            let ty = row.columns()[i].type_();
            let value = if *ty == Type::INT8 {
                row.get::<_, Option<i64>>(i).map_or(Sql::Null, Sql::Integer)
            } else if *ty == Type::TEXT {
                row.get::<_, Option<String>>(i).map_or(Sql::Null, Sql::Text)
            } else if *ty == Type::BYTEA {
                row.get::<_, Option<Vec<u8>>>(i)
                    .map_or(Sql::Null, Sql::Blob)
            } else {
                panic!("{}.{col}: unexpected type {ty}", spec.table)
            };
            fields.insert(col.clone(), to_json(col, value));
        }
        out.insert(row_key(spec, &fields), fields);
    }
    assert_eq!(out.len(), found.len(), "{}: duplicate row keys", spec.table);
    out
}

#[tokio::test]
#[ignore = "needs PG_TEST_URL; see AGENTS.md"]
async fn every_baseline_round_trips_through_postgres() {
    let mut failed = Vec::new();
    for path in fixtures() {
        let stem = path
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .replace('-', "_");
        let mut pg = scratch(&format!("roundtrip_{stem}")).await;
        pg.apply_schema().await;
        let sqlite = open_fixture(&path);

        let (mut diffs, mut copied) = (Vec::new(), 0);
        for table in tables(&sqlite)
            .into_iter()
            .filter(|t| t != "sqlite_sequence")
        {
            let cols = columns(&sqlite, &table);
            let n = copy_table(&mut pg.client, &sqlite, &table, &cols).await;
            copied += n;
            let spec = spec(&table);
            let (base, back) = (
                rows(&sqlite, spec, &cols),
                pg_rows(&pg.client, spec, &cols).await,
            );
            assert_eq!(base.len(), n, "{table}: duplicate row keys in the fixture");
            diffs.extend(diff_rows(
                &table,
                &base,
                &back,
                ("fixture", "Postgres copy"),
            ));
        }

        eprintln!("{}: {copied} row(s) copied", path.display());
        if diffs.is_empty() {
            pg.finish().await;
        } else {
            for d in diffs.iter().take(25) {
                eprintln!("  {d}");
            }
            failed.push(format!("{}: {} difference(s)", path.display(), diffs.len()));
        }
    }
    assert!(failed.is_empty(), "round-trip differs: {failed:?}");
}
