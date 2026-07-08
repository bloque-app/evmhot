mod pg;
mod redb;

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

pub use pg::PostgresStorage;
pub use redb::RedbStorage;

#[derive(Clone, Debug)]
pub struct Erc20Deposit {
    pub key: String,
    pub account_id: String,
    pub amount: String,
    pub token_address: String,
    pub token_symbol: String,
}

/// Storage backend for accounts, deposits and sweep bookkeeping.
///
/// Implemented by [`RedbStorage`] (embedded file database) and
/// [`PostgresStorage`] (PostgreSQL server).
#[async_trait]
pub trait Storage: Send + Sync {
    #[allow(dead_code)]
    async fn get_next_derivation_index(&self) -> Result<u32>;

    async fn register_account(
        &self,
        id: &str,
        index: u32,
        address: &str,
        webhook_url: &str,
    ) -> Result<()>;

    async fn get_registration_id_by_address(&self, address: &str) -> Result<Option<String>>;

    async fn get_account_by_address(&self, address: &str) -> Result<Option<String>>;

    async fn get_account_by_id(&self, id: &str) -> Result<Option<(u32, String, String)>>;

    async fn get_webhook_url(&self, account_id: &str) -> Result<Option<String>>;

    /// Record a deposit and return true if it was newly recorded, false if it was a duplicate
    async fn record_deposit(&self, tx_hash: &str, account_id: &str, amount: &str) -> Result<bool>;

    async fn mark_deposit_swept(&self, tx_hash: &str) -> Result<()>;

    async fn get_detected_deposits(&self) -> Result<Vec<(String, String, String)>>;

    async fn get_last_processed_block(&self) -> Result<u64>;

    async fn set_last_processed_block(&self, block: u64) -> Result<()>;

    async fn store_token_metadata(
        &self,
        address: &str,
        symbol: &str,
        decimals: u8,
        name: &str,
    ) -> Result<()>;

    async fn get_token_metadata(&self, address: &str) -> Result<Option<(String, u8, String)>>;

    /// Record an ERC20 deposit and return true if it was newly recorded, false if it was a duplicate
    async fn record_erc20_deposit(
        &self,
        tx_hash: &str,
        log_index: u64,
        account_id: &str,
        amount: &str,
        token_address: &str,
        token_symbol: &str,
    ) -> Result<bool>;

    async fn get_detected_erc20_deposits(&self) -> Result<Vec<Erc20Deposit>>;

    async fn mark_erc20_deposit_swept(&self, key: &str) -> Result<()>;

    /// Mark all detected ERC20 deposits for a given (account_id, token_address) as swept.
    /// Returns the list of deposit keys that were marked.
    async fn mark_erc20_deposits_swept_for_account_token(
        &self,
        account_id: &str,
        token_address: &str,
    ) -> Result<Vec<String>>;

    /// Increment zero-balance retry count for a deposit. Returns the new count.
    async fn increment_zero_balance_count(&self, key: &str) -> Result<u64>;

    /// Store the on-chain sweep tx hash for a single deposit key.
    #[allow(dead_code)]
    async fn set_sweep_tx_hash(&self, key: &str, tx_hash: &str) -> Result<()>;

    /// Store the on-chain sweep tx hash for multiple deposit keys in one transaction.
    async fn set_sweep_tx_hash_for_keys(&self, keys: &[String], tx_hash: &str) -> Result<()>;

    /// Read sweep metadata for a deposit key.
    #[allow(dead_code)]
    async fn get_sweep_meta(&self, key: &str) -> Result<Option<(String, u64)>>;

    /// Increment the sweep failure count for a deposit. Returns the new count.
    async fn increment_sweep_failure_count(&self, key: &str) -> Result<u64>;

    /// Mark a single ERC20 deposit as permanently failed.
    #[allow(dead_code)]
    async fn mark_erc20_deposit_failed(&self, key: &str) -> Result<()>;

    /// Mark all detected ERC20 deposits for a given (account_id, token_address) as permanently failed.
    /// Returns the list of deposit keys that were marked.
    async fn mark_erc20_deposits_failed_for_account_token(
        &self,
        account_id: &str,
        token_address: &str,
    ) -> Result<Vec<String>>;
}

/// Handle to the configured storage backend.
///
/// The driver is selected from the database URL scheme:
/// `postgres://` / `postgresql://` use PostgreSQL, anything else is treated
/// as a redb file path.
#[derive(Clone)]
pub struct Db(Arc<dyn Storage>);

impl Db {
    pub async fn new(database_url: &str) -> Result<Self> {
        if database_url.starts_with("postgres://") || database_url.starts_with("postgresql://") {
            Ok(Self(Arc::new(
                PostgresStorage::connect(database_url).await?,
            )))
        } else {
            Ok(Self(Arc::new(RedbStorage::new(database_url)?)))
        }
    }
}

impl std::ops::Deref for Db {
    type Target = dyn Storage;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}
