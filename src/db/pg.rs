use anyhow::Result;
use async_trait::async_trait;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;

use super::{Erc20Deposit, Storage};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS accounts (
    id TEXT PRIMARY KEY,
    derivation_index BIGINT NOT NULL,
    address TEXT NOT NULL,
    webhook_url TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS accounts_address_idx ON accounts (address);

CREATE TABLE IF NOT EXISTS deposits (
    tx_hash TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    amount TEXT NOT NULL,
    status TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS deposits_status_idx ON deposits (status);

CREATE TABLE IF NOT EXISTS state (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS token_metadata (
    address TEXT PRIMARY KEY,
    symbol TEXT NOT NULL,
    decimals BIGINT NOT NULL,
    name TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS erc20_deposits (
    key TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    amount TEXT NOT NULL,
    token_address TEXT NOT NULL,
    token_symbol TEXT NOT NULL,
    status TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS erc20_deposits_status_idx ON erc20_deposits (status);

CREATE TABLE IF NOT EXISTS sweep_meta (
    key TEXT PRIMARY KEY,
    sweep_tx_hash TEXT NOT NULL DEFAULT '',
    zero_balance_retry_count BIGINT NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS sweep_failures (
    key TEXT PRIMARY KEY,
    count BIGINT NOT NULL DEFAULT 0
);
"#;

/// PostgreSQL storage driver.
#[derive(Clone)]
pub struct PostgresStorage {
    pool: PgPool,
}

impl PostgresStorage {
    pub async fn connect(database_url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(database_url)
            .await?;

        sqlx::raw_sql(SCHEMA).execute(&pool).await?;

        Ok(Self { pool })
    }
}

#[async_trait]
impl Storage for PostgresStorage {
    async fn get_next_derivation_index(&self) -> Result<u32> {
        let row =
            sqlx::query("SELECT COALESCE(MAX(derivation_index), -1) AS max_index FROM accounts")
                .fetch_one(&self.pool)
                .await?;
        let max_index: i64 = row.get("max_index");
        Ok((max_index + 1) as u32)
    }

    async fn register_account(
        &self,
        id: &str,
        index: u32,
        address: &str,
        webhook_url: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO accounts (id, derivation_index, address, webhook_url)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO UPDATE
             SET derivation_index = EXCLUDED.derivation_index,
                 address = EXCLUDED.address,
                 webhook_url = EXCLUDED.webhook_url",
        )
        .bind(id)
        .bind(index as i64)
        .bind(address)
        .bind(webhook_url)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_registration_id_by_address(&self, address: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT id FROM accounts WHERE address = $1 LIMIT 1")
            .bind(address)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get("id")))
    }

    async fn get_account_by_address(&self, address: &str) -> Result<Option<String>> {
        self.get_registration_id_by_address(address).await
    }

    async fn get_account_by_id(&self, id: &str) -> Result<Option<(u32, String, String)>> {
        let row = sqlx::query(
            "SELECT derivation_index, address, webhook_url FROM accounts WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| {
            let index: i64 = r.get("derivation_index");
            (index as u32, r.get("address"), r.get("webhook_url"))
        }))
    }

    async fn get_webhook_url(&self, account_id: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT webhook_url FROM accounts WHERE id = $1")
            .bind(account_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get("webhook_url")))
    }

    async fn record_deposit(&self, tx_hash: &str, account_id: &str, amount: &str) -> Result<bool> {
        let result = sqlx::query(
            "INSERT INTO deposits (tx_hash, account_id, amount, status)
             VALUES ($1, $2, $3, 'detected')
             ON CONFLICT (tx_hash) DO NOTHING",
        )
        .bind(tx_hash)
        .bind(account_id)
        .bind(amount)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn mark_deposit_swept(&self, tx_hash: &str) -> Result<()> {
        sqlx::query("UPDATE deposits SET status = 'swept' WHERE tx_hash = $1")
            .bind(tx_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn get_detected_deposits(&self) -> Result<Vec<(String, String, String)>> {
        let rows = sqlx::query(
            "SELECT tx_hash, account_id, amount FROM deposits WHERE status = 'detected' ORDER BY tx_hash",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get("tx_hash"), r.get("account_id"), r.get("amount")))
            .collect())
    }

    async fn get_last_processed_block(&self) -> Result<u64> {
        let row = sqlx::query("SELECT value FROM state WHERE key = 'last_block'")
            .fetch_optional(&self.pool)
            .await?;
        Ok(row
            .map(|r| r.get::<String, _>("value").parse().unwrap_or(0))
            .unwrap_or(0))
    }

    async fn set_last_processed_block(&self, block: u64) -> Result<()> {
        sqlx::query(
            "INSERT INTO state (key, value) VALUES ('last_block', $1)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(block.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ========== ERC20 Token Metadata ==========

    async fn store_token_metadata(
        &self,
        address: &str,
        symbol: &str,
        decimals: u8,
        name: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO token_metadata (address, symbol, decimals, name)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (address) DO UPDATE
             SET symbol = EXCLUDED.symbol,
                 decimals = EXCLUDED.decimals,
                 name = EXCLUDED.name",
        )
        .bind(address)
        .bind(symbol)
        .bind(decimals as i64)
        .bind(name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_token_metadata(&self, address: &str) -> Result<Option<(String, u8, String)>> {
        let row =
            sqlx::query("SELECT symbol, decimals, name FROM token_metadata WHERE address = $1")
                .bind(address)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|r| {
            let decimals: i64 = r.get("decimals");
            (r.get("symbol"), decimals as u8, r.get("name"))
        }))
    }

    // ========== ERC20 Deposits ==========

    async fn record_erc20_deposit(
        &self,
        tx_hash: &str,
        log_index: u64,
        account_id: &str,
        amount: &str,
        token_address: &str,
        token_symbol: &str,
    ) -> Result<bool> {
        let key = format!("{}:{}", tx_hash, log_index);
        let result = sqlx::query(
            "INSERT INTO erc20_deposits (key, account_id, amount, token_address, token_symbol, status)
             VALUES ($1, $2, $3, $4, $5, 'detected')
             ON CONFLICT (key) DO NOTHING",
        )
        .bind(&key)
        .bind(account_id)
        .bind(amount)
        .bind(token_address)
        .bind(token_symbol)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn get_detected_erc20_deposits(&self) -> Result<Vec<Erc20Deposit>> {
        let rows = sqlx::query(
            "SELECT key, account_id, amount, token_address, token_symbol
             FROM erc20_deposits WHERE status = 'detected' ORDER BY key",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| Erc20Deposit {
                key: r.get("key"),
                account_id: r.get("account_id"),
                amount: r.get("amount"),
                token_address: r.get("token_address"),
                token_symbol: r.get("token_symbol"),
            })
            .collect())
    }

    async fn mark_erc20_deposit_swept(&self, key: &str) -> Result<()> {
        sqlx::query("UPDATE erc20_deposits SET status = 'swept' WHERE key = $1")
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn mark_erc20_deposits_swept_for_account_token(
        &self,
        account_id: &str,
        token_address: &str,
    ) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "UPDATE erc20_deposits SET status = 'swept'
             WHERE status = 'detected' AND account_id = $1 AND token_address = $2
             RETURNING key",
        )
        .bind(account_id)
        .bind(token_address)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.get("key")).collect())
    }

    // ========== Sweep Metadata ==========

    async fn increment_zero_balance_count(&self, key: &str) -> Result<u64> {
        let row = sqlx::query(
            "INSERT INTO sweep_meta (key, zero_balance_retry_count) VALUES ($1, 1)
             ON CONFLICT (key) DO UPDATE
             SET zero_balance_retry_count = sweep_meta.zero_balance_retry_count + 1
             RETURNING zero_balance_retry_count",
        )
        .bind(key)
        .fetch_one(&self.pool)
        .await?;
        let count: i64 = row.get("zero_balance_retry_count");
        Ok(count as u64)
    }

    async fn set_sweep_tx_hash(&self, key: &str, tx_hash: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO sweep_meta (key, sweep_tx_hash) VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET sweep_tx_hash = EXCLUDED.sweep_tx_hash",
        )
        .bind(key)
        .bind(tx_hash)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn set_sweep_tx_hash_for_keys(&self, keys: &[String], tx_hash: &str) -> Result<()> {
        let mut txn = self.pool.begin().await?;
        for key in keys {
            sqlx::query(
                "INSERT INTO sweep_meta (key, sweep_tx_hash) VALUES ($1, $2)
                 ON CONFLICT (key) DO UPDATE SET sweep_tx_hash = EXCLUDED.sweep_tx_hash",
            )
            .bind(key)
            .bind(tx_hash)
            .execute(&mut *txn)
            .await?;
        }
        txn.commit().await?;
        Ok(())
    }

    async fn get_sweep_meta(&self, key: &str) -> Result<Option<(String, u64)>> {
        let row = sqlx::query(
            "SELECT sweep_tx_hash, zero_balance_retry_count FROM sweep_meta WHERE key = $1",
        )
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| {
            let count: i64 = r.get("zero_balance_retry_count");
            (r.get("sweep_tx_hash"), count as u64)
        }))
    }

    // ========== Sweep Failure Tracking ==========

    async fn increment_sweep_failure_count(&self, key: &str) -> Result<u64> {
        let row = sqlx::query(
            "INSERT INTO sweep_failures (key, count) VALUES ($1, 1)
             ON CONFLICT (key) DO UPDATE SET count = sweep_failures.count + 1
             RETURNING count",
        )
        .bind(key)
        .fetch_one(&self.pool)
        .await?;
        let count: i64 = row.get("count");
        Ok(count as u64)
    }

    async fn mark_erc20_deposit_failed(&self, key: &str) -> Result<()> {
        sqlx::query("UPDATE erc20_deposits SET status = 'failed' WHERE key = $1")
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn mark_erc20_deposits_failed_for_account_token(
        &self,
        account_id: &str,
        token_address: &str,
    ) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "UPDATE erc20_deposits SET status = 'failed'
             WHERE status = 'detected' AND account_id = $1 AND token_address = $2
             RETURNING key",
        )
        .bind(account_id)
        .bind(token_address)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.get("key")).collect())
    }
}
