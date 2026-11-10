-- F-002a: sender of each inbound deposit (REQ-WH-9) and one row per sweep that
-- settled a deposit (REQ-SW-10). Nullable column and a new table only, so rows
-- written before this migration stay valid.
ALTER TABLE deposits ADD COLUMN from_address TEXT;
ALTER TABLE erc20_deposits ADD COLUMN from_address TEXT;

CREATE TABLE sweeps (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  chain TEXT NOT NULL,
  tx_hash TEXT NOT NULL,
  log_index INTEGER NOT NULL DEFAULT 0,
  sweep_tx_hash TEXT NOT NULL,
  sweep_block_number INTEGER NOT NULL,
  sweep_block_hash TEXT NOT NULL,
  sweep_log_index INTEGER NOT NULL,
  sweep_amount TEXT NOT NULL,
  UNIQUE (chain, tx_hash, log_index, sweep_tx_hash, sweep_log_index)
);
