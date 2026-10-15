-- PostgreSQL schema for the explorer, ported from the SQLite one `init_db`
-- builds in src/db.rs. Same tables, columns and index names (less the unused
-- idx_transfer_tx_hash, see transfer_events), so the Rust layer ports as a
-- driver swap plus the SQL notes below.
--
-- Idempotent: safe to rerun as the role that owns the tables (a least-
-- privilege app role cannot: CREATE ... IF NOT EXISTS checks privileges
-- before existence). docker-compose.yml mounts it into
-- /docker-entrypoint-initdb.d for a fresh volume. A fresh schema only:
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
-- * Result types differ: EXISTS is boolean, SUM(bigint) and anything times
--   1.0 are numeric, SUM(int) is bigint. Cast to bigint where Rust reads i64
--   and to float8 where it reads f64 (both gas sums in compute_and_store_stats,
--   whose unwrap_or would otherwise turn the decode error into zero stats).
-- * Type the placeholder NULLs in TX_LIST_COLS (NULL::bytea, NULL::text,
--   NULL::text): an untyped NULL column is text, which a bytea read rejects.
-- * Bind every LIMIT/OFFSET as i64; get_anchoring_events passes a u32.
-- * Token-name compares fold under "C": get_token_by_symbol_or_name's
--   lower(col) = lower($1) and search_tokens' `symbol = ?1 COLLATE NOCASE`
--   (PostgreSQL has no NOCASE) both become lower(col) = lower($1 COLLATE "C");
--   a plain lower($1) folds under the database locale.
-- * search_tokens' LIKE becomes ILIKE, run unprepared or with
--   plan_cache_mode = force_custom_plan (see the trigram indexes).
-- * The address counts (get_address_transaction_count,
--   get_address_transfer_count) split `from_addr=$1 OR to_addr=$1` into
--   count(from_addr=$1) + count(to_addr=$1 AND from_addr<>$1); the OR form
--   reads every matching heap page.
-- * get_transactions selects its page's keys first and joins for the
--   columns (see idx_tx_block_position).

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
-- An index-only scan skips the heap only on pages vacuum has marked
-- all-visible, and the default insert trigger scales with the table: a
-- fixed one keeps the newest rows, the ones the stats and pages read, marked.
) WITH (autovacuum_vacuum_insert_scale_factor = 0,
        autovacuum_vacuum_insert_threshold = 10000);
-- The stats sum the last day's tx_count every few seconds and count its
-- blocks (compute_and_store_stats in indexer.rs). INCLUDE makes both
-- index-only on vacuumed pages, rather than a heap fetch per block.
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
-- Insert vacuum as on blocks, for the index-only address counts.
) WITH (autovacuum_vacuum_insert_scale_factor = 0,
        autovacuum_vacuum_insert_threshold = 50000);
-- The address page walks each side newest first (get_address_transactions).
-- The to-side also filters from_addr<>$1, and the address tx count
-- (get_address_transaction_count) needs only these columns, so INCLUDE lets
-- both halves of that count run index-only once it is split from its OR into
-- count(from) + count(to, from<>).
CREATE INDEX IF NOT EXISTS idx_tx_from_block
    ON transactions (from_addr, block_number, position);
CREATE INDEX IF NOT EXISTS idx_tx_to_block
    ON transactions (to_addr, block_number, position) INCLUDE (from_addr);
-- The chain-wide listing (get_transactions) pages in this order, and a
-- block's rows come off it in position order. SQLite's deferred seek kept the
-- walk to a deep OFFSET in the index; here the port must take the page's
-- (block_number, position) off this index first, index-only, then join
-- transactions for the columns, as the address page does, or every skipped
-- row is a heap fetch.
CREATE INDEX IF NOT EXISTS idx_tx_block_position
    ON transactions (block_number, position);

-- Updated whenever a block changes a token's net holder count
-- (add_to_holder_counts), and re-upserted with its total_supply on every mint
-- or burn (upsert_token_meta), so pages are left room for HOT updates;
-- holder_count stays unindexed for the same reason.
CREATE TABLE IF NOT EXISTS token_metadata (
    address BYTEA PRIMARY KEY,
    -- "C": byte order and ASCII-only case folding, as in SQLite (see header).
    name TEXT COLLATE "C" NOT NULL DEFAULT '',
    symbol TEXT COLLATE "C" NOT NULL DEFAULT '',
    decimals BIGINT NOT NULL DEFAULT 18,
    currency TEXT NOT NULL DEFAULT '',
    -- Text, as in SQLite: decode_uint256_result reads the whole return data,
    -- so any contract can make it longer than NUMERIC's 131072 digits, which
    -- would fail the block's write. Nothing in SQL compares or sums it.
    total_supply TEXT NOT NULL DEFAULT '0',
    logo_uri TEXT NOT NULL DEFAULT '',
    holder_count BIGINT NOT NULL DEFAULT 0,
    created_at BIGINT NOT NULL DEFAULT 0,
    updated_at BIGINT NOT NULL DEFAULT 0
) WITH (fillfactor = 80);
-- Search on Enter: exact case-insensitive symbol or name
-- (get_token_by_symbol_or_name). The index serves lower($1) either way;
-- bind it as lower($1 COLLATE "C") so the term folds like the column.
CREATE INDEX IF NOT EXISTS idx_token_symbol_lower ON token_metadata (lower(symbol));
CREATE INDEX IF NOT EXISTS idx_token_name_lower ON token_metadata (lower(name));
-- Typeahead per keystroke: substring match on symbol or name (search_tokens).
-- SQLite's LIKE folds ASCII case and PostgreSQL's does not, so the port
-- writes ILIKE, which these serve for terms of 3+ characters; a leading %
-- defeats any btree. A 2-character term yields no trigram, so a GIN scan
-- reads the whole index: run the query with a custom plan, which picks a seq
-- scan there, not a cached generic one.
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
    -- Text, as in SQLite: a business decision, not a type limit. It is a
    -- decoded uint256 (base-10 digits, 78 at most), which NUMERIC(78, 0)
    -- would hold, and nothing in SQL sums, compares or sorts it today.
    -- TODO(postgres): review this choice once PostgreSQL support is fully
    -- implemented (NUMERIC would type-check values and allow SQL arithmetic,
    -- at the cost of $n::text::numeric binds and amount::text reads).
    amount TEXT NOT NULL,
    timestamp BIGINT NOT NULL DEFAULT 0,
    created_at BIGINT NOT NULL DEFAULT 0,
    UNIQUE (block_number, log_index)
-- Insert vacuum as on blocks, for the index-only transfer walks and counts.
) WITH (autovacuum_vacuum_insert_scale_factor = 0,
        autovacuum_vacuum_insert_threshold = 50000);
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
-- transactions is a scan of every row, on every stats refresh. The writer
-- bumps each row once per newly stored block (transactions only when the
-- block adds some), so one batch transaction can update a row many times:
-- room for HOT updates.
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
    -- Signed: 0x00..00 holds minus the net minted (every mint debits it, and
    -- the genesis pass skips it), and a holder funded before indexing began,
    -- or at genesis until the genesis pass adds it, reads negative.
    -- Never 0 (such rows are deleted). Decimal text, as in SQLite: a
    -- business decision, as for transfer_events.amount. "C" so the digit
    -- compares in idx_tb_holding are byte compares.
    -- TODO(postgres): review this choice once PostgreSQL support is fully
    -- implemented (NUMERIC would order itself, making HOLDING `balance >= 0`
    -- and the holders order plain `balance DESC`).
    balance TEXT COLLATE "C" NOT NULL DEFAULT '0',
    updated_at BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (token_addr, holder_addr)
);
-- The address page's holdings (get_address_holdings).
CREATE INDEX IF NOT EXISTS idx_tb_holder ON token_balances (holder_addr);
-- The holdings of one token, in listing order: holder counts
-- (upsert_token_meta, count_holders) are index scans and the holders page
-- (get_token_holders) needs no sort: a longer digit string is a larger
-- holding, and equal lengths compare digit by digit. Built from HOLDING
-- itself, as in SQLite: a partial index only serves queries whose filter
-- provably implies its own, so HOLDING in db.rs stays exactly
-- `balance NOT LIKE '-%'`.
CREATE INDEX IF NOT EXISTS idx_tb_holding
    ON token_balances (token_addr, LENGTH(balance) DESC, balance DESC)
    WHERE balance NOT LIKE '-%';

-- Each seen holder's balance at genesis, zero included: no event carries it.
-- Text keys as in token_balances, which the rebuild feeds them into.
CREATE TABLE IF NOT EXISTS genesis_balances (
    token_addr TEXT COLLATE "C" NOT NULL,
    holder_addr TEXT COLLATE "C" NOT NULL,
    -- balanceOf decoded like total_supply (whole return data): text, as in
    -- SQLite, for the same reason. Only Rust reads it.
    balance TEXT NOT NULL,
    PRIMARY KEY (token_addr, holder_addr)
);
