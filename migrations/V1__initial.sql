CREATE TABLE accounts (
  id TEXT PRIMARY KEY NOT NULL,
  derivation_index INTEGER NOT NULL,
  address TEXT NOT NULL UNIQUE,
  webhook_url TEXT NOT NULL
);

CREATE TABLE deposits (
  chain TEXT NOT NULL,
  tx_hash TEXT NOT NULL,
  account_id TEXT NOT NULL,
  amount TEXT NOT NULL,
  status TEXT NOT NULL,
  PRIMARY KEY (chain, tx_hash)
);
CREATE INDEX idx_deposits_chain_status ON deposits (chain, status);

CREATE TABLE erc20_deposits (
  chain TEXT NOT NULL,
  tx_hash TEXT NOT NULL,
  log_index INTEGER NOT NULL,
  account_id TEXT NOT NULL,
  amount TEXT NOT NULL,
  token_address TEXT NOT NULL,
  token_symbol TEXT NOT NULL,
  status TEXT NOT NULL,
  PRIMARY KEY (chain, tx_hash, log_index)
);
CREATE INDEX idx_erc20_deposits_chain_status ON erc20_deposits (chain, status);

CREATE TABLE token_metadata (
  chain TEXT NOT NULL,
  token_address TEXT NOT NULL,
  symbol TEXT NOT NULL,
  decimals INTEGER NOT NULL,
  name TEXT NOT NULL,
  PRIMARY KEY (chain, token_address)
);

CREATE TABLE sweep_meta (
  chain TEXT NOT NULL,
  tx_hash TEXT NOT NULL,
  log_index INTEGER NOT NULL DEFAULT 0,
  sweep_tx_hash TEXT NOT NULL DEFAULT '',
  zero_balance_retry_count INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (chain, tx_hash, log_index)
);

CREATE TABLE sweep_failures (
  chain TEXT NOT NULL,
  tx_hash TEXT NOT NULL,
  log_index INTEGER NOT NULL DEFAULT 0,
  consecutive_failure_count INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (chain, tx_hash, log_index)
);

CREATE TABLE state (
  key TEXT PRIMARY KEY NOT NULL,
  value TEXT NOT NULL
);
