-- F-002a review fixes: record each sweep's target, mark deposits that need an
-- operator (needs_review, with a kind), and persist a sweep intent before any
-- sweep transaction is signed, so a sweep that lands but is not recorded is
-- found again instead of being swept twice.
ALTER TABLE sweeps ADD COLUMN sweep_target TEXT;
ALTER TABLE erc20_deposits ADD COLUMN review_kind TEXT;

CREATE TABLE sweep_intents (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  chain TEXT NOT NULL,
  group_key TEXT NOT NULL,
  keys TEXT NOT NULL,
  part_index INTEGER NOT NULL,
  amount TEXT NOT NULL,
  target TEXT NOT NULL,
  state TEXT NOT NULL,
  tx_hash TEXT,
  passes INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_sweep_intents_state ON sweep_intents (chain, state);
