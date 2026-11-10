//! Postgres storage backend (evmhot Postgres storage migration).
//!
//! Deliberately much simpler than [`super::sqlite`]: Postgres handles
//! concurrent writers natively via MVCC/row locking, so there is no
//! single-writer actor, no priority lanes, no WAL/checkpoint logic here —
//! every method just borrows a pooled connection and runs its SQL directly.
//! The `next_index` counter (BIP44 derivation index allocation) is the one
//! place that needs an explicit atomicity argument; see
//! [`PostgresBackend::register_account_auto`].
use super::{
    DepositQueueCounts, Erc20Deposit, Erc20Settlement, IntentTx, NativeSettlement, NewErc20Deposit,
    NewNativeDeposit, NewSweepIntent, OutboxEntry, SweepIntent, SweepRecord, WebhookDeliveryRecord,
    WriteQueueError,
};
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
/// that loses database connectivity fails its load balancer's health checks
/// within one unhealthy-threshold window instead of silently serving errors.
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
                sweeps = summary.sweeps.1,
                sweep_intents = summary.sweep_intents.1,
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
    /// `register_account_auto` always has a counter row to increment. The
    /// counter starts above every stored derivation index (REQ-RG-3): an
    /// existing store can hold hashed indices up to 2^31, so seeding 0 would
    /// re-issue addresses already handed out.
    fn seed_next_index_if_missing(&self) -> Result<()> {
        let mut guard = self.conn()?;
        guard.client().execute(
            "INSERT INTO state (key, value)
             SELECT 'next_index', COALESCE(MAX(derivation_index) + 1, 0)::text FROM accounts
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
    /// Indices are 32-bit raw child numbers (SPEC REQ-KY-1); the ceiling
    /// check at 2^32 is REQ-RG-5 (F-002).
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
        let next_index_u32 = u32::try_from(next_index).map_err(|_| {
            anyhow!("registration refused: derivation index space is exhausted (REQ-RG-5)")
        })?;

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
            "UPDATE deposits SET status = 'swept', review_kind = NULL WHERE chain = $1 AND tx_hash = $2",
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
            "SELECT tx_hash, log_index, account_id, amount, token_address, token_symbol, from_address
             FROM erc20_deposits WHERE chain = $1 AND status = 'detected'
             ORDER BY seq",
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
                    from_address: row.get(6),
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
        let native_needs_review: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM deposits WHERE chain = $1 AND status = 'needs_review'",
                &[&chain],
            )?
            .get(0);
        let erc20_needs_review: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM erc20_deposits WHERE chain = $1 AND status = 'needs_review'",
                &[&chain],
            )?
            .get(0);
        Ok(DepositQueueCounts {
            native_detected: native_detected as u64,
            native_failed: native_failed as u64,
            native_needs_review: native_needs_review as u64,
            erc20_detected: erc20_detected as u64,
            erc20_failed: erc20_failed as u64,
            erc20_needs_review: erc20_needs_review as u64,
        })
    }

    pub fn retry_native_deposit(&self, chain: &str, tx_hash: &str) -> Result<bool> {
        let mut guard = self.conn()?;
        let mut tx = guard.client().transaction()?;
        let pattern = format!("%\"{tx_hash}\"%");
        if let Some(row) = tx.query_opt(
            "SELECT tx_hash FROM sweep_intents
             WHERE chain = $1 AND state IN ('pending', 'review_gone') AND keys LIKE $2 LIMIT 1",
            &[&chain, &pattern],
        )? {
            let hash: Option<String> = row.get(0);
            return Err(anyhow!(
                "cannot retry: a sweep is still in flight or being re-checked (tx {})",
                hash.as_deref().unwrap_or("not yet signed")
            ));
        }
        let changes = tx.execute(
            "UPDATE deposits SET status = 'detected', review_kind = NULL
             WHERE chain = $1 AND tx_hash = $2 AND status IN ('failed', 'needs_review')",
            &[&chain, &tx_hash],
        )?;
        let updated = changes == 1;
        if updated {
            tx.execute(
                "UPDATE sweep_intents SET state = 'closed_by_operator'
                 WHERE chain = $1 AND state = 'review' AND keys LIKE $2",
                &[&chain, &pattern],
            )?;
            tx.execute(
                "DELETE FROM sweep_alerts WHERE chain = $1 AND deposit_key = $2",
                &[&chain, &tx_hash],
            )?;
        }
        tx.commit()?;
        Ok(updated)
    }

    pub fn retry_erc20_deposit(&self, chain: &str, tx_hash: &str, log_index: u64) -> Result<bool> {
        let mut guard = self.conn()?;
        let mut tx = guard.client().transaction()?;
        let log_index = log_index as i64;
        let key = format!("{tx_hash}:{log_index}");
        let pattern = format!("%\"{key}\"%");
        // A sweep still in flight could land: it is never abandoned from here.
        if let Some(row) = tx.query_opt(
            "SELECT tx_hash FROM sweep_intents
             WHERE chain = $1 AND state IN ('pending', 'review_gone') AND keys LIKE $2 LIMIT 1",
            &[&chain, &pattern],
        )? {
            let hash: Option<String> = row.get(0);
            return Err(anyhow!(
                "cannot retry: a sweep is still in flight or being re-checked (tx {})",
                hash.as_deref().unwrap_or("not yet signed")
            ));
        }
        let changes = tx.execute(
            "UPDATE erc20_deposits SET status = 'detected', review_kind = NULL
             WHERE chain = $1 AND tx_hash = $2 AND log_index = $3
               AND status IN ('failed', 'needs_review')",
            &[&chain, &tx_hash, &log_index],
        )?;
        let updated = changes == 1;
        if updated {
            tx.execute(
                "DELETE FROM sweep_failures WHERE chain = $1 AND tx_hash = $2 AND log_index = $3",
                &[&chain, &tx_hash, &log_index],
            )?;
            // The operator has looked at the review intents: they are closed,
            // never re-opened. What is owed is computed from recorded sweeps alone.
            tx.execute(
                "UPDATE sweep_intents SET state = 'closed_by_operator'
                 WHERE chain = $1 AND state = 'review' AND keys LIKE $2",
                &[&chain, &pattern],
            )?;
            tx.execute(
                "DELETE FROM sweep_alerts WHERE chain = $1 AND deposit_key = $2",
                &[&chain, &key],
            )?;
        }
        tx.commit()?;
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
        let entry = OutboxEntry {
            id: id.to_string(),
            event: event.to_string(),
            registration_id: registration_id.to_string(),
            webhook_url: webhook_url.to_string(),
            payload: payload.to_string(),
        };
        let mut guard = self.conn()?;
        let mut tx = guard.client().transaction()?;
        let notify = upsert_outbox(&mut tx, &entry)?;
        tx.commit()?;
        Ok(notify)
    }

    pub fn record_native_deposit_atomic(
        &self,
        deposit: &NewNativeDeposit,
        outbox: Option<&OutboxEntry>,
    ) -> Result<bool> {
        let mut guard = self.conn()?;
        let mut tx = guard.client().transaction()?;
        let changes = tx.execute(
            "INSERT INTO deposits (chain, tx_hash, account_id, amount, status, from_address)
             VALUES ($1, $2, $3, $4, 'detected', $5)
             ON CONFLICT (chain, tx_hash) DO NOTHING",
            &[
                &deposit.chain,
                &deposit.tx_hash,
                &deposit.account_id,
                &deposit.amount,
                &deposit.from_address,
            ],
        )?;
        let is_new = changes == 1;
        if is_new {
            if let Some(entry) = outbox {
                upsert_outbox(&mut tx, entry)?;
            }
        }
        tx.commit()?;
        Ok(is_new)
    }

    pub fn record_erc20_deposit_atomic(
        &self,
        deposit: &NewErc20Deposit,
        outbox: Option<&OutboxEntry>,
    ) -> Result<bool> {
        let mut guard = self.conn()?;
        let mut tx = guard.client().transaction()?;
        let changes = tx.execute(
            "INSERT INTO erc20_deposits
             (chain, tx_hash, log_index, account_id, amount, token_address, token_symbol, status, from_address)
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'detected', $8)
             ON CONFLICT (chain, tx_hash, log_index) DO NOTHING",
            &[
                &deposit.chain,
                &deposit.tx_hash,
                &(deposit.log_index as i64),
                &deposit.account_id,
                &deposit.amount,
                &deposit.token_address,
                &deposit.token_symbol,
                &deposit.from_address,
            ],
        )?;
        let is_new = changes == 1;
        if is_new {
            if let Some(entry) = outbox {
                upsert_outbox(&mut tx, entry)?;
            }
        }
        tx.commit()?;
        Ok(is_new)
    }

    pub fn complete_native_sweep(
        &self,
        chain: &str,
        tx_hash: &str,
        outbox: Option<&OutboxEntry>,
    ) -> Result<bool> {
        let mut guard = self.conn()?;
        let mut tx = guard.client().transaction()?;
        tx.execute(
            "UPDATE deposits SET status = 'swept' WHERE chain = $1 AND tx_hash = $2",
            &[&chain, &tx_hash],
        )?;
        let enqueued = match outbox {
            Some(entry) => upsert_outbox(&mut tx, entry)?,
            None => false,
        };
        tx.commit()?;
        Ok(enqueued)
    }

    pub fn complete_erc20_sweep(
        &self,
        chain: &str,
        settlements: &[Erc20Settlement],
    ) -> Result<usize> {
        let mut guard = self.conn()?;
        let mut tx = guard.client().transaction()?;
        let mut enqueued = 0;
        for settlement in settlements {
            let (tx_hash, log_index) = parse_local_key(&settlement.key)?;
            tx.execute(
                "UPDATE erc20_deposits SET status = 'swept'
                 WHERE chain = $1 AND tx_hash = $2 AND log_index = $3",
                &[&chain, &tx_hash, &log_index],
            )?;
            if let Some(entry) = &settlement.outbox {
                if upsert_outbox(&mut tx, entry)? {
                    enqueued += 1;
                }
            }
            if let Some(entry) = &settlement.notify {
                if upsert_outbox(&mut tx, entry)? {
                    enqueued += 1;
                }
            }
        }
        tx.commit()?;
        Ok(enqueued)
    }

    pub fn record_sweep(&self, chain: &str, keys: &[String], record: &SweepRecord) -> Result<()> {
        let mut guard = self.conn()?;
        let mut tx = guard.client().transaction()?;
        for key in keys {
            let (tx_hash, log_index) = parse_local_key(key)?;
            tx.execute(
                "INSERT INTO sweeps
                 (chain, tx_hash, log_index, sweep_tx_hash, sweep_block_number,
                  sweep_block_hash, sweep_log_index, sweep_amount, sweep_target)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                 ON CONFLICT DO NOTHING",
                &[
                    &chain,
                    &tx_hash,
                    &log_index,
                    &record.sweep_tx_hash,
                    &(record.sweep_block_number as i64),
                    &record.sweep_block_hash,
                    &(record.sweep_log_index as i64),
                    &record.sweep_amount,
                    &record.sweep_target,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_sweeps(&self, chain: &str, local_key: &str) -> Result<Vec<SweepRecord>> {
        let (tx_hash, log_index) = parse_local_key(local_key)?;
        let mut guard = self.conn()?;
        let rows = guard.client().query(
            "SELECT sweep_tx_hash, sweep_block_number, sweep_block_hash, sweep_log_index, sweep_amount,
                    sweep_target
             FROM sweeps WHERE chain = $1 AND tx_hash = $2 AND log_index = $3 ORDER BY seq",
            &[&chain, &tx_hash, &log_index],
        )?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let block_number: i64 = r.get(1);
                let log_index: i64 = r.get(3);
                SweepRecord {
                    sweep_tx_hash: r.get(0),
                    sweep_block_number: block_number as u64,
                    sweep_block_hash: r.get(2),
                    sweep_log_index: log_index as u64,
                    sweep_amount: r.get(4),
                    sweep_target: r.get(5),
                }
            })
            .collect())
    }

    pub fn create_sweep_intent(&self, new: &NewSweepIntent) -> Result<i64> {
        let keys = serde_json::to_string(&new.keys)?;
        let mut guard = self.conn()?;
        let row = guard.client().query_one(
            "INSERT INTO sweep_intents
             (chain, account_id, address, token_address, group_key, keys, part_index, amount, target,
              state, tx_hash, passes)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'pending', NULL, 0) RETURNING id",
            &[
                &new.chain,
                &new.account_id,
                &new.address,
                &new.token_address,
                &new.group_key,
                &keys,
                &(new.part_index as i64),
                &new.amount,
                &new.target,
            ],
        )?;
        Ok(row.get(0))
    }

    pub fn set_sweep_intent_signed(&self, id: i64, nonce: u64, tx: &IntentTx) -> Result<()> {
        let mut guard = self.conn()?;
        let mut t = guard.client().transaction()?;
        let row = t.query_one(
            "SELECT txs FROM sweep_intents WHERE id = $1 FOR UPDATE",
            &[&id],
        )?;
        let existing: String = row.get(0);
        let mut txs: Vec<IntentTx> = serde_json::from_str(&existing).unwrap_or_default();
        txs.push(tx.clone());
        t.execute(
            "UPDATE sweep_intents
             SET nonce = COALESCE(nonce, $2), tx_hash = $3, raw_tx = $4, txs = $5,
                 passes = 0, gone_block = 0
             WHERE id = $1",
            &[
                &id,
                &(nonce as i64),
                &tx.hash,
                &tx.raw,
                &serde_json::to_string(&txs)?,
            ],
        )?;
        t.commit()?;
        Ok(())
    }

    pub fn note_nonce_used_block(&self, id: i64, block: u64) -> Result<u64> {
        let mut guard = self.conn()?;
        let row = guard.client().query_one(
            "UPDATE sweep_intents
             SET gone_block = CASE WHEN gone_block = 0 THEN $2 ELSE gone_block END
             WHERE id = $1 RETURNING gone_block",
            &[&id, &(block as i64)],
        )?;
        let first: i64 = row.get(0);
        Ok(first as u64)
    }

    pub fn move_sweep_intent(&self, id: i64, from: &str, to: &str) -> Result<()> {
        let mut guard = self.conn()?;
        guard.client().execute(
            "UPDATE sweep_intents SET state = $3 WHERE id = $1 AND state = $2",
            &[&id, &from, &to],
        )?;
        Ok(())
    }

    pub fn settle_reviewed_sweep_intent(
        &self,
        chain: &str,
        id: i64,
        keys: &[String],
        record: &SweepRecord,
        account_id: &str,
        token_address: &str,
    ) -> Result<usize> {
        let mut guard = self.conn()?;
        let mut tx = guard.client().transaction()?;
        tx.execute(
            "UPDATE sweep_intents SET state = 'succeeded' WHERE id = $1",
            &[&id],
        )?;
        for key in keys {
            let (tx_hash, log_index) = parse_local_key(key)?;
            tx.execute(
                "INSERT INTO sweeps
                 (chain, tx_hash, log_index, sweep_tx_hash, sweep_block_number,
                  sweep_block_hash, sweep_log_index, sweep_amount, sweep_target)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                 ON CONFLICT DO NOTHING",
                &[
                    &chain,
                    &tx_hash,
                    &log_index,
                    &record.sweep_tx_hash,
                    &(record.sweep_block_number as i64),
                    &record.sweep_block_hash,
                    &(record.sweep_log_index as i64),
                    &record.sweep_amount,
                    &record.sweep_target,
                ],
            )?;
            tx.execute(
                "UPDATE erc20_deposits SET status = 'detected', review_kind = NULL
                 WHERE chain = $1 AND tx_hash = $2 AND log_index = $3 AND status = 'needs_review'",
                &[&chain, &tx_hash, &log_index],
            )?;
        }
        let released = tx.execute(
            "UPDATE erc20_deposits SET status = 'detected', review_kind = NULL
             WHERE chain = $1 AND account_id = $2 AND LOWER(token_address) = LOWER($3)
               AND status = 'needs_review' AND review_kind = 'balance_shortfall'",
            &[&chain, &account_id, &token_address],
        )?;
        tx.commit()?;
        Ok(released as usize)
    }

    pub fn mark_review_gone(&self, id: i64, block: u64) -> Result<()> {
        let mut guard = self.conn()?;
        guard.client().execute(
            "UPDATE sweep_intents SET state = 'review_gone', gone_block = $2
             WHERE id = $1 AND state IN ('pending', 'review_gone')",
            &[&id, &(block as i64)],
        )?;
        Ok(())
    }

    pub fn native_deposit_amounts(
        &self,
        chain: &str,
        keys: &[String],
    ) -> Result<Vec<(String, String)>> {
        let mut guard = self.conn()?;
        let mut out = Vec::new();
        for key in keys {
            if let Some(row) = guard.client().query_opt(
                "SELECT amount FROM deposits WHERE chain = $1 AND tx_hash = $2",
                &[&chain, key],
            )? {
                out.push((key.clone(), row.get::<_, String>(0)));
            }
        }
        Ok(out)
    }

    pub fn release_review_deposits_for_keys(
        &self,
        chain: &str,
        keys: &[String],
        native: bool,
    ) -> Result<()> {
        let mut guard = self.conn()?;
        let mut tx = guard.client().transaction()?;
        for key in keys {
            if native {
                tx.execute(
                    "UPDATE deposits SET status = 'detected', review_kind = NULL
                     WHERE chain = $1 AND tx_hash = $2 AND status = 'needs_review'",
                    &[&chain, key],
                )?;
            } else {
                let (tx_hash, log_index) = parse_local_key(key)?;
                tx.execute(
                    "UPDATE erc20_deposits SET status = 'detected', review_kind = NULL
                     WHERE chain = $1 AND tx_hash = $2 AND log_index = $3
                       AND status = 'needs_review'",
                    &[&chain, &tx_hash, &log_index],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn highest_landed_intent_nonce(
        &self,
        chain: &str,
        account_id: &str,
    ) -> Result<Option<u64>> {
        let mut guard = self.conn()?;
        let row = guard.client().query_one(
            "SELECT MAX(nonce) FROM sweep_intents
             WHERE chain = $1 AND account_id = $2
               AND state IN ('succeeded', 'failed', 'review', 'review_gone', 'closed_by_operator')",
            &[&chain, &account_id],
        )?;
        let n: Option<i64> = row.get(0);
        Ok(n.map(|n| n as u64))
    }

    pub fn settle_native_intent(
        &self,
        chain: &str,
        id: i64,
        settlements: &[NativeSettlement],
    ) -> Result<usize> {
        let mut guard = self.conn()?;
        let mut tx = guard.client().transaction()?;
        tx.execute(
            "UPDATE sweep_intents SET state = 'succeeded' WHERE id = $1",
            &[&id],
        )?;
        let mut enqueued = 0;
        for s in settlements {
            tx.execute(
                "UPDATE deposits SET status = 'swept' WHERE chain = $1 AND tx_hash = $2",
                &[&chain, &s.tx_hash],
            )?;
            if let Some(entry) = &s.outbox {
                if upsert_outbox(&mut tx, entry)? {
                    enqueued += 1;
                }
            }
        }
        tx.commit()?;
        Ok(enqueued)
    }

    pub fn mark_native_deposits_needs_review_for_keys(
        &self,
        chain: &str,
        keys: &[String],
        kind: &str,
    ) -> Result<()> {
        let mut guard = self.conn()?;
        let mut tx = guard.client().transaction()?;
        for key in keys {
            tx.execute(
                "UPDATE deposits SET status = 'needs_review', review_kind = $3
                 WHERE chain = $1 AND tx_hash = $2 AND status = 'detected'",
                &[&chain, key, &kind],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn blocking_native_deposits(&self, chain: &str, account_id: &str) -> Result<Vec<String>> {
        let mut guard = self.conn()?;
        let rows = guard.client().query(
            "SELECT tx_hash FROM deposits
             WHERE chain = $1 AND account_id = $2 AND status IN ('needs_review', 'failed')
             ORDER BY tx_hash",
            &[&chain, &account_id],
        )?;
        Ok(rows.into_iter().map(|r| r.get(0)).collect())
    }

    pub fn set_sweep_intent_tx(&self, id: i64, tx_hash: &str) -> Result<()> {
        let mut guard = self.conn()?;
        guard.client().execute(
            "UPDATE sweep_intents SET tx_hash = $2 WHERE id = $1",
            &[&id, &tx_hash],
        )?;
        Ok(())
    }

    pub fn finish_sweep_intent(&self, id: i64, state: &str) -> Result<()> {
        let mut guard = self.conn()?;
        guard.client().execute(
            "UPDATE sweep_intents SET state = $2 WHERE id = $1 AND state IN ('pending', 'review_gone')",
            &[&id, &state],
        )?;
        Ok(())
    }

    pub fn bump_sweep_intent_passes(&self, id: i64) -> Result<u64> {
        let mut guard = self.conn()?;
        let row = guard.client().query_one(
            "UPDATE sweep_intents SET passes = passes + 1 WHERE id = $1 RETURNING passes",
            &[&id],
        )?;
        let passes: i64 = row.get(0);
        Ok(passes as u64)
    }

    pub fn sweep_intents_in_state(&self, chain: &str, state: &str) -> Result<Vec<SweepIntent>> {
        let mut guard = self.conn()?;
        let rows = guard.client().query(
            "SELECT id, group_key, keys, part_index, amount, target, tx_hash, passes, nonce, raw_tx,
                    account_id, token_address, address, txs, gone_block
             FROM sweep_intents WHERE chain = $1 AND state = $2 ORDER BY id",
            &[&chain, &state],
        )?;
        rows.into_iter()
            .map(|r| {
                let keys_json: String = r.get(2);
                let part_index: i64 = r.get(3);
                let passes: i64 = r.get(7);
                let nonce: Option<i64> = r.get(8);
                let txs_json: String = r.get(13);
                let gone: i64 = r.get(14);
                // A transaction list that does not parse is an error, not an
                // empty list (which would read as "never signed").
                Ok(SweepIntent {
                    id: r.get(0),
                    account_id: r.get(10),
                    address: r.get(12),
                    txs: serde_json::from_str(&txs_json)
                        .map_err(|e| anyhow!("sweep intent txs: {e}"))?,
                    gone_block: gone as u64,
                    token_address: r.get(11),
                    group_key: r.get(1),
                    keys: serde_json::from_str(&keys_json)
                        .map_err(|e| anyhow!("sweep intent keys: {e}"))?,
                    part_index: part_index as u64,
                    amount: r.get(4),
                    target: r.get(5),
                    tx_hash: r.get(6),
                    nonce: nonce.map(|n| n as u64),
                    raw_tx: r.get(9),
                    passes: passes as u64,
                    reviewed: state != "pending",
                })
            })
            .collect()
    }

    pub fn all_pending_sweep_intents(&self, chain: &str) -> Result<Vec<SweepIntent>> {
        self.sweep_intents_in_state(chain, "pending")
    }

    pub fn pending_sweep_intents(&self, chain: &str, keys: &[String]) -> Result<Vec<SweepIntent>> {
        Ok(self
            .all_pending_sweep_intents(chain)?
            .into_iter()
            .filter(|i| i.keys.iter().any(|k| keys.contains(k)))
            .collect())
    }

    pub fn sweep_intent_states_for_keys(
        &self,
        chain: &str,
        keys: &[String],
    ) -> Result<Vec<String>> {
        let mut guard = self.conn()?;
        let mut out = Vec::new();
        for key in keys {
            let pattern = format!("%\"{key}\"%");
            let rows = guard.client().query(
                "SELECT state FROM sweep_intents WHERE chain = $1 AND keys LIKE $2",
                &[&chain, &pattern],
            )?;
            out.extend(rows.into_iter().map(|r| r.get::<_, String>(0)));
        }
        Ok(out)
    }

    pub fn sweep_intent_history_for_keys(
        &self,
        chain: &str,
        keys: &[String],
    ) -> Result<Vec<(String, Option<String>)>> {
        let mut guard = self.conn()?;
        let mut out = Vec::new();
        for key in keys {
            let pattern = format!("%\"{key}\"%");
            let rows = guard.client().query(
                "SELECT state, tx_hash FROM sweep_intents WHERE chain = $1 AND keys LIKE $2 ORDER BY id",
                &[&chain, &pattern],
            )?;
            out.extend(
                rows.into_iter()
                    .map(|r| (r.get::<_, String>(0), r.get::<_, Option<String>>(1))),
            );
        }
        Ok(out)
    }

    pub fn blocking_erc20_deposits(
        &self,
        chain: &str,
        account_id: &str,
        token_address: &str,
    ) -> Result<Vec<Erc20Deposit>> {
        let mut guard = self.conn()?;
        let rows = guard.client().query(
            "SELECT tx_hash, log_index, account_id, amount, token_address, token_symbol, from_address
             FROM erc20_deposits
             WHERE chain = $1 AND account_id = $2 AND LOWER(token_address) = LOWER($3)
               AND status IN ('needs_review', 'failed') ORDER BY seq",
            &[&chain, &account_id, &token_address],
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
                    from_address: row.get(6),
                }
            })
            .collect())
    }

    pub fn alert_once(&self, chain: &str, deposit_key: &str, kind: &str) -> Result<bool> {
        let mut guard = self.conn()?;
        let n = guard.client().execute(
            "INSERT INTO sweep_alerts (chain, deposit_key, kind) VALUES ($1, $2, $3)
             ON CONFLICT DO NOTHING",
            &[&chain, &deposit_key, &kind],
        )?;
        Ok(n == 1)
    }

    pub fn settle_sweep_intent(
        &self,
        chain: &str,
        id: i64,
        keys: &[String],
        record: &SweepRecord,
        settlements: &[Erc20Settlement],
    ) -> Result<usize> {
        let mut guard = self.conn()?;
        let mut tx = guard.client().transaction()?;
        tx.execute(
            "UPDATE sweep_intents SET state = 'succeeded' WHERE id = $1",
            &[&id],
        )?;
        for key in keys {
            let (tx_hash, log_index) = parse_local_key(key)?;
            tx.execute(
                "INSERT INTO sweeps
                 (chain, tx_hash, log_index, sweep_tx_hash, sweep_block_number,
                  sweep_block_hash, sweep_log_index, sweep_amount, sweep_target)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                 ON CONFLICT DO NOTHING",
                &[
                    &chain,
                    &tx_hash,
                    &log_index,
                    &record.sweep_tx_hash,
                    &(record.sweep_block_number as i64),
                    &record.sweep_block_hash,
                    &(record.sweep_log_index as i64),
                    &record.sweep_amount,
                    &record.sweep_target,
                ],
            )?;
        }
        let mut enqueued = 0;
        for settlement in settlements {
            let (tx_hash, log_index) = parse_local_key(&settlement.key)?;
            tx.execute(
                "UPDATE erc20_deposits SET status = 'swept'
                 WHERE chain = $1 AND tx_hash = $2 AND log_index = $3",
                &[&chain, &tx_hash, &log_index],
            )?;
            if let Some(entry) = &settlement.outbox {
                if upsert_outbox(&mut tx, entry)? {
                    enqueued += 1;
                }
            }
            if let Some(entry) = &settlement.notify {
                if upsert_outbox(&mut tx, entry)? {
                    enqueued += 1;
                }
            }
        }
        tx.commit()?;
        Ok(enqueued)
    }

    pub fn mark_erc20_deposits_needs_review_for_keys(
        &self,
        chain: &str,
        keys: &[String],
        kind: &str,
    ) -> Result<()> {
        let mut guard = self.conn()?;
        let mut tx = guard.client().transaction()?;
        for key in keys {
            let (tx_hash, log_index) = parse_local_key(key)?;
            tx.execute(
                "UPDATE erc20_deposits SET status = 'needs_review', review_kind = $4
                 WHERE chain = $1 AND tx_hash = $2 AND log_index = $3 AND status = 'detected'",
                &[&chain, &tx_hash, &log_index, &kind],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_review_kind(&self, chain: &str, local_key: &str) -> Result<Option<String>> {
        let (tx_hash, log_index) = parse_local_key(local_key)?;
        let mut guard = self.conn()?;
        let row = guard.client().query_opt(
            "SELECT review_kind FROM erc20_deposits
             WHERE chain = $1 AND tx_hash = $2 AND log_index = $3",
            &[&chain, &tx_hash, &log_index],
        )?;
        Ok(row.and_then(|r| r.get::<_, Option<String>>(0)))
    }

    pub fn get_deposit_from_address(&self, chain: &str, tx_hash: &str) -> Result<Option<String>> {
        let mut guard = self.conn()?;
        let row = guard.client().query_opt(
            "SELECT from_address FROM deposits WHERE chain = $1 AND tx_hash = $2",
            &[&chain, &tx_hash],
        )?;
        Ok(row.and_then(|r| r.get::<_, Option<String>>(0)))
    }

    pub fn mark_erc20_deposits_swept_for_keys(&self, chain: &str, keys: &[String]) -> Result<()> {
        let mut guard = self.conn()?;
        let mut tx = guard.client().transaction()?;
        for key in keys {
            let (tx_hash, log_index) = parse_local_key(key)?;
            tx.execute(
                "UPDATE erc20_deposits SET status = 'swept'
                 WHERE chain = $1 AND tx_hash = $2 AND log_index = $3 AND status = 'detected'",
                &[&chain, &tx_hash, &log_index],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn mark_erc20_deposits_failed_for_keys(&self, chain: &str, keys: &[String]) -> Result<()> {
        let mut guard = self.conn()?;
        let mut tx = guard.client().transaction()?;
        for key in keys {
            let (tx_hash, log_index) = parse_local_key(key)?;
            tx.execute(
                "UPDATE erc20_deposits SET status = 'failed'
                 WHERE chain = $1 AND tx_hash = $2 AND log_index = $3 AND status = 'detected'",
                &[&chain, &tx_hash, &log_index],
            )?;
        }
        tx.commit()?;
        Ok(())
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
        self.record_webhook_attempt_after(id, event, http_status, error, status, 0)
    }

    /// As `record_webhook_attempt`; a row that stays pending is not offered again
    /// for `retry_after_secs` (it is leased for that long).
    pub fn record_webhook_attempt_after(
        &self,
        id: &str,
        event: &str,
        http_status: Option<u16>,
        error: Option<&str>,
        status: &str,
        retry_after_secs: u64,
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
                 leased_until = CASE WHEN $5 = 'pending' AND $7::BIGINT > 0 THEN $6::BIGINT + $7::BIGINT ELSE NULL END,
                 updated_at = $6
             WHERE id = $1 AND event = $2",
            &[
                &id,
                &event,
                &http_status,
                &error,
                &status,
                &now,
                &(retry_after_secs as i64),
            ],
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
        self.get_pending_webhook_delivery_keys_with_notify(max_retries, max_retries, batch_size)
    }

    /// As above, with its own attempt limit for `deposit_swept:notify` rows.
    pub fn get_pending_webhook_delivery_keys_with_notify(
        &self,
        max_retries: u32,
        notify_max_retries: u32,
        batch_size: u32,
    ) -> Result<Vec<(String, String)>> {
        let now = now_unix_secs();
        let mut guard = self.conn()?;
        let max_retries = max_retries as i64;
        let notify_max_retries = notify_max_retries as i64;
        let batch_size = batch_size as i64;
        let rows = guard.client().query(
            "SELECT id, event FROM webhook_deliveries
             WHERE status = 'pending'
               AND attempt_count < CASE WHEN event = 'deposit_swept:notify' THEN $4::BIGINT ELSE $1::BIGINT END
               AND (leased_until IS NULL OR leased_until < $2)
             ORDER BY updated_at ASC
             LIMIT $3",
            &[&max_retries, &now, &batch_size, &notify_max_retries],
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

/// Inserts or refreshes a pending outbox row inside `tx`. Returns true when
/// the worker should be notified.
fn upsert_outbox(tx: &mut postgres::Transaction<'_>, entry: &OutboxEntry) -> Result<bool> {
    let now = now_unix_secs();
    let existing: Option<String> = tx
        .query_opt(
            "SELECT status FROM webhook_deliveries WHERE id = $1 AND event = $2",
            &[&entry.id, &entry.event],
        )?
        .map(|r| r.get(0));

    if existing.as_deref() == Some("delivered") {
        return Ok(false);
    }

    if existing.is_none() {
        tx.execute(
            "INSERT INTO webhook_deliveries
             (id, event, registration_id, webhook_url, payload, status, attempt_count,
              last_http_status, last_error, leased_until, updated_at)
             VALUES ($1, $2, $3, $4, $5, 'pending', 0, NULL, NULL, NULL, $6)",
            &[
                &entry.id,
                &entry.event,
                &entry.registration_id,
                &entry.webhook_url,
                &entry.payload,
                &now,
            ],
        )?;
        return Ok(true);
    }

    if existing.as_deref() == Some("failed") {
        return Ok(false);
    }

    tx.execute(
        "UPDATE webhook_deliveries
         SET webhook_url = $3, payload = $4, updated_at = $5
         WHERE id = $1 AND event = $2 AND status = 'pending'",
        &[
            &entry.id,
            &entry.event,
            &entry.webhook_url,
            &entry.payload,
            &now,
        ],
    )?;
    Ok(true)
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
    let accounts_count: i64 = client
        .query_one("SELECT COUNT(*) FROM accounts", &[])?
        .get(0);
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
