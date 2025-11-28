use anyhow::Result;
use redb::{Database, ReadableTable, TableDefinition};
use std::sync::Arc;

const ACCOUNTS: TableDefinition<&str, (u32, &str)> = TableDefinition::new("accounts");
const ADDRESS_TO_ID: TableDefinition<&str, &str> = TableDefinition::new("address_to_id");
const DEPOSITS: TableDefinition<&str, (&str, &str, &str)> = TableDefinition::new("deposits"); // tx_hash -> (account_id, amount, status)
const STATE: TableDefinition<&str, &str> = TableDefinition::new("state");

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
        }
        write_txn.commit()?;

        Ok(Self { db: Arc::new(db) })
    }

    pub fn get_next_derivation_index(&self) -> Result<u32> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ACCOUNTS)?;
        // This is inefficient O(N) but fine for MVP.
        // Better: Store a counter in STATE table.
        let last = table.iter()?.last();

        match last {
            Some(Ok((_, v))) => Ok(v.value().0 + 1),
            _ => Ok(0),
        }
    }

    pub fn register_account(&self, id: &str, index: u32, address: &str) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut accounts = write_txn.open_table(ACCOUNTS)?;
            accounts.insert(id, (index, address))?;

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

    pub fn get_account_by_id(&self, id: &str) -> Result<Option<(u32, String)>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ACCOUNTS)?;
        let result = table.get(id)?;
        Ok(result.map(|v| {
            let val = v.value();
            (val.0, val.1.to_string())
        }))
    }

    pub fn record_deposit(&self, tx_hash: &str, account_id: &str, amount: &str) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut deposits = write_txn.open_table(DEPOSITS)?;
            // Check if exists to avoid overwrite if that matters, but here we just insert
            if deposits.get(tx_hash)?.is_none() {
                deposits.insert(tx_hash, (account_id, amount, "detected"))?;
            }
        }
        write_txn.commit()?;
        Ok(())
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
}
