//! Postgres backend test suite (evmhot Postgres storage migration, decision
//! D6 from the eng review).
//!
//! Everything in this file is gated on Docker being available and skips
//! (with a printed message, not a failure) when it is not — so `cargo test`
//! in an environment without Docker still exercises the full SQLite arm
//! unmodified (see `src/db/sqlite.rs`'s own `#[cfg(test)]` module) and just
//! quietly misses the Postgres arm.
//!
//! Uses `testcontainers` + `testcontainers-modules` (`postgres:16-alpine`) to
//! spin up ephemeral, disposable Postgres instances — nothing here ever
//! touches a real database.
//!
//! All tests that mutate process-wide env vars (`DB_TLS_MODE`,
//! `DB_POOL_SIZE`, `EVM_MIGRATE_FROM_SQLITE`) take `ENV_GUARD` for their
//! entire body, so this file's tests never race each other even though
//! `cargo test` runs test functions on multiple threads by default.

use evmhot::db::{Db, WriteQueueError};
use rusqlite::Connection as RusqliteConnection;
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;
use tempfile::NamedTempFile;
use testcontainers::clients::Cli;
use testcontainers::RunnableImage;
use testcontainers_modules::postgres::Postgres;

/// Serializes every test in this file that touches the process-wide
/// `DB_TLS_MODE` / `DB_POOL_SIZE` / `EVM_MIGRATE_FROM_SQLITE` env vars.
static ENV_GUARD: Mutex<()> = Mutex::new(());

/// `testcontainers` 0.15's `Container::get_host_port_ipv4` returns a port
/// mapping cached once at container creation. On this Docker setup (and
/// apparently only here -- not representative of a real RDS endpoint, whose
/// host/port never change across a failover), `docker start` after `docker
/// stop` reassigns a *new* ephemeral host port for a dynamically-published
/// container port, which the cached value goes stale against. Shell out to
/// `docker port` for a live answer instead.
fn live_host_port(container_id: &str, internal_port: u16) -> Option<u16> {
    let output = Command::new("docker")
        .args(["port", container_id, &format!("{internal_port}/tcp")])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    // e.g. "0.0.0.0:55024\n[::]:55024\n" -- take the first line's port.
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()?
        .rsplit(':')
        .next()?
        .trim()
        .parse()
        .ok()
}

fn docker_available() -> bool {
    Command::new("docker")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Starts a disposable `postgres:16-alpine` container and returns its
/// connection URL plus the `Container` handle that must stay alive (in
/// scope, not dropped) for as long as the container should keep running.
fn start_postgres() -> (testcontainers::Container<'static, Postgres>, String) {
    // `Cli::default()` is cheap (no container yet); leaking it is fine since
    // these are short-lived test-process-only containers, and it lets us
    // return a `'static` `Container` without fighting the borrow checker
    // across the helper-function boundary.
    let docker: &'static Cli = Box::leak(Box::new(Cli::default()));
    // Overridable so environments with a restricted/slow image registry
    // proxy can point at whatever Postgres image tag they already have
    // cached locally; production/CI defaults to 16-alpine per the migration
    // plan.
    let tag = std::env::var("TEST_POSTGRES_IMAGE_TAG").unwrap_or_else(|_| "16-alpine".to_string());
    let image = RunnableImage::from(Postgres::default().with_host_auth()).with_tag(tag);
    let container = docker.run(image);
    let port = container.get_host_port_ipv4(5432);
    let url = format!("postgres://postgres@127.0.0.1:{port}/postgres");
    (container, url)
}

/// Sets the env knobs the Postgres backend reads at `Db::connect` time.
/// Caller must hold `ENV_GUARD`.
fn set_pg_env(tls_disable: bool, pool_size: Option<u32>, migrate_from_sqlite: Option<&str>) {
    if tls_disable {
        std::env::set_var("DB_TLS_MODE", "disable");
    } else {
        std::env::remove_var("DB_TLS_MODE");
    }
    match pool_size {
        Some(n) => std::env::set_var("DB_POOL_SIZE", n.to_string()),
        None => std::env::remove_var("DB_POOL_SIZE"),
    }
    match migrate_from_sqlite {
        Some(p) => std::env::set_var("EVM_MIGRATE_FROM_SQLITE", p),
        None => std::env::remove_var("EVM_MIGRATE_FROM_SQLITE"),
    }
}

/// Connects a `Db` against `url` with `DB_TLS_MODE=disable` (the dry-run/test
/// mode; real RDS connections use the default `verify-ca`). Caller must hold
/// `ENV_GUARD`.
fn connect_pg(url: &str, pool_size: Option<u32>, migrate_from_sqlite: Option<&str>) -> Db {
    set_pg_env(true, pool_size, migrate_from_sqlite);
    Db::new(url).expect("failed to connect Db against test Postgres container")
}

/// Writes a minimal-but-non-empty SQLite `wallet.db` (passes the
/// empty-source migration guard) and returns the temp file handle (keep it
/// alive for the duration of the test).
///
/// Seeds one representative row into *every* migrated table (not just
/// `accounts`/`state`) so the bootstrap-migration integration test proves
/// end-to-end correctness for `deposits`, `erc20_deposits`,
/// `token_metadata`, `sweep_meta`, `sweep_failures`, and
/// `webhook_deliveries` too -- previously only `accounts`/`state` were
/// exercised through the real migration path in an automated test.
fn seed_sqlite_source(next_index: i64, block_cursor: (&str, i64)) -> NamedTempFile {
    let tmp = NamedTempFile::new().unwrap();
    let mut conn = RusqliteConnection::open(tmp.path()).unwrap();
    evmhot::db::apply_pragmas_for_import(&conn).unwrap();
    evmhot::db::migrations().to_latest(&mut conn).unwrap();
    conn.execute(
        "INSERT INTO accounts (id, derivation_index, address, webhook_url)
         VALUES ('seed-user', 0, '0xseed', 'https://example.com/webhook')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO deposits (chain, tx_hash, account_id, amount, status, from_address)
         VALUES ('base', '0xdep1', 'seed-user', '100', 'detected', '0xsender-native')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO erc20_deposits
         (chain, tx_hash, log_index, account_id, amount, token_address, token_symbol, status,
          from_address)
         VALUES ('base', '0xerc1', 0, 'seed-user', '50', '0xtoken1', 'USDC', 'detected',
                 '0xsender-token')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sweeps
         (chain, tx_hash, log_index, sweep_tx_hash, sweep_block_number, sweep_block_hash,
          sweep_log_index, sweep_amount)
         VALUES ('base', '0xerc1', 0, '0xsweepA', 77, '0xblockA', 12, '50')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sweep_intents
         (chain, account_id, address, token_address, group_key, keys, part_index, amount, target,
          state, tx_hash, nonce, raw_tx, passes)
         VALUES ('base', 'seed-user', '0xseed', '0xtoken1', 'g', '[\"0xerc1:0\"]', 0, '50',
                 '0xtarget', 'succeeded', '0xsweepA', 5, '0xraw', 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO token_metadata (chain, token_address, symbol, decimals, name)
         VALUES ('base', '0xtoken1', 'USDC', 6, 'USD Coin')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sweep_meta (chain, tx_hash, log_index, sweep_tx_hash, zero_balance_retry_count)
         VALUES ('base', '0xdep1', 0, '0xsweep1', 2)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sweep_failures (chain, tx_hash, log_index, consecutive_failure_count)
         VALUES ('base', '0xdep1', 0, 3)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO webhook_deliveries
         (id, event, registration_id, webhook_url, payload, status, attempt_count,
          last_http_status, last_error, leased_until, updated_at)
         VALUES ('wh1', 'deposit.detected', 'seed-user', 'https://example.com/webhook',
                 '{\"foo\":\"bar\"}', 'pending', 1, 502, 'timeout', 1700000100, 1700000000)",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE state SET value = ?1 WHERE key = 'next_index'",
        rusqlite::params![next_index.to_string()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO state (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![
            format!("last_block:{}", block_cursor.0),
            block_cursor.1.to_string()
        ],
    )
    .unwrap();
    drop(conn);
    tmp
}

/// Opens a plain (no-TLS) raw `postgres::Client` against a test container's
/// URL, for asserting exact migrated row values with raw SQL -- the same
/// style `sqlite_import::verify_migration` uses -- independent of any
/// business-logic filtering the `Db` facade's higher-level methods apply
/// (e.g. status filters), so this is a direct proof of what actually landed
/// in the table.
fn raw_pg_client(url: &str) -> postgres::Client {
    postgres::Client::connect(url, postgres::NoTls)
        .expect("failed to open raw verification connection to test Postgres container")
}

/// Asserts that every row `seed_sqlite_source` wrote into `deposits`,
/// `erc20_deposits`, `token_metadata`, `sweep_meta`, `sweep_failures`, and
/// `webhook_deliveries` landed in Postgres with the exact values from the
/// SQLite source -- and that each table has exactly one row (proves a
/// second bootstrap-migration attempt against an already-populated
/// destination does not duplicate rows). `context` is included in panic
/// messages to identify which call site (first vs. second connect) failed.
fn assert_all_migrated_tables(url: &str, context: &str) {
    let mut client = raw_pg_client(url);

    let deposit = client
        .query_one(
            "SELECT account_id, amount, status FROM deposits WHERE chain = 'base' AND tx_hash = '0xdep1'",
            &[],
        )
        .unwrap_or_else(|e| panic!("{context}: deposits row missing or query failed: {e}"));
    assert_eq!(
        deposit.get::<_, String>(0),
        "seed-user",
        "{context}: deposits.account_id"
    );
    assert_eq!(
        deposit.get::<_, String>(1),
        "100",
        "{context}: deposits.amount"
    );
    assert_eq!(
        deposit.get::<_, String>(2),
        "detected",
        "{context}: deposits.status"
    );
    // F-002a: the sender and the sweep coordinates travel with the rows
    // (REQ-ST-2: a move to a new database carries every row).
    let senders: (Option<String>, Option<String>) = (
        client
            .query_one(
                "SELECT from_address FROM deposits WHERE chain = 'base' AND tx_hash = '0xdep1'",
                &[],
            )
            .unwrap()
            .get(0),
        client
            .query_one(
                "SELECT from_address FROM erc20_deposits WHERE chain = 'base' AND tx_hash = '0xerc1'",
                &[],
            )
            .unwrap()
            .get(0),
    );
    assert_eq!(
        senders,
        (
            Some("0xsender-native".to_string()),
            Some("0xsender-token".to_string())
        ),
        "{context}: from_address"
    );
    let sweep = client
        .query_one(
            "SELECT sweep_tx_hash, sweep_block_number, sweep_block_hash, sweep_log_index, sweep_amount
             FROM sweeps WHERE chain = 'base' AND tx_hash = '0xerc1' AND log_index = 0",
            &[],
        )
        .unwrap_or_else(|e| panic!("{context}: sweeps row missing or query failed: {e}"));
    assert_eq!(
        sweep.get::<_, String>(0),
        "0xsweepA",
        "{context}: sweeps.sweep_tx_hash"
    );
    assert_eq!(
        sweep.get::<_, i64>(1),
        77,
        "{context}: sweeps.sweep_block_number"
    );
    assert_eq!(
        sweep.get::<_, String>(2),
        "0xblockA",
        "{context}: sweeps.sweep_block_hash"
    );
    assert_eq!(
        sweep.get::<_, i64>(3),
        12,
        "{context}: sweeps.sweep_log_index"
    );
    assert_eq!(
        sweep.get::<_, String>(4),
        "50",
        "{context}: sweeps.sweep_amount"
    );
    let intent = client
        .query_one(
            "SELECT state, tx_hash, nonce, raw_tx FROM sweep_intents WHERE chain = 'base'",
            &[],
        )
        .unwrap_or_else(|e| panic!("{context}: sweep_intents row missing: {e}"));
    assert_eq!(
        intent.get::<_, String>(0),
        "succeeded",
        "{context}: intent state"
    );
    assert_eq!(
        intent.get::<_, Option<i64>>(2),
        Some(5),
        "{context}: intent nonce"
    );
    assert_eq!(
        intent.get::<_, Option<String>>(3).as_deref(),
        Some("0xraw"),
        "{context}: raw_tx"
    );
    let sweep_count: i64 = client
        .query_one("SELECT COUNT(*) FROM sweeps", &[])
        .unwrap()
        .get(0);
    assert_eq!(
        sweep_count, 1,
        "{context}: sweeps must not be duplicated by a re-run"
    );

    let deposit_count: i64 = client
        .query_one("SELECT COUNT(*) FROM deposits WHERE chain = 'base'", &[])
        .unwrap()
        .get(0);
    assert_eq!(
        deposit_count, 1,
        "{context}: deposits must not be duplicated by a re-run"
    );

    let erc20 = client
        .query_one(
            "SELECT account_id, amount, token_address, token_symbol, status
             FROM erc20_deposits WHERE chain = 'base' AND tx_hash = '0xerc1' AND log_index = 0",
            &[],
        )
        .unwrap_or_else(|e| panic!("{context}: erc20_deposits row missing or query failed: {e}"));
    assert_eq!(
        erc20.get::<_, String>(0),
        "seed-user",
        "{context}: erc20_deposits.account_id"
    );
    assert_eq!(
        erc20.get::<_, String>(1),
        "50",
        "{context}: erc20_deposits.amount"
    );
    assert_eq!(
        erc20.get::<_, String>(2),
        "0xtoken1",
        "{context}: erc20_deposits.token_address"
    );
    assert_eq!(
        erc20.get::<_, String>(3),
        "USDC",
        "{context}: erc20_deposits.token_symbol"
    );
    assert_eq!(
        erc20.get::<_, String>(4),
        "detected",
        "{context}: erc20_deposits.status"
    );

    let token = client
        .query_one(
            "SELECT symbol, decimals, name FROM token_metadata
             WHERE chain = 'base' AND token_address = '0xtoken1'",
            &[],
        )
        .unwrap_or_else(|e| panic!("{context}: token_metadata row missing or query failed: {e}"));
    assert_eq!(
        token.get::<_, String>(0),
        "USDC",
        "{context}: token_metadata.symbol"
    );
    assert_eq!(
        token.get::<_, i16>(1),
        6,
        "{context}: token_metadata.decimals"
    );
    assert_eq!(
        token.get::<_, String>(2),
        "USD Coin",
        "{context}: token_metadata.name"
    );

    let sweep_meta = client
        .query_one(
            "SELECT sweep_tx_hash, zero_balance_retry_count FROM sweep_meta
             WHERE chain = 'base' AND tx_hash = '0xdep1' AND log_index = 0",
            &[],
        )
        .unwrap_or_else(|e| panic!("{context}: sweep_meta row missing or query failed: {e}"));
    assert_eq!(
        sweep_meta.get::<_, String>(0),
        "0xsweep1",
        "{context}: sweep_meta.sweep_tx_hash"
    );
    assert_eq!(
        sweep_meta.get::<_, i64>(1),
        2,
        "{context}: sweep_meta.zero_balance_retry_count"
    );

    let sweep_failure = client
        .query_one(
            "SELECT consecutive_failure_count FROM sweep_failures
             WHERE chain = 'base' AND tx_hash = '0xdep1' AND log_index = 0",
            &[],
        )
        .unwrap_or_else(|e| panic!("{context}: sweep_failures row missing or query failed: {e}"));
    assert_eq!(
        sweep_failure.get::<_, i64>(0),
        3,
        "{context}: sweep_failures.consecutive_failure_count"
    );

    let webhook = client
        .query_one(
            "SELECT registration_id, webhook_url, payload, status, attempt_count,
                    last_http_status, last_error, leased_until, updated_at
             FROM webhook_deliveries WHERE id = 'wh1' AND event = 'deposit.detected'",
            &[],
        )
        .unwrap_or_else(|e| {
            panic!("{context}: webhook_deliveries row missing or query failed: {e}")
        });
    assert_eq!(
        webhook.get::<_, String>(0),
        "seed-user",
        "{context}: webhook_deliveries.registration_id"
    );
    assert_eq!(
        webhook.get::<_, String>(1),
        "https://example.com/webhook",
        "{context}: webhook_deliveries.webhook_url"
    );
    assert_eq!(
        webhook.get::<_, String>(2),
        "{\"foo\":\"bar\"}",
        "{context}: webhook_deliveries.payload"
    );
    assert_eq!(
        webhook.get::<_, String>(3),
        "pending",
        "{context}: webhook_deliveries.status"
    );
    assert_eq!(
        webhook.get::<_, i64>(4),
        1,
        "{context}: webhook_deliveries.attempt_count"
    );
    assert_eq!(
        webhook.get::<_, Option<i32>>(5),
        Some(502),
        "{context}: webhook_deliveries.last_http_status"
    );
    assert_eq!(
        webhook.get::<_, Option<String>>(6),
        Some("timeout".to_string()),
        "{context}: webhook_deliveries.last_error"
    );
    assert_eq!(
        webhook.get::<_, Option<i64>>(7),
        Some(1700000100),
        "{context}: webhook_deliveries.leased_until"
    );
    assert_eq!(
        webhook.get::<_, i64>(8),
        1700000000,
        "{context}: webhook_deliveries.updated_at"
    );
}

/// Shared behavior assertions run against a live `Db`, regardless of
/// backend. This is the drift guard for the intentionally-duplicated SQL
/// between `db/sqlite.rs` and `db/postgres.rs` (decision D6 from the
/// Postgres-migration eng review) -- every method exercised here must behave
/// identically on both backends.
fn assert_full_crud_cycle(db: &Db) {
    // Accounts / derivation index allocation.
    let (idx0, addr0, created0) = db
        .register_account_auto("acct-a", "https://hook.example/a", |i| {
            Ok(format!("0xaddr{i}"))
        })
        .unwrap();
    assert_eq!(idx0, 0);
    assert_eq!(addr0, "0xaddr0");
    assert!(created0);

    let (idx1, addr1, created1) = db
        .register_account_auto("acct-b", "https://hook.example/b", |i| {
            Ok(format!("0xaddr{i}"))
        })
        .unwrap();
    assert_eq!(idx1, 1);
    assert_eq!(addr1, "0xaddr1");
    assert!(created1);

    // Re-registering the same id is a no-op that returns the existing index.
    let (idx0_again, addr0_again, created_again) = db
        .register_account_auto("acct-a", "https://hook.example/a", |_| {
            panic!("derive_address must not be called for an already-registered id")
        })
        .unwrap();
    assert_eq!(idx0_again, 0);
    assert_eq!(addr0_again, "0xaddr0");
    assert!(!created_again);

    assert_eq!(
        db.get_registration_id_by_address("0xaddr1").unwrap(),
        Some("acct-b".to_string())
    );
    assert_eq!(
        db.get_account_by_id("acct-a").unwrap(),
        Some((
            0,
            "0xaddr0".to_string(),
            "https://hook.example/a".to_string()
        ))
    );
    assert_eq!(
        db.get_webhook_url("acct-b").unwrap(),
        Some("https://hook.example/b".to_string())
    );

    // Native deposits.
    assert!(db
        .record_deposit("base", "0xtx1", "acct-a", "1000")
        .unwrap());
    assert!(!db
        .record_deposit("base", "0xtx1", "acct-a", "1000")
        .unwrap()); // dup is a no-op
    let detected = db.get_detected_deposits("base").unwrap();
    assert_eq!(
        detected,
        vec![(
            "0xtx1".to_string(),
            "acct-a".to_string(),
            "1000".to_string()
        )]
    );

    db.mark_deposit_failed("base", "0xtx1").unwrap();
    assert!(db.retry_native_deposit("base", "0xtx1").unwrap());
    assert_eq!(db.get_detected_deposits("base").unwrap().len(), 1);
    db.mark_deposit_swept("base", "0xtx1").unwrap();
    assert_eq!(db.get_detected_deposits("base").unwrap().len(), 0);

    // Block cursors.
    assert_eq!(db.get_last_processed_block("base").unwrap(), 0);
    db.set_last_processed_block("base", 100).unwrap();
    assert_eq!(db.get_last_processed_block("base").unwrap(), 100);
    db.set_last_processed_block_priority("base", 150).unwrap();
    assert_eq!(db.get_last_processed_block("base").unwrap(), 150);

    // Token metadata.
    db.store_token_metadata("base", "0xtoken1", "USDC", 6, "USD Coin")
        .unwrap();
    assert_eq!(
        db.get_token_metadata("base", "0xtoken1").unwrap(),
        Some(("USDC".to_string(), 6, "USD Coin".to_string()))
    );

    // ERC20 deposits.
    assert!(db
        .record_erc20_deposit("base", "0xtx2", 0, "acct-a", "500", "0xtoken1", "USDC")
        .unwrap());
    assert!(!db
        .record_erc20_deposit("base", "0xtx2", 0, "acct-a", "500", "0xtoken1", "USDC")
        .unwrap());
    let erc20_detected = db.get_detected_erc20_deposits("base").unwrap();
    assert_eq!(erc20_detected.len(), 1);
    assert_eq!(erc20_detected[0].key, "0xtx2:0");
    assert_eq!(erc20_detected[0].amount, "500");

    let swept = db
        .mark_erc20_deposits_swept_for_account_token("base", "acct-a", "0xtoken1")
        .unwrap();
    assert_eq!(swept, vec![("0xtx2:0".to_string(), "500".to_string())]);
    assert_eq!(db.get_detected_erc20_deposits("base").unwrap().len(), 0);

    assert!(db
        .record_erc20_deposit("base", "0xtx3", 1, "acct-a", "200", "0xtoken1", "USDC")
        .unwrap());
    let failed = db
        .mark_erc20_deposits_failed_for_account_token("base", "acct-a", "0xtoken1")
        .unwrap();
    assert_eq!(failed, vec!["0xtx3:1".to_string()]);
    assert!(db.retry_erc20_deposit("base", "0xtx3", 1).unwrap());

    // Sweep bookkeeping.
    assert_eq!(
        db.increment_zero_balance_count("base", "0xtx3:1").unwrap(),
        1
    );
    assert_eq!(
        db.increment_zero_balance_count("base", "0xtx3:1").unwrap(),
        2
    );
    db.set_sweep_tx_hash_for_keys("base", &["0xtx3:1".to_string()], "0xsweep1")
        .unwrap();
    assert_eq!(
        db.increment_sweep_failure_count("base", "0xtx3:1").unwrap(),
        1
    );
    assert_eq!(db.get_sweep_failure_count("base", "0xtx3:1").unwrap(), 1);

    // Deposit queue counts reflect the current mix of detected/failed rows.
    let counts = db.deposit_queue_counts("base").unwrap();
    assert!(counts.has_pending());

    // Webhook delivery lifecycle.
    assert!(db
        .upsert_webhook_delivery("wh1", "deposit", "acct-a", "https://hook.example/a", "{}")
        .unwrap());
    assert!(db
        .upsert_webhook_delivery("wh1", "deposit", "acct-a", "https://hook.example/a", "{}")
        .unwrap()); // still pending -> treated as an update, not a duplicate insert
    let pending = db.get_pending_webhook_delivery_keys(5, 10).unwrap();
    assert!(pending.contains(&("wh1".to_string(), "deposit".to_string())));

    assert!(db
        .claim_webhook_delivery("wh1", "deposit", 9_999_999_999, 5)
        .unwrap());
    let attempts = db
        .record_webhook_attempt("wh1", "deposit", Some(500), Some("boom"), "failed")
        .unwrap();
    assert_eq!(attempts, 1);
    let record = db.get_webhook_delivery("wh1", "deposit").unwrap().unwrap();
    assert_eq!(record.status, "failed");
    assert_eq!(record.last_http_status, Some(500));

    assert!(db.retry_webhook_delivery("wh1", "deposit").unwrap());
    let record = db.get_webhook_delivery("wh1", "deposit").unwrap().unwrap();
    assert_eq!(record.status, "pending");
    assert_eq!(record.attempt_count, 0);
}

#[test]
fn sqlite_full_crud_cycle_regression_guard() {
    // No Docker/env dependency: always runs, proving the shared assertions
    // above are a faithful description of the (unchanged) SQLite backend
    // before we hold the Postgres arm to the same bar.
    let tmp = NamedTempFile::new().unwrap();
    let db = Db::new(tmp.path().to_str().unwrap()).unwrap();
    assert_full_crud_cycle(&db);
}

#[test]
fn postgres_full_crud_cycle_matches_sqlite() {
    if !docker_available() {
        eprintln!("skipping postgres_full_crud_cycle_matches_sqlite: Docker not available");
        return;
    }
    let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let (_container, url) = start_postgres();
    let db = connect_pg(&url, None, None);
    assert_full_crud_cycle(&db);
}

#[test]
fn postgres_concurrent_register_account_auto_allocates_distinct_sequential_indices() {
    if !docker_available() {
        eprintln!(
            "skipping postgres_concurrent_register_account_auto_allocates_distinct_sequential_indices: Docker not available"
        );
        return;
    }
    let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let (_container, url) = start_postgres();
    let db = connect_pg(&url, None, None);

    const N: usize = 16;
    let handles: Vec<_> = (0..N)
        .map(|i| {
            let db = db.clone();
            std::thread::spawn(move || {
                let (index, _address, created) = db
                    .register_account_auto(
                        &format!("concurrent-{i}"),
                        "https://hook.example",
                        |idx| Ok(format!("0xconcurrent{idx}")),
                    )
                    .unwrap();
                assert!(created);
                index
            })
        })
        .collect();

    let mut indices: Vec<u32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    indices.sort_unstable();
    let expected: Vec<u32> = (0..N as u32).collect();
    assert_eq!(
        indices, expected,
        "N parallel register_account_auto calls must allocate N distinct sequential indices \
         (the atomicity guarantee the SQLite writer thread used to provide, now provided by \
         Postgres's row lock on the next_index UPDATE ... RETURNING)"
    );
}

#[test]
fn postgres_pool_exhaustion_maps_to_write_queue_error() {
    if !docker_available() {
        eprintln!(
            "skipping postgres_pool_exhaustion_maps_to_write_queue_error: Docker not available"
        );
        return;
    }
    let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let (_container, url) = start_postgres();
    // Pool of exactly 1: the first thread's in-flight transaction holds the
    // only connection while the second thread's call blocks on pool.get()
    // until r2d2's connection_timeout (5s, decision D9) fires.
    let db = connect_pg(&url, Some(1), None);

    // Must outlast the backend's fixed 5s pool-acquire timeout (decision D9)
    // so the contender actually times out waiting for the pool's one
    // connection, rather than just queueing behind it and succeeding late.
    let db_holder = db.clone();
    let holder = std::thread::spawn(move || {
        db_holder
            .register_account_auto("holder", "https://hook.example", |idx| {
                std::thread::sleep(Duration::from_secs(7));
                Ok(format!("0xholder{idx}"))
            })
            .unwrap();
    });

    // Give the holder thread a head start so it has acquired the pool's one
    // connection before we try to acquire a second one.
    std::thread::sleep(Duration::from_millis(500));

    let contender = db.register_account_auto("contender", "https://hook.example", |idx| {
        Ok(format!("0xcontender{idx}"))
    });

    holder.join().unwrap();

    let err = contender.expect_err(
        "a second concurrent write against a pool of size 1 must fail while the first \
         write's transaction is in flight",
    );
    let write_queue_err = err.downcast_ref::<WriteQueueError>().expect(
        "pool-exhaustion error must be a WriteQueueError (so api.rs's existing 503 \
                 mapping keeps working unchanged on the Postgres path)",
    );
    assert!(
        matches!(write_queue_err, WriteQueueError::Timeout(_)),
        "expected WriteQueueError::Timeout, got: {write_queue_err:?}"
    );
}

#[test]
fn postgres_health_check_reflects_container_availability() {
    if !docker_available() {
        eprintln!(
            "skipping postgres_health_check_reflects_container_availability: Docker not available"
        );
        return;
    }
    let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let (container, url) = start_postgres();
    let db = connect_pg(&url, None, None);

    assert!(
        db.writer_healthy(),
        "writer_healthy() must be true while the container is up"
    );

    container.stop();
    // The pool's acquire timeout (5s) plus a little slack is enough for an
    // in-flight or next `SELECT 1` to observe the outage; writer_healthy()
    // itself only waits up to its own 2s health-check timeout per call, so
    // poll briefly rather than sleeping for the full pool timeout.
    let became_unhealthy = (0..20).any(|_| {
        std::thread::sleep(Duration::from_millis(500));
        !db.writer_healthy()
    });
    assert!(
        became_unhealthy,
        "writer_healthy() must become false once the container is stopped"
    );

    container.start();
    // Docker (at least on Docker Desktop) can remap a dynamically-assigned
    // host port when restarting a stopped container, unlike a real RDS
    // endpoint/port surviving a failover -- so re-derive the URL from the
    // container rather than trusting the pre-outage one, and reconnect with
    // a fresh `Db` (the point being tested is "the service can become
    // healthy again once its target is reachable", i.e. decision D3's ALB
    // health-check semantics, not "this exact process's stale TCP socket
    // magically un-breaks").
    std::thread::sleep(Duration::from_millis(500));
    let became_healthy_again = (0..20).any(|_| {
        let Some(port) = live_host_port(container.id(), 5432) else {
            std::thread::sleep(Duration::from_millis(500));
            return false;
        };
        let recovered_url = format!("postgres://postgres@127.0.0.1:{port}/postgres");
        match Db::new(&recovered_url) {
            Ok(fresh_db) if fresh_db.writer_healthy() => true,
            _ => {
                std::thread::sleep(Duration::from_millis(500));
                false
            }
        }
    });
    assert!(
        became_healthy_again,
        "a fresh Db reconnecting after the container comes back up must observe writer_healthy() == true"
    );
}

#[test]
fn postgres_bootstrap_migration_runs_once_at_connect_and_is_idempotent() {
    if !docker_available() {
        eprintln!(
            "skipping postgres_bootstrap_migration_runs_once_at_connect_and_is_idempotent: Docker not available"
        );
        return;
    }
    let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let (_container, url) = start_postgres();

    let sqlite_source = seed_sqlite_source(7, ("base", 12345));
    let sqlite_path = sqlite_source.path().to_str().unwrap().to_string();

    // First connect: Postgres is empty and EVM_MIGRATE_FROM_SQLITE points at
    // a real snapshot -> bootstrap migration must run automatically.
    let db1 = connect_pg(&url, None, Some(&sqlite_path));
    assert_eq!(
        db1.get_account_by_id("seed-user").unwrap(),
        Some((
            0,
            "0xseed".to_string(),
            "https://example.com/webhook".to_string()
        ))
    );
    assert_eq!(db1.get_last_processed_block("base").unwrap(), 12345);
    assert_all_migrated_tables(&url, "after first (bootstrap) connect");

    // Registering a new account continues the migrated next_index counter
    // rather than restarting from 0 (the correctness-critical property: a
    // fresh start from 0 here would mean re-issuing derivation indices and
    // producing duplicate deposit addresses).
    let (next_idx, _addr, created) = db1
        .register_account_auto("post-migration-user", "https://hook.example", |idx| {
            Ok(format!("0xpm{idx}"))
        })
        .unwrap();
    assert!(created);
    assert_eq!(
        next_idx, 7,
        "next_index must continue from the migrated counter, not restart at 0"
    );

    // Second connect against the SAME (now-populated) Postgres instance,
    // with EVM_MIGRATE_FROM_SQLITE still set: must be a no-op (the
    // empty-destination guard skips migration once accounts/next_index
    // already exist), not a duplicate-insert or an overwrite.
    let db2 = connect_pg(&url, None, Some(&sqlite_path));
    assert_eq!(
        db2.get_account_by_id("post-migration-user")
            .unwrap()
            .map(|(idx, ..)| idx),
        Some(7),
        "re-running the bootstrap migration against an already-populated Postgres must not \
         disturb data written after the first migration"
    );
    let (idx_again, _addr, created_again) = db2
        .register_account_auto("post-migration-user", "https://hook.example", |_| {
            panic!("must not re-derive an address for an id already registered before the second connect")
        })
        .unwrap();
    assert_eq!(idx_again, 7);
    assert!(!created_again);

    // Re-running the bootstrap migration must be a true no-op for the other
    // six tables too: still exactly the one seeded row each, not duplicated
    // (which `ON CONFLICT DO NOTHING` should prevent) and not disturbed.
    assert_all_migrated_tables(&url, "after second (idempotent) connect");
}

#[test]
fn postgres_bootstrap_migration_skips_cleanly_when_source_file_missing() {
    if !docker_available() {
        eprintln!(
            "skipping postgres_bootstrap_migration_skips_cleanly_when_source_file_missing: Docker not available"
        );
        return;
    }
    let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let (_container, url) = start_postgres();

    // EVM_MIGRATE_FROM_SQLITE points at a file that doesn't exist: startup
    // must proceed normally (logged warning, not a hard failure), leaving a
    // freshly-seeded next_index=0 counter.
    let db = connect_pg(&url, None, Some("/nonexistent/path/wallet.db"));
    let (idx, _addr, created) = db
        .register_account_auto("fresh-user", "https://hook.example", |idx| {
            Ok(format!("0xfresh{idx}"))
        })
        .unwrap();
    assert!(created);
    assert_eq!(
        idx, 0,
        "with no source file present, next_index must start from the fresh seed (0)"
    );
}

#[test]
fn postgres_bootstrap_migration_skips_when_env_var_unset() {
    if !docker_available() {
        eprintln!(
            "skipping postgres_bootstrap_migration_skips_when_env_var_unset: Docker not available"
        );
        return;
    }
    let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let (_container, url) = start_postgres();

    // EVM_MIGRATE_FROM_SQLITE unset entirely (the default/non-cutover state):
    // no migration attempt at all.
    let db = connect_pg(&url, None, None);
    assert_eq!(db.get_account_by_id("seed-user").unwrap(), None);
}

#[test]
fn postgres_bootstrap_migration_rolls_back_on_empty_source_guard() {
    if !docker_available() {
        eprintln!(
            "skipping postgres_bootstrap_migration_rolls_back_on_empty_source_guard: Docker not available"
        );
        return;
    }
    let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let (_container, url) = start_postgres();

    // A freshly-migrated (schema-only, zero accounts) SQLite file must never
    // be allowed to "successfully" migrate zero rows into Postgres -- that
    // would silently let the service re-issue derivation indices from 0
    // against a Postgres that looks intentionally empty rather than
    // accidentally so. `sqlite_import`'s empty-source guard covers the
    // migration function directly (see its own #[cfg(test)] module); here we
    // confirm the end-to-end effect through the public Db surface: startup
    // must fail loudly, not silently proceed as an empty/legit boot.
    let empty_source = NamedTempFile::new().unwrap();
    {
        let mut conn = RusqliteConnection::open(empty_source.path()).unwrap();
        evmhot::db::apply_pragmas_for_import(&conn).unwrap();
        evmhot::db::migrations().to_latest(&mut conn).unwrap();
    }

    set_pg_env(true, None, Some(empty_source.path().to_str().unwrap()));
    let result = Db::new(&url);
    assert!(
        result.is_err(),
        "connecting with EVM_MIGRATE_FROM_SQLITE pointed at an empty/freshly-seeded source \
         must fail startup, not silently boot against an empty Postgres"
    );
}

/// REQ-RG-3: on an existing store with accounts but no counter row, the
/// counter is seeded above the highest stored derivation index (hashed
/// indices from older registrations can be large, even above 2^31), never at 0.
#[test]
fn postgres_counter_seed_starts_above_existing_hashed_index() {
    if !docker_available() {
        eprintln!(
            "skipping postgres_counter_seed_starts_above_existing_hashed_index: Docker not available"
        );
        return;
    }
    let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let (_container, url) = start_postgres();

    const HASHED_INDEX: i64 = 2_147_500_000; // synthetic, above 2^31

    // First connect creates the schema (and seeds an empty store at 0).
    drop(connect_pg(&url, None, None));

    // Simulate a store that has accounts but no counter row.
    {
        let mut client = raw_pg_client(&url);
        client
            .execute(
                "INSERT INTO accounts (id, derivation_index, address, webhook_url)
                 VALUES ('hashed-user', $1, '0xhashed', 'https://hook.example')",
                &[&HASHED_INDEX],
            )
            .unwrap();
        client
            .execute("DELETE FROM state WHERE key = 'next_index'", &[])
            .unwrap();
    }

    // Reconnect: the seed runs against the populated store.
    let db = connect_pg(&url, None, None);
    let (idx, _addr, created) = db
        .register_account_auto("after-seed-user", "https://hook.example", |idx| {
            Ok(format!("0xseed{idx}"))
        })
        .unwrap();
    assert!(created);
    assert!(
        i64::from(idx) > HASHED_INDEX,
        "the next registration must get an index strictly above the highest stored one, got {idx}"
    );
    assert_eq!(i64::from(idx), HASHED_INDEX + 1);
}

// ---------------------------------------------------------------- F-002a

fn f002a_outbox(id: &str, event: &str) -> evmhot::db::OutboxEntry {
    evmhot::db::OutboxEntry {
        id: id.to_string(),
        event: event.to_string(),
        registration_id: "acct-a".to_string(),
        webhook_url: "https://hook.example/a".to_string(),
        payload: format!("{{\"id\":\"{id}\",\"event\":\"{event}\"}}"),
    }
}

fn f002a_token_deposit(
    tx: &str,
    log_index: u64,
    from: Option<&str>,
) -> evmhot::db::NewErc20Deposit {
    evmhot::db::NewErc20Deposit {
        chain: "base".to_string(),
        tx_hash: tx.to_string(),
        log_index,
        account_id: "acct-a".to_string(),
        amount: "10".to_string(),
        token_address: "0xtoken1".to_string(),
        token_symbol: "TKN".to_string(),
        from_address: from.map(str::to_string),
    }
}

/// REQ-WH-7, REQ-WH-9, REQ-SW-10 on a live `Db`, whichever backend.
fn assert_f002a_store_cycle(db: &Db) {
    let native = evmhot::db::NewNativeDeposit {
        chain: "base".to_string(),
        tx_hash: "0xn1".to_string(),
        account_id: "acct-a".to_string(),
        amount: "5".to_string(),
        from_address: Some("0xsender-n".to_string()),
    };
    assert!(db
        .record_native_deposit_atomic(
            &native,
            Some(&f002a_outbox("base:0xn1", "deposit_detected"))
        )
        .unwrap());
    assert!(!db
        .record_native_deposit_atomic(
            &native,
            Some(&f002a_outbox("base:0xn1", "deposit_detected"))
        )
        .unwrap());
    assert_eq!(
        db.get_deposit_from_address("base", "0xn1")
            .unwrap()
            .as_deref(),
        Some("0xsender-n")
    );
    assert!(db
        .get_webhook_delivery("base:0xn1", "deposit_detected")
        .unwrap()
        .is_some());

    let token = f002a_token_deposit("0xt1", 3, Some("0xsender-t"));
    assert!(db
        .record_erc20_deposit_atomic(
            &token,
            Some(&f002a_outbox("base:0xt1:3", "deposit_detected"))
        )
        .unwrap());
    let detected = db.get_detected_erc20_deposits("base").unwrap();
    assert_eq!(detected.len(), 1);
    assert_eq!(detected[0].from_address.as_deref(), Some("0xsender-t"));

    let rec = |tx: &str, log: u64| evmhot::db::SweepRecord {
        sweep_tx_hash: tx.to_string(),
        sweep_block_number: 9,
        sweep_block_hash: "0xblock".to_string(),
        sweep_log_index: log,
        sweep_amount: "10".to_string(),
        sweep_target: Some("0xtarget".to_string()),
    };
    let keys = vec!["0xt1:3".to_string()];
    db.record_sweep("base", &keys, &rec("0xs1", 4)).unwrap();
    db.record_sweep("base", &keys, &rec("0xs2", 5)).unwrap();
    db.record_sweep("base", &keys, &rec("0xs1", 4)).unwrap();
    assert_eq!(
        db.get_sweeps("base", "0xt1:3").unwrap(),
        vec![rec("0xs1", 4), rec("0xs2", 5)]
    );

    let settlements = vec![evmhot::db::Erc20Settlement {
        key: "0xt1:3".to_string(),
        outbox: Some(f002a_outbox("base:0xt1:3", "deposit_swept")),
        notify: Some(evmhot::db::OutboxEntry {
            webhook_url: "https://swap.example/hook".to_string(),
            event: "deposit_swept:notify".to_string(),
            ..f002a_outbox("base:0xt1:3", "deposit_swept")
        }),
    }];
    // One row for the registration webhook, one for the target's notify_url.
    assert_eq!(db.complete_erc20_sweep("base", &settlements).unwrap(), 2);
    assert!(db.get_detected_erc20_deposits("base").unwrap().is_empty());
    assert!(db
        .get_webhook_delivery("base:0xt1:3", "deposit_swept")
        .unwrap()
        .is_some());
    // The copy for the target's notify_url is a separate row, same transaction.
    let note = db
        .get_webhook_delivery("base:0xt1:3", "deposit_swept:notify")
        .unwrap()
        .expect("notify row");
    assert_eq!(note.webhook_url, "https://swap.example/hook");
    assert!(db
        .complete_native_sweep(
            "base",
            "0xn1",
            Some(&f002a_outbox("base:0xn1", "deposit_swept"))
        )
        .unwrap());
    assert!(db.get_detected_deposits("base").unwrap().is_empty());
}

fn seed_account(db: &Db) {
    db.register_account("acct-a", 0, "0xaddr-a", "https://hook.example/a")
        .unwrap();
}

#[test]
fn sqlite_f002a_store_cycle() {
    let tmp = NamedTempFile::new().unwrap();
    let db = Db::new(tmp.path().to_str().unwrap()).unwrap();
    seed_account(&db);
    assert_f002a_store_cycle(&db);
}

#[test]
fn postgres_f002a_store_cycle_matches_sqlite() {
    if !docker_available() {
        eprintln!("skipping postgres_f002a_store_cycle_matches_sqlite: Docker not available");
        return;
    }
    let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let (_container, url) = start_postgres();
    let db = connect_pg(&url, None, None);
    seed_account(&db);
    assert_f002a_store_cycle(&db);
}

/// An older Postgres store (tables without the F-002a columns) gains them on
/// connect, and its old rows stay readable (nullable columns, no backfill).
#[test]
fn postgres_f002a_schema_upgrades_an_existing_store() {
    if !docker_available() {
        eprintln!(
            "skipping postgres_f002a_schema_upgrades_an_existing_store: Docker not available"
        );
        return;
    }
    let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let (_container, url) = start_postgres();
    {
        let mut client = raw_pg_client(&url);
        client
            .batch_execute(
                "CREATE TABLE erc20_deposits (
                   chain TEXT NOT NULL, tx_hash TEXT NOT NULL, log_index BIGINT NOT NULL,
                   account_id TEXT NOT NULL, amount TEXT NOT NULL, token_address TEXT NOT NULL,
                   token_symbol TEXT NOT NULL, status TEXT NOT NULL,
                   PRIMARY KEY (chain, tx_hash, log_index));
                 INSERT INTO erc20_deposits VALUES ('base', '0xold', 1, 'acct-a', '7', '0xtoken1', 'TKN', 'detected');",
            )
            .unwrap();
    }
    let db = connect_pg(&url, None, None);
    let detected = db.get_detected_erc20_deposits("base").unwrap();
    assert_eq!(detected.len(), 1);
    assert_eq!(detected[0].from_address, None);
}

/// REQ-WH-7 on Postgres: when the outbox write fails, the state change it
/// reports is rolled back with it.
#[test]
fn postgres_req_wh_7_failed_outbox_write_rolls_back_the_state_change() {
    if !docker_available() {
        eprintln!("skipping postgres_req_wh_7_failed_outbox_write_rolls_back_the_state_change: Docker not available");
        return;
    }
    let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let (_container, url) = start_postgres();
    let db = connect_pg(&url, None, None);
    seed_account(&db);
    let token = f002a_token_deposit("0xt1", 3, Some("0xsender-t"));
    db.record_erc20_deposit_atomic(&token, None).unwrap();
    db.record_native_deposit_atomic(
        &evmhot::db::NewNativeDeposit {
            chain: "base".to_string(),
            tx_hash: "0xn0".to_string(),
            account_id: "acct-a".to_string(),
            amount: "5".to_string(),
            from_address: None,
        },
        None,
    )
    .unwrap();

    let mut client = raw_pg_client(&url);
    client
        .batch_execute(
            "CREATE FUNCTION fail_outbox() RETURNS trigger AS $$
             BEGIN RAISE EXCEPTION 'outbox unavailable'; END $$ LANGUAGE plpgsql;
             CREATE TRIGGER fail_outbox BEFORE INSERT ON webhook_deliveries
             FOR EACH ROW EXECUTE FUNCTION fail_outbox();",
        )
        .unwrap();

    // Detection: nothing is stored.
    let fresh = f002a_token_deposit("0xt2", 4, Some("0xsender-t"));
    assert!(db
        .record_erc20_deposit_atomic(
            &fresh,
            Some(&f002a_outbox("base:0xt2:4", "deposit_detected"))
        )
        .is_err());
    assert!(db
        .record_native_deposit_atomic(
            &evmhot::db::NewNativeDeposit {
                chain: "base".to_string(),
                tx_hash: "0xn1".to_string(),
                account_id: "acct-a".to_string(),
                amount: "5".to_string(),
                from_address: None,
            },
            Some(&f002a_outbox("base:0xn1", "deposit_detected")),
        )
        .is_err());
    let detected = db.get_detected_erc20_deposits("base").unwrap();
    assert_eq!(
        detected.len(),
        1,
        "the failed detection left no deposit behind"
    );
    assert_eq!(detected[0].key, "0xt1:3");
    assert_eq!(db.get_detected_deposits("base").unwrap().len(), 1);

    // Sweep completion: nothing is marked swept.
    let settlements = vec![evmhot::db::Erc20Settlement {
        key: "0xt1:3".to_string(),
        outbox: Some(f002a_outbox("base:0xt1:3", "deposit_swept")),
        notify: None,
    }];
    assert!(db.complete_erc20_sweep("base", &settlements).is_err());
    assert!(db
        .complete_native_sweep(
            "base",
            "0xn0",
            Some(&f002a_outbox("base:0xn0", "deposit_swept"))
        )
        .is_err());
    assert_eq!(db.get_detected_erc20_deposits("base").unwrap().len(), 1);
    assert_eq!(db.get_detected_deposits("base").unwrap().len(), 1);

    client
        .batch_execute("DROP TRIGGER fail_outbox ON webhook_deliveries;")
        .unwrap();
    assert_eq!(db.complete_erc20_sweep("base", &settlements).unwrap(), 1);
    assert!(db.get_detected_erc20_deposits("base").unwrap().is_empty());
}

fn assert_registration_refused_at_the_ceiling(db: &Db, set_counter: impl Fn(u64)) {
    let derive = |i: u32| -> anyhow::Result<String> { Ok(format!("0xaddr{i}")) };
    // The last index, 2^32 - 1, is still issued, once.
    set_counter(u64::from(u32::MAX));
    let (index, _, created) = db
        .register_account_auto("last", "https://x", derive)
        .unwrap();
    assert_eq!((index, created), (u32::MAX, true));

    // The counter is now 2^32: refused, never wrapped.
    let err = db
        .register_account_auto("one-too-many", "https://x", derive)
        .unwrap_err();
    assert!(err.to_string().contains("REQ-RG-5"), "{err}");
    assert!(db.get_account_by_id("one-too-many").unwrap().is_none());
    // Refusing again is stable (the counter did not move).
    assert!(db
        .register_account_auto("one-too-many", "https://x", derive)
        .is_err());
    // A repeated registration of an existing id still answers.
    let (index, _, created) = db
        .register_account_auto("last", "https://x", derive)
        .unwrap();
    assert_eq!((index, created), (u32::MAX, false));
}

#[test]
fn sqlite_req_rg_5_registration_at_2_pow_32_is_refused() {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_str().unwrap().to_string();
    let db = Db::new(&path).unwrap();
    assert_registration_refused_at_the_ceiling(&db, |value| {
        let conn = RusqliteConnection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO state (key, value) VALUES ('next_index', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [value.to_string()],
        )
        .unwrap();
    });
}

#[test]
fn postgres_req_rg_5_registration_at_2_pow_32_is_refused() {
    if !docker_available() {
        eprintln!(
            "skipping postgres_req_rg_5_registration_at_2_pow_32_is_refused: Docker not available"
        );
        return;
    }
    let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let (_container, url) = start_postgres();
    let db = connect_pg(&url, None, None);
    assert_registration_refused_at_the_ceiling(&db, |value| {
        let mut client = raw_pg_client(&url);
        client
            .execute(
                "UPDATE state SET value = $1 WHERE key = 'next_index'",
                &[&value.to_string()],
            )
            .unwrap();
    });
}

/// Detected deposits come back in insertion order (the first of a group keys
/// its counters), intents settle atomically, and the retry route moves
/// `needs_review` back to `detected`. Same on both backends.
fn assert_order_intents_and_review(db: &Db) {
    // Inserted in reverse hash order, so a primary-key order would differ.
    for (i, tx) in ["0xfff", "0xccc", "0xeee", "0xaaa", "0xddd"]
        .iter()
        .enumerate()
    {
        db.record_erc20_deposit_atomic(&f002a_token_deposit(tx, i as u64, Some("0xs")), None)
            .unwrap();
    }
    let keys: Vec<String> = db
        .get_detected_erc20_deposits("base")
        .unwrap()
        .into_iter()
        .map(|d| d.key)
        .collect();
    assert_eq!(
        keys,
        ["0xfff:0", "0xccc:1", "0xeee:2", "0xaaa:3", "0xddd:4"]
    );

    let group = vec!["0xfff:0".to_string(), "0xccc:1".to_string()];
    let id = db
        .create_sweep_intent(&evmhot::db::NewSweepIntent {
            chain: "base".to_string(),
            account_id: "acct-a".to_string(),
            address: "0xaddr-a".to_string(),
            token_address: "0xtoken1".to_string(),
            group_key: "gk".to_string(),
            keys: group.clone(),
            part_index: 0,
            amount: "20".to_string(),
            target: "0xtarget".to_string(),
        })
        .unwrap();
    assert!(db.pending_sweep_intents("base", &group).unwrap().len() == 1);
    assert!(db
        .pending_sweep_intents("base", &["0xaaa:3".to_string()])
        .unwrap()
        .is_empty());
    db.set_sweep_intent_tx(id, "0xhash").unwrap();
    assert_eq!(db.bump_sweep_intent_passes(id).unwrap(), 1);
    let pending = db.pending_sweep_intents("base", &group).unwrap();
    assert_eq!(pending[0].tx_hash.as_deref(), Some("0xhash"));
    assert_eq!(pending[0].keys, group);

    let rec = evmhot::db::SweepRecord {
        sweep_tx_hash: "0xhash".to_string(),
        sweep_block_number: 3,
        sweep_block_hash: "0xb".to_string(),
        sweep_log_index: 9,
        sweep_amount: "20".to_string(),
        sweep_target: Some("0xtarget".to_string()),
    };
    db.settle_sweep_intent("base", id, &group, &rec, &[])
        .unwrap();
    assert!(db.pending_sweep_intents("base", &group).unwrap().is_empty());
    assert_eq!(db.get_sweeps("base", "0xccc:1").unwrap(), vec![rec]);

    // needs_review is out of the detected queue, counted, and retried only by
    // the retry call.
    let review = vec!["0xaaa:3".to_string()];
    db.mark_erc20_deposits_needs_review_for_keys("base", &review, "sweep_review")
        .unwrap();
    assert_eq!(
        db.get_review_kind("base", "0xaaa:3").unwrap().as_deref(),
        Some("sweep_review")
    );
    assert_eq!(
        db.deposit_queue_counts("base").unwrap().erc20_needs_review,
        1
    );
    assert_eq!(db.get_detected_erc20_deposits("base").unwrap().len(), 4);
    assert!(db.retry_erc20_deposit("base", "0xaaa", 3).unwrap());
    assert_eq!(db.get_detected_erc20_deposits("base").unwrap().len(), 5);
    assert_eq!(db.get_review_kind("base", "0xaaa:3").unwrap(), None);
}

#[test]
fn sqlite_detected_order_intents_and_review() {
    let tmp = NamedTempFile::new().unwrap();
    let db = Db::new(tmp.path().to_str().unwrap()).unwrap();
    seed_account(&db);
    assert_order_intents_and_review(&db);
}

#[test]
fn postgres_detected_order_is_stable() {
    if !docker_available() {
        eprintln!("skipping postgres_detected_order_is_stable: Docker not available");
        return;
    }
    let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let (_container, url) = start_postgres();
    let db = connect_pg(&url, None, None);
    seed_account(&db);
    assert_order_intents_and_review(&db);
}
