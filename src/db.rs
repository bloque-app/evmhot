use anyhow::{anyhow, Result};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, Connection, OptionalExtension};
use rusqlite_migration::{Migrations, M};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DepositQueueCounts {
    pub native_detected: u64,
    pub native_failed: u64,
    pub erc20_detected: u64,
    pub erc20_failed: u64,
}

impl DepositQueueCounts {
    pub fn has_pending(&self) -> bool {
        self.native_detected > 0
            || self.native_failed > 0
            || self.erc20_detected > 0
            || self.erc20_failed > 0
    }
}

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
    write: Arc<Mutex<Connection>>,
    read: Pool<SqliteConnectionManager>,
}

/// Strip `sqlite:` scheme; rusqlite expects a filesystem path.
pub fn normalize_db_path(database_url: &str) -> &str {
    database_url.strip_prefix("sqlite:").unwrap_or(database_url)
}

pub fn migrations() -> Migrations<'static> {
    Migrations::new(vec![M::up(include_str!("../migrations/V1__initial.sql"))])
}

fn apply_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA busy_timeout=5000;",
    )?;
    Ok(())
}

pub fn apply_pragmas_for_import(conn: &Connection) -> Result<()> {
    apply_pragmas(conn).map_err(Into::into)
}

/// Parse `"0xtx:42"` -> (`0xtx`, 42). Bare `"0xtx"` -> (`0xtx`, 0).
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

impl Db {
    pub fn new(database_url: &str) -> Result<Self> {
        let path = normalize_db_path(database_url);
        let mut write_conn = Connection::open(path)?;
        apply_pragmas(&write_conn)?;
        migrations().to_latest(&mut write_conn)?;

        let manager = SqliteConnectionManager::file(path).with_init(|c| apply_pragmas(&*c));
        let read_pool = Pool::builder().build(manager)?;

        Ok(Self {
            write: Arc::new(Mutex::new(write_conn)),
            read: read_pool,
        })
    }

    fn with_write<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T>,
    {
        let conn = self
            .write
            .lock()
            .map_err(|_| anyhow!("database write lock poisoned"))?;
        f(&conn)
    }

    #[allow(dead_code)]
    pub fn get_next_derivation_index(&self) -> Result<u32> {
        let conn = self.read.get()?;
        let idx: u32 = conn.query_row(
            "SELECT COALESCE(MAX(derivation_index) + 1, 0) FROM accounts",
            [],
            |row| row.get(0),
        )?;
        Ok(idx)
    }

    pub fn register_account(
        &self,
        id: &str,
        index: u32,
        address: &str,
        webhook_url: &str,
    ) -> Result<()> {
        self.with_write(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO accounts (id, derivation_index, address, webhook_url)
                 VALUES (?1, ?2, ?3, ?4)",
                params![id, index, address, webhook_url],
            )?;
            Ok(())
        })
    }

    pub fn get_registration_id_by_address(&self, address: &str) -> Result<Option<String>> {
        let conn = self.read.get()?;
        conn.query_row(
            "SELECT id FROM accounts WHERE address = ?1",
            [address],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn get_account_by_address(&self, address: &str) -> Result<Option<String>> {
        self.get_registration_id_by_address(address)
    }

    pub fn get_account_by_id(&self, id: &str) -> Result<Option<(u32, String, String)>> {
        let conn = self.read.get()?;
        conn.query_row(
            "SELECT derivation_index, address, webhook_url FROM accounts WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn get_webhook_url(&self, account_id: &str) -> Result<Option<String>> {
        let conn = self.read.get()?;
        conn.query_row(
            "SELECT webhook_url FROM accounts WHERE id = ?1",
            [account_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn record_deposit(
        &self,
        chain: &str,
        tx_hash: &str,
        account_id: &str,
        amount: &str,
    ) -> Result<bool> {
        self.with_write(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO deposits (chain, tx_hash, account_id, amount, status)
                 VALUES (?1, ?2, ?3, ?4, 'detected')",
                params![chain, tx_hash, account_id, amount],
            )?;
            Ok(conn.changes() == 1)
        })
    }

    pub fn mark_deposit_swept(&self, chain: &str, tx_hash: &str) -> Result<()> {
        self.with_write(|conn| {
            conn.execute(
                "UPDATE deposits SET status = 'swept' WHERE chain = ?1 AND tx_hash = ?2",
                params![chain, tx_hash],
            )?;
            Ok(())
        })
    }

    pub fn mark_deposit_failed(&self, chain: &str, tx_hash: &str) -> Result<()> {
        self.with_write(|conn| {
            conn.execute(
                "UPDATE deposits SET status = 'failed' WHERE chain = ?1 AND tx_hash = ?2",
                params![chain, tx_hash],
            )?;
            Ok(())
        })
    }

    pub fn get_detected_deposits(&self, chain: &str) -> Result<Vec<(String, String, String)>> {
        let conn = self.read.get()?;
        let mut stmt = conn.prepare(
            "SELECT tx_hash, account_id, amount FROM deposits
             WHERE chain = ?1 AND status = 'detected'",
        )?;
        let rows = stmt.query_map([chain], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn get_last_processed_block(&self, chain: &str) -> Result<u64> {
        let conn = self.read.get()?;
        let key = last_block_key(chain);
        let val: Option<String> = conn
            .query_row("SELECT value FROM state WHERE key = ?1", [key], |row| row.get(0))
            .optional()?;
        Ok(val.map(|v| v.parse().unwrap_or(0)).unwrap_or(0))
    }

    pub fn set_last_processed_block(&self, chain: &str, block: u64) -> Result<()> {
        let key = last_block_key(chain);
        let block_str = block.to_string();
        self.with_write(|conn| {
            conn.execute(
                "INSERT INTO state (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, block_str],
            )?;
            Ok(())
        })
    }

    pub fn store_token_metadata(
        &self,
        chain: &str,
        address: &str,
        symbol: &str,
        decimals: u8,
        name: &str,
    ) -> Result<()> {
        self.with_write(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO token_metadata
                 (chain, token_address, symbol, decimals, name)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![chain, address, symbol, decimals, name],
            )?;
            Ok(())
        })
    }

    pub fn get_token_metadata(
        &self,
        chain: &str,
        address: &str,
    ) -> Result<Option<(String, u8, String)>> {
        let conn = self.read.get()?;
        conn.query_row(
            "SELECT symbol, decimals, name FROM token_metadata
             WHERE chain = ?1 AND token_address = ?2",
            params![chain, address],
            |row| Ok((row.get(0)?, row.get::<_, u8>(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(Into::into)
    }

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
        self.with_write(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO erc20_deposits
                 (chain, tx_hash, log_index, account_id, amount, token_address, token_symbol, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'detected')",
                params![
                    chain,
                    tx_hash,
                    log_index as i64,
                    account_id,
                    amount,
                    token_address,
                    token_symbol
                ],
            )?;
            Ok(conn.changes() == 1)
        })
    }

    pub fn get_detected_erc20_deposits(&self, chain: &str) -> Result<Vec<Erc20Deposit>> {
        let conn = self.read.get()?;
        let mut stmt = conn.prepare(
            "SELECT tx_hash, log_index, account_id, amount, token_address, token_symbol
             FROM erc20_deposits WHERE chain = ?1 AND status = 'detected'",
        )?;
        let rows = stmt.query_map([chain], |row| {
            let tx_hash: String = row.get(0)?;
            let log_index: i64 = row.get(1)?;
            Ok(Erc20Deposit {
                key: format!("{tx_hash}:{log_index}"),
                account_id: row.get(2)?,
                amount: row.get(3)?,
                token_address: row.get(4)?,
                token_symbol: row.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn mark_erc20_deposit_swept(&self, chain: &str, local_key: &str) -> Result<()> {
        let (tx_hash, log_index) = parse_local_key(local_key)?;
        self.with_write(|conn| {
            conn.execute(
                "UPDATE erc20_deposits SET status = 'swept'
                 WHERE chain = ?1 AND tx_hash = ?2 AND log_index = ?3",
                params![chain, tx_hash, log_index],
            )?;
            Ok(())
        })
    }

    pub fn mark_erc20_deposits_swept_for_account_token(
        &self,
        chain: &str,
        account_id: &str,
        token_address: &str,
    ) -> Result<Vec<(String, String)>> {
        self.with_write(|conn| {
            let mut stmt = conn.prepare(
                "UPDATE erc20_deposits SET status = 'swept'
                 WHERE chain = ?1 AND account_id = ?2 AND token_address = ?3 AND status = 'detected'
                 RETURNING tx_hash, log_index, amount",
            )?;
            let rows = stmt.query_map(params![chain, account_id, token_address], |row| {
                let tx_hash: String = row.get(0)?;
                let log_index: i64 = row.get(1)?;
                let amount: String = row.get(2)?;
                Ok((format!("{tx_hash}:{log_index}"), amount))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(Into::into)
        })
    }

    pub fn increment_zero_balance_count(&self, chain: &str, local_key: &str) -> Result<u64> {
        let (tx_hash, log_index) = parse_local_key(local_key)?;
        self.with_write(|conn| {
            conn.execute(
                "INSERT INTO sweep_meta (chain, tx_hash, log_index, sweep_tx_hash, zero_balance_retry_count)
                 VALUES (?1, ?2, ?3, '', 1)
                 ON CONFLICT(chain, tx_hash, log_index) DO UPDATE SET
                   zero_balance_retry_count = zero_balance_retry_count + 1",
                params![chain, tx_hash, log_index],
            )?;
            let count: i64 = conn.query_row(
                "SELECT zero_balance_retry_count FROM sweep_meta
                 WHERE chain = ?1 AND tx_hash = ?2 AND log_index = ?3",
                params![chain, tx_hash, log_index],
                |row| row.get(0),
            )?;
            Ok(count as u64)
        })
    }

    #[allow(dead_code)]
    pub fn set_sweep_tx_hash(&self, chain: &str, local_key: &str, tx_hash: &str) -> Result<()> {
        let (deposit_tx, log_index) = parse_local_key(local_key)?;
        self.with_write(|conn| {
            conn.execute(
                "INSERT INTO sweep_meta (chain, tx_hash, log_index, sweep_tx_hash, zero_balance_retry_count)
                 VALUES (?1, ?2, ?3, ?4, 0)
                 ON CONFLICT(chain, tx_hash, log_index) DO UPDATE SET sweep_tx_hash = excluded.sweep_tx_hash",
                params![chain, deposit_tx, log_index, tx_hash],
            )?;
            Ok(())
        })
    }

    pub fn set_sweep_tx_hash_for_keys(
        &self,
        chain: &str,
        local_keys: &[String],
        tx_hash: &str,
    ) -> Result<()> {
        self.with_write(|conn| {
            for local_key in local_keys {
                let (deposit_tx, log_index) = parse_local_key(local_key)?;
                let existing: i64 = conn
                    .query_row(
                        "SELECT zero_balance_retry_count FROM sweep_meta
                         WHERE chain = ?1 AND tx_hash = ?2 AND log_index = ?3",
                        params![chain, deposit_tx, log_index],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);
                conn.execute(
                    "INSERT INTO sweep_meta (chain, tx_hash, log_index, sweep_tx_hash, zero_balance_retry_count)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(chain, tx_hash, log_index) DO UPDATE SET sweep_tx_hash = excluded.sweep_tx_hash",
                    params![chain, deposit_tx, log_index, tx_hash, existing],
                )?;
            }
            Ok(())
        })
    }

    #[allow(dead_code)]
    pub fn get_sweep_meta(&self, chain: &str, local_key: &str) -> Result<Option<(String, u64)>> {
        let (tx_hash, log_index) = parse_local_key(local_key)?;
        let conn = self.read.get()?;
        conn.query_row(
            "SELECT sweep_tx_hash, zero_balance_retry_count FROM sweep_meta
             WHERE chain = ?1 AND tx_hash = ?2 AND log_index = ?3",
            params![chain, tx_hash, log_index],
            |row| {
                let count: i64 = row.get(1)?;
                Ok((row.get(0)?, count as u64))
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn increment_sweep_failure_count(&self, chain: &str, local_key: &str) -> Result<u64> {
        let (tx_hash, log_index) = parse_local_key(local_key)?;
        self.with_write(|conn| {
            conn.execute(
                "INSERT INTO sweep_failures (chain, tx_hash, log_index, consecutive_failure_count)
                 VALUES (?1, ?2, ?3, 1)
                 ON CONFLICT(chain, tx_hash, log_index) DO UPDATE SET
                   consecutive_failure_count = consecutive_failure_count + 1",
                params![chain, tx_hash, log_index],
            )?;
            let count: i64 = conn.query_row(
                "SELECT consecutive_failure_count FROM sweep_failures
                 WHERE chain = ?1 AND tx_hash = ?2 AND log_index = ?3",
                params![chain, tx_hash, log_index],
                |row| row.get(0),
            )?;
            Ok(count as u64)
        })
    }

    pub fn mark_erc20_deposit_failed(&self, chain: &str, local_key: &str) -> Result<()> {
        let (tx_hash, log_index) = parse_local_key(local_key)?;
        self.with_write(|conn| {
            conn.execute(
                "UPDATE erc20_deposits SET status = 'failed'
                 WHERE chain = ?1 AND tx_hash = ?2 AND log_index = ?3",
                params![chain, tx_hash, log_index],
            )?;
            Ok(())
        })
    }

    pub fn mark_erc20_deposits_failed_for_account_token(
        &self,
        chain: &str,
        account_id: &str,
        token_address: &str,
    ) -> Result<Vec<String>> {
        self.with_write(|conn| {
            let mut stmt = conn.prepare(
                "UPDATE erc20_deposits SET status = 'failed'
                 WHERE chain = ?1 AND account_id = ?2 AND token_address = ?3 AND status = 'detected'
                 RETURNING tx_hash, log_index",
            )?;
            let rows = stmt.query_map(params![chain, account_id, token_address], |row| {
                let tx_hash: String = row.get(0)?;
                let log_index: i64 = row.get(1)?;
                Ok(format!("{tx_hash}:{log_index}"))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(Into::into)
        })
    }

    pub fn deposit_queue_counts(&self, chain: &str) -> Result<DepositQueueCounts> {
        let conn = self.read.get()?;
        let native_detected: i64 = conn.query_row(
            "SELECT COUNT(*) FROM deposits WHERE chain = ?1 AND status = 'detected'",
            [chain],
            |row| row.get(0),
        )?;
        let native_failed: i64 = conn.query_row(
            "SELECT COUNT(*) FROM deposits WHERE chain = ?1 AND status = 'failed'",
            [chain],
            |row| row.get(0),
        )?;
        let erc20_detected: i64 = conn.query_row(
            "SELECT COUNT(*) FROM erc20_deposits WHERE chain = ?1 AND status = 'detected'",
            [chain],
            |row| row.get(0),
        )?;
        let erc20_failed: i64 = conn.query_row(
            "SELECT COUNT(*) FROM erc20_deposits WHERE chain = ?1 AND status = 'failed'",
            [chain],
            |row| row.get(0),
        )?;
        Ok(DepositQueueCounts {
            native_detected: native_detected as u64,
            native_failed: native_failed as u64,
            erc20_detected: erc20_detected as u64,
            erc20_failed: erc20_failed as u64,
        })
    }

    pub fn retry_native_deposit(&self, chain: &str, tx_hash: &str) -> Result<bool> {
        self.with_write(|conn| {
            conn.execute(
                "UPDATE deposits SET status = 'detected'
                 WHERE chain = ?1 AND tx_hash = ?2 AND status = 'failed'",
                params![chain, tx_hash],
            )?;
            Ok(conn.changes() == 1)
        })
    }

    pub fn retry_erc20_deposit(&self, chain: &str, tx_hash: &str, log_index: u64) -> Result<bool> {
        self.with_write(|conn| {
            conn.execute(
                "UPDATE erc20_deposits SET status = 'detected'
                 WHERE chain = ?1 AND tx_hash = ?2 AND log_index = ?3 AND status = 'failed'",
                params![chain, tx_hash, log_index as i64],
            )?;
            let updated = conn.changes() == 1;
            if updated {
                conn.execute(
                    "DELETE FROM sweep_failures
                     WHERE chain = ?1 AND tx_hash = ?2 AND log_index = ?3",
                    params![chain, tx_hash, log_index as i64],
                )?;
            }
            Ok(updated)
        })
    }

    pub fn get_sweep_failure_count(&self, chain: &str, local_key: &str) -> Result<u64> {
        let (tx_hash, log_index) = parse_local_key(local_key)?;
        let conn = self.read.get()?;
        let count: Option<i64> = conn
            .query_row(
                "SELECT consecutive_failure_count FROM sweep_failures
                 WHERE chain = ?1 AND tx_hash = ?2 AND log_index = ?3",
                params![chain, tx_hash, log_index],
                |row| row.get(0),
            )
            .optional()?;
        Ok(count.unwrap_or(0) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_chain_isolated_deposits() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Db::new(tmp.path().to_str().unwrap()).unwrap();

        db.record_deposit("base", "0xabc", "user1", "100").unwrap();
        db.record_deposit("polygon", "0xabc", "user2", "200")
            .unwrap();

        let base = db.get_detected_deposits("base").unwrap();
        let polygon = db.get_detected_deposits("polygon").unwrap();

        assert_eq!(base.len(), 1);
        assert_eq!(base[0].0, "0xabc");
        assert_eq!(base[0].2, "100");
        assert_eq!(polygon.len(), 1);
        assert_eq!(polygon[0].2, "200");
    }

    #[test]
    fn test_per_chain_last_block() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Db::new(tmp.path().to_str().unwrap()).unwrap();

        db.set_last_processed_block("base", 100).unwrap();
        db.set_last_processed_block("polygon", 200).unwrap();

        assert_eq!(db.get_last_processed_block("base").unwrap(), 100);
        assert_eq!(db.get_last_processed_block("polygon").unwrap(), 200);
    }

    #[test]
    fn test_record_deposit_duplicate_returns_false() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Db::new(tmp.path().to_str().unwrap()).unwrap();

        assert!(db
            .record_deposit("base", "0xabc", "user1", "100")
            .unwrap());
        assert!(!db
            .record_deposit("base", "0xabc", "user1", "100")
            .unwrap());
    }

    #[test]
    fn test_record_erc20_deposit_duplicate_returns_false() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Db::new(tmp.path().to_str().unwrap()).unwrap();

        assert!(db
            .record_erc20_deposit(
                "polygon",
                "0xabc",
                1,
                "user1",
                "100",
                "0xtoken",
                "USDC"
            )
            .unwrap());
        assert!(!db
            .record_erc20_deposit(
                "polygon",
                "0xabc",
                1,
                "user1",
                "100",
                "0xtoken",
                "USDC"
            )
            .unwrap());
    }

    #[test]
    fn test_increment_zero_balance_count_monotonic() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Db::new(tmp.path().to_str().unwrap()).unwrap();

        assert_eq!(
            db.increment_zero_balance_count("polygon", "0xabc:1")
                .unwrap(),
            1
        );
        assert_eq!(
            db.increment_zero_balance_count("polygon", "0xabc:1")
                .unwrap(),
            2
        );
        assert_eq!(
            db.increment_sweep_failure_count("polygon", "0xabc:1")
                .unwrap(),
            1
        );
    }

    #[test]
    fn test_db_new_idempotent_on_same_path() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap();

        let db1 = Db::new(path).unwrap();
        db1.register_account("u1", 0, "0x1", "https://example.com")
            .unwrap();

        let db2 = Db::new(path).unwrap();
        let acct = db2.get_account_by_id("u1").unwrap().unwrap();
        assert_eq!(acct.1, "0x1");
    }

    #[test]
    fn test_retry_erc20_deposit_resets_failed_status_and_clears_failures() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Db::new(tmp.path().to_str().unwrap()).unwrap();

        db.record_erc20_deposit(
            "base",
            "0xabc",
            120,
            "user1",
            "100",
            "0xtoken",
            "USDC",
        )
        .unwrap();
        db.mark_erc20_deposit_failed("base", "0xabc:120").unwrap();
        db.increment_sweep_failure_count("base", "0xabc:120")
            .unwrap();

        assert_eq!(db.get_detected_erc20_deposits("base").unwrap().len(), 0);
        assert_eq!(db.get_sweep_failure_count("base", "0xabc:120").unwrap(), 1);

        assert!(db.retry_erc20_deposit("base", "0xabc", 120).unwrap());
        assert_eq!(db.get_detected_erc20_deposits("base").unwrap().len(), 1);
        assert_eq!(db.get_sweep_failure_count("base", "0xabc:120").unwrap(), 0);
        assert!(!db.retry_erc20_deposit("base", "0xabc", 120).unwrap());
    }

    #[test]
    fn test_retry_native_deposit() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Db::new(tmp.path().to_str().unwrap()).unwrap();

        db.record_deposit("polygon", "0xabc", "user1", "100").unwrap();
        db.mark_deposit_failed("polygon", "0xabc").unwrap();

        assert!(db.retry_native_deposit("polygon", "0xabc").unwrap());
        assert_eq!(db.get_detected_deposits("polygon").unwrap().len(), 1);
    }

    #[test]
    fn test_deposit_queue_counts() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Db::new(tmp.path().to_str().unwrap()).unwrap();

        db.record_deposit("base", "0x1", "u1", "100").unwrap();
        db.record_erc20_deposit("base", "0x2", 1, "u1", "200", "0xt", "USDC")
            .unwrap();
        db.mark_erc20_deposit_failed("base", "0x2:1").unwrap();

        let counts = db.deposit_queue_counts("base").unwrap();
        assert_eq!(
            counts,
            DepositQueueCounts {
                native_detected: 1,
                native_failed: 0,
                erc20_detected: 0,
                erc20_failed: 1,
            }
        );
    }

    #[test]
    fn test_normalize_db_path_strips_sqlite_scheme() {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("wallet.db");
        let bare_str = bare.to_str().unwrap();

        let db_bare = Db::new(bare_str).unwrap();
        db_bare
            .register_account("u1", 0, "0x1", "https://example.com")
            .unwrap();

        let prefixed = format!("sqlite:{bare_str}");
        let db_prefixed = Db::new(&prefixed).unwrap();
        assert!(db_prefixed.get_account_by_id("u1").unwrap().is_some());
    }
}
