use anyhow::Result;
use redb::{Database, ReadableTable, TableDefinition};
use std::sync::Arc;

const ACCOUNTS: TableDefinition<&str, (u32, &str, &str)> = TableDefinition::new("accounts"); // account_id -> (index, address, webhook_url)
const ADDRESS_TO_ID: TableDefinition<&str, &str> = TableDefinition::new("address_to_id");
const DEPOSITS: TableDefinition<&str, (&str, &str, &str)> = TableDefinition::new("deposits"); // tx_hash -> (account_id, amount, status)
const STATE: TableDefinition<&str, &str> = TableDefinition::new("state");
const TOKEN_METADATA: TableDefinition<&str, (&str, u64, &str)> =
    TableDefinition::new("token_metadata"); // token_address -> (symbol, decimals, name)
const ERC20_DEPOSITS: TableDefinition<&str, (&str, &str, &str, &str, &str)> =
    TableDefinition::new("erc20_deposits"); // tx_hash:log_index -> (account_id, amount, token_address, token_symbol, status)

#[derive(Clone, Debug)]
pub struct Erc20Deposit {
    pub key: String,
    pub account_id: String,
    pub amount: String,
    pub token_address: String,
    pub token_symbol: String,
}

#[derive(Clone)]
pub struct Db {
    db: Arc<Database>,
}

impl Db {
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
        }
        write_txn.commit()?;

        Ok(Self { db: Arc::new(db) })
    }

    #[allow(dead_code)]
    pub fn get_next_derivation_index(&self) -> Result<u32> {
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

    pub fn register_account(
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

    pub fn get_account_by_address(&self, address: &str) -> Result<Option<String>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ADDRESS_TO_ID)?;
        let result = table.get(address)?;
        Ok(result.map(|v| v.value().to_string()))
    }

    pub fn get_account_by_id(&self, id: &str) -> Result<Option<(u32, String, String)>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ACCOUNTS)?;
        let result = table.get(id)?;
        Ok(result.map(|v| {
            let val = v.value();
            (val.0, val.1.to_string(), val.2.to_string())
        }))
    }

    pub fn get_webhook_url(&self, account_id: &str) -> Result<Option<String>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ACCOUNTS)?;
        let result = table.get(account_id)?;
        Ok(result.map(|v| v.value().2.to_string()))
    }

    /// Record a deposit and return true if it was newly recorded, false if it was a duplicate
    pub fn record_deposit(&self, tx_hash: &str, account_id: &str, amount: &str) -> Result<bool> {
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

    pub fn mark_deposit_swept(&self, tx_hash: &str) -> Result<()> {
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

    pub fn get_detected_deposits(&self) -> Result<Vec<(String, String, String)>> {
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

    pub fn get_last_processed_block(&self) -> Result<u64> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(STATE)?;
        let result = table.get("last_block")?;
        Ok(result.map(|v| v.value().parse().unwrap_or(0)).unwrap_or(0))
    }

    pub fn set_last_processed_block(&self, block: u64) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut state = write_txn.open_table(STATE)?;
            state.insert("last_block", block.to_string().as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    // ========== ERC20 Token Metadata ==========

    pub fn store_token_metadata(
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

    pub fn get_token_metadata(&self, address: &str) -> Result<Option<(String, u8, String)>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(TOKEN_METADATA)?;
        let result = table.get(address)?;
        Ok(result.map(|v| {
            let val = v.value();
            (val.0.to_string(), val.1 as u8, val.2.to_string())
        }))
    }

    // ========== ERC20 Deposits ==========

    /// Record an ERC20 deposit and return true if it was newly recorded, false if it was a duplicate
    pub fn record_erc20_deposit(
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

    pub fn get_detected_erc20_deposits(&self) -> Result<Vec<Erc20Deposit>> {
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

    pub fn mark_erc20_deposit_swept(&self, key: &str) -> Result<()> {
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
}
