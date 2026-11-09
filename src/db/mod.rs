//! Storage layer for evmhot.
//!
//! `Db` is a thin enum-dispatch facade (decision D1 from the Postgres-migration
//! eng review: enum over trait object — zero public-signature changes,
//! compiler-checked exhaustiveness) over two interchangeable backends:
//!
//! - [`sqlite`]: the original `rusqlite`-backed implementation (single-writer
//!   actor, priority lanes, WAL checkpoint logic). This is the rollback path
//!   and stays completely unexercised-but-present after a Postgres cutover.
//! - [`postgres`]: a `postgres` + `r2d2_postgres` backed implementation with
//!   no writer actor — Postgres handles concurrent writers natively via
//!   MVCC/row locking, which is the whole point of this migration (SQLite's
//!   WAL mode over a network filesystem has multi-second per-commit lock latency; Postgres
//!   does not).
//!
//! Every public method on `Db` is a two-armed `match` that delegates to
//! whichever backend was selected by [`is_postgres_url`] at construction
//! time, based on the `DATABASE_URL` scheme. Call sites elsewhere in the
//! crate (`lib.rs`, `monitor.rs`, `sweeper.rs`, `webhook.rs`, `api.rs`) are
//! completely unaware of which backend is live.
mod postgres;
mod sqlite;

use anyhow::Result;
use std::time::Duration;

pub use sqlite::{apply_pragmas_for_import, migrations, normalize_db_path, WriterConfig};

/// r2d2's own default read-pool size, kept as the `Db::new` default so
/// existing callers (in particular the SQLite test suite's direct `Db::new`
/// call sites) keep their current behavior.
const DEFAULT_READ_POOL_MAX_SIZE: u32 = 10;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DepositQueueCounts {
    pub native_detected: u64,
    pub native_failed: u64,
    pub erc20_detected: u64,
    pub erc20_failed: u64,
    /// Token deposits an operator must look at (`needs_review`).
    pub erc20_needs_review: u64,
}

impl DepositQueueCounts {
    pub fn has_pending(&self) -> bool {
        self.native_detected > 0
            || self.native_failed > 0
            || self.erc20_detected > 0
            || self.erc20_failed > 0
            || self.erc20_needs_review > 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebhookDeliveryRecord {
    pub id: String,
    pub event: String,
    pub registration_id: String,
    pub webhook_url: String,
    pub payload: String,
    pub status: String,
    pub attempt_count: u64,
    pub last_http_status: Option<u16>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Erc20Deposit {
    pub key: String,
    pub account_id: String,
    pub amount: String,
    pub token_address: String,
    pub token_symbol: String,
    /// Sender of the inbound transfer (REQ-WH-9). `None` for rows recorded
    /// before the sender was stored.
    pub from_address: Option<String>,
}

/// A native deposit to record, with its sender (REQ-WH-9).
#[derive(Clone, Debug)]
pub struct NewNativeDeposit {
    pub chain: String,
    pub tx_hash: String,
    pub account_id: String,
    pub amount: String,
    pub from_address: Option<String>,
}

/// A token deposit to record, with its sender (REQ-WH-9).
#[derive(Clone, Debug)]
pub struct NewErc20Deposit {
    pub chain: String,
    pub tx_hash: String,
    pub log_index: u64,
    pub account_id: String,
    pub amount: String,
    pub token_address: String,
    pub token_symbol: String,
    pub from_address: Option<String>,
}

/// One outbox row (`webhook_deliveries`), written in the same transaction as
/// the state change it reports (REQ-WH-7).
#[derive(Clone, Debug)]
pub struct OutboxEntry {
    pub id: String,
    pub event: String,
    pub registration_id: String,
    pub webhook_url: String,
    pub payload: String,
}

/// The sweep coordinate (REQ-SW-10): where the `Transfer` into the target sits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SweepRecord {
    pub sweep_tx_hash: String,
    pub sweep_block_number: u64,
    pub sweep_block_hash: String,
    /// Block-level log index of the `Transfer` into the target, from the receipt.
    pub sweep_log_index: u64,
    pub sweep_amount: String,
    /// The address the `Transfer` went to. `None` for sweeps recorded before
    /// the target was stored.
    pub sweep_target: Option<String>,
}

/// A sweep part that is about to be, or has been, sent. Persisted before the
/// transaction is signed so a part that lands unrecorded is found again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SweepIntent {
    pub id: i64,
    pub group_key: String,
    pub keys: Vec<String>,
    pub part_index: u64,
    pub amount: String,
    pub target: String,
    pub tx_hash: Option<String>,
    /// Nonce the sweep was signed with. `None` until it is signed.
    pub nonce: Option<u64>,
    /// Hex of the signed raw transaction, kept so it can be rebroadcast.
    pub raw_tx: Option<String>,
    pub passes: u64,
}

/// A token deposit to mark swept, with the `deposit_swept` outbox row that
/// goes with it (none when the registration has no webhook URL).
#[derive(Clone, Debug)]
pub struct Erc20Settlement {
    pub key: String,
    pub outbox: Option<OutboxEntry>,
}

/// Typed errors for the write-queue fast paths, so HTTP layers can map
/// queue-full / timeout conditions to 503s instead of generic 500s.
///
/// Shared by both backends. Message text is deliberately backend-neutral
/// (decision D7 from the Postgres-migration eng review) — the SQLite arm
/// still means "the interactive writer-actor lane is full/timed out", and
/// the Postgres arm means "the r2d2 connection pool is exhausted/timed out
/// acquiring a connection" — so an incident responder reading logs on the
/// Postgres path is never pointed at writer-actor code that isn't running.
#[derive(Debug, thiserror::Error)]
pub enum WriteQueueError {
    /// The write path is at capacity; the caller should fail fast (HTTP 503)
    /// and let the client retry.
    #[error("database write capacity exhausted")]
    QueueFull,
    /// The write was submitted but no result arrived within the configured
    /// timeout. The command may still execute later (at-least-once); all
    /// interactive writes are idempotent, so a retry is safe.
    #[error("database write timed out after {0:?} (write may still complete)")]
    Timeout(Duration),
    /// The storage backend is not accepting writes (SQLite: the dedicated
    /// writer thread died; Postgres: unrecoverable pool/connection failure).
    /// In production this precedes a process abort; only reads can still be
    /// served.
    #[error("database writer is not running")]
    WriterGone,
}

fn is_postgres_url(database_url: &str) -> bool {
    database_url.starts_with("postgres://") || database_url.starts_with("postgresql://")
}

enum DbInner {
    Sqlite(sqlite::Db),
    Postgres(postgres::PostgresBackend),
}

/// `Db` clones are cheap regardless of backend: the whole enum lives behind
/// one `Arc`, so `Clone` is just an atomic refcount bump (matches the
/// pre-migration `Db`, whose own fields were already `Arc`-backed
/// internally).
#[derive(Clone)]
pub struct Db {
    inner: std::sync::Arc<DbInner>,
}

impl Db {
    pub fn new(database_url: &str) -> Result<Self> {
        Self::with_pool_size(database_url, DEFAULT_READ_POOL_MAX_SIZE)
    }

    /// `max_size` only governs the SQLite arm's read pool (`DB_READ_POOL_SIZE`);
    /// the Postgres arm sizes its single pool from `DB_POOL_SIZE` instead
    /// (decision D9 from eng review), since there is no separate read/write
    /// split once the writer-actor is gone.
    pub fn with_pool_size(database_url: &str, max_size: u32) -> Result<Self> {
        let inner = if is_postgres_url(database_url) {
            DbInner::Postgres(postgres::PostgresBackend::connect(database_url)?)
        } else {
            DbInner::Sqlite(sqlite::Db::with_pool_size(database_url, max_size)?)
        };
        Ok(Self {
            inner: std::sync::Arc::new(inner),
        })
    }

    /// False once the storage backend can no longer accept writes (SQLite:
    /// dedicated writer thread died; Postgres: `SELECT 1` through the pool
    /// failed/timed out — decision D3 from eng review).
    pub fn writer_healthy(&self) -> bool {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.writer_healthy(),
            DbInner::Postgres(p) => p.writer_healthy(),
        }
    }

    /// Runs a `Db` operation on Tokio's blocking thread pool. Every `Db`
    /// method (either backend) is synchronous, so calling them directly from
    /// an async fn risks stalling a Tokio worker thread. `Db` is a cheap
    /// `Clone` (an `Arc`), so this just moves a clone onto `spawn_blocking`.
    pub async fn blocking<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Db) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let db = self.clone();
        tokio::task::spawn_blocking(move || f(&db))
            .await
            .map_err(|e| anyhow::anyhow!("Db blocking task panicked or was cancelled: {e}"))?
    }

    #[allow(dead_code)]
    pub fn get_next_derivation_index(&self) -> Result<u32> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.get_next_derivation_index(),
            DbInner::Postgres(p) => p.get_next_derivation_index(),
        }
    }

    pub fn register_account(
        &self,
        id: &str,
        index: u32,
        address: &str,
        webhook_url: &str,
    ) -> Result<()> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.register_account(id, index, address, webhook_url),
            DbInner::Postgres(p) => p.register_account(id, index, address, webhook_url),
        }
    }

    pub fn register_account_auto(
        &self,
        id: &str,
        webhook_url: &str,
        derive_address: impl Fn(u32) -> Result<String> + Send + 'static,
    ) -> Result<(u32, String, bool)> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.register_account_auto(id, webhook_url, derive_address),
            DbInner::Postgres(p) => p.register_account_auto(id, webhook_url, derive_address),
        }
    }

    pub fn get_registration_id_by_address(&self, address: &str) -> Result<Option<String>> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.get_registration_id_by_address(address),
            DbInner::Postgres(p) => p.get_registration_id_by_address(address),
        }
    }

    pub fn get_account_by_address(&self, address: &str) -> Result<Option<String>> {
        self.get_registration_id_by_address(address)
    }

    pub fn get_account_by_id(&self, id: &str) -> Result<Option<(u32, String, String)>> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.get_account_by_id(id),
            DbInner::Postgres(p) => p.get_account_by_id(id),
        }
    }

    pub fn get_webhook_url(&self, account_id: &str) -> Result<Option<String>> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.get_webhook_url(account_id),
            DbInner::Postgres(p) => p.get_webhook_url(account_id),
        }
    }

    pub fn record_deposit(
        &self,
        chain: &str,
        tx_hash: &str,
        account_id: &str,
        amount: &str,
    ) -> Result<bool> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.record_deposit(chain, tx_hash, account_id, amount),
            DbInner::Postgres(p) => p.record_deposit(chain, tx_hash, account_id, amount),
        }
    }

    pub fn mark_deposit_swept(&self, chain: &str, tx_hash: &str) -> Result<()> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.mark_deposit_swept(chain, tx_hash),
            DbInner::Postgres(p) => p.mark_deposit_swept(chain, tx_hash),
        }
    }

    pub fn mark_deposit_failed(&self, chain: &str, tx_hash: &str) -> Result<()> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.mark_deposit_failed(chain, tx_hash),
            DbInner::Postgres(p) => p.mark_deposit_failed(chain, tx_hash),
        }
    }

    pub fn get_detected_deposits(&self, chain: &str) -> Result<Vec<(String, String, String)>> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.get_detected_deposits(chain),
            DbInner::Postgres(p) => p.get_detected_deposits(chain),
        }
    }

    pub fn get_last_processed_block(&self, chain: &str) -> Result<u64> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.get_last_processed_block(chain),
            DbInner::Postgres(p) => p.get_last_processed_block(chain),
        }
    }

    pub fn set_last_processed_block(&self, chain: &str, block: u64) -> Result<()> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.set_last_processed_block(chain, block),
            DbInner::Postgres(p) => p.set_last_processed_block(chain, block),
        }
    }

    pub fn set_last_processed_block_priority(&self, chain: &str, block: u64) -> Result<()> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.set_last_processed_block_priority(chain, block),
            DbInner::Postgres(p) => p.set_last_processed_block_priority(chain, block),
        }
    }

    pub fn store_token_metadata(
        &self,
        chain: &str,
        address: &str,
        symbol: &str,
        decimals: u8,
        name: &str,
    ) -> Result<()> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.store_token_metadata(chain, address, symbol, decimals, name),
            DbInner::Postgres(p) => p.store_token_metadata(chain, address, symbol, decimals, name),
        }
    }

    pub fn get_token_metadata(
        &self,
        chain: &str,
        address: &str,
    ) -> Result<Option<(String, u8, String)>> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.get_token_metadata(chain, address),
            DbInner::Postgres(p) => p.get_token_metadata(chain, address),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_erc20_deposit(
        &self,
        chain: &str,
        tx_hash: &str,
        log_index: u64,
        account_id: &str,
        amount: &str,
        token_address: &str,
        token_symbol: &str,
    ) -> Result<bool> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.record_erc20_deposit(
                chain,
                tx_hash,
                log_index,
                account_id,
                amount,
                token_address,
                token_symbol,
            ),
            DbInner::Postgres(p) => p.record_erc20_deposit(
                chain,
                tx_hash,
                log_index,
                account_id,
                amount,
                token_address,
                token_symbol,
            ),
        }
    }

    pub fn get_detected_erc20_deposits(&self, chain: &str) -> Result<Vec<Erc20Deposit>> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.get_detected_erc20_deposits(chain),
            DbInner::Postgres(p) => p.get_detected_erc20_deposits(chain),
        }
    }

    pub fn mark_erc20_deposit_swept(&self, chain: &str, local_key: &str) -> Result<()> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.mark_erc20_deposit_swept(chain, local_key),
            DbInner::Postgres(p) => p.mark_erc20_deposit_swept(chain, local_key),
        }
    }

    pub fn mark_erc20_deposits_swept_for_account_token(
        &self,
        chain: &str,
        account_id: &str,
        token_address: &str,
    ) -> Result<Vec<(String, String)>> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => {
                s.mark_erc20_deposits_swept_for_account_token(chain, account_id, token_address)
            }
            DbInner::Postgres(p) => {
                p.mark_erc20_deposits_swept_for_account_token(chain, account_id, token_address)
            }
        }
    }

    pub fn increment_zero_balance_count(&self, chain: &str, local_key: &str) -> Result<u64> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.increment_zero_balance_count(chain, local_key),
            DbInner::Postgres(p) => p.increment_zero_balance_count(chain, local_key),
        }
    }

    #[allow(dead_code)]
    pub fn set_sweep_tx_hash(&self, chain: &str, local_key: &str, tx_hash: &str) -> Result<()> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.set_sweep_tx_hash(chain, local_key, tx_hash),
            DbInner::Postgres(p) => p.set_sweep_tx_hash(chain, local_key, tx_hash),
        }
    }

    pub fn set_sweep_tx_hash_for_keys(
        &self,
        chain: &str,
        local_keys: &[String],
        tx_hash: &str,
    ) -> Result<()> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.set_sweep_tx_hash_for_keys(chain, local_keys, tx_hash),
            DbInner::Postgres(p) => p.set_sweep_tx_hash_for_keys(chain, local_keys, tx_hash),
        }
    }

    #[allow(dead_code)]
    pub fn get_sweep_meta(&self, chain: &str, local_key: &str) -> Result<Option<(String, u64)>> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.get_sweep_meta(chain, local_key),
            DbInner::Postgres(p) => p.get_sweep_meta(chain, local_key),
        }
    }

    pub fn increment_sweep_failure_count(&self, chain: &str, local_key: &str) -> Result<u64> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.increment_sweep_failure_count(chain, local_key),
            DbInner::Postgres(p) => p.increment_sweep_failure_count(chain, local_key),
        }
    }

    pub fn mark_erc20_deposit_failed(&self, chain: &str, local_key: &str) -> Result<()> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.mark_erc20_deposit_failed(chain, local_key),
            DbInner::Postgres(p) => p.mark_erc20_deposit_failed(chain, local_key),
        }
    }

    pub fn mark_erc20_deposits_failed_for_account_token(
        &self,
        chain: &str,
        account_id: &str,
        token_address: &str,
    ) -> Result<Vec<String>> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => {
                s.mark_erc20_deposits_failed_for_account_token(chain, account_id, token_address)
            }
            DbInner::Postgres(p) => {
                p.mark_erc20_deposits_failed_for_account_token(chain, account_id, token_address)
            }
        }
    }

    pub fn deposit_queue_counts(&self, chain: &str) -> Result<DepositQueueCounts> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.deposit_queue_counts(chain),
            DbInner::Postgres(p) => p.deposit_queue_counts(chain),
        }
    }

    pub fn retry_native_deposit(&self, chain: &str, tx_hash: &str) -> Result<bool> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.retry_native_deposit(chain, tx_hash),
            DbInner::Postgres(p) => p.retry_native_deposit(chain, tx_hash),
        }
    }

    pub fn retry_erc20_deposit(&self, chain: &str, tx_hash: &str, log_index: u64) -> Result<bool> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.retry_erc20_deposit(chain, tx_hash, log_index),
            DbInner::Postgres(p) => p.retry_erc20_deposit(chain, tx_hash, log_index),
        }
    }

    pub fn get_sweep_failure_count(&self, chain: &str, local_key: &str) -> Result<u64> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.get_sweep_failure_count(chain, local_key),
            DbInner::Postgres(p) => p.get_sweep_failure_count(chain, local_key),
        }
    }

    /// Records a native deposit and, when it is new, its `deposit_detected`
    /// outbox row, in one transaction (REQ-WH-7). Returns whether it was new.
    pub fn record_native_deposit_atomic(
        &self,
        deposit: &NewNativeDeposit,
        outbox: Option<&OutboxEntry>,
    ) -> Result<bool> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.record_native_deposit_atomic(deposit, outbox),
            DbInner::Postgres(p) => p.record_native_deposit_atomic(deposit, outbox),
        }
    }

    /// Records a token deposit and, when it is new, its `deposit_detected`
    /// outbox row, in one transaction (REQ-WH-7). Returns whether it was new.
    pub fn record_erc20_deposit_atomic(
        &self,
        deposit: &NewErc20Deposit,
        outbox: Option<&OutboxEntry>,
    ) -> Result<bool> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.record_erc20_deposit_atomic(deposit, outbox),
            DbInner::Postgres(p) => p.record_erc20_deposit_atomic(deposit, outbox),
        }
    }

    /// Marks a native deposit swept and enqueues its `deposit_swept` row in one
    /// transaction (REQ-WH-7). Returns whether an outbox row was written.
    pub fn complete_native_sweep(
        &self,
        chain: &str,
        tx_hash: &str,
        outbox: Option<&OutboxEntry>,
    ) -> Result<bool> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.complete_native_sweep(chain, tx_hash, outbox),
            DbInner::Postgres(p) => p.complete_native_sweep(chain, tx_hash, outbox),
        }
    }

    /// Marks token deposits swept and enqueues their `deposit_swept` rows in
    /// one transaction (REQ-WH-7). Returns how many outbox rows were written.
    pub fn complete_erc20_sweep(
        &self,
        chain: &str,
        settlements: &[Erc20Settlement],
    ) -> Result<usize> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.complete_erc20_sweep(chain, settlements),
            DbInner::Postgres(p) => p.complete_erc20_sweep(chain, settlements),
        }
    }

    /// Stores one sweep against each of the given deposit keys (REQ-SW-10).
    /// Idempotent per `(deposit, sweep tx, log index)`.
    pub fn record_sweep(&self, chain: &str, keys: &[String], record: &SweepRecord) -> Result<()> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.record_sweep(chain, keys, record),
            DbInner::Postgres(p) => p.record_sweep(chain, keys, record),
        }
    }

    /// The sweeps recorded against a deposit, oldest first.
    pub fn get_sweeps(&self, chain: &str, local_key: &str) -> Result<Vec<SweepRecord>> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.get_sweeps(chain, local_key),
            DbInner::Postgres(p) => p.get_sweeps(chain, local_key),
        }
    }

    /// Persists a pending sweep intent before the transaction is signed.
    pub fn create_sweep_intent(
        &self,
        chain: &str,
        group_key: &str,
        keys: &[String],
        part_index: u64,
        amount: &str,
        target: &str,
    ) -> Result<i64> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => {
                s.create_sweep_intent(chain, group_key, keys, part_index, amount, target)
            }
            DbInner::Postgres(p) => {
                p.create_sweep_intent(chain, group_key, keys, part_index, amount, target)
            }
        }
    }

    /// Stores the signed transaction (nonce, hash, raw bytes) before it is broadcast.
    pub fn set_sweep_intent_signed(
        &self,
        id: i64,
        nonce: u64,
        tx_hash: &str,
        raw_tx: &str,
    ) -> Result<()> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.set_sweep_intent_signed(id, nonce, tx_hash, raw_tx),
            DbInner::Postgres(p) => p.set_sweep_intent_signed(id, nonce, tx_hash, raw_tx),
        }
    }

    pub fn set_sweep_intent_tx(&self, id: i64, tx_hash: &str) -> Result<()> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.set_sweep_intent_tx(id, tx_hash),
            DbInner::Postgres(p) => p.set_sweep_intent_tx(id, tx_hash),
        }
    }

    /// Moves a pending intent to a terminal state (`failed`, `review`).
    pub fn finish_sweep_intent(&self, id: i64, state: &str) -> Result<()> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.finish_sweep_intent(id, state),
            DbInner::Postgres(p) => p.finish_sweep_intent(id, state),
        }
    }

    /// Counts one more pass without a receipt and returns the total.
    pub fn bump_sweep_intent_passes(&self, id: i64) -> Result<u64> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.bump_sweep_intent_passes(id),
            DbInner::Postgres(p) => p.bump_sweep_intent_passes(id),
        }
    }

    /// Pending intents that touch any of the given deposit keys.
    pub fn pending_sweep_intents(&self, chain: &str, keys: &[String]) -> Result<Vec<SweepIntent>> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.pending_sweep_intents(chain, keys),
            DbInner::Postgres(p) => p.pending_sweep_intents(chain, keys),
        }
    }

    /// One transaction: the intent becomes `succeeded`, its sweep is recorded
    /// against its deposits, and (for the last part) the deposits are marked
    /// swept with their `deposit_swept` outbox rows (REQ-WH-7). Returns how
    /// many outbox rows were written.
    pub fn settle_sweep_intent(
        &self,
        chain: &str,
        id: i64,
        keys: &[String],
        record: &SweepRecord,
        settlements: &[Erc20Settlement],
    ) -> Result<usize> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.settle_sweep_intent(chain, id, keys, record, settlements),
            DbInner::Postgres(p) => p.settle_sweep_intent(chain, id, keys, record, settlements),
        }
    }

    /// Moves token deposits to `needs_review` (never auto-settled or retried).
    pub fn mark_erc20_deposits_needs_review_for_keys(
        &self,
        chain: &str,
        keys: &[String],
        kind: &str,
    ) -> Result<()> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.mark_erc20_deposits_needs_review_for_keys(chain, keys, kind),
            DbInner::Postgres(p) => p.mark_erc20_deposits_needs_review_for_keys(chain, keys, kind),
        }
    }

    pub fn get_review_kind(&self, chain: &str, local_key: &str) -> Result<Option<String>> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.get_review_kind(chain, local_key),
            DbInner::Postgres(p) => p.get_review_kind(chain, local_key),
        }
    }

    /// Sender recorded for a native deposit (`None` for older rows).
    pub fn get_deposit_from_address(&self, chain: &str, tx_hash: &str) -> Result<Option<String>> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.get_deposit_from_address(chain, tx_hash),
            DbInner::Postgres(p) => p.get_deposit_from_address(chain, tx_hash),
        }
    }

    /// Marks exactly these token deposits swept, with no sweep transaction (the
    /// bounded zero-balance rule, REQ-SW-3).
    pub fn mark_erc20_deposits_swept_for_keys(&self, chain: &str, keys: &[String]) -> Result<()> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.mark_erc20_deposits_swept_for_keys(chain, keys),
            DbInner::Postgres(p) => p.mark_erc20_deposits_swept_for_keys(chain, keys),
        }
    }

    /// Marks exactly these token deposits failed (REQ-SW-4).
    pub fn mark_erc20_deposits_failed_for_keys(&self, chain: &str, keys: &[String]) -> Result<()> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.mark_erc20_deposits_failed_for_keys(chain, keys),
            DbInner::Postgres(p) => p.mark_erc20_deposits_failed_for_keys(chain, keys),
        }
    }

    pub fn upsert_webhook_delivery(
        &self,
        id: &str,
        event: &str,
        registration_id: &str,
        webhook_url: &str,
        payload: &str,
    ) -> Result<bool> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => {
                s.upsert_webhook_delivery(id, event, registration_id, webhook_url, payload)
            }
            DbInner::Postgres(p) => {
                p.upsert_webhook_delivery(id, event, registration_id, webhook_url, payload)
            }
        }
    }

    pub fn claim_webhook_delivery(
        &self,
        id: &str,
        event: &str,
        lease_until: i64,
        max_retries: u32,
    ) -> Result<bool> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.claim_webhook_delivery(id, event, lease_until, max_retries),
            DbInner::Postgres(p) => p.claim_webhook_delivery(id, event, lease_until, max_retries),
        }
    }

    pub fn record_webhook_attempt(
        &self,
        id: &str,
        event: &str,
        http_status: Option<u16>,
        error: Option<&str>,
        status: &str,
    ) -> Result<u64> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.record_webhook_attempt(id, event, http_status, error, status),
            DbInner::Postgres(p) => p.record_webhook_attempt(id, event, http_status, error, status),
        }
    }

    pub fn get_pending_webhook_delivery_keys(
        &self,
        max_retries: u32,
        batch_size: u32,
    ) -> Result<Vec<(String, String)>> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.get_pending_webhook_delivery_keys(max_retries, batch_size),
            DbInner::Postgres(p) => p.get_pending_webhook_delivery_keys(max_retries, batch_size),
        }
    }

    pub fn get_webhook_delivery(
        &self,
        id: &str,
        event: &str,
    ) -> Result<Option<WebhookDeliveryRecord>> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.get_webhook_delivery(id, event),
            DbInner::Postgres(p) => p.get_webhook_delivery(id, event),
        }
    }

    pub fn retry_webhook_delivery(&self, id: &str, event: &str) -> Result<bool> {
        match self.inner.as_ref() {
            DbInner::Sqlite(s) => s.retry_webhook_delivery(id, event),
            DbInner::Postgres(p) => p.retry_webhook_delivery(id, event),
        }
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;

    #[test]
    fn test_is_postgres_url_scheme_detection() {
        assert!(is_postgres_url("postgres://user:pass@host/db"));
        assert!(is_postgres_url("postgresql://user:pass@host/db"));
        assert!(!is_postgres_url("sqlite:wallet.db"));
        assert!(!is_postgres_url("/tmp/wallet.db"));
        assert!(!is_postgres_url("wallet.db"));
    }

    #[test]
    fn test_sqlite_url_still_routes_to_sqlite_backend() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Db::new(tmp.path().to_str().unwrap()).unwrap();
        assert!(matches!(db.inner.as_ref(), DbInner::Sqlite(_)));
        db.register_account("u1", 0, "0x1", "https://example.com")
            .unwrap();
        assert_eq!(
            db.get_account_by_id("u1").unwrap().unwrap().1,
            "0x1".to_string()
        );
    }
}
