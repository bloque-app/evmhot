use crate::db::{apply_pragmas_for_import, migrations};
use crate::redb_store::RedbStore;
use anyhow::{anyhow, Result};
use rusqlite::{params, Connection};
use std::path::Path;
use tracing::warn;

#[derive(Debug, Default)]
pub struct ImportSummary {
    pub accounts: (usize, usize),
    pub deposits: (usize, usize),
    pub erc20_deposits: (usize, usize),
    pub token_metadata: (usize, usize),
    pub state: (usize, usize),
    pub sweep_meta: (usize, usize),
    pub sweep_failures: (usize, usize),
    pub orphan_address_mappings: usize,
    pub block_cursors: Vec<(String, u64)>,
}

pub fn migrate_redb_file_to_sqlite(
    from_redb: &Path,
    to_sqlite: &Path,
    legacy_chain: &str,
    force: bool,
) -> Result<ImportSummary> {
    if to_sqlite.exists() {
        if force {
            std::fs::remove_file(to_sqlite)?;
            let _ = std::fs::remove_file(to_sqlite.with_extension("db-wal"));
            let _ = std::fs::remove_file(to_sqlite.with_extension("db-shm"));
        } else {
            return Err(anyhow!(
                "SQLite file already exists: {} (use --force to overwrite)",
                to_sqlite.display()
            ));
        }
    }

    let store = RedbStore::open(
        from_redb
            .to_str()
            .ok_or_else(|| anyhow!("invalid redb path"))?,
    )?;
    store.migrate_v1_to_v2(legacy_chain)?;
    let snapshot = store.export_snapshot()?;

    let to_str = to_sqlite
        .to_str()
        .ok_or_else(|| anyhow!("invalid sqlite path"))?;
    let mut conn = Connection::open(to_str)?;
    apply_pragmas_for_import(&conn)?;
    migrations().to_latest(&mut conn)?;

    let tx = conn.unchecked_transaction()?;

    let mut summary = ImportSummary::default();
    summary.accounts.0 = snapshot.accounts.len();

    for (id, index, address, webhook) in &snapshot.accounts {
        tx.execute(
            "INSERT OR IGNORE INTO accounts (id, derivation_index, address, webhook_url)
             VALUES (?1, ?2, ?3, ?4)",
            params![id, index, address, webhook],
        )?;
        if tx.changes() == 1 {
            summary.accounts.1 += 1;
        }
    }

    let account_addresses: std::collections::HashSet<String> = snapshot
        .accounts
        .iter()
        .map(|(_, _, addr, _)| addr.clone())
        .collect();

    for (address, id) in &snapshot.address_to_id {
        if !account_addresses.contains(address) {
            warn!(
                orphan_address = %address,
                registration_id = %id,
                "address_to_id entry has no matching account row"
            );
            summary.orphan_address_mappings += 1;
        }
    }

    summary.deposits.0 = snapshot.deposits.len();
    for (chain, tx_hash, account_id, amount, status) in &snapshot.deposits {
        tx.execute(
            "INSERT OR IGNORE INTO deposits (chain, tx_hash, account_id, amount, status)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![chain, tx_hash, account_id, amount, status],
        )?;
        if tx.changes() == 1 {
            summary.deposits.1 += 1;
        }
    }

    summary.erc20_deposits.0 = snapshot.erc20_deposits.len();
    for (
        chain,
        tx_hash,
        log_index,
        account_id,
        amount,
        token_address,
        token_symbol,
        status,
    ) in &snapshot.erc20_deposits
    {
        tx.execute(
            "INSERT OR IGNORE INTO erc20_deposits
             (chain, tx_hash, log_index, account_id, amount, token_address, token_symbol, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                chain,
                tx_hash,
                log_index,
                account_id,
                amount,
                token_address,
                token_symbol,
                status
            ],
        )?;
        if tx.changes() == 1 {
            summary.erc20_deposits.1 += 1;
        }
    }

    summary.token_metadata.0 = snapshot.token_metadata.len();
    for (chain, token_address, symbol, decimals, name) in &snapshot.token_metadata {
        tx.execute(
            "INSERT OR IGNORE INTO token_metadata (chain, token_address, symbol, decimals, name)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![chain, token_address, symbol, decimals, name],
        )?;
        if tx.changes() == 1 {
            summary.token_metadata.1 += 1;
        }
    }

    summary.state.0 = snapshot.state.len();
    for (key, value) in &snapshot.state {
        tx.execute(
            "INSERT OR IGNORE INTO state (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        if tx.changes() == 1 {
            summary.state.1 += 1;
        }
        if let Some(chain) = key.strip_prefix("last_block:") {
            let block: u64 = value.parse().unwrap_or(0);
            summary.block_cursors.push((chain.to_string(), block));
        }
    }

    summary.sweep_meta.0 = snapshot.sweep_meta.len();
    for (chain, tx_hash, log_index, sweep_tx_hash, count) in &snapshot.sweep_meta {
        tx.execute(
            "INSERT OR IGNORE INTO sweep_meta
             (chain, tx_hash, log_index, sweep_tx_hash, zero_balance_retry_count)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![chain, tx_hash, log_index, sweep_tx_hash, *count as i64],
        )?;
        if tx.changes() == 1 {
            summary.sweep_meta.1 += 1;
        }
    }

    summary.sweep_failures.0 = snapshot.sweep_failures.len();
    for (chain, tx_hash, log_index, count) in &snapshot.sweep_failures {
        tx.execute(
            "INSERT OR IGNORE INTO sweep_failures (chain, tx_hash, log_index, consecutive_failure_count)
             VALUES (?1, ?2, ?3, ?4)",
            params![chain, tx_hash, log_index, *count as i64],
        )?;
        if tx.changes() == 1 {
            summary.sweep_failures.1 += 1;
        }
    }

    tx.commit()?;

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::redb_store::RedbStore;
    use tempfile::NamedTempFile;

    #[test]
    fn test_import_legacy_redb_preserves_state() {
        let redb_tmp = NamedTempFile::new().unwrap();
        let redb_path = redb_tmp.path().to_str().unwrap();

        {
            let store = RedbStore::open_without_migration(redb_path).unwrap();
            store
                .insert_legacy_v1_state_for_test("0xlegacy", "user1", "500", "42", "swept")
                .unwrap();
            store
                .insert_v2_erc20_deposit_for_test(
                    "polygon",
                    "0xswept",
                    1,
                    "user1",
                    "100",
                    "0xtoken",
                    "USDC",
                    "swept",
                )
                .unwrap();
            store
                .insert_v2_sweep_meta_for_test("polygon", "0xswept", 1, "0xsweep_tx", 3)
                .unwrap();
        }

        let sqlite_tmp = NamedTempFile::new().unwrap();
        let sqlite_path = sqlite_tmp.path();

        let summary =
            migrate_redb_file_to_sqlite(redb_tmp.path(), sqlite_path, "polygon", true).unwrap();

        assert_eq!(summary.deposits.0, 1);
        assert!(summary.block_cursors.iter().any(|(c, b)| c == "polygon" && *b == 42));

        let db = Db::new(sqlite_path.to_str().unwrap()).unwrap();
        assert_eq!(db.get_last_processed_block("polygon").unwrap(), 42);
        assert!(db.get_detected_deposits("polygon").unwrap().is_empty());
        assert!(db.get_detected_erc20_deposits("polygon").unwrap().is_empty());

        let meta = db.get_sweep_meta("polygon", "0xswept:1").unwrap();
        assert_eq!(meta, Some(("0xsweep_tx".to_string(), 3)));
        assert_eq!(
            db.increment_zero_balance_count("polygon", "0xswept:1")
                .unwrap(),
            4
        );
    }
}
