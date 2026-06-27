use anyhow::{anyhow, Result};
use redb::{Database, ReadableTable, TableDefinition};
use std::sync::Arc;

const ACCOUNTS: TableDefinition<&str, (u32, &str, &str)> = TableDefinition::new("accounts");
const ADDRESS_TO_ID: TableDefinition<&str, &str> = TableDefinition::new("address_to_id");
const DEPOSITS: TableDefinition<&str, (&str, &str, &str)> = TableDefinition::new("deposits");
const STATE: TableDefinition<&str, &str> = TableDefinition::new("state");
const TOKEN_METADATA: TableDefinition<&str, (&str, u64, &str)> =
    TableDefinition::new("token_metadata");
const ERC20_DEPOSITS: TableDefinition<&str, (&str, &str, &str, &str, &str)> =
    TableDefinition::new("erc20_deposits");
const SWEEP_META: TableDefinition<&str, (&str, u64)> = TableDefinition::new("sweep_meta");
const SWEEP_FAILURES: TableDefinition<&str, u64> = TableDefinition::new("sweep_failures");

pub const SCHEMA_VERSION: u32 = 2;

#[derive(Clone)]
pub struct RedbStore {
    db: Arc<Database>,
}

#[derive(Debug, Default)]
pub struct RedbSnapshot {
    pub accounts: Vec<(String, u32, String, String)>,
    pub address_to_id: Vec<(String, String)>,
    pub deposits: Vec<(String, String, String, String, String)>,
    pub erc20_deposits: Vec<(String, String, i64, String, String, String, String, String)>,
    pub token_metadata: Vec<(String, String, String, u8, String)>,
    pub state: Vec<(String, String)>,
    pub sweep_meta: Vec<(String, String, i64, String, u64)>,
    pub sweep_failures: Vec<(String, String, i64, u64)>,
}

fn deposit_key(chain: &str, tx_hash: &str) -> String {
    format!("{chain}:{tx_hash}")
}

fn token_metadata_key(chain: &str, token_address: &str) -> String {
    format!("{chain}:{token_address}")
}

fn last_block_key(chain: &str) -> String {
    format!("last_block:{chain}")
}

/// Parse `"0xtx:42"` -> (`0xtx`, 42). Bare `"0xtx"` -> (`0xtx`, 0).
pub fn parse_local_key(local_key: &str) -> Result<(String, i64)> {
    if let Some((tx, idx)) = local_key.rsplit_once(':') {
        if !idx.is_empty() && idx.chars().all(|c| c.is_ascii_digit()) {
            return Ok((tx.to_string(), idx.parse()?));
        }
    }
    Ok((local_key.to_string(), 0))
}

fn split_chain_key(full_key: &str) -> Result<(String, String)> {
    let (chain, rest) = full_key
        .split_once(':')
        .ok_or_else(|| anyhow!("invalid chain-prefixed key: {full_key}"))?;
    Ok((chain.to_string(), rest.to_string()))
}

impl RedbStore {
    pub fn open(path: &str) -> Result<Self> {
        let db = Database::open(path).map_err(|e| {
            anyhow!(
                "failed to open redb at {path}: {e}. \
                 If this file was copied via scp/cp while the service was running, \
                 stop the writer first, copy on the server (cp … /tmp/snapshot.db), then transfer the snapshot."
            )
        })?;
        Ok(Self { db: Arc::new(db) })
    }

    #[cfg(test)]
    pub fn open_without_migration(path: &str) -> Result<Self> {
        Ok(Self {
            db: Arc::new(Self::create_empty(path)?),
        })
    }

    #[cfg(test)]
    fn create_empty(path: &str) -> Result<Database> {
        let db = Database::create(path)?;
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
        Ok(db)
    }

    #[cfg(test)]
    pub fn insert_legacy_v1_state_for_test(
        &self,
        tx_hash: &str,
        account_id: &str,
        amount: &str,
        last_block: &str,
        status: &str,
    ) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut deposits = write_txn.open_table(DEPOSITS)?;
            deposits.insert(tx_hash, (account_id, amount, status))?;
            let mut state = write_txn.open_table(STATE)?;
            state.insert("last_block", last_block)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub fn insert_v2_erc20_deposit_for_test(
        &self,
        chain: &str,
        tx_hash: &str,
        log_index: u64,
        account_id: &str,
        amount: &str,
        token_address: &str,
        token_symbol: &str,
        status: &str,
    ) -> Result<()> {
        let key = format!("{chain}:{tx_hash}:{log_index}");
        let write_txn = self.db.begin_write()?;
        {
            let mut deposits = write_txn.open_table(ERC20_DEPOSITS)?;
            deposits.insert(
                key.as_str(),
                (account_id, amount, token_address, token_symbol, status),
            )?;
        }
        write_txn.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub fn insert_v2_sweep_meta_for_test(
        &self,
        chain: &str,
        tx_hash: &str,
        log_index: u64,
        sweep_tx_hash: &str,
        zero_balance_count: u64,
    ) -> Result<()> {
        let key = format!("{chain}:{tx_hash}:{log_index}");
        let write_txn = self.db.begin_write()?;
        {
            let mut meta = write_txn.open_table(SWEEP_META)?;
            meta.insert(key.as_str(), (sweep_tx_hash, zero_balance_count))?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// One-time migration: namespace legacy single-chain keys under `legacy_chain`.
    #[allow(clippy::type_complexity)]
    pub fn migrate_v1_to_v2(&self, legacy_chain: &str) -> Result<()> {
        let version = self.get_schema_version()?;
        if version >= SCHEMA_VERSION {
            return Ok(());
        }

        let write_txn = self.db.begin_write()?;
        {
            {
                let mut deposits = write_txn.open_table(DEPOSITS)?;
                let legacy_keys: Vec<(String, (String, String, String))> = {
                    let mut keys = Vec::new();
                    for item in deposits.iter()? {
                        let (key, value) = item?;
                        let key_str = key.value().to_string();
                        if !key_str.contains(':') {
                            let val = value.value();
                            keys.push((
                                key_str,
                                (val.0.to_string(), val.1.to_string(), val.2.to_string()),
                            ));
                        }
                    }
                    keys
                };
                for (old_key, (account_id, amount, status)) in legacy_keys {
                    let new_key = deposit_key(legacy_chain, &old_key);
                    deposits.remove(old_key.as_str())?;
                    deposits.insert(
                        new_key.as_str(),
                        (account_id.as_str(), amount.as_str(), status.as_str()),
                    )?;
                }
            }

            {
                let mut deposits = write_txn.open_table(ERC20_DEPOSITS)?;
                let legacy_keys: Vec<(String, (String, String, String, String, String))> = {
                    let mut keys = Vec::new();
                    for item in deposits.iter()? {
                        let (key, value) = item?;
                        let key_str = key.value();
                        if key_str.starts_with(&format!("{legacy_chain}:"))
                            || key_str.starts_with("last_block:")
                        {
                            continue;
                        }
                        let is_legacy = !key_str.contains(':') || key_str.starts_with("0x");
                        if is_legacy {
                            let val = value.value();
                            keys.push((
                                key_str.to_string(),
                                (
                                    val.0.to_string(),
                                    val.1.to_string(),
                                    val.2.to_string(),
                                    val.3.to_string(),
                                    val.4.to_string(),
                                ),
                            ));
                        }
                    }
                    keys
                };
                for (old_key, (account_id, amount, token_address, token_symbol, status)) in
                    legacy_keys
                {
                    let new_key = format!("{legacy_chain}:{old_key}");
                    deposits.remove(old_key.as_str())?;
                    deposits.insert(
                        new_key.as_str(),
                        (
                            account_id.as_str(),
                            amount.as_str(),
                            token_address.as_str(),
                            token_symbol.as_str(),
                            status.as_str(),
                        ),
                    )?;
                }
            }

            {
                let mut metadata = write_txn.open_table(TOKEN_METADATA)?;
                let legacy_keys: Vec<(String, (String, u64, String))> = {
                    let mut keys = Vec::new();
                    for item in metadata.iter()? {
                        let (key, value) = item?;
                        let key_str = key.value().to_string();
                        if key_str.starts_with("0x") && !key_str.contains(':') {
                            let val = value.value();
                            keys.push((key_str, (val.0.to_string(), val.1, val.2.to_string())));
                        }
                    }
                    keys
                };
                for (old_key, (symbol, decimals, name)) in legacy_keys {
                    let new_key = token_metadata_key(legacy_chain, &old_key);
                    metadata.remove(old_key.as_str())?;
                    metadata
                        .insert(new_key.as_str(), (symbol.as_str(), decimals, name.as_str()))?;
                }
            }

            {
                let mut state = write_txn.open_table(STATE)?;
                let legacy_block = state.get("last_block")?.map(|v| v.value().to_string());
                if let Some(block) = legacy_block {
                    state.remove("last_block")?;
                    state.insert(last_block_key(legacy_chain).as_str(), block.as_str())?;
                }
            }

            {
                let mut meta = write_txn.open_table(SWEEP_META)?;
                let legacy_meta: Vec<(String, (String, u64))> = {
                    let mut keys = Vec::new();
                    for item in meta.iter()? {
                        let (key, value) = item?;
                        let key_str = key.value().to_string();
                        if key_str.starts_with("0x")
                            && !key_str.starts_with(&format!("{legacy_chain}:"))
                        {
                            let val = value.value();
                            keys.push((key_str, (val.0.to_string(), val.1)));
                        }
                    }
                    keys
                };
                for (old_key, (sweep_tx, count)) in legacy_meta {
                    let new_key = if old_key.contains(':') {
                        format!("{legacy_chain}:{old_key}")
                    } else {
                        deposit_key(legacy_chain, &old_key)
                    };
                    meta.remove(old_key.as_str())?;
                    meta.insert(new_key.as_str(), (sweep_tx.as_str(), count))?;
                }
            }

            {
                let mut failures = write_txn.open_table(SWEEP_FAILURES)?;
                let legacy_failures: Vec<(String, u64)> = {
                    let mut keys = Vec::new();
                    for item in failures.iter()? {
                        let (key, value) = item?;
                        let key_str = key.value().to_string();
                        if key_str.starts_with("0x")
                            && !key_str.starts_with(&format!("{legacy_chain}:"))
                        {
                            keys.push((key_str, value.value()));
                        }
                    }
                    keys
                };
                for (old_key, count) in legacy_failures {
                    let new_key = if old_key.contains(':') {
                        format!("{legacy_chain}:{old_key}")
                    } else {
                        deposit_key(legacy_chain, &old_key)
                    };
                    failures.remove(old_key.as_str())?;
                    failures.insert(new_key.as_str(), count)?;
                }
            }

            let mut state = write_txn.open_table(STATE)?;
            state.insert("schema_version", SCHEMA_VERSION.to_string().as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn get_schema_version(&self) -> Result<u32> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(STATE)?;
        let result = table.get("schema_version")?;
        Ok(result.map(|v| v.value().parse().unwrap_or(0)).unwrap_or(0))
    }

    pub fn export_snapshot(&self) -> Result<RedbSnapshot> {
        let mut snapshot = RedbSnapshot::default();
        let read_txn = self.db.begin_read()?;

        {
            let table = read_txn.open_table(ACCOUNTS)?;
            for item in table.iter()? {
                let (id, value) = item?;
                let val = value.value();
                snapshot.accounts.push((
                    id.value().to_string(),
                    val.0,
                    val.1.to_string(),
                    val.2.to_string(),
                ));
            }
        }

        {
            let table = read_txn.open_table(ADDRESS_TO_ID)?;
            for item in table.iter()? {
                let (address, id) = item?;
                snapshot
                    .address_to_id
                    .push((address.value().to_string(), id.value().to_string()));
            }
        }

        {
            let table = read_txn.open_table(DEPOSITS)?;
            for item in table.iter()? {
                let (key, value) = item?;
                let key_str = key.value();
                let (chain, tx_hash) = split_chain_key(key_str)?;
                let (account_id, amount, status) = value.value();
                snapshot.deposits.push((
                    chain,
                    tx_hash,
                    account_id.to_string(),
                    amount.to_string(),
                    status.to_string(),
                ));
            }
        }

        {
            let table = read_txn.open_table(ERC20_DEPOSITS)?;
            for item in table.iter()? {
                let (key, value) = item?;
                let key_str = key.value();
                let (chain, local) = split_chain_key(key_str)?;
                let (tx_hash, log_index) = parse_local_key(&local)?;
                let (account_id, amount, token_address, token_symbol, status) = value.value();
                snapshot.erc20_deposits.push((
                    chain,
                    tx_hash,
                    log_index,
                    account_id.to_string(),
                    amount.to_string(),
                    token_address.to_string(),
                    token_symbol.to_string(),
                    status.to_string(),
                ));
            }
        }

        {
            let table = read_txn.open_table(TOKEN_METADATA)?;
            for item in table.iter()? {
                let (key, value) = item?;
                let key_str = key.value();
                let (chain, token_address) = split_chain_key(key_str)?;
                let (symbol, decimals, name) = value.value();
                snapshot.token_metadata.push((
                    chain,
                    token_address,
                    symbol.to_string(),
                    decimals as u8,
                    name.to_string(),
                ));
            }
        }

        {
            let table = read_txn.open_table(STATE)?;
            for item in table.iter()? {
                let (key, value) = item?;
                let key_str = key.value();
                if key_str == "schema_version" {
                    continue;
                }
                snapshot
                    .state
                    .push((key_str.to_string(), value.value().to_string()));
            }
        }

        {
            let table = read_txn.open_table(SWEEP_META)?;
            for item in table.iter()? {
                let (key, value) = item?;
                let key_str = key.value();
                let (chain, local) = split_chain_key(key_str)?;
                let (tx_hash, log_index) = parse_local_key(&local)?;
                let (sweep_tx_hash, count) = value.value();
                snapshot.sweep_meta.push((
                    chain,
                    tx_hash,
                    log_index,
                    sweep_tx_hash.to_string(),
                    count,
                ));
            }
        }

        {
            let table = read_txn.open_table(SWEEP_FAILURES)?;
            for item in table.iter()? {
                let (key, value) = item?;
                let key_str = key.value();
                let (chain, local) = split_chain_key(key_str)?;
                let (tx_hash, log_index) = parse_local_key(&local)?;
                snapshot
                    .sweep_failures
                    .push((chain, tx_hash, log_index, value.value()));
            }
        }

        Ok(snapshot)
    }
}
