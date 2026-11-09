CREATE TABLE webhook_deliveries (
  id TEXT NOT NULL,
  event TEXT NOT NULL,
  registration_id TEXT NOT NULL,
  webhook_url TEXT NOT NULL,
  payload TEXT NOT NULL,
  status TEXT NOT NULL,
  attempt_count INTEGER NOT NULL DEFAULT 0,
  last_http_status INTEGER,
  last_error TEXT,
  leased_until INTEGER,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (id, event)
);
CREATE INDEX idx_webhook_deliveries_pending
  ON webhook_deliveries (status, attempt_count, updated_at);
