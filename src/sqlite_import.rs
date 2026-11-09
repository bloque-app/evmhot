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
//!   2. The standalone `migrate_sqlite_to_postgres` binary, for dry runs
//!      against a copy of the source database.
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
    /// Sender (REQ-WH-9). `None` for rows written before it was stored.
    pub from_address: Option<String>,
    /// Why it needs an operator (`needs_review` deposits).
    pub review_kind: Option<String>,
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
    /// Sender (REQ-WH-9). `None` for rows written before it was stored.
    pub from_address: Option<String>,
    /// Why it needs an operator (`needs_review` deposits).
    pub review_kind: Option<String>,
}

/// A sweep part's intent (sent or about to be).
#[derive(Debug, Default, Clone)]
pub struct SweepIntentRow {
    pub id: i64,
    pub chain: String,
    pub account_id: String,
    pub address: String,
    pub token_address: String,
    pub group_key: String,
    pub keys: String,
    pub part_index: i64,
    pub amount: String,
    pub target: String,
    pub state: String,
    pub tx_hash: Option<String>,
    pub nonce: Option<i64>,
    pub raw_tx: Option<String>,
    pub txs: String,
    pub passes: i64,
    pub gone_block: i64,
}

/// One sweep that settled a deposit (REQ-SW-10).
#[derive(Debug, Default, Clone)]
pub struct SweepRow {
    pub chain: String,
    pub tx_hash: String,
    pub log_index: i64,
    pub sweep_tx_hash: String,
    pub sweep_block_number: i64,
    pub sweep_block_hash: String,
    pub sweep_log_index: i64,
    pub sweep_amount: String,
    /// `None` for sweeps recorded before the target was stored.
    pub sweep_target: Option<String>,
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
/// slow, network-mounted) SQLite source.
#[derive(Debug, Default, Clone)]
pub struct SqliteSnapshot {
    pub accounts: Vec<AccountRow>,
    pub deposits: Vec<DepositRow>,
    pub erc20_deposits: Vec<Erc20DepositRow>,
    pub token_metadata: Vec<TokenMetadataRow>,
    pub sweep_meta: Vec<SweepMetaRow>,
    pub sweep_failures: Vec<SweepFailureRow>,
    pub webhook_deliveries: Vec<WebhookDeliveryRow>,
    pub sweeps: Vec<SweepRow>,
    pub sweep_intents: Vec<SweepIntentRow>,
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

/// Opens `path` read-only (it never mutates the source, which may be a live
/// store or a copy of one) and reads every table into memory.
pub fn read_sqlite_snapshot(path: &Path) -> Result<SqliteSnapshot> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("failed to open SQLite source read-only: {}", path.display()))?;

    let mut snapshot = SqliteSnapshot::default();

    let mut stmt =
        conn.prepare("SELECT id, derivation_index, address, webhook_url FROM accounts")?;
    let rows = stmt.query_map([], |r| {
        Ok(AccountRow {
            id: r.get(0)?,
            derivation_index: r.get(1)?,
            address: r.get(2)?,
            webhook_url: r.get(3)?,
        })
    })?;
    snapshot.accounts = rows.collect::<rusqlite::Result<Vec<_>>>()?;

    // A source written before F-002a has no `from_address` column.
    let from_expr = |table: &str| -> Result<&'static str> {
        Ok(if table_has_column(&conn, table, "from_address")? {
            "from_address"
        } else {
            "NULL"
        })
    };

    let mut stmt = conn.prepare(&format!(
        "SELECT chain, tx_hash, account_id, amount, status, {}, {} FROM deposits ORDER BY rowid",
        from_expr("deposits")?,
        if table_has_column(&conn, "deposits", "review_kind")? {
            "review_kind"
        } else {
            "NULL"
        }
    ))?;
    let rows = stmt.query_map([], |r| {
        Ok(DepositRow {
            chain: r.get(0)?,
            tx_hash: r.get(1)?,
            account_id: r.get(2)?,
            amount: r.get(3)?,
            status: r.get(4)?,
            from_address: r.get(5)?,
            review_kind: r.get(6)?,
        })
    })?;
    snapshot.deposits = rows.collect::<rusqlite::Result<Vec<_>>>()?;

    let mut stmt = conn.prepare(&format!(
        "SELECT chain, tx_hash, log_index, account_id, amount, token_address, token_symbol, status, {}, {}
         FROM erc20_deposits ORDER BY rowid",
        from_expr("erc20_deposits")?,
        if table_has_column(&conn, "erc20_deposits", "review_kind")? {
            "review_kind"
        } else {
            "NULL"
        }
    ))?;
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
            from_address: r.get(8)?,
            review_kind: r.get(9)?,
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

    let mut stmt = conn.prepare(
        "SELECT chain, tx_hash, log_index, consecutive_failure_count FROM sweep_failures",
    )?;
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

    if table_exists(&conn, "sweeps")? {
        let target_expr = if table_has_column(&conn, "sweeps", "sweep_target")? {
            "sweep_target"
        } else {
            "NULL"
        };
        let mut stmt = conn.prepare(&format!(
            "SELECT chain, tx_hash, log_index, sweep_tx_hash, sweep_block_number,
                    sweep_block_hash, sweep_log_index, sweep_amount, {target_expr}
             FROM sweeps ORDER BY seq"
        ))?;
        let rows = stmt.query_map([], |r| {
            Ok(SweepRow {
                chain: r.get(0)?,
                tx_hash: r.get(1)?,
                log_index: r.get(2)?,
                sweep_tx_hash: r.get(3)?,
                sweep_block_number: r.get(4)?,
                sweep_block_hash: r.get(5)?,
                sweep_log_index: r.get(6)?,
                sweep_amount: r.get(7)?,
                sweep_target: r.get(8)?,
            })
        })?;
        snapshot.sweeps = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    }

    if table_exists(&conn, "sweep_intents")? {
        let mut stmt = conn.prepare(
            "SELECT id, chain, account_id, token_address, group_key, keys, part_index, amount,
                    target, state, tx_hash, nonce, raw_tx, passes, address, txs, gone_block
             FROM sweep_intents ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SweepIntentRow {
                id: r.get(0)?,
                chain: r.get(1)?,
                account_id: r.get(2)?,
                token_address: r.get(3)?,
                group_key: r.get(4)?,
                keys: r.get(5)?,
                part_index: r.get(6)?,
                amount: r.get(7)?,
                target: r.get(8)?,
                state: r.get(9)?,
                tx_hash: r.get(10)?,
                nonce: r.get(11)?,
                raw_tx: r.get(12)?,
                passes: r.get(13)?,
                address: r.get(14)?,
                txs: r.get(15)?,
                gone_block: r.get(16)?,
            })
        })?;
        snapshot.sweep_intents = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    }

    let mut stmt = conn.prepare("SELECT key, value FROM state")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    snapshot.state = rows.collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(snapshot)
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |r| r.get::<_, i64>(0),
    )? > 0)
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
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
    pub sweeps: (usize, usize),
    pub sweep_intents: (usize, usize),
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
             (this looks like an empty/freshly-seeded database, not a copy \
             of a populated store — migrating it would let the service \
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
    assert_no_pending_intents(snapshot)?;
    migrate_after_checks(snapshot, client)
}

/// A sweep still in flight must be resolved before the store moves.
fn assert_no_pending_intents(snapshot: &SqliteSnapshot) -> Result<()> {
    if let Some(intent) = snapshot.sweep_intents.iter().find(|i| i.state == "pending") {
        bail!(
            "refusing to migrate: sweep intent {} ({}) is still pending (tx {}). A sweep in \
             flight must be resolved (let the sweeper settle it, or review it) before the store \
             moves, or the destination would not know to check it",
            intent.id,
            intent.chain,
            intent.tx_hash.as_deref().unwrap_or("not yet signed")
        );
    }
    Ok(())
}

fn migrate_after_checks(
    snapshot: &SqliteSnapshot,
    client: &mut Client,
) -> Result<MigrationSummary> {
    let mut summary = MigrationSummary::default();
    let mut tx = client.transaction()?;

    summary.accounts.0 = snapshot.accounts.len();
    for row in &snapshot.accounts {
        let changes = tx.execute(
            "INSERT INTO accounts (id, derivation_index, address, webhook_url)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO NOTHING",
            &[
                &row.id,
                &row.derivation_index,
                &row.address,
                &row.webhook_url,
            ],
        )?;
        summary.accounts.1 += changes as usize;
    }

    summary.deposits.0 = snapshot.deposits.len();
    for row in &snapshot.deposits {
        let changes = tx.execute(
            "INSERT INTO deposits (chain, tx_hash, account_id, amount, status, from_address, review_kind)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (chain, tx_hash) DO NOTHING",
            &[
                &row.chain,
                &row.tx_hash,
                &row.account_id,
                &row.amount,
                &row.status,
                &row.from_address,
                &row.review_kind,
            ],
        )?;
        summary.deposits.1 += changes as usize;
    }

    summary.erc20_deposits.0 = snapshot.erc20_deposits.len();
    for row in &snapshot.erc20_deposits {
        let changes = tx.execute(
            "INSERT INTO erc20_deposits
             (chain, tx_hash, log_index, account_id, amount, token_address, token_symbol, status,
              from_address, review_kind)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
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
                &row.from_address,
                &row.review_kind,
            ],
        )?;
        summary.erc20_deposits.1 += changes as usize;
    }

    summary.sweeps.0 = snapshot.sweeps.len();
    for row in &snapshot.sweeps {
        let changes = tx.execute(
            "INSERT INTO sweeps
             (chain, tx_hash, log_index, sweep_tx_hash, sweep_block_number,
              sweep_block_hash, sweep_log_index, sweep_amount, sweep_target)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT DO NOTHING",
            &[
                &row.chain,
                &row.tx_hash,
                &row.log_index,
                &row.sweep_tx_hash,
                &row.sweep_block_number,
                &row.sweep_block_hash,
                &row.sweep_log_index,
                &row.sweep_amount,
                &row.sweep_target,
            ],
        )?;
        summary.sweeps.1 += changes as usize;
    }

    summary.sweep_intents.0 = snapshot.sweep_intents.len();
    for row in &snapshot.sweep_intents {
        let changes = tx.execute(
            "INSERT INTO sweep_intents
             (id, chain, account_id, token_address, group_key, keys, part_index, amount, target,
              state, tx_hash, nonce, raw_tx, passes, address, txs, gone_block)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
             ON CONFLICT (id) DO NOTHING",
            &[
                &row.id,
                &row.chain,
                &row.account_id,
                &row.token_address,
                &row.group_key,
                &row.keys,
                &row.part_index,
                &row.amount,
                &row.target,
                &row.state,
                &row.tx_hash,
                &row.nonce,
                &row.raw_tx,
                &row.passes,
                &row.address,
                &row.txs,
                &row.gone_block,
            ],
        )?;
        summary.sweep_intents.1 += changes as usize;
    }
    // New intents continue after the copied ones.
    tx.execute(
        "SELECT setval(pg_get_serial_sequence('sweep_intents', 'id'),
                       COALESCE((SELECT MAX(id) FROM sweep_intents), 1),
                       (SELECT COUNT(*) FROM sweep_intents) > 0)",
        &[],
    )?;

    summary.token_metadata.0 = snapshot.token_metadata.len();
    for row in &snapshot.token_metadata {
        // `token_metadata.decimals` is `SMALLINT` (int2) in the Postgres
        // schema -- matches `PostgresBackend::store_token_metadata`'s own
        // `u8` parameter -- but SQLite has no fixed-width integer types, so
        // `read_sqlite_snapshot` reads it as `i64`. ERC20 decimals always
        // fit comfortably in i16 (never seen above 18), but check explicitly
        // rather than truncating silently, same as `last_http_status` below.
        let decimals = i16::try_from(row.decimals).with_context(|| {
            format!(
                "token_metadata[{}:{}].decimals={} does not fit in i16",
                row.chain, row.token_address, row.decimals
            )
        })?;
        let changes = tx.execute(
            "INSERT INTO token_metadata (chain, token_address, symbol, decimals, name)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (chain, token_address) DO NOTHING",
            &[
                &row.chain,
                &row.token_address,
                &row.symbol,
                &decimals,
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
            &[
                &row.chain,
                &row.tx_hash,
                &row.log_index,
                &row.consecutive_failure_count,
            ],
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
/// handful of account addresses and deposit statuses. Used after a dry run
/// and after a real migration (via the binary's `--verify` mode).
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
        "sweep_intents.count",
        snapshot.sweep_intents.len() as i64,
        pg_count(client, "sweep_intents")?,
    );
    report.check(
        "sweeps.count",
        snapshot.sweeps.len() as i64,
        pg_count(client, "sweeps")?,
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
        report.check(
            &format!("state.last_block:{chain}"),
            Some(expected_block),
            actual,
        );
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
            Some((
                row.derivation_index,
                row.address.clone(),
                row.webhook_url.clone(),
            )),
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
        conn.execute("UPDATE state SET value = '1' WHERE key = 'next_index'", [])
            .unwrap();
        drop(conn);
        tmp
    }

    #[test]
    fn migration_refuses_a_pending_sweep_intent() {
        let mut snapshot = SqliteSnapshot::default();
        snapshot.sweep_intents.push(SweepIntentRow {
            id: 7,
            chain: "base".into(),
            state: "pending".into(),
            tx_hash: Some("0xabc".into()),
            ..Default::default()
        });
        let err = assert_no_pending_intents(&snapshot)
            .unwrap_err()
            .to_string();
        assert!(err.contains("pending") && err.contains("0xabc"), "{err}");
        snapshot.sweep_intents[0].state = "succeeded".into();
        assert!(assert_no_pending_intents(&snapshot).is_ok());
    }

    #[test]
    fn snapshot_reads_deposits_in_insertion_order_and_carries_intents_and_review_kind() {
        let tmp = seed_sqlite_file();
        let conn = RusqliteConnection::open(tmp.path()).unwrap();
        for (i, tx) in ["0xfff", "0xaaa", "0xccc"].iter().enumerate() {
            conn.execute(
                "INSERT INTO erc20_deposits
                 (chain, tx_hash, log_index, account_id, amount, token_address, token_symbol, status, review_kind)
                 VALUES ('base', ?1, ?2, 'u1', '1', '0xt', 'T', 'needs_review', 'no_sender')",
                rusqlite::params![tx, i as i64],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO sweep_intents
             (chain, account_id, address, token_address, group_key, keys, part_index, amount, target,
              state, tx_hash, nonce, raw_tx, passes)
             VALUES ('base', 'u1', '0x1', '0xt', 'g', '[]', 0, '1', '0xtarget', 'succeeded', '0xh', 4, '0xraw', 2)",
            [],
        )
        .unwrap();
        drop(conn);
        let snapshot = read_sqlite_snapshot(tmp.path()).unwrap();
        let order: Vec<&str> = snapshot
            .erc20_deposits
            .iter()
            .map(|d| d.tx_hash.as_str())
            .collect();
        assert_eq!(order, ["0xfff", "0xaaa", "0xccc"]);
        assert_eq!(
            snapshot.erc20_deposits[0].review_kind.as_deref(),
            Some("no_sender")
        );
        assert_eq!(snapshot.sweep_intents.len(), 1);
        assert_eq!(snapshot.sweep_intents[0].nonce, Some(4));
        assert_eq!(snapshot.sweep_intents[0].raw_tx.as_deref(), Some("0xraw"));
    }

    #[test]
    fn test_read_sqlite_snapshot_reads_all_tables() {
        let tmp = seed_sqlite_file();
        let snapshot = read_sqlite_snapshot(tmp.path()).unwrap();
        assert_eq!(snapshot.accounts.len(), 1);
        assert_eq!(snapshot.deposits.len(), 1);
        assert_eq!(snapshot.next_index(), Some(1));
        assert_eq!(snapshot.block_cursors(), vec![("base".to_string(), 42)]);
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
