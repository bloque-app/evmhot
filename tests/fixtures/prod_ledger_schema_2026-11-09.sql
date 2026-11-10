-- evmhot's eight tables as they exist in production (the ledger database, schema public), read on 2026-11-09 from
-- information_schema.columns and pg_indexes with the read-only database tool. Used to test the schema upgrade evmhot
-- runs on first start against the real starting point, not against an older evmhot schema.
-- Row counts that day: accounts 89,481; deposits 659; erc20_deposits 532; state 3; sweep_failures 0; sweep_meta 412;
-- token_metadata 36; webhook_deliveries 274.

CREATE TABLE accounts (
  id TEXT NOT NULL,
  derivation_index BIGINT NOT NULL,
  address TEXT NOT NULL,
  webhook_url TEXT NOT NULL
);
CREATE UNIQUE INDEX accounts_pkey ON accounts USING btree (id);
ALTER TABLE accounts ADD CONSTRAINT accounts_pkey PRIMARY KEY USING INDEX accounts_pkey;
CREATE UNIQUE INDEX accounts_address_key ON accounts USING btree (address);

CREATE TABLE deposits (
  chain TEXT NOT NULL,
  tx_hash TEXT NOT NULL,
  account_id TEXT NOT NULL,
  amount TEXT NOT NULL,
  status TEXT NOT NULL
);
CREATE UNIQUE INDEX deposits_pkey ON deposits USING btree (chain, tx_hash);
ALTER TABLE deposits ADD CONSTRAINT deposits_pkey PRIMARY KEY USING INDEX deposits_pkey;
CREATE INDEX idx_deposits_chain_status ON deposits USING btree (chain, status);

CREATE TABLE erc20_deposits (
  chain TEXT NOT NULL,
  tx_hash TEXT NOT NULL,
  log_index BIGINT NOT NULL,
  account_id TEXT NOT NULL,
  amount TEXT NOT NULL,
  token_address TEXT NOT NULL,
  token_symbol TEXT NOT NULL,
  status TEXT NOT NULL
);
CREATE UNIQUE INDEX erc20_deposits_pkey ON erc20_deposits USING btree (chain, tx_hash, log_index);
ALTER TABLE erc20_deposits ADD CONSTRAINT erc20_deposits_pkey PRIMARY KEY USING INDEX erc20_deposits_pkey;
CREATE INDEX idx_erc20_deposits_chain_status ON erc20_deposits USING btree (chain, status);

CREATE TABLE state (
  key TEXT NOT NULL,
  value TEXT NOT NULL
);
CREATE UNIQUE INDEX state_pkey ON state USING btree (key);
ALTER TABLE state ADD CONSTRAINT state_pkey PRIMARY KEY USING INDEX state_pkey;

CREATE TABLE sweep_failures (
  chain TEXT NOT NULL,
  tx_hash TEXT NOT NULL,
  log_index BIGINT NOT NULL DEFAULT 0,
  consecutive_failure_count BIGINT NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX sweep_failures_pkey ON sweep_failures USING btree (chain, tx_hash, log_index);
ALTER TABLE sweep_failures ADD CONSTRAINT sweep_failures_pkey PRIMARY KEY USING INDEX sweep_failures_pkey;

CREATE TABLE sweep_meta (
  chain TEXT NOT NULL,
  tx_hash TEXT NOT NULL,
  log_index BIGINT NOT NULL DEFAULT 0,
  sweep_tx_hash TEXT NOT NULL DEFAULT ''::text,
  zero_balance_retry_count BIGINT NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX sweep_meta_pkey ON sweep_meta USING btree (chain, tx_hash, log_index);
ALTER TABLE sweep_meta ADD CONSTRAINT sweep_meta_pkey PRIMARY KEY USING INDEX sweep_meta_pkey;

CREATE TABLE token_metadata (
  chain TEXT NOT NULL,
  token_address TEXT NOT NULL,
  symbol TEXT NOT NULL,
  decimals SMALLINT NOT NULL,
  name TEXT NOT NULL
);
CREATE UNIQUE INDEX token_metadata_pkey ON token_metadata USING btree (chain, token_address);
ALTER TABLE token_metadata ADD CONSTRAINT token_metadata_pkey PRIMARY KEY USING INDEX token_metadata_pkey;

CREATE TABLE webhook_deliveries (
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
  updated_at BIGINT NOT NULL
);
CREATE UNIQUE INDEX webhook_deliveries_pkey ON webhook_deliveries USING btree (id, event);
ALTER TABLE webhook_deliveries ADD CONSTRAINT webhook_deliveries_pkey PRIMARY KEY USING INDEX webhook_deliveries_pkey;
CREATE INDEX idx_webhook_deliveries_pending ON webhook_deliveries USING btree (status, attempt_count, updated_at);
