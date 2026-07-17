//! SQLite -> Postgres data migration (evmhot Postgres storage migration).
//!
//! Mirrors the existing [`crate::redb_import`] precedent: read the entire
//! source database into an in-memory snapshot, then write it into the
//! destination inside one transaction (all-or-nothing — a crash mid-migration
//! leaves the destination untouched, so the next attempt starts clean).
//!
//! Used from two places:
//!   1. [`crate::db::postgres::PostgresBackend`]'s automatic bootstrap
//!      migration at service startup (triggered by `EVM_MIGRATE_FROM_SQLITE`
//!      + an empty destination).
//!   2. The standalone `migrate_sqlite_to_postgres` binary, for laptop-driven
//!      dry runs against a real production SQLite snapshot.
use anyhow::{bail, Context, Result};
use postgres::Client;
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

/// Single source of truth for the Postgres schema, shared by
/// `db::postgres::PostgresBackend` (production/service boot) and this
/// module's callers (the standalone migration binary, which needs to
/// prepare an empty scratch destination for dry runs).
pub const POSTGRES_SCHEMA_SQL: &str = include_str!("db/postgres_schema.sql");

/// Idempotent: safe to call against an already-bootstrapped database.
pub fn ensure_postgres_schema(client: &mut Client) -> Result<()> {
    client
        .batch_execute(POSTGRES_SCHEMA_SQL)
        .context("failed to bootstrap Postgres schema (DDL-capable credentials required)")?;
    Ok(())
}

#[derive(Debug, Default, Clone)]
pub struct AccountRow {
    pub id: String,
    pub derivation_index: i64,
    pub address: String,
    pub webhook_url: String,
}

#[derive(Debug, Default, Clone)]
pub struct DepositRow {
    pub chain: String,
    pub tx_hash: String,
    pub account_id: String,
    pub amount: String,
    pub status: String,
}

#[derive(Debug, Default, Clone)]
pub struct Erc20DepositRow {
    pub chain: String,
    pub tx_hash: String,
    pub log_index: i64,
    pub account_id: String,
    pub amount: String,
    pub token_address: String,
    pub token_symbol: String,
    pub status: String,
}

#[derive(Debug, Default, Clone)]
pub struct TokenMetadataRow {
    pub chain: String,
    pub token_address: String,
    pub symbol: String,
    pub decimals: i64,
    pub name: String,
}

#[derive(Debug, Default, Clone)]
pub struct SweepMetaRow {
    pub chain: String,
    pub tx_hash: String,
    pub log_index: i64,
    pub sweep_tx_hash: String,
    pub zero_balance_retry_count: i64,
}

#[derive(Debug, Default, Clone)]
pub struct SweepFailureRow {
    pub chain: String,
    pub tx_hash: String,
    pub log_index: i64,
    pub consecutive_failure_count: i64,
}

#[derive(Debug, Default, Clone)]
pub struct WebhookDeliveryRow {
    pub id: String,
    pub event: String,
    pub registration_id: String,
    pub webhook_url: String,
    pub payload: String,
    pub status: String,
    pub attempt_count: i64,
    pub last_http_status: Option<i64>,
    pub last_error: Option<String>,
    pub leased_until: Option<i64>,
    pub updated_at: i64,
}

/// Full in-memory copy of a source SQLite database. Read once, up front, so
/// the destination-side transaction never blocks on the (potentially
/// slow/EFS-mounted) SQLite source.
#[derive(Debug, Default, Clone)]
pub struct SqliteSnapshot {
    pub accounts: Vec<AccountRow>,
    pub deposits: Vec<DepositRow>,
    pub erc20_deposits: Vec<Erc20DepositRow>,
    pub token_metadata: Vec<TokenMetadataRow>,
    pub sweep_meta: Vec<SweepMetaRow>,
    pub sweep_failures: Vec<SweepFailureRow>,
    pub webhook_deliveries: Vec<WebhookDeliveryRow>,
    /// Raw `state` table key/value pairs, including `next_index` and every
    /// `last_block:<chain>` cursor.
    pub state: Vec<(String, String)>,
}

impl SqliteSnapshot {
    pub fn next_index(&self) -> Option<i64> {
        self.state
            .iter()
            .find(|(k, _)| k == "next_index")
            .and_then(|(_, v)| v.parse().ok())
    }

    pub fn block_cursors(&self) -> Vec<(String, i64)> {
        self.state
            .iter()
            .filter_map(|(k, v)| {
                k.strip_prefix("last_block:")
                    .map(|chain| (chain.to_string(), v.parse().unwrap_or(0)))
            })
            .collect()
    }
}

/// Opens `path` read-only (never mutates the source — critical for both the
/// production bootstrap migration, which runs against the live EFS file, and
/// laptop dry runs against a copied production snapshot) and reads every
/// table into memory.
pub fn read_sqlite_snapshot(path: &Path) -> Result<SqliteSnapshot> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("failed to open SQLite source read-only: {}", path.display()))?;

    let mut snapshot = SqliteSnapshot::default();

    let mut stmt = conn.prepare("SELECT id, derivation_index, address, webhook_url FROM accounts")?;
    let rows = stmt.query_map([], |r| {
        Ok(AccountRow {
            id: r.get(0)?,
            derivation_index: r.get(1)?,
            address: r.get(2)?,
            webhook_url: r.get(3)?,
        })
    })?;
    snapshot.accounts = rows.collect::<rusqlite::Result<Vec<_>>>()?;

    let mut stmt = conn.prepare("SELECT chain, tx_hash, account_id, amount, status FROM deposits")?;
    let rows = stmt.query_map([], |r| {
        Ok(DepositRow {
            chain: r.get(0)?,
            tx_hash: r.get(1)?,
            account_id: r.get(2)?,
            amount: r.get(3)?,
            status: r.get(4)?,
        })
    })?;
    snapshot.deposits = rows.collect::<rusqlite::Result<Vec<_>>>()?;

    let mut stmt = conn.prepare(
        "SELECT chain, tx_hash, log_index, account_id, amount, token_address, token_symbol, status
         FROM erc20_deposits",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Erc20DepositRow {
            chain: r.get(0)?,
            tx_hash: r.get(1)?,
            log_index: r.get(2)?,
            account_id: r.get(3)?,
            amount: r.get(4)?,
            token_address: r.get(5)?,
            token_symbol: r.get(6)?,
            status: r.get(7)?,
        })
    })?;
    snapshot.erc20_deposits = rows.collect::<rusqlite::Result<Vec<_>>>()?;

    let mut stmt =
        conn.prepare("SELECT chain, token_address, symbol, decimals, name FROM token_metadata")?;
    let rows = stmt.query_map([], |r| {
        Ok(TokenMetadataRow {
            chain: r.get(0)?,
            token_address: r.get(1)?,
            symbol: r.get(2)?,
            decimals: r.get(3)?,
            name: r.get(4)?,
        })
    })?;
    snapshot.token_metadata = rows.collect::<rusqlite::Result<Vec<_>>>()?;

    let mut stmt = conn.prepare(
        "SELECT chain, tx_hash, log_index, sweep_tx_hash, zero_balance_retry_count FROM sweep_meta",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(SweepMetaRow {
            chain: r.get(0)?,
            tx_hash: r.get(1)?,
            log_index: r.get(2)?,
            sweep_tx_hash: r.get(3)?,
            zero_balance_retry_count: r.get(4)?,
        })
    })?;
    snapshot.sweep_meta = rows.collect::<rusqlite::Result<Vec<_>>>()?;

    let mut stmt = conn
        .prepare("SELECT chain, tx_hash, log_index, consecutive_failure_count FROM sweep_failures")?;
    let rows = stmt.query_map([], |r| {
        Ok(SweepFailureRow {
            chain: r.get(0)?,
            tx_hash: r.get(1)?,
            log_index: r.get(2)?,
            consecutive_failure_count: r.get(3)?,
        })
    })?;
    snapshot.sweep_failures = rows.collect::<rusqlite::Result<Vec<_>>>()?;

    let mut stmt = conn.prepare(
        "SELECT id, event, registration_id, webhook_url, payload, status, attempt_count,
                last_http_status, last_error, leased_until, updated_at
         FROM webhook_deliveries",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(WebhookDeliveryRow {
            id: r.get(0)?,
            event: r.get(1)?,
            registration_id: r.get(2)?,
            webhook_url: r.get(3)?,
            payload: r.get(4)?,
            status: r.get(5)?,
            attempt_count: r.get(6)?,
            last_http_status: r.get(7)?,
            last_error: r.get(8)?,
            leased_until: r.get(9)?,
            updated_at: r.get(10)?,
        })
    })?;
    snapshot.webhook_deliveries = rows.collect::<rusqlite::Result<Vec<_>>>()?;

    let mut stmt = conn.prepare("SELECT key, value FROM state")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    snapshot.state = rows.collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(snapshot)
}

/// Per-table (source_count, inserted_count) migration summary, mirroring
/// `redb_import::ImportSummary`'s shape.
#[derive(Debug, Default, Clone)]
pub struct MigrationSummary {
    pub accounts: (usize, usize),
    pub deposits: (usize, usize),
    pub erc20_deposits: (usize, usize),
    pub token_metadata: (usize, usize),
    pub sweep_meta: (usize, usize),
    pub sweep_failures: (usize, usize),
    pub webhook_deliveries: (usize, usize),
    pub next_index: Option<i64>,
    pub block_cursors: Vec<(String, i64)>,
}

/// Convenience wrapper: read the source file, then migrate.
pub fn migrate_sqlite_file_to_postgres(
    sqlite_path: &str,
    client: &mut Client,
) -> Result<MigrationSummary> {
    let snapshot = read_sqlite_snapshot(Path::new(sqlite_path))?;
    migrate_snapshot_to_postgres(&snapshot, client)
}

/// Empty-source guard (critical-gap fix from eng review): refuses to migrate
/// a source that looks freshly-seeded/empty, which would otherwise silently
/// leave Postgres with zero accounts and let the service re-issue
/// derivation indices from scratch — a duplicate-deposit-address
/// catastrophe, not just a data-loss one.
fn assert_source_is_non_empty(snapshot: &SqliteSnapshot) -> Result<()> {
    if snapshot.accounts.is_empty() {
        bail!(
            "refusing to migrate: source SQLite file has zero accounts \
             (this looks like an empty/freshly-seeded database, not a real \
             production snapshot — migrating it would let the service \
             re-issue derivation indices from index 0, producing duplicate \
             deposit addresses)"
        );
    }
    if snapshot.next_index().is_none() {
        bail!(
            "refusing to migrate: source SQLite file has accounts but no \
             'next_index' row in `state` (unexpected — every V3+ schema \
             seeds this row; check the source file is the real wallet.db, \
             not a partial/corrupted copy)"
        );
    }
    Ok(())
}

/// Migrates `snapshot` into `client` inside one transaction: either every
/// row lands, or (on any error) none do and the caller's next attempt starts
/// from an empty Postgres again. `ON CONFLICT DO NOTHING`/`DO UPDATE` on every
/// insert makes a full re-run idempotent (used by both the bootstrap
/// migration's empty-destination guard and by re-running the standalone
/// binary against a partially-migrated scratch database during a dry run).
pub fn migrate_snapshot_to_postgres(
    snapshot: &SqliteSnapshot,
    client: &mut Client,
) -> Result<MigrationSummary> {
    assert_source_is_non_empty(snapshot)?;

    let mut summary = MigrationSummary::default();
    let mut tx = client.transaction()?;

    summary.accounts.0 = snapshot.accounts.len();
    for row in &snapshot.accounts {
        let changes = tx.execute(
            "INSERT INTO accounts (id, derivation_index, address, webhook_url)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO NOTHING",
            &[&row.id, &row.derivation_index, &row.address, &row.webhook_url],
        )?;
        summary.accounts.1 += changes as usize;
    }

    summary.deposits.0 = snapshot.deposits.len();
    for row in &snapshot.deposits {
        let changes = tx.execute(
            "INSERT INTO deposits (chain, tx_hash, account_id, amount, status)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (chain, tx_hash) DO NOTHING",
            &[&row.chain, &row.tx_hash, &row.account_id, &row.amount, &row.status],
        )?;
        summary.deposits.1 += changes as usize;
    }

    summary.erc20_deposits.0 = snapshot.erc20_deposits.len();
    for row in &snapshot.erc20_deposits {
        let changes = tx.execute(
            "INSERT INTO erc20_deposits
             (chain, tx_hash, log_index, account_id, amount, token_address, token_symbol, status)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (chain, tx_hash, log_index) DO NOTHING",
            &[
                &row.chain,
                &row.tx_hash,
                &row.log_index,
                &row.account_id,
                &row.amount,
                &row.token_address,
                &row.token_symbol,
                &row.status,
            ],
        )?;
        summary.erc20_deposits.1 += changes as usize;
    }

    summary.token_metadata.0 = snapshot.token_metadata.len();
    for row in &snapshot.token_metadata {
        let changes = tx.execute(
            "INSERT INTO token_metadata (chain, token_address, symbol, decimals, name)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (chain, token_address) DO NOTHING",
            &[
                &row.chain,
                &row.token_address,
                &row.symbol,
                &(row.decimals as i16),
                &row.name,
            ],
        )?;
        summary.token_metadata.1 += changes as usize;
    }

    summary.sweep_meta.0 = snapshot.sweep_meta.len();
    for row in &snapshot.sweep_meta {
        let changes = tx.execute(
            "INSERT INTO sweep_meta
             (chain, tx_hash, log_index, sweep_tx_hash, zero_balance_retry_count)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (chain, tx_hash, log_index) DO NOTHING",
            &[
                &row.chain,
                &row.tx_hash,
                &row.log_index,
                &row.sweep_tx_hash,
                &row.zero_balance_retry_count,
            ],
        )?;
        summary.sweep_meta.1 += changes as usize;
    }

    summary.sweep_failures.0 = snapshot.sweep_failures.len();
    for row in &snapshot.sweep_failures {
        let changes = tx.execute(
            "INSERT INTO sweep_failures (chain, tx_hash, log_index, consecutive_failure_count)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (chain, tx_hash, log_index) DO NOTHING",
            &[&row.chain, &row.tx_hash, &row.log_index, &row.consecutive_failure_count],
        )?;
        summary.sweep_failures.1 += changes as usize;
    }

    summary.webhook_deliveries.0 = snapshot.webhook_deliveries.len();
    for row in &snapshot.webhook_deliveries {
        // `last_http_status` is `webhook_deliveries.last_http_status INTEGER`
        // (int4) in the Postgres schema -- matches `PostgresBackend`'s own
        // reads/writes of this column (`Option<i32>`) -- but SQLite has no
        // fixed-width integer types, so `read_sqlite_snapshot` reads it as
        // `Option<i64>`. HTTP status codes always fit in i32; cast at the
        // insert boundary rather than widening the column to BIGINT.
        let last_http_status = row
            .last_http_status
            .map(i32::try_from)
            .transpose()
            .with_context(|| {
                format!(
                    "webhook_deliveries[{}:{}].last_http_status={:?} does not fit in i32",
                    row.id, row.event, row.last_http_status
                )
            })?;
        let changes = tx.execute(
            "INSERT INTO webhook_deliveries
             (id, event, registration_id, webhook_url, payload, status, attempt_count,
              last_http_status, last_error, leased_until, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             ON CONFLICT (id, event) DO NOTHING",
            &[
                &row.id,
                &row.event,
                &row.registration_id,
                &row.webhook_url,
                &row.payload,
                &row.status,
                &row.attempt_count,
                &last_http_status,
                &row.last_error,
                &row.leased_until,
                &row.updated_at,
            ],
        )?;
        summary.webhook_deliveries.1 += changes as usize;
    }

    // `state` last: includes `next_index` and every `last_block:<chain>`
    // cursor. ON CONFLICT DO NOTHING (not DO UPDATE) so re-running this
    // migration against a partially-populated destination never clobbers
    // writes Postgres has already accepted since a prior partial run.
    for (key, value) in &snapshot.state {
        tx.execute(
            "INSERT INTO state (key, value) VALUES ($1, $2) ON CONFLICT (key) DO NOTHING",
            &[key, value],
        )?;
    }
    summary.next_index = snapshot.next_index();
    summary.block_cursors = snapshot.block_cursors();

    tx.commit()?;
    Ok(summary)
}

/// One field that didn't match between source and destination during
/// `--verify`.
#[derive(Debug, Clone)]
pub struct VerifyMismatch {
    pub field: String,
    pub source: String,
    pub destination: String,
}

#[derive(Debug, Default)]
pub struct VerifyReport {
    pub mismatches: Vec<VerifyMismatch>,
}

impl VerifyReport {
    pub fn is_match(&self) -> bool {
        self.mismatches.is_empty()
    }

    fn check<T: std::fmt::Debug + PartialEq>(&mut self, field: &str, source: T, destination: T) {
        if source != destination {
            self.mismatches.push(VerifyMismatch {
                field: field.to_string(),
                source: format!("{source:?}"),
                destination: format!("{destination:?}"),
            });
        }
    }
}

/// Read-only source-vs-destination comparison: per-table counts,
/// `next_index`, every `last_block:<chain>` cursor, plus spot-checks of a
/// handful of account addresses and deposit statuses. Used both after a
/// laptop dry run and after the production bootstrap migration (via the
/// binary's `--verify` mode).
pub fn verify_migration(snapshot: &SqliteSnapshot, client: &mut Client) -> Result<VerifyReport> {
    let mut report = VerifyReport::default();

    let pg_count = |client: &mut Client, table: &str| -> Result<i64> {
        Ok(client
            .query_one(&format!("SELECT COUNT(*) FROM {table}"), &[])?
            .get(0))
    };

    report.check(
        "accounts.count",
        snapshot.accounts.len() as i64,
        pg_count(client, "accounts")?,
    );
    report.check(
        "deposits.count",
        snapshot.deposits.len() as i64,
        pg_count(client, "deposits")?,
    );
    report.check(
        "erc20_deposits.count",
        snapshot.erc20_deposits.len() as i64,
        pg_count(client, "erc20_deposits")?,
    );
    report.check(
        "token_metadata.count",
        snapshot.token_metadata.len() as i64,
        pg_count(client, "token_metadata")?,
    );
    report.check(
        "sweep_meta.count",
        snapshot.sweep_meta.len() as i64,
        pg_count(client, "sweep_meta")?,
    );
    report.check(
        "sweep_failures.count",
        snapshot.sweep_failures.len() as i64,
        pg_count(client, "sweep_failures")?,
    );
    report.check(
        "webhook_deliveries.count",
        snapshot.webhook_deliveries.len() as i64,
        pg_count(client, "webhook_deliveries")?,
    );

    if let Some(expected) = snapshot.next_index() {
        let actual: Option<String> = client
            .query_opt("SELECT value FROM state WHERE key = 'next_index'", &[])?
            .map(|r| r.get(0));
        let actual = actual.and_then(|v| v.parse::<i64>().ok());
        report.check("state.next_index", Some(expected), actual);
    }

    for (chain, expected_block) in snapshot.block_cursors() {
        let key = format!("last_block:{chain}");
        let actual: Option<String> = client
            .query_opt("SELECT value FROM state WHERE key = $1", &[&key])?
            .map(|r| r.get(0));
        let actual = actual.and_then(|v| v.parse::<i64>().ok());
        report.check(&format!("state.last_block:{chain}"), Some(expected_block), actual);
    }

    // Spot-check up to 25 account addresses and their derivation indices —
    // proof the migrated data is not just the right *count*, but the right
    // *rows*.
    for row in snapshot.accounts.iter().take(25) {
        let actual: Option<(i64, String, String)> = client
            .query_opt(
                "SELECT derivation_index, address, webhook_url FROM accounts WHERE id = $1",
                &[&row.id],
            )?
            .map(|r| (r.get(0), r.get(1), r.get(2)));
        report.check(
            &format!("accounts[{}]", row.id),
            Some((row.derivation_index, row.address.clone(), row.webhook_url.clone())),
            actual,
        );
    }

    // Spot-check up to 25 deposit statuses.
    for row in snapshot.deposits.iter().take(25) {
        let actual: Option<String> = client
            .query_opt(
                "SELECT status FROM deposits WHERE chain = $1 AND tx_hash = $2",
                &[&row.chain, &row.tx_hash],
            )?
            .map(|r| r.get(0));
        report.check(
            &format!("deposits[{}:{}].status", row.chain, row.tx_hash),
            Some(row.status.clone()),
            actual,
        );
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection as RusqliteConnection;
    use tempfile::NamedTempFile;

    fn seed_sqlite_file() -> NamedTempFile {
        let tmp = NamedTempFile::new().unwrap();
        let mut conn = RusqliteConnection::open(tmp.path()).unwrap();
        crate::db::apply_pragmas_for_import(&conn).unwrap();
        crate::db::migrations().to_latest(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO accounts (id, derivation_index, address, webhook_url)
             VALUES ('u1', 0, '0x1', 'https://example.com')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO deposits (chain, tx_hash, account_id, amount, status)
             VALUES ('base', '0xabc', 'u1', '100', 'detected')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO state (key, value) VALUES ('last_block:base', '42')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE state SET value = '1' WHERE key = 'next_index'",
            [],
        )
        .unwrap();
        drop(conn);
        tmp
    }

    #[test]
    fn test_read_sqlite_snapshot_reads_all_tables() {
        let tmp = seed_sqlite_file();
        let snapshot = read_sqlite_snapshot(tmp.path()).unwrap();
        assert_eq!(snapshot.accounts.len(), 1);
        assert_eq!(snapshot.deposits.len(), 1);
        assert_eq!(snapshot.next_index(), Some(1));
        assert_eq!(
            snapshot.block_cursors(),
            vec![("base".to_string(), 42)]
        );
    }

    #[test]
    fn test_empty_source_guard_rejects_zero_accounts() {
        let snapshot = SqliteSnapshot::default();
        let err = assert_source_is_non_empty(&snapshot).unwrap_err();
        assert!(err.to_string().contains("zero accounts"));
    }

    #[test]
    fn test_empty_source_guard_rejects_missing_next_index() {
        let mut snapshot = SqliteSnapshot::default();
        snapshot.accounts.push(AccountRow {
            id: "u1".to_string(),
            derivation_index: 0,
            address: "0x1".to_string(),
            webhook_url: "https://example.com".to_string(),
        });
        let err = assert_source_is_non_empty(&snapshot).unwrap_err();
        assert!(err.to_string().contains("next_index"));
    }

    #[test]
    fn test_seed_sqlite_file_helper_has_expected_derivation_index() {
        let tmp = seed_sqlite_file();
        let snapshot = read_sqlite_snapshot(tmp.path()).unwrap();
        assert_eq!(snapshot.accounts[0].derivation_index, 0);
    }
}
