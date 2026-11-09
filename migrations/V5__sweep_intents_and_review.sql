-- F-002a review fixes: record each sweep's target, mark deposits that need an
-- operator (needs_review, with a kind), and persist a sweep intent before any
-- sweep transaction is signed, so a sweep that lands but is not recorded is
-- found again instead of being swept twice.
ALTER TABLE sweeps ADD COLUMN sweep_target TEXT;
ALTER TABLE erc20_deposits ADD COLUMN review_kind TEXT;
ALTER TABLE deposits ADD COLUMN review_kind TEXT;

CREATE TABLE sweep_intents (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  chain TEXT NOT NULL,
  account_id TEXT NOT NULL,
  address TEXT NOT NULL,
  token_address TEXT NOT NULL,
  group_key TEXT NOT NULL,
  keys TEXT NOT NULL,
  part_index INTEGER NOT NULL,
  amount TEXT NOT NULL,
  target TEXT NOT NULL,
  state TEXT NOT NULL,
  tx_hash TEXT,
  nonce INTEGER,
  raw_tx TEXT,
  txs TEXT NOT NULL DEFAULT '[]',
  passes INTEGER NOT NULL DEFAULT 0,
  gone_passes INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_sweep_intents_state ON sweep_intents (chain, state);
-- At most one signed, unresolved transaction per deposit address, across tokens and the
-- native coin: an intent owns its nonce until it is resolved.
CREATE UNIQUE INDEX uq_sweep_intents_open ON sweep_intents (chain, account_id) WHERE state = 'pending';

-- An alert marker is logged once per (deposit, kind); this remembers it was.
CREATE TABLE sweep_alerts (
  chain TEXT NOT NULL,
  deposit_key TEXT NOT NULL,
  kind TEXT NOT NULL,
  PRIMARY KEY (chain, deposit_key, kind)
);
