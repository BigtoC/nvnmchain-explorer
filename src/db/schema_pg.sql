-- PostgreSQL schema for the explorer, ported from the SQLite one `init_db`
-- builds in src/db.rs. Same tables, columns and index names (less the unused
-- idx_transfer_tx_hash, see transfer_events), so the Rust layer ports as a
-- driver swap plus the SQL notes below.
--
-- Idempotent: safe to run on every start, and docker-compose.yml mounts it
-- into /docker-entrypoint-initdb.d for a fresh volume. A fresh schema only:
-- the SQLite DROPs for long-retired tables and indexes are not carried over.
--
-- Conventions, as in SQLite:
-- * Every integer is BIGINT. SQLite's INTEGER is 64-bit and the Rust side
--   reads i64 everywhere; a narrower type would fail to decode.
-- * Times are epoch seconds (`timestamp_ms` is milliseconds), not timestamptz.
-- * Hashes and addresses are BYTEA (hex_blob), except the token_balances and
--   genesis_balances keys, which the code writes as checksummed text.
-- * No foreign keys, as in SQLite: the balance tables key on checksummed text
--   while token_metadata keys on bytes, balances can precede a token's
--   metadata row, and each FK would add a parent probe to every row of the
--   per-block write path.
-- * token_metadata's name and symbol are COLLATE "C", as SQLite's BINARY:
--   ORDER BY compares bytes and lower()/ILIKE fold ASCII only, on any host.
--
-- Porting notes the schema alone cannot carry:
-- * `INSERT OR IGNORE` becomes `ON CONFLICT (<the unique columns>) DO NOTHING`,
--   target named: transfer_events also has the id key.
-- * add_to_counter's `SET n = n + excluded.n` is ambiguous in PostgreSQL;
--   write `SET n = counters.n + excluded.n`.
-- * EXISTS is boolean and SUM(bigint) is numeric here; cast where Rust reads i64.
-- * NUMERIC columns (amount, total_supply, both balance columns) bind as
--   $n::text::numeric and select as col::text; Rust holds them as decimal
--   Strings, and the drivers will not bind a String to a numeric parameter.
-- * Collate the bound side of a token-name compare too: lower($1 COLLATE "C"),
--   or lower($1) folds under the database locale and stops matching what
--   lower(symbol) folds under "C".

-- Trigram indexes for the token search box; see token_metadata.
CREATE EXTENSION IF NOT EXISTS pg_trgm;

CREATE TABLE IF NOT EXISTS blocks (
    number BIGINT PRIMARY KEY,
    hash BYTEA NOT NULL UNIQUE,
    parent_hash BYTEA NOT NULL,
    timestamp BIGINT NOT NULL,
    timestamp_ms BIGINT NOT NULL DEFAULT 0,
    gas_used BIGINT NOT NULL DEFAULT 0,
    gas_limit BIGINT NOT NULL DEFAULT 0,
    miner BYTEA NOT NULL DEFAULT '\x'::bytea,
    tx_count BIGINT NOT NULL DEFAULT 0,
    -- Text, not NUMERIC: decimal_amount keeps a value it cannot parse verbatim.
    base_fee TEXT NOT NULL DEFAULT '0',
    size BIGINT NOT NULL DEFAULT 0,
    extra_data TEXT NOT NULL DEFAULT '',
    epoch BIGINT NOT NULL DEFAULT 0,
    view BIGINT NOT NULL DEFAULT 0,
    proposer BYTEA NOT NULL DEFAULT '\x'::bytea,
    created_at BIGINT NOT NULL DEFAULT 0
);
-- The stats sum the last day's tx_count every few seconds and count its
-- blocks (compute_and_store_stats in indexer.rs). INCLUDE makes both
-- index-only: a PostgreSQL index does not reach the row without a heap fetch
-- per block.
CREATE INDEX IF NOT EXISTS idx_blocks_timestamp
    ON blocks (timestamp) INCLUDE (tx_count);

CREATE TABLE IF NOT EXISTS transactions (
    hash BYTEA PRIMARY KEY,
    block_number BIGINT NOT NULL,
    position BIGINT NOT NULL DEFAULT 0,
    from_addr BYTEA NOT NULL,
    to_addr BYTEA,
    status BIGINT NOT NULL DEFAULT 1,
    gas_used BIGINT NOT NULL DEFAULT 0,
    -- Hex ('0x...', the receipt's effectiveGasPrice), unlike blocks.base_fee.
    base_fee TEXT NOT NULL DEFAULT '0',
    contract_address BYTEA,
    fee_token BYTEA,
    -- Text: decimal_amount can pass a non-numeric node value through.
    fee_amount TEXT NOT NULL DEFAULT '0',
    input TEXT NOT NULL DEFAULT '0x',
    raw BYTEA,
    -- JSON kept as text: never queried, and JSONB would reorder keys and
    -- reject the \u0000 escape serde writes for NULs in contract strings.
    trace_data TEXT,
    receipt_data TEXT,
    timestamp BIGINT NOT NULL DEFAULT 0,
    created_at BIGINT NOT NULL DEFAULT 0
);
-- The address page walks each side newest first (get_address_transactions).
-- The to-side also filters from_addr<>$1, and the address tx count
-- (get_address_transaction_count) needs only these columns, so INCLUDE lets
-- both halves of that count run index-only once it is split from its OR into
-- count(from) + count(to, from<>).
CREATE INDEX IF NOT EXISTS idx_tx_from_block
    ON transactions (from_addr, block_number, position);
CREATE INDEX IF NOT EXISTS idx_tx_to_block
    ON transactions (to_addr, block_number, position) INCLUDE (from_addr);
-- The chain-wide listing pages in this order, and a block's rows come off
-- it in position order.
CREATE INDEX IF NOT EXISTS idx_tx_block_position
    ON transactions (block_number, position);

-- Rewritten every block that moves balances (holder_count), so pages are
-- left room for HOT updates; holder_count stays unindexed for the same reason.
CREATE TABLE IF NOT EXISTS token_metadata (
    address BYTEA PRIMARY KEY,
    -- "C": byte order and ASCII-only case folding, as in SQLite (see header).
    name TEXT COLLATE "C" NOT NULL DEFAULT '',
    symbol TEXT COLLATE "C" NOT NULL DEFAULT '',
    decimals BIGINT NOT NULL DEFAULT 18,
    currency TEXT NOT NULL DEFAULT '',
    -- Always a base-10 integer (decode_uint256_result), but of the whole
    -- return data, which a nonstandard token can make longer than 32 bytes:
    -- NUMERIC with no precision cap. Select it as total_supply::text.
    total_supply NUMERIC NOT NULL DEFAULT 0,
    logo_uri TEXT NOT NULL DEFAULT '',
    holder_count BIGINT NOT NULL DEFAULT 0,
    created_at BIGINT NOT NULL DEFAULT 0,
    updated_at BIGINT NOT NULL DEFAULT 0
) WITH (fillfactor = 80);
-- Search on Enter: exact case-insensitive symbol or name
-- (get_token_by_symbol_or_name). The query text must be
-- lower(col) = lower($1 COLLATE "C") for these to match.
CREATE INDEX IF NOT EXISTS idx_token_symbol_lower ON token_metadata (lower(symbol));
CREATE INDEX IF NOT EXISTS idx_token_name_lower ON token_metadata (lower(name));
-- Typeahead per keystroke: substring match on symbol or name (search_tokens).
-- SQLite's LIKE folds ASCII case and PostgreSQL's does not, so the port
-- writes ILIKE, which these serve; a leading % defeats any btree.
CREATE INDEX IF NOT EXISTS idx_token_symbol_trgm
    ON token_metadata USING gin (symbol gin_trgm_ops);
CREATE INDEX IF NOT EXISTS idx_token_name_trgm
    ON token_metadata USING gin (name gin_trgm_ops);

CREATE TABLE IF NOT EXISTS transfer_events (
    -- BY DEFAULT so migrated ids can be copied in (the kv genesis cursor
    -- holds one). After copying ids in, run
    --   SELECT setval(pg_get_serial_sequence('transfer_events', 'id'), max(id))
    --   FROM transfer_events;
    -- or the next indexer insert collides on the id key, which
    -- ON CONFLICT (block_number, log_index) does not catch. The genesis pass
    -- pages on it (holders_without_genesis_balance), which assumes ids commit
    -- in order: one writer only.
    id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    tx_hash BYTEA NOT NULL,
    block_number BIGINT NOT NULL,
    log_index BIGINT NOT NULL DEFAULT 0,
    token_addr BYTEA NOT NULL,
    from_addr BYTEA NOT NULL,
    to_addr BYTEA NOT NULL,
    -- A decoded uint256 (always base-10 digits), which 78 digits hold.
    -- Select it as amount::text: no Rust decimal type holds a uint256.
    amount NUMERIC(78, 0) NOT NULL,
    timestamp BIGINT NOT NULL DEFAULT 0,
    created_at BIGINT NOT NULL DEFAULT 0,
    UNIQUE (block_number, log_index)
);
-- SQLite's idx_transfer_tx_hash is not carried over: no query filters on
-- tx_hash (the transfer pages join transactions on it, probing that table's
-- key), and it would cost a btree insert on every transfer.
-- The token transfers page (get_token_transfers) and the address page
-- (get_address_transfers) walk these newest first and keep only id,
-- block_number and log_index. SQLite's indexes carry the rowid, which id is,
-- so they were covering; here INCLUDE (id) restores that. The to-side also
-- filters from_addr<>$1, and the address transfer count
-- (get_address_transfer_count) reads only these columns: with the OR split
-- into count(from) + count(to, from<>), both halves run index-only.
CREATE INDEX IF NOT EXISTS idx_transfer_token_block
    ON transfer_events (token_addr, block_number, log_index) INCLUDE (id);
CREATE INDEX IF NOT EXISTS idx_transfer_from_block
    ON transfer_events (from_addr, block_number, log_index) INCLUDE (id);
CREATE INDEX IF NOT EXISTS idx_transfer_to_block
    ON transfer_events (to_addr, block_number, log_index) INCLUDE (id, from_addr);

-- Writes to the anchoring contract, keyed by registry: its events carry the
-- id in data, not a topic, so `eth_getLogs` cannot filter on it. SQLite has
-- no key here beyond UNIQUE (block_number, log_index); it is the key.
CREATE TABLE IF NOT EXISTS anchoring_events (
    tx_hash BYTEA NOT NULL,
    block_number BIGINT NOT NULL,
    log_index BIGINT NOT NULL,
    timestamp BIGINT NOT NULL DEFAULT 0,
    event TEXT NOT NULL,
    registry_id BIGINT NOT NULL,
    record_id BIGINT NOT NULL DEFAULT 0,
    caller BYTEA NOT NULL,
    PRIMARY KEY (block_number, log_index)
);
-- The registry page reads one id newest first, in this order
-- (get_anchoring_events).
CREATE INDEX IF NOT EXISTS idx_anchoring_registry
    ON anchoring_events (registry_id, block_number DESC, log_index DESC);

-- 'chain_head' changes every head and 'stats' every few seconds: room for
-- HOT updates.
CREATE TABLE IF NOT EXISTS kv (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL DEFAULT '',
    updated_at BIGINT NOT NULL DEFAULT 0
) WITH (fillfactor = 50);

-- Row counts for the stats, kept by the writer. A COUNT(*) over
-- transactions is a scan of every row, on every stats refresh. Both rows
-- are updated in every block's transaction.
CREATE TABLE IF NOT EXISTS counters (
    name TEXT PRIMARY KEY,
    n BIGINT NOT NULL DEFAULT 0
) WITH (fillfactor = 50);

-- Keys are text, not BYTEA, because that is what the code writes: the
-- checksummed '0x...' strings of the decoded Transfer (SQLite's BLOB
-- affinity stored them as given). Equality only, so collation "C" makes it a
-- byte compare; case matters, and every writer keeps the EIP-55 spelling.
-- BYTEA would halve them but means hex_blob on every bind of this table and
-- genesis_balances together, since the rebuild feeds one into the other.
CREATE TABLE IF NOT EXISTS token_balances (
    token_addr TEXT COLLATE "C" NOT NULL,
    holder_addr TEXT COLLATE "C" NOT NULL,
    -- Signed: negative when indexing began after the holder was funded.
    -- Never 0 (such rows are deleted). A running sum over events any
    -- contract can emit, so no precision cap: an overflow would abort the
    -- block's transaction and stall the indexer. Select it as balance::text.
    balance NUMERIC NOT NULL DEFAULT 0,
    updated_at BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (token_addr, holder_addr)
);
-- The address page's holdings (get_address_holdings).
CREATE INDEX IF NOT EXISTS idx_tb_holder ON token_balances (holder_addr);
-- The holdings of one token, in listing order: holder counts
-- (upsert_token_meta, count_holders) are index scans and the holders page
-- (get_token_holders) needs no sort. NUMERIC orders itself, so SQLite's
-- LENGTH(balance) DESC, balance DESC becomes balance DESC, and HOLDING
-- (`balance NOT LIKE '-%'`) becomes `balance >= 0`. A partial index only serves queries whose filter provably
-- implies its own, so HOLDING in db.rs must be exactly `balance >= 0` and the
-- holders page must ORDER BY balance DESC. Qualify it there: beside
-- `SELECT balance::text`, a bare ORDER BY balance names that text output
-- column, which sorts as text and loses the index.
CREATE INDEX IF NOT EXISTS idx_tb_holding
    ON token_balances (token_addr, balance DESC)
    WHERE balance >= 0;

-- Each seen holder's balance at genesis, zero included: no event carries it.
-- Text keys as in token_balances, which the rebuild feeds them into.
CREATE TABLE IF NOT EXISTS genesis_balances (
    token_addr TEXT COLLATE "C" NOT NULL,
    holder_addr TEXT COLLATE "C" NOT NULL,
    -- balanceOf decoded like total_supply (whole return data): uncapped.
    balance NUMERIC NOT NULL,
    PRIMARY KEY (token_addr, holder_addr)
);

-- Function signatures for selectors no built-in ABI declares, as answered
-- by the configured directory. An empty `signature` is a remembered miss:
-- the directory did not know it either, and asking again on every page view
-- would be a request per view forever.
CREATE TABLE IF NOT EXISTS selector_names (
    selector TEXT PRIMARY KEY,
    signature TEXT NOT NULL DEFAULT '',
    fetched_at BIGINT NOT NULL DEFAULT 0
);
