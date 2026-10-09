//! Data correctness against a baseline database, with no network.
//!
//! Two fixtures, both written from the live chain by `record_baseline`:
//!
//! - `fixtures/baseline/canary-rpc.json`: every JSON-RPC answer the indexer
//!   was given for `RANGES`, keyed by method and params.
//! - `fixtures/baseline/canary.db`: the SQLite database it wrote from them.
//!
//! `replay_reproduces_baseline` serves the recorded answers from a local stub
//! node, indexes the same blocks into a fresh database, and compares every
//! table row by row. Anything that differs is a change in what the explorer
//! stores, which a refactor or a new database backend must not make.
//!
//! To record both again from the build checked out (the old files must be
//! deleted first; `NVNM_RPC` picks another node):
//!
//! ```text
//! cargo test --test baseline record_baseline -- --ignored --nocapture
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;

use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use futures_util::{stream, StreamExt, TryStreamExt};
use nvnmchain_explorer::config::DEFAULT_RPC_URL;
use nvnmchain_explorer::db::{self, Db};
use nvnmchain_explorer::indexer::fetch_block_bundle;
use nvnmchain_explorer::rpc::ChainRpc;
use nvnmchain_explorer::tokens::balances_at_genesis;
use rusqlite::types::Value as Sql;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

/// Canary's first Transfer (102504); 26 anchoring writes, the failed tx at
/// 105627 and three contract deploys; a TIP-20 token's creation, role grant
/// and first mint; and legacy, EIP-1559 and 0x76 transactions side by side.
const RANGES: [RangeInclusive<u64>; 4] = [
    102_500..=102_510,
    105_465..=105_640,
    1_442_885..=1_442_930,
    1_445_775..=1_445_945,
];

/// Blocks fetched at once, and blocks per commit.
const CONCURRENCY: usize = 16;
const COMMIT: usize = 64;

/// Wall-clock write times: the only values two runs over the same answers
/// may disagree on.
const VOLATILE: &[&str] = &["created_at", "updated_at", "fetched_at"];

/// The most differences printed; the count is always complete.
const SHOWN: usize = 25;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/baseline")
        .join(name)
}

/// The database under test. A Postgres backend opens here once `db` has one.
fn open_db(path: &str) -> Db {
    Arc::new(Mutex::new(db::init_db(path).expect("open database")))
}

/// What a recorded call is looked up by: its method and params, not its id.
fn call_key(call: &Value) -> String {
    format!(
        "{} {}",
        call["method"].as_str().unwrap_or(""),
        call["params"]
    )
}

/// Recorded answers, each a response without its id: `{"result": ...}` or
/// `{"error": ...}`.
type Tape = BTreeMap<String, Value>;

#[derive(Clone)]
struct Node {
    tape: Arc<Mutex<Tape>>,
    /// Recording: where calls missing from the tape are forwarded.
    upstream: Option<String>,
    /// Replaying: calls missing from the tape.
    missed: Arc<Mutex<BTreeSet<String>>>,
    client: reqwest::Client,
}

impl Node {
    /// Ask upstream for `calls`, retrying what a public node drops, and keep
    /// the first answer to each: a later run is given exactly what this one was.
    async fn record(&self, url: &str, calls: Vec<Value>) -> Result<(), StatusCode> {
        let mut wait = Duration::from_millis(250);
        for _ in 0..6 {
            let sent = self.client.post(url).json(&calls).send().await;
            if let Ok(resp) = sent.and_then(|r| r.error_for_status()) {
                if let Ok(Value::Array(items)) = resp.json::<Value>().await {
                    let mut tape = self.tape.lock().unwrap();
                    for (i, call) in calls.iter().enumerate() {
                        let item = items.iter().find(|r| r["id"] == json!(i));
                        let Some(item) = item else { continue };
                        let answer = match item.get("error").filter(|e| !e.is_null()) {
                            Some(error) => json!({ "error": error }),
                            None => json!({ "result": item["result"] }),
                        };
                        tape.entry(call_key(call)).or_insert(answer);
                    }
                    return Ok(());
                }
            }
            tokio::time::sleep(wait).await;
            wait *= 2;
        }
        Err(StatusCode::BAD_GATEWAY)
    }
}

async fn answer(
    State(node): State<Node>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    let calls = match &body {
        Value::Array(calls) => calls.clone(),
        call => vec![call.clone()],
    };
    if let Some(url) = &node.upstream {
        let missing: Vec<Value> = {
            let tape = node.tape.lock().unwrap();
            calls
                .iter()
                .filter(|c| !tape.contains_key(&call_key(c)))
                .enumerate()
                .map(|(i, c)| json!({"jsonrpc": "2.0", "id": i, "method": c["method"], "params": c["params"]}))
                .collect()
        };
        if !missing.is_empty() {
            node.record(url, missing).await?;
        }
    }
    let tape = node.tape.lock().unwrap();
    let out: Vec<Value> = calls
        .iter()
        .map(|call| {
            let key = call_key(call);
            let mut response = tape.get(&key).cloned().unwrap_or_else(|| {
                node.missed.lock().unwrap().insert(key.clone());
                json!({ "error": { "code": -32000, "message": format!("not recorded: {key}") } })
            });
            response["jsonrpc"] = json!("2.0");
            response["id"] = call["id"].clone();
            response
        })
        .collect();
    Ok(Json(match body {
        Value::Array(_) => Value::Array(out),
        _ => out.into_iter().next().unwrap_or(Value::Null),
    }))
}

/// A node on a local port answering from `tape`, and forwarding what it lacks
/// to `upstream` when given one.
async fn serve(tape: Tape, upstream: Option<String>) -> (Node, String) {
    let node = Node {
        tape: Arc::new(Mutex::new(tape)),
        upstream,
        missed: Arc::default(),
        client: reqwest::Client::new(),
    };
    let app = Router::new()
        .route("/", post(answer))
        .with_state(node.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (node, url)
}

/// Index `RANGES` from `rpc_url` into `db`: fetched concurrently, written in
/// ascending block order, then the genesis pass the explorer runs alongside
/// indexing, which fills `genesis_balances` and adds each holder's block-0
/// balance to `token_balances`.
async fn index(rpc_url: &str, db: &Db) -> anyhow::Result<()> {
    let rpc = ChainRpc::new(rpc_url)?;
    let numbers: Vec<u64> = RANGES.iter().cloned().flatten().collect();
    let mut bundles = stream::iter(numbers)
        .map(|n| {
            let rpc = &rpc;
            async move {
                let bundle = fetch_block_bundle(rpc, n).await?;
                bundle.with_context(|| format!("block {n}: no bundle"))
            }
        })
        .buffered(CONCURRENCY)
        .try_chunks(COMMIT);
    while let Some(chunk) = bundles.next().await {
        db::save_block_bundles(db, &chunk.map_err(|e| e.1)?)?;
    }
    // `indexer::add_genesis_balances`, which is private.
    while let Some((cursor, holders)) = db::holders_without_genesis_balance(db, 1000)? {
        let balances = balances_at_genesis(&rpc, &holders).await?;
        let rows: Vec<_> = holders.into_iter().zip(balances).collect();
        db::save_genesis_balances(db, &rows, cursor)?;
    }
    Ok(())
}

/// Every table's compared columns and rows, each row rendered and sorted.
fn dump(conn: &Connection) -> BTreeMap<String, (Vec<String>, Vec<String>)> {
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let mut out = BTreeMap::new();
    for table in tables {
        let cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info(?1)")
            .unwrap()
            .query_map([&table], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .filter(|c: &String| !VOLATILE.contains(&c.as_str()))
            .collect();
        let list: Vec<String> = cols.iter().map(|c| format!("\"{c}\"")).collect();
        let mut stmt = conn
            .prepare(&format!("SELECT {} FROM \"{table}\"", list.join(", ")))
            .unwrap();
        let mut rows: Vec<String> = stmt
            .query_map([], |r| {
                let fields = cols
                    .iter()
                    .enumerate()
                    .map(|(i, col)| {
                        Ok(match r.get(i)? {
                            Sql::Blob(b) => format!("{col}=0x{}", hex::encode(b)),
                            value => format!("{col}={value:?}"),
                        })
                    })
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(fields.join(" "))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        rows.sort();
        out.insert(table, (cols, rows));
    }
    out
}

/// Every difference between the baseline and the replayed database.
fn compare(baseline: &Connection, fresh: &Connection) -> Vec<String> {
    let (base, new) = (dump(baseline), dump(fresh));
    let mut diffs = Vec::new();
    let tables: BTreeSet<&String> = base.keys().chain(new.keys()).collect();
    for table in tables {
        let (Some((base_cols, base_rows)), Some((cols, rows))) = (base.get(table), new.get(table))
        else {
            let side = if base.contains_key(table) {
                "baseline"
            } else {
                "replay"
            };
            diffs.push(format!("{table}: only in the {side}"));
            continue;
        };
        if base_cols != cols {
            diffs.push(format!("{table}: columns were {base_cols:?}, now {cols:?}"));
            continue;
        }
        eprintln!("  {table}: {} row(s)", base_rows.len());
        let (base_set, set): (BTreeSet<_>, BTreeSet<_>) =
            (base_rows.iter().collect(), rows.iter().collect());
        diffs.extend(
            base_set
                .difference(&set)
                .map(|r| format!("{table}: only in the baseline: {r}")),
        );
        diffs.extend(
            set.difference(&base_set)
                .map(|r| format!("{table}: only in the replay: {r}")),
        );
    }
    diffs
}

#[tokio::test(flavor = "multi_thread")]
async fn replay_reproduces_baseline() {
    let tape: Tape = serde_json::from_slice(&std::fs::read(fixture("canary-rpc.json")).unwrap())
        .expect("recorded answers");
    let (node, url) = serve(tape, None).await;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("replay.db");
    let indexed = index(&url, &open_db(path.to_str().unwrap())).await;
    let missed = node.missed.lock().unwrap().clone();
    assert!(
        missed.is_empty(),
        "the indexer asked for answers never recorded; record the baseline again: {missed:?}"
    );
    indexed.expect("index the recorded answers");

    // Read-only and immutable: nothing here can migrate or repair the baseline.
    let baseline = Connection::open_with_flags(
        format!("file:{}?immutable=1", fixture("canary.db").display()),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("open baseline");
    let diffs = compare(&baseline, &Connection::open(&path).unwrap());
    for d in diffs.iter().take(SHOWN) {
        eprintln!("  {d}");
    }
    assert!(
        diffs.is_empty(),
        "{} difference(s) from the baseline",
        diffs.len()
    );
}

/// Index `RANGES` from the live node through a recording stub, then write the
/// answers it was given and the database it wrote as the two fixtures.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "records the baseline from the live chain; run explicitly"]
async fn record_baseline() {
    let (tape_path, db_path) = (fixture("canary-rpc.json"), fixture("canary.db"));
    for path in [&tape_path, &db_path] {
        assert!(
            !path.exists(),
            "{} exists; delete it to record again",
            path.display()
        );
    }
    let upstream = std::env::var("NVNM_RPC").unwrap_or_else(|_| DEFAULT_RPC_URL.to_string());
    eprintln!("recording {RANGES:?} from {upstream}");
    let (node, url) = serve(Tape::new(), Some(upstream)).await;

    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    index(&url, &open_db(db_path.to_str().unwrap()))
        .await
        .expect("index the live chain");
    // Self-contained: no WAL beside it, and no free pages.
    let conn = Connection::open(&db_path).unwrap();
    conn.pragma_update(None, "journal_mode", "DELETE").unwrap();
    conn.execute_batch("VACUUM").unwrap();
    for (table, (_, rows)) in dump(&conn) {
        eprintln!("  {table}: {} row(s)", rows.len());
    }

    // One answer per line, so a new recording diffs readably.
    let tape = node.tape.lock().unwrap();
    let lines: Vec<String> = tape
        .iter()
        .map(|(key, answer)| format!("{}: {answer}", json!(key)))
        .collect();
    std::fs::write(&tape_path, format!("{{\n{}\n}}\n", lines.join(",\n"))).unwrap();
    eprintln!("  {} answer(s) recorded", tape.len());
}
