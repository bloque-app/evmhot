use anyhow::Result;
use async_trait::async_trait;
use redb::{Database, ReadableTable, TableDefinition};
use std::sync::Arc;

use super::{Erc20Deposit, Storage};

const ACCOUNTS: TableDefinition<&str, (u32, &str, &str)> = TableDefinition::new("accounts"); // account_id -> (index, address, webhook_url)
const ADDRESS_TO_ID: TableDefinition<&str, &str> = TableDefinition::new("address_to_id");
const DEPOSITS: TableDefinition<&str, (&str, &str, &str)> = TableDefinition::new("deposits"); // tx_hash -> (account_id, amount, status)
const STATE: TableDefinition<&str, &str> = TableDefinition::new("state");
const TOKEN_METADATA: TableDefinition<&str, (&str, u64, &str)> =
    TableDefinition::new("token_metadata"); // token_address -> (symbol, decimals, name)
const ERC20_DEPOSITS: TableDefinition<&str, (&str, &str, &str, &str, &str)> =
    TableDefinition::new("erc20_deposits"); // tx_hash:log_index -> (account_id, amount, token_address, token_symbol, status)
const SWEEP_META: TableDefinition<&str, (&str, u64)> = TableDefinition::new("sweep_meta"); // deposit_key -> (sweep_tx_hash, zero_balance_retry_count)
const SWEEP_FAILURES: TableDefinition<&str, u64> = TableDefinition::new("sweep_failures"); // deposit_key -> consecutive_failure_count

/// Embedded file-based storage driver backed by redb.
#[derive(Clone)]
pub struct RedbStorage {
    db: Arc<Database>,
}

impl RedbStorage {
    pub fn new(path: &str) -> Result<Self> {
        let db = Database::create(path)?;

        // Initialize tables
        let write_txn = db.begin_write()?;
        {
            let _ = write_txn.open_table(ACCOUNTS)?;
            let _ = write_txn.open_table(ADDRESS_TO_ID)?;
            let _ = write_txn.open_table(DEPOSITS)?;
            let _ = write_txn.open_table(STATE)?;
            let _ = write_txn.open_table(TOKEN_METADATA)?;
            let _ = write_txn.open_table(ERC20_DEPOSITS)?;
            let _ = write_txn.open_table(SWEEP_META)?;
            let _ = write_txn.open_table(SWEEP_FAILURES)?;
        }
        write_txn.commit()?;

        Ok(Self { db: Arc::new(db) })
    }
}

#[async_trait]
impl Storage for RedbStorage {
    async fn get_next_derivation_index(&self) -> Result<u32> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ACCOUNTS)?;
        // This is inefficient O(N) but fine for MVP.
        // Better: Store a counter in STATE table.
        let last = table.iter()?.next_back();

        match last {
            Some(Ok((_, v))) => Ok(v.value().0 + 1),
            _ => Ok(0),
        }
    }

    async fn register_account(
        &self,
        id: &str,
        index: u32,
        address: &str,
        webhook_url: &str,
    ) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut accounts = write_txn.open_table(ACCOUNTS)?;
            accounts.insert(id, (index, address, webhook_url))?;

            let mut addr_map = write_txn.open_table(ADDRESS_TO_ID)?;
            addr_map.insert(address, id)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    async fn get_registration_id_by_address(&self, address: &str) -> Result<Option<String>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ADDRESS_TO_ID)?;
        let result = table.get(address)?;
        Ok(result.map(|v| v.value().to_string()))
    }

    async fn get_account_by_address(&self, address: &str) -> Result<Option<String>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ADDRESS_TO_ID)?;
        let result = table.get(address)?;
        Ok(result.map(|v| v.value().to_string()))
    }

    async fn get_account_by_id(&self, id: &str) -> Result<Option<(u32, String, String)>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ACCOUNTS)?;
        let result = table.get(id)?;
        Ok(result.map(|v| {
            let val = v.value();
            (val.0, val.1.to_string(), val.2.to_string())
        }))
    }

    async fn get_webhook_url(&self, account_id: &str) -> Result<Option<String>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ACCOUNTS)?;
        let result = table.get(account_id)?;
        Ok(result.map(|v| v.value().2.to_string()))
    }

    async fn record_deposit(&self, tx_hash: &str, account_id: &str, amount: &str) -> Result<bool> {
        let write_txn = self.db.begin_write()?;
        let is_new = {
            let mut deposits = write_txn.open_table(DEPOSITS)?;
            // Check if exists to avoid overwrite and duplicates
            if deposits.get(tx_hash)?.is_none() {
                deposits.insert(tx_hash, (account_id, amount, "detected"))?;
                true
            } else {
                false
            }
        };
        write_txn.commit()?;
        Ok(is_new)
    }

    async fn mark_deposit_swept(&self, tx_hash: &str) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut deposits = write_txn.open_table(DEPOSITS)?;
            let (account_id, amount) = {
                let current_val = deposits.get(tx_hash)?;
                if let Some(v) = current_val {
                    let val = v.value();
                    (val.0.to_string(), val.1.to_string())
                } else {
                    return Ok(());
                }
            };

            deposits.insert(tx_hash, (account_id.as_str(), amount.as_str(), "swept"))?;
        }
        write_txn.commit()?;
        Ok(())
    }

    async fn get_detected_deposits(&self) -> Result<Vec<(String, String, String)>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(DEPOSITS)?;
        let mut results = Vec::new();
        for item in table.iter()? {
            let (tx_hash, value) = item?;
            let (account_id, amount, status) = value.value();
            if status == "detected" {
                results.push((
                    tx_hash.value().to_string(),
                    account_id.to_string(),
                    amount.to_string(),
                ));
            }
        }
        Ok(results)
    }

    async fn get_last_processed_block(&self) -> Result<u64> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(STATE)?;
        let result = table.get("last_block")?;
        Ok(result.map(|v| v.value().parse().unwrap_or(0)).unwrap_or(0))
    }

    async fn set_last_processed_block(&self, block: u64) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut state = write_txn.open_table(STATE)?;
            state.insert("last_block", block.to_string().as_str())?;
        }
        write_txn.commit()?;
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
        let write_txn = self.db.begin_write()?;
        {
            let mut metadata = write_txn.open_table(TOKEN_METADATA)?;
            metadata.insert(address, (symbol, decimals as u64, name))?;
        }
        write_txn.commit()?;
        Ok(())
    }

    async fn get_token_metadata(&self, address: &str) -> Result<Option<(String, u8, String)>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(TOKEN_METADATA)?;
        let result = table.get(address)?;
        Ok(result.map(|v| {
            let val = v.value();
            (val.0.to_string(), val.1 as u8, val.2.to_string())
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
        let write_txn = self.db.begin_write()?;
        let is_new = {
            let mut deposits = write_txn.open_table(ERC20_DEPOSITS)?;
            let key = format!("{}:{}", tx_hash, log_index);
            if deposits.get(key.as_str())?.is_none() {
                deposits.insert(
                    key.as_str(),
                    (account_id, amount, token_address, token_symbol, "detected"),
                )?;
                true
            } else {
                false
            }
        };
        write_txn.commit()?;
        Ok(is_new)
    }

    async fn get_detected_erc20_deposits(&self) -> Result<Vec<Erc20Deposit>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ERC20_DEPOSITS)?;
        let mut results = Vec::new();
        for item in table.iter()? {
            let (key, value) = item?;
            let (account_id, amount, token_address, token_symbol, status) = value.value();
            if status == "detected" {
                results.push(Erc20Deposit {
                    key: key.value().to_string(), // tx_hash:log_index
                    account_id: account_id.to_string(),
                    amount: amount.to_string(),
                    token_address: token_address.to_string(),
                    token_symbol: token_symbol.to_string(),
                });
            }
        }
        Ok(results)
    }

    async fn mark_erc20_deposit_swept(&self, key: &str) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut deposits = write_txn.open_table(ERC20_DEPOSITS)?;
            let (account_id, amount, token_address, token_symbol) = {
                let current_val = deposits.get(key)?;
                if let Some(v) = current_val {
                    let val = v.value();
                    (
                        val.0.to_string(),
                        val.1.to_string(),
                        val.2.to_string(),
                        val.3.to_string(),
                    )
                } else {
                    return Ok(());
                }
            };

            deposits.insert(
                key,
                (
                    account_id.as_str(),
                    amount.as_str(),
                    token_address.as_str(),
                    token_symbol.as_str(),
                    "swept",
                ),
            )?;
        }
        write_txn.commit()?;
        Ok(())
    }

    async fn mark_erc20_deposits_swept_for_account_token(
        &self,
        account_id: &str,
        token_address: &str,
    ) -> Result<Vec<String>> {
        let write_txn = self.db.begin_write()?;
        let mut marked_keys = Vec::new();
        {
            let mut deposits = write_txn.open_table(ERC20_DEPOSITS)?;

            // First pass: collect keys that need updating
            let keys_to_update: Vec<(String, String, String, String)> = {
                let mut to_update = Vec::new();
                for item in deposits.iter()? {
                    let (key, value) = item?;
                    let (acc_id, amount, tok_addr, tok_symbol, status) = value.value();
                    if status == "detected" && acc_id == account_id && tok_addr == token_address {
                        to_update.push((
                            key.value().to_string(),
                            amount.to_string(),
                            tok_symbol.to_string(),
                            acc_id.to_string(),
                        ));
                    }
                }
                to_update
            };

            // Second pass: update the entries
            for (key, amount, tok_symbol, acc_id) in &keys_to_update {
                deposits.insert(
                    key.as_str(),
                    (
                        acc_id.as_str(),
                        amount.as_str(),
                        token_address,
                        tok_symbol.as_str(),
                        "swept",
                    ),
                )?;
                marked_keys.push(key.clone());
            }
        }
        write_txn.commit()?;
        Ok(marked_keys)
    }

    // ========== Sweep Metadata (new table, existing schemas unchanged) ==========

    async fn increment_zero_balance_count(&self, key: &str) -> Result<u64> {
        let write_txn = self.db.begin_write()?;
        let new_count = {
            let mut meta = write_txn.open_table(SWEEP_META)?;
            let (sweep_tx_hash, count) = match meta.get(key)? {
                Some(v) => {
                    let val = v.value();
                    (val.0.to_string(), val.1)
                }
                None => (String::new(), 0),
            };
            let new_count = count + 1;
            meta.insert(key, (sweep_tx_hash.as_str(), new_count))?;
            new_count
        };
        write_txn.commit()?;
        Ok(new_count)
    }

    async fn set_sweep_tx_hash(&self, key: &str, tx_hash: &str) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut meta = write_txn.open_table(SWEEP_META)?;
            let count = match meta.get(key)? {
                Some(v) => v.value().1,
                None => 0,
            };
            meta.insert(key, (tx_hash, count))?;
        }
        write_txn.commit()?;
        Ok(())
    }

    async fn set_sweep_tx_hash_for_keys(&self, keys: &[String], tx_hash: &str) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut meta = write_txn.open_table(SWEEP_META)?;
            for key in keys {
                let count = match meta.get(key.as_str())? {
                    Some(v) => v.value().1,
                    None => 0,
                };
                meta.insert(key.as_str(), (tx_hash, count))?;
            }
        }
        write_txn.commit()?;
        Ok(())
    }

    async fn get_sweep_meta(&self, key: &str) -> Result<Option<(String, u64)>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(SWEEP_META)?;
        let result = table.get(key)?;
        Ok(result.map(|v| {
            let val = v.value();
            (val.0.to_string(), val.1)
        }))
    }

    // ========== Sweep Failure Tracking ==========

    async fn increment_sweep_failure_count(&self, key: &str) -> Result<u64> {
        let write_txn = self.db.begin_write()?;
        let new_count = {
            let mut failures = write_txn.open_table(SWEEP_FAILURES)?;
            let count = match failures.get(key)? {
                Some(v) => v.value(),
                None => 0,
            };
            let new_count = count + 1;
            failures.insert(key, new_count)?;
            new_count
        };
        write_txn.commit()?;
        Ok(new_count)
    }

    async fn mark_erc20_deposit_failed(&self, key: &str) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut deposits = write_txn.open_table(ERC20_DEPOSITS)?;
            let (account_id, amount, token_address, token_symbol) = {
                let current_val = deposits.get(key)?;
                if let Some(v) = current_val {
                    let val = v.value();
                    (
                        val.0.to_string(),
                        val.1.to_string(),
                        val.2.to_string(),
                        val.3.to_string(),
                    )
                } else {
                    return Ok(());
                }
            };

            deposits.insert(
                key,
                (
                    account_id.as_str(),
                    amount.as_str(),
                    token_address.as_str(),
                    token_symbol.as_str(),
                    "failed",
                ),
            )?;
        }
        write_txn.commit()?;
        Ok(())
    }

    async fn mark_erc20_deposits_failed_for_account_token(
        &self,
        account_id: &str,
        token_address: &str,
    ) -> Result<Vec<String>> {
        let write_txn = self.db.begin_write()?;
        let mut marked_keys = Vec::new();
        {
            let mut deposits = write_txn.open_table(ERC20_DEPOSITS)?;

            let keys_to_update: Vec<(String, String, String, String)> = {
                let mut to_update = Vec::new();
                for item in deposits.iter()? {
                    let (key, value) = item?;
                    let (acc_id, amount, tok_addr, tok_symbol, status) = value.value();
                    if status == "detected" && acc_id == account_id && tok_addr == token_address {
                        to_update.push((
                            key.value().to_string(),
                            amount.to_string(),
                            tok_symbol.to_string(),
                            acc_id.to_string(),
                        ));
                    }
                }
                to_update
            };

            for (key, amount, tok_symbol, acc_id) in &keys_to_update {
                deposits.insert(
                    key.as_str(),
                    (
                        acc_id.as_str(),
                        amount.as_str(),
                        token_address,
                        tok_symbol.as_str(),
                        "failed",
                    ),
                )?;
                marked_keys.push(key.clone());
            }
        }
        write_txn.commit()?;
        Ok(marked_keys)
    }
}
