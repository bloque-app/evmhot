-- Idempotent Postgres schema for evm_hot_wallet, translated from
-- migrations/V1__initial.sql, V2__webhook_deliveries.sql, and
-- V3__next_index_counter.sql (SQLite). Matches PR #6's bootstrap style
-- (CREATE TABLE IF NOT EXISTS, run on every connect) to avoid adding a new
-- migration-framework dependency for a single-schema-version backend.
--
-- Notes on type choices vs. the SQLite schema:
--   * INTEGER counters that participate in derivation-index arithmetic
--     (derivation_index, log_index) use BIGINT rather than INTEGER: the
--     production next_index counter is already within ~42k of 2^31, which
--     is exactly one past Postgres INTEGER's signed range boundary
--     (2147483647) games with i32 overflow right at the danger zone we're
--     trying to guard against, so BIGINT gives headroom instead of adding
--     to the risk.
--   * decimals uses SMALLINT (u8 always fits).
--   * All amounts/hashes/ids stay TEXT, exactly like SQLite (arbitrary
--     precision amounts are handled as decimal strings by callers, not by
--     the database).

CREATE TABLE IF NOT EXISTS accounts (
  id TEXT PRIMARY KEY,
  derivation_index BIGINT NOT NULL,
  address TEXT NOT NULL UNIQUE,
  webhook_url TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS deposits (
  chain TEXT NOT NULL,
  tx_hash TEXT NOT NULL,
  account_id TEXT NOT NULL,
  amount TEXT NOT NULL,
  status TEXT NOT NULL,
  PRIMARY KEY (chain, tx_hash)
);
CREATE INDEX IF NOT EXISTS idx_deposits_chain_status ON deposits (chain, status);

CREATE TABLE IF NOT EXISTS erc20_deposits (
  chain TEXT NOT NULL,
  tx_hash TEXT NOT NULL,
  log_index BIGINT NOT NULL,
  account_id TEXT NOT NULL,
  amount TEXT NOT NULL,
  token_address TEXT NOT NULL,
  token_symbol TEXT NOT NULL,
  status TEXT NOT NULL,
  PRIMARY KEY (chain, tx_hash, log_index)
);
CREATE INDEX IF NOT EXISTS idx_erc20_deposits_chain_status ON erc20_deposits (chain, status);

CREATE TABLE IF NOT EXISTS token_metadata (
  chain TEXT NOT NULL,
  token_address TEXT NOT NULL,
  symbol TEXT NOT NULL,
  decimals SMALLINT NOT NULL,
  name TEXT NOT NULL,
  PRIMARY KEY (chain, token_address)
);

CREATE TABLE IF NOT EXISTS sweep_meta (
  chain TEXT NOT NULL,
  tx_hash TEXT NOT NULL,
  log_index BIGINT NOT NULL DEFAULT 0,
  sweep_tx_hash TEXT NOT NULL DEFAULT '',
  zero_balance_retry_count BIGINT NOT NULL DEFAULT 0,
  PRIMARY KEY (chain, tx_hash, log_index)
);

CREATE TABLE IF NOT EXISTS sweep_failures (
  chain TEXT NOT NULL,
  tx_hash TEXT NOT NULL,
  log_index BIGINT NOT NULL DEFAULT 0,
  consecutive_failure_count BIGINT NOT NULL DEFAULT 0,
  PRIMARY KEY (chain, tx_hash, log_index)
);

-- Holds the 'next_index' derivation counter (see register_account_auto) and
-- one 'last_block:<chain>' row per configured chain. Seeded to next_index=0
-- by `PostgresBackend::seed_next_index_if_missing` on first connect if the
-- bootstrap migration didn't already populate it.
CREATE TABLE IF NOT EXISTS state (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS webhook_deliveries (
  id TEXT NOT NULL,
  event TEXT NOT NULL,
  registration_id TEXT NOT NULL,
  webhook_url TEXT NOT NULL,
  payload TEXT NOT NULL,
  status TEXT NOT NULL,
  attempt_count BIGINT NOT NULL DEFAULT 0,
  last_http_status INTEGER,
  last_error TEXT,
  leased_until BIGINT,
  updated_at BIGINT NOT NULL,
  PRIMARY KEY (id, event)
);
CREATE INDEX IF NOT EXISTS idx_webhook_deliveries_pending
  ON webhook_deliveries (status, attempt_count, updated_at);
