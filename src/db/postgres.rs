//! Postgres storage backend (evmhot Postgres storage migration).
//!
//! Deliberately much simpler than [`super::sqlite`]: Postgres handles
//! concurrent writers natively via MVCC/row locking, so there is no
//! single-writer actor, no priority lanes, no WAL/checkpoint logic here —
//! every method just borrows a pooled connection and runs its SQL directly.
//! The `next_index` counter (BIP44 derivation index allocation) is the one
//! place that needs an explicit atomicity argument; see
//! [`PostgresBackend::register_account_auto`].
use super::{DepositQueueCounts, Erc20Deposit, WebhookDeliveryRecord, WriteQueueError};
use anyhow::{anyhow, Context, Result};
use postgres::{Client, NoTls, Row};
use postgres_openssl::MakeTlsConnector;
use r2d2_postgres::PostgresConnectionManager;
use std::time::Duration;

/// Bundled AWS RDS CA chain (decision D5 from eng review — wallet service,
/// verify the server identity, not just encrypt). Downloaded from
/// `https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem`.
const RDS_CA_BUNDLE: &str = include_str!("../../certs/rds-global-bundle.pem");

/// Health-check pool-acquire timeout (decision D3 from eng review): a task
/// that loses RDS connectivity fails ALB health checks within one
/// unhealthy-threshold window instead of silently serving errors.
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(2);

/// Advisory-lock key for the bootstrap migration (decision from eng review:
/// `pg_advisory_lock` so two ECS tasks briefly alive during a rolling deploy
/// can't race the migration). Arbitrary but stable across deploys.
const MIGRATION_LOCK_KEY: i64 = 0x65766d5f706731; // "evm_pg1" as bytes

fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Wraps the two possible pool shapes so callers don't need to care whether
/// TLS is active. `postgres::Client` (the deref target of both r2d2 managers'
/// `Connection` type) is identical either way — TLS only affects how the
/// connection is *established*, not its query API — so every domain method
/// below just asks for `&mut Client` via [`PgConnGuard::client`].
enum PgPool {
    Plain(r2d2::Pool<PostgresConnectionManager<NoTls>>),
    Tls(r2d2::Pool<PostgresConnectionManager<MakeTlsConnector>>),
}

enum PgConnGuard {
    Plain(r2d2::PooledConnection<PostgresConnectionManager<NoTls>>),
    Tls(r2d2::PooledConnection<PostgresConnectionManager<MakeTlsConnector>>),
}

impl PgConnGuard {
    fn client(&mut self) -> &mut Client {
        match self {
            PgConnGuard::Plain(c) => &mut *c,
            PgConnGuard::Tls(c) => &mut *c,
        }
    }
}

impl PgPool {
    /// Acquire with the pool's configured acquire timeout. Any failure here
    /// (exhaustion or a dead connection) maps onto `WriteQueueError` so the
    /// existing 503 handling in `api.rs`/`lib.rs` keeps working unchanged
    /// (decision D9 from eng review).
    fn get(&self) -> std::result::Result<PgConnGuard, WriteQueueError> {
        match self {
            PgPool::Plain(p) => p
                .get()
                .map(PgConnGuard::Plain)
                .map_err(|_| WriteQueueError::Timeout(POOL_ACQUIRE_TIMEOUT)),
            PgPool::Tls(p) => p
                .get()
                .map(PgConnGuard::Tls)
                .map_err(|_| WriteQueueError::Timeout(POOL_ACQUIRE_TIMEOUT)),
        }
    }

    /// Same as `get`, but with an explicit short timeout for health checks so
    /// a slow/dead pool never makes `writer_healthy()` itself hang.
    fn get_timeout(&self, timeout: Duration) -> Option<PgConnGuard> {
        match self {
            PgPool::Plain(p) => p.get_timeout(timeout).ok().map(PgConnGuard::Plain),
            PgPool::Tls(p) => p.get_timeout(timeout).ok().map(PgConnGuard::Tls),
        }
    }
}

/// r2d2 acquire timeout (decision D9 from eng review): 5s, mapped onto the
/// existing `WriteQueueError::Timeout` 503 path.
const POOL_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// `DB_POOL_SIZE` default (decision D9 from eng review): ~50x headroom over
/// observed peak (~1 rps) while staying polite on the shared `services-db`
/// instance's `max_connections`.
const DEFAULT_POOL_SIZE: u32 = 10;

pub(crate) struct PostgresBackend {
    pool: PgPool,
}

impl PostgresBackend {
    /// Connects, bootstraps the schema (idempotent), then runs the
    /// SQLite→Postgres bootstrap migration if the trigger conditions hold
    /// (see [`crate::sqlite_import`]).
    ///
    /// TLS mode is controlled by `DB_TLS_MODE` (`verify-ca` default, using
    /// the bundled AWS RDS CA chain; `disable` for local/Docker Postgres that
    /// doesn't speak TLS at all, e.g. the `postgres:16-alpine` dry-run
    /// container). Pool size is `DB_POOL_SIZE` (default 10).
    pub fn connect(database_url: &str) -> Result<Self> {
        let pool_size = env_u32("DB_POOL_SIZE", DEFAULT_POOL_SIZE);
        let tls_mode = std::env::var("DB_TLS_MODE").unwrap_or_else(|_| "verify-ca".to_string());

        let pool = if tls_mode.eq_ignore_ascii_case("disable") {
            let manager = PostgresConnectionManager::new(database_url.parse()?, NoTls);
            let pool = r2d2::Pool::builder()
                .max_size(pool_size)
                .connection_timeout(POOL_ACQUIRE_TIMEOUT)
                .build(manager)
                .context("failed to build Postgres connection pool (NoTls)")?;
            PgPool::Plain(pool)
        } else {
            let connector = build_tls_connector()?;
            let manager = PostgresConnectionManager::new(database_url.parse()?, connector);
            let pool = r2d2::Pool::builder()
                .max_size(pool_size)
                .connection_timeout(POOL_ACQUIRE_TIMEOUT)
                .build(manager)
                .context("failed to build Postgres connection pool (TLS verify-ca)")?;
            PgPool::Tls(pool)
        };

        let backend = Self { pool };
        backend.bootstrap_schema()?;
        backend.run_bootstrap_migration_if_needed()?;
        backend.seed_next_index_if_missing()?;
        Ok(backend)
    }

    fn conn(&self) -> Result<PgConnGuard> {
        self.pool.get().map_err(anyhow::Error::from)
    }

    fn bootstrap_schema(&self) -> Result<()> {
        let mut guard = self.conn()?;
        crate::sqlite_import::ensure_postgres_schema(guard.client())
    }

    /// Runs the automatic bootstrap migration (see
    /// `crate::sqlite_import::migrate_sqlite_file_to_postgres`) when all
    /// trigger conditions hold: `EVM_MIGRATE_FROM_SQLITE` points at an
    /// existing file, and Postgres is currently empty (no accounts, no
    /// `next_index` row). Idempotent and safe to leave the env var set
    /// permanently — once Postgres is populated this becomes a no-op on
    /// every subsequent boot.
    fn run_bootstrap_migration_if_needed(&self) -> Result<()> {
        let sqlite_path = match std::env::var("EVM_MIGRATE_FROM_SQLITE") {
            Ok(p) if !p.trim().is_empty() => p,
            _ => return Ok(()),
        };
        if !std::path::Path::new(&sqlite_path).exists() {
            tracing::warn!(
                path = %sqlite_path,
                "EVM_MIGRATE_FROM_SQLITE is set but the file does not exist; skipping bootstrap migration"
            );
            return Ok(());
        }

        let mut guard = self.conn()?;
        let client = guard.client();

        // pg_advisory_lock: two ECS tasks briefly alive during a rolling
        // deploy must not race the migration. Session-scoped; released
        // explicitly below (also released automatically if the connection
        // drops).
        client
            .execute("SELECT pg_advisory_lock($1)", &[&MIGRATION_LOCK_KEY])
            .context("failed to acquire migration advisory lock")?;

        let result = (|| -> Result<()> {
            let already_populated = postgres_already_populated(client)?;
            if already_populated {
                tracing::info!(
                    "Postgres already has accounts/next_index; bootstrap migration skipped (no-op)"
                );
                return Ok(());
            }

            tracing::info!(path = %sqlite_path, "starting SQLite->Postgres bootstrap migration");
            let summary =
                crate::sqlite_import::migrate_sqlite_file_to_postgres(&sqlite_path, client)?;
            tracing::info!(
                accounts = summary.accounts.1,
                deposits = summary.deposits.1,
                erc20_deposits = summary.erc20_deposits.1,
                token_metadata = summary.token_metadata.1,
                sweep_meta = summary.sweep_meta.1,
                sweep_failures = summary.sweep_failures.1,
                webhook_deliveries = summary.webhook_deliveries.1,
                next_index = ?summary.next_index,
                block_cursors = ?summary.block_cursors,
                "bootstrap migration complete"
            );
            Ok(())
        })();

        let _ = client.execute("SELECT pg_advisory_unlock($1)", &[&MIGRATION_LOCK_KEY]);
        result
    }

    /// Defensive seed for the case where no migration ran (fresh Postgres in
    /// local dev/tests, or `EVM_MIGRATE_ENABLED` never set) — mirrors
    /// `migrations/V3__next_index_counter.sql`'s SQLite seeding behavior so
    /// `register_account_auto` always has a counter row to increment.
    fn seed_next_index_if_missing(&self) -> Result<()> {
        let mut guard = self.conn()?;
        guard.client().execute(
            "INSERT INTO state (key, value) VALUES ('next_index', '0')
             ON CONFLICT (key) DO NOTHING",
            &[],
        )?;
        Ok(())
    }

    /// `SELECT 1` through the pool with a ~2s acquire timeout (decision D3
    /// from eng review).
    pub fn writer_healthy(&self) -> bool {
        let Some(mut guard) = self.pool.get_timeout(HEALTH_CHECK_TIMEOUT) else {
            return false;
        };
        guard.client().query_one("SELECT 1", &[]).is_ok()
    }

    #[allow(dead_code)]
    pub fn get_next_derivation_index(&self) -> Result<u32> {
        let mut guard = self.conn()?;
        let row = guard.client().query_one(
            "SELECT COALESCE(MAX(derivation_index) + 1, 0) FROM accounts",
            &[],
        )?;
        let idx: i64 = row.get(0);
        Ok(idx as u32)
    }

    pub fn register_account(
        &self,
        id: &str,
        index: u32,
        address: &str,
        webhook_url: &str,
    ) -> Result<()> {
        let mut guard = self.conn()?;
        guard.client().execute(
            "INSERT INTO accounts (id, derivation_index, address, webhook_url)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO UPDATE SET
               derivation_index = excluded.derivation_index,
               address = excluded.address,
               webhook_url = excluded.webhook_url",
            &[&id, &(index as i64), &address, &webhook_url],
        )?;
        Ok(())
    }

    /// Atomic `next_index` allocation (correctness-critical — see the
    /// migration plan's "next_index atomicity" section). SQLite serializes
    /// this via the single-writer thread; Postgres has no such thread, so
    /// the `UPDATE ... RETURNING` below does the equivalent job: the row
    /// lock Postgres takes on the `state` row for the duration of the
    /// `UPDATE` serializes concurrent registrations correctly, entirely
    /// within RDS (no NFS/network-filesystem latency involved).
    ///
    /// The counter is already near `2^31` in production — see
    /// `evmhot/TODOS.md` for the follow-up BIP44 hardened-derivation ceiling
    /// guard, orthogonal to this migration.
    pub fn register_account_auto(
        &self,
        id: &str,
        webhook_url: &str,
        derive_address: impl Fn(u32) -> Result<String> + Send + 'static,
    ) -> Result<(u32, String, bool)> {
        let mut guard = self.conn()?;
        let client = guard.client();
        let mut tx = client.transaction()?;

        let existing = tx
            .query_opt(
                "SELECT derivation_index, address FROM accounts WHERE id = $1",
                &[&id],
            )?
            .map(|row| {
                let index: i64 = row.get(0);
                let address: String = row.get(1);
                (index as u32, address)
            });
        if let Some((index, address)) = existing {
            tx.commit()?;
            return Ok((index, address, false));
        }

        let next_index_row = tx.query_one(
            "UPDATE state SET value = (value::bigint + 1)::text
             WHERE key = 'next_index'
             RETURNING (value::bigint - 1)::bigint",
            &[],
        )?;
        let next_index: i64 = next_index_row.get(0);
        let next_index_u32 = u32::try_from(next_index)
            .map_err(|_| anyhow!("next_index counter overflowed u32 (near BIP44 2^31 boundary)"))?;

        let address = derive_address(next_index_u32)?;

        tx.execute(
            "INSERT INTO accounts (id, derivation_index, address, webhook_url)
             VALUES ($1, $2, $3, $4)",
            &[&id, &next_index, &address, &webhook_url],
        )?;
        tx.commit()?;

        Ok((next_index_u32, address, true))
    }

    pub fn get_registration_id_by_address(&self, address: &str) -> Result<Option<String>> {
        let mut guard = self.conn()?;
        let row = guard
            .client()
            .query_opt("SELECT id FROM accounts WHERE address = $1", &[&address])?;
        Ok(row.map(|r| r.get(0)))
    }

    pub fn get_account_by_id(&self, id: &str) -> Result<Option<(u32, String, String)>> {
        let mut guard = self.conn()?;
        let row = guard.client().query_opt(
            "SELECT derivation_index, address, webhook_url FROM accounts WHERE id = $1",
            &[&id],
        )?;
        Ok(row.map(|r| {
            let index: i64 = r.get(0);
            (index as u32, r.get(1), r.get(2))
        }))
    }

    pub fn get_webhook_url(&self, account_id: &str) -> Result<Option<String>> {
        let mut guard = self.conn()?;
        let row = guard.client().query_opt(
            "SELECT webhook_url FROM accounts WHERE id = $1",
            &[&account_id],
        )?;
        Ok(row.map(|r| r.get(0)))
    }

    pub fn record_deposit(
        &self,
        chain: &str,
        tx_hash: &str,
        account_id: &str,
        amount: &str,
    ) -> Result<bool> {
        let mut guard = self.conn()?;
        let changes = guard.client().execute(
            "INSERT INTO deposits (chain, tx_hash, account_id, amount, status)
             VALUES ($1, $2, $3, $4, 'detected')
             ON CONFLICT (chain, tx_hash) DO NOTHING",
            &[&chain, &tx_hash, &account_id, &amount],
        )?;
        Ok(changes == 1)
    }

    pub fn mark_deposit_swept(&self, chain: &str, tx_hash: &str) -> Result<()> {
        let mut guard = self.conn()?;
        guard.client().execute(
            "UPDATE deposits SET status = 'swept' WHERE chain = $1 AND tx_hash = $2",
            &[&chain, &tx_hash],
        )?;
        Ok(())
    }

    pub fn mark_deposit_failed(&self, chain: &str, tx_hash: &str) -> Result<()> {
        let mut guard = self.conn()?;
        guard.client().execute(
            "UPDATE deposits SET status = 'failed' WHERE chain = $1 AND tx_hash = $2",
            &[&chain, &tx_hash],
        )?;
        Ok(())
    }

    pub fn get_detected_deposits(&self, chain: &str) -> Result<Vec<(String, String, String)>> {
        let mut guard = self.conn()?;
        let rows = guard.client().query(
            "SELECT tx_hash, account_id, amount FROM deposits
             WHERE chain = $1 AND status = 'detected'",
            &[&chain],
        )?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get(0), r.get(1), r.get(2)))
            .collect())
    }

    pub fn get_last_processed_block(&self, chain: &str) -> Result<u64> {
        let mut guard = self.conn()?;
        let key = last_block_key(chain);
        let row = guard
            .client()
            .query_opt("SELECT value FROM state WHERE key = $1", &[&key])?;
        Ok(row
            .map(|r| r.get::<_, String>(0))
            .map(|v| v.parse().unwrap_or(0))
            .unwrap_or(0))
    }

    pub fn set_last_processed_block(&self, chain: &str, block: u64) -> Result<()> {
        self.set_last_processed_block_inner(chain, block)
    }

    pub fn set_last_processed_block_priority(&self, chain: &str, block: u64) -> Result<()> {
        // No priority lane on the Postgres path (no writer actor to
        // saturate); every write already goes straight to the pool.
        self.set_last_processed_block_inner(chain, block)
    }

    fn set_last_processed_block_inner(&self, chain: &str, block: u64) -> Result<()> {
        let mut guard = self.conn()?;
        let key = last_block_key(chain);
        guard.client().execute(
            "INSERT INTO state (key, value) VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            &[&key, &block.to_string()],
        )?;
        Ok(())
    }

    pub fn store_token_metadata(
        &self,
        chain: &str,
        address: &str,
        symbol: &str,
        decimals: u8,
        name: &str,
    ) -> Result<()> {
        let mut guard = self.conn()?;
        guard.client().execute(
            "INSERT INTO token_metadata (chain, token_address, symbol, decimals, name)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (chain, token_address) DO UPDATE SET
               symbol = excluded.symbol, decimals = excluded.decimals, name = excluded.name",
            &[&chain, &address, &symbol, &(decimals as i16), &name],
        )?;
        Ok(())
    }

    pub fn get_token_metadata(
        &self,
        chain: &str,
        address: &str,
    ) -> Result<Option<(String, u8, String)>> {
        let mut guard = self.conn()?;
        let row = guard.client().query_opt(
            "SELECT symbol, decimals, name FROM token_metadata
             WHERE chain = $1 AND token_address = $2",
            &[&chain, &address],
        )?;
        Ok(row.map(|r| {
            let decimals: i16 = r.get(1);
            (r.get(0), decimals as u8, r.get(2))
        }))
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
        let mut guard = self.conn()?;
        let changes = guard.client().execute(
            "INSERT INTO erc20_deposits
             (chain, tx_hash, log_index, account_id, amount, token_address, token_symbol, status)
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'detected')
             ON CONFLICT (chain, tx_hash, log_index) DO NOTHING",
            &[
                &chain,
                &tx_hash,
                &(log_index as i64),
                &account_id,
                &amount,
                &token_address,
                &token_symbol,
            ],
        )?;
        Ok(changes == 1)
    }

    pub fn get_detected_erc20_deposits(&self, chain: &str) -> Result<Vec<Erc20Deposit>> {
        let mut guard = self.conn()?;
        let rows = guard.client().query(
            "SELECT tx_hash, log_index, account_id, amount, token_address, token_symbol
             FROM erc20_deposits WHERE chain = $1 AND status = 'detected'",
            &[&chain],
        )?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let tx_hash: String = row.get(0);
                let log_index: i64 = row.get(1);
                Erc20Deposit {
                    key: format!("{tx_hash}:{log_index}"),
                    account_id: row.get(2),
                    amount: row.get(3),
                    token_address: row.get(4),
                    token_symbol: row.get(5),
                }
            })
            .collect())
    }

    pub fn mark_erc20_deposit_swept(&self, chain: &str, local_key: &str) -> Result<()> {
        let (tx_hash, log_index) = parse_local_key(local_key)?;
        let mut guard = self.conn()?;
        guard.client().execute(
            "UPDATE erc20_deposits SET status = 'swept'
             WHERE chain = $1 AND tx_hash = $2 AND log_index = $3",
            &[&chain, &tx_hash, &log_index],
        )?;
        Ok(())
    }

    pub fn mark_erc20_deposits_swept_for_account_token(
        &self,
        chain: &str,
        account_id: &str,
        token_address: &str,
    ) -> Result<Vec<(String, String)>> {
        let mut guard = self.conn()?;
        let rows = guard.client().query(
            "UPDATE erc20_deposits SET status = 'swept'
             WHERE chain = $1 AND account_id = $2 AND token_address = $3 AND status = 'detected'
             RETURNING tx_hash, log_index, amount",
            &[&chain, &account_id, &token_address],
        )?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let tx_hash: String = r.get(0);
                let log_index: i64 = r.get(1);
                let amount: String = r.get(2);
                (format!("{tx_hash}:{log_index}"), amount)
            })
            .collect())
    }

    pub fn increment_zero_balance_count(&self, chain: &str, local_key: &str) -> Result<u64> {
        let (tx_hash, log_index) = parse_local_key(local_key)?;
        let mut guard = self.conn()?;
        let row = guard.client().query_one(
            "INSERT INTO sweep_meta (chain, tx_hash, log_index, sweep_tx_hash, zero_balance_retry_count)
             VALUES ($1, $2, $3, '', 1)
             ON CONFLICT (chain, tx_hash, log_index) DO UPDATE SET
               zero_balance_retry_count = sweep_meta.zero_balance_retry_count + 1
             RETURNING zero_balance_retry_count",
            &[&chain, &tx_hash, &log_index],
        )?;
        let count: i64 = row.get(0);
        Ok(count as u64)
    }

    #[allow(dead_code)]
    pub fn set_sweep_tx_hash(&self, chain: &str, local_key: &str, tx_hash: &str) -> Result<()> {
        let (deposit_tx, log_index) = parse_local_key(local_key)?;
        let mut guard = self.conn()?;
        guard.client().execute(
            "INSERT INTO sweep_meta (chain, tx_hash, log_index, sweep_tx_hash, zero_balance_retry_count)
             VALUES ($1, $2, $3, $4, 0)
             ON CONFLICT (chain, tx_hash, log_index) DO UPDATE SET sweep_tx_hash = excluded.sweep_tx_hash",
            &[&chain, &deposit_tx, &log_index, &tx_hash],
        )?;
        Ok(())
    }

    pub fn set_sweep_tx_hash_for_keys(
        &self,
        chain: &str,
        local_keys: &[String],
        tx_hash: &str,
    ) -> Result<()> {
        let mut guard = self.conn()?;
        let client = guard.client();
        for local_key in local_keys {
            let (deposit_tx, log_index) = parse_local_key(local_key)?;
            client.execute(
                "INSERT INTO sweep_meta (chain, tx_hash, log_index, sweep_tx_hash, zero_balance_retry_count)
                 VALUES ($1, $2, $3, $4, 0)
                 ON CONFLICT (chain, tx_hash, log_index) DO UPDATE SET sweep_tx_hash = excluded.sweep_tx_hash",
                &[&chain, &deposit_tx, &log_index, &tx_hash],
            )?;
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn get_sweep_meta(&self, chain: &str, local_key: &str) -> Result<Option<(String, u64)>> {
        let (tx_hash, log_index) = parse_local_key(local_key)?;
        let mut guard = self.conn()?;
        let row = guard.client().query_opt(
            "SELECT sweep_tx_hash, zero_balance_retry_count FROM sweep_meta
             WHERE chain = $1 AND tx_hash = $2 AND log_index = $3",
            &[&chain, &tx_hash, &log_index],
        )?;
        Ok(row.map(|r| {
            let count: i64 = r.get(1);
            (r.get(0), count as u64)
        }))
    }

    pub fn increment_sweep_failure_count(&self, chain: &str, local_key: &str) -> Result<u64> {
        let (tx_hash, log_index) = parse_local_key(local_key)?;
        let mut guard = self.conn()?;
        let row = guard.client().query_one(
            "INSERT INTO sweep_failures (chain, tx_hash, log_index, consecutive_failure_count)
             VALUES ($1, $2, $3, 1)
             ON CONFLICT (chain, tx_hash, log_index) DO UPDATE SET
               consecutive_failure_count = sweep_failures.consecutive_failure_count + 1
             RETURNING consecutive_failure_count",
            &[&chain, &tx_hash, &log_index],
        )?;
        let count: i64 = row.get(0);
        Ok(count as u64)
    }

    pub fn mark_erc20_deposit_failed(&self, chain: &str, local_key: &str) -> Result<()> {
        let (tx_hash, log_index) = parse_local_key(local_key)?;
        let mut guard = self.conn()?;
        guard.client().execute(
            "UPDATE erc20_deposits SET status = 'failed'
             WHERE chain = $1 AND tx_hash = $2 AND log_index = $3",
            &[&chain, &tx_hash, &log_index],
        )?;
        Ok(())
    }

    pub fn mark_erc20_deposits_failed_for_account_token(
        &self,
        chain: &str,
        account_id: &str,
        token_address: &str,
    ) -> Result<Vec<String>> {
        let mut guard = self.conn()?;
        let rows = guard.client().query(
            "UPDATE erc20_deposits SET status = 'failed'
             WHERE chain = $1 AND account_id = $2 AND token_address = $3 AND status = 'detected'
             RETURNING tx_hash, log_index",
            &[&chain, &account_id, &token_address],
        )?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let tx_hash: String = r.get(0);
                let log_index: i64 = r.get(1);
                format!("{tx_hash}:{log_index}")
            })
            .collect())
    }

    pub fn deposit_queue_counts(&self, chain: &str) -> Result<DepositQueueCounts> {
        let mut guard = self.conn()?;
        let client = guard.client();
        let native_detected: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM deposits WHERE chain = $1 AND status = 'detected'",
                &[&chain],
            )?
            .get(0);
        let native_failed: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM deposits WHERE chain = $1 AND status = 'failed'",
                &[&chain],
            )?
            .get(0);
        let erc20_detected: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM erc20_deposits WHERE chain = $1 AND status = 'detected'",
                &[&chain],
            )?
            .get(0);
        let erc20_failed: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM erc20_deposits WHERE chain = $1 AND status = 'failed'",
                &[&chain],
            )?
            .get(0);
        Ok(DepositQueueCounts {
            native_detected: native_detected as u64,
            native_failed: native_failed as u64,
            erc20_detected: erc20_detected as u64,
            erc20_failed: erc20_failed as u64,
        })
    }

    pub fn retry_native_deposit(&self, chain: &str, tx_hash: &str) -> Result<bool> {
        let mut guard = self.conn()?;
        let changes = guard.client().execute(
            "UPDATE deposits SET status = 'detected'
             WHERE chain = $1 AND tx_hash = $2 AND status = 'failed'",
            &[&chain, &tx_hash],
        )?;
        Ok(changes == 1)
    }

    pub fn retry_erc20_deposit(&self, chain: &str, tx_hash: &str, log_index: u64) -> Result<bool> {
        let mut guard = self.conn()?;
        let client = guard.client();
        let log_index = log_index as i64;
        let changes = client.execute(
            "UPDATE erc20_deposits SET status = 'detected'
             WHERE chain = $1 AND tx_hash = $2 AND log_index = $3 AND status = 'failed'",
            &[&chain, &tx_hash, &log_index],
        )?;
        let updated = changes == 1;
        if updated {
            client.execute(
                "DELETE FROM sweep_failures WHERE chain = $1 AND tx_hash = $2 AND log_index = $3",
                &[&chain, &tx_hash, &log_index],
            )?;
        }
        Ok(updated)
    }

    pub fn get_sweep_failure_count(&self, chain: &str, local_key: &str) -> Result<u64> {
        let (tx_hash, log_index) = parse_local_key(local_key)?;
        let mut guard = self.conn()?;
        let row = guard.client().query_opt(
            "SELECT consecutive_failure_count FROM sweep_failures
             WHERE chain = $1 AND tx_hash = $2 AND log_index = $3",
            &[&chain, &tx_hash, &log_index],
        )?;
        Ok(row.map(|r| r.get::<_, i64>(0) as u64).unwrap_or(0))
    }

    pub fn upsert_webhook_delivery(
        &self,
        id: &str,
        event: &str,
        registration_id: &str,
        webhook_url: &str,
        payload: &str,
    ) -> Result<bool> {
        let mut guard = self.conn()?;
        let client = guard.client();
        let mut tx = client.transaction()?;
        let now = now_unix_secs();

        let existing: Option<String> = tx
            .query_opt(
                "SELECT status FROM webhook_deliveries WHERE id = $1 AND event = $2",
                &[&id, &event],
            )?
            .map(|r| r.get(0));

        if existing.as_deref() == Some("delivered") {
            tx.commit()?;
            return Ok(false);
        }

        if existing.is_none() {
            tx.execute(
                "INSERT INTO webhook_deliveries
                 (id, event, registration_id, webhook_url, payload, status, attempt_count,
                  last_http_status, last_error, leased_until, updated_at)
                 VALUES ($1, $2, $3, $4, $5, 'pending', 0, NULL, NULL, NULL, $6)",
                &[&id, &event, &registration_id, &webhook_url, &payload, &now],
            )?;
            tx.commit()?;
            return Ok(true);
        }

        if existing.as_deref() == Some("failed") {
            tx.commit()?;
            return Ok(false);
        }

        tx.execute(
            "UPDATE webhook_deliveries
             SET webhook_url = $3, payload = $4, updated_at = $5
             WHERE id = $1 AND event = $2 AND status = 'pending'",
            &[&id, &event, &webhook_url, &payload, &now],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn claim_webhook_delivery(
        &self,
        id: &str,
        event: &str,
        lease_until: i64,
        max_retries: u32,
    ) -> Result<bool> {
        let now = now_unix_secs();
        let mut guard = self.conn()?;
        let max_retries = max_retries as i64;
        let changes = guard.client().execute(
            "UPDATE webhook_deliveries
             SET leased_until = $3, updated_at = $4
             WHERE id = $1 AND event = $2
               AND status = 'pending'
               AND attempt_count < $5
               AND (leased_until IS NULL OR leased_until < $4)",
            &[&id, &event, &lease_until, &now, &max_retries],
        )?;
        Ok(changes == 1)
    }

    pub fn record_webhook_attempt(
        &self,
        id: &str,
        event: &str,
        http_status: Option<u16>,
        error: Option<&str>,
        status: &str,
    ) -> Result<u64> {
        let mut guard = self.conn()?;
        let client = guard.client();
        let now = now_unix_secs();
        let http_status = http_status.map(i32::from);
        client.execute(
            "UPDATE webhook_deliveries
             SET attempt_count = attempt_count + 1,
                 last_http_status = $3,
                 last_error = $4,
                 status = $5,
                 leased_until = NULL,
                 updated_at = $6
             WHERE id = $1 AND event = $2",
            &[&id, &event, &http_status, &error, &status, &now],
        )?;
        let count: i64 = client
            .query_one(
                "SELECT attempt_count FROM webhook_deliveries WHERE id = $1 AND event = $2",
                &[&id, &event],
            )?
            .get(0);
        Ok(count as u64)
    }

    pub fn get_pending_webhook_delivery_keys(
        &self,
        max_retries: u32,
        batch_size: u32,
    ) -> Result<Vec<(String, String)>> {
        let now = now_unix_secs();
        let mut guard = self.conn()?;
        let max_retries = max_retries as i64;
        let batch_size = batch_size as i64;
        let rows = guard.client().query(
            "SELECT id, event FROM webhook_deliveries
             WHERE status = 'pending'
               AND attempt_count < $1
               AND (leased_until IS NULL OR leased_until < $2)
             ORDER BY updated_at ASC
             LIMIT $3",
            &[&max_retries, &now, &batch_size],
        )?;
        Ok(rows.into_iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    pub fn get_webhook_delivery(
        &self,
        id: &str,
        event: &str,
    ) -> Result<Option<WebhookDeliveryRecord>> {
        let mut guard = self.conn()?;
        let row = guard.client().query_opt(
            "SELECT id, event, registration_id, webhook_url, payload, status, attempt_count,
                    last_http_status, last_error
             FROM webhook_deliveries WHERE id = $1 AND event = $2",
            &[&id, &event],
        )?;
        Ok(row.map(row_to_webhook_delivery))
    }

    pub fn retry_webhook_delivery(&self, id: &str, event: &str) -> Result<bool> {
        let now = now_unix_secs();
        let mut guard = self.conn()?;
        let changes = guard.client().execute(
            "UPDATE webhook_deliveries
             SET status = 'pending', attempt_count = 0, leased_until = NULL,
                 last_http_status = NULL, last_error = NULL, updated_at = $3
             WHERE id = $1 AND event = $2 AND status = 'failed'",
            &[&id, &event, &now],
        )?;
        Ok(changes == 1)
    }
}

fn row_to_webhook_delivery(row: Row) -> WebhookDeliveryRecord {
    let attempt_count: i64 = row.get(6);
    let last_http_status: Option<i32> = row.get(7);
    WebhookDeliveryRecord {
        id: row.get(0),
        event: row.get(1),
        registration_id: row.get(2),
        webhook_url: row.get(3),
        payload: row.get(4),
        status: row.get(5),
        attempt_count: attempt_count as u64,
        last_http_status: last_http_status.map(|s| s as u16),
        last_error: row.get(8),
    }
}

fn now_unix_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Parse `"0xtx:42"` -> (`0xtx`, 42). Bare `"0xtx"` -> (`0xtx`, 0). Identical
/// contract to the SQLite arm's `parse_local_key`.
fn parse_local_key(local_key: &str) -> Result<(String, i64)> {
    if let Some((tx, idx)) = local_key.rsplit_once(':') {
        if !idx.is_empty() && idx.chars().all(|c| c.is_ascii_digit()) {
            return Ok((tx.to_string(), idx.parse()?));
        }
    }
    Ok((local_key.to_string(), 0))
}

fn last_block_key(chain: &str) -> String {
    format!("last_block:{chain}")
}

fn postgres_already_populated(client: &mut Client) -> Result<bool> {
    let accounts_count: i64 = client.query_one("SELECT COUNT(*) FROM accounts", &[])?.get(0);
    let has_next_index = client
        .query_opt("SELECT 1 FROM state WHERE key = 'next_index'", &[])?
        .is_some();
    Ok(accounts_count > 0 || has_next_index)
}

fn build_tls_connector() -> Result<MakeTlsConnector> {
    use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
    use std::io::Write;

    let mut builder = SslConnector::builder(SslMethod::tls())
        .context("failed to create OpenSSL connector builder")?;
    builder.set_verify(SslVerifyMode::PEER);

    // The CA bundle is compiled in (see `RDS_CA_BUNDLE`), but
    // `SslConnectorBuilder` only loads CA certs from a file path, so write it
    // to a short-lived temp file once at startup.
    let mut ca_file = tempfile::NamedTempFile::new()
        .context("failed to create temp file for the bundled RDS CA chain")?;
    ca_file
        .write_all(RDS_CA_BUNDLE.as_bytes())
        .context("failed to write the bundled RDS CA chain to a temp file")?;
    builder
        .set_ca_file(ca_file.path())
        .context("failed to load the bundled RDS CA chain")?;

    Ok(MakeTlsConnector::new(builder.build()))
}
