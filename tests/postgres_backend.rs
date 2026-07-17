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

use evm_hot_wallet::db::{Db, WriteQueueError};
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
fn seed_sqlite_source(next_index: i64, block_cursor: (&str, i64)) -> NamedTempFile {
    let tmp = NamedTempFile::new().unwrap();
    let mut conn = RusqliteConnection::open(tmp.path()).unwrap();
    evm_hot_wallet::db::apply_pragmas_for_import(&conn).unwrap();
    evm_hot_wallet::db::migrations().to_latest(&mut conn).unwrap();
    conn.execute(
        "INSERT INTO accounts (id, derivation_index, address, webhook_url)
         VALUES ('seed-user', 0, '0xseed', 'https://example.com/webhook')",
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
        rusqlite::params![format!("last_block:{}", block_cursor.0), block_cursor.1.to_string()],
    )
    .unwrap();
    drop(conn);
    tmp
}

/// Shared behavior assertions run against a live `Db`, regardless of
/// backend. This is the drift guard for the intentionally-duplicated SQL
/// between `db/sqlite.rs` and `db/postgres.rs` (decision D6 from the
/// Postgres-migration eng review) -- every method exercised here must behave
/// identically on both backends.
fn assert_full_crud_cycle(db: &Db) {
    // Accounts / derivation index allocation.
    let (idx0, addr0, created0) = db
        .register_account_auto("acct-a", "https://hook.example/a", |i| Ok(format!("0xaddr{i}")))
        .unwrap();
    assert_eq!(idx0, 0);
    assert_eq!(addr0, "0xaddr0");
    assert!(created0);

    let (idx1, addr1, created1) = db
        .register_account_auto("acct-b", "https://hook.example/b", |i| Ok(format!("0xaddr{i}")))
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
        Some((0, "0xaddr0".to_string(), "https://hook.example/a".to_string()))
    );
    assert_eq!(
        db.get_webhook_url("acct-b").unwrap(),
        Some("https://hook.example/b".to_string())
    );

    // Native deposits.
    assert!(db.record_deposit("base", "0xtx1", "acct-a", "1000").unwrap());
    assert!(!db.record_deposit("base", "0xtx1", "acct-a", "1000").unwrap()); // dup is a no-op
    let detected = db.get_detected_deposits("base").unwrap();
    assert_eq!(detected, vec![("0xtx1".to_string(), "acct-a".to_string(), "1000".to_string())]);

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
    db.store_token_metadata("base", "0xtoken1", "USDC", 6, "USD Coin").unwrap();
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
    assert_eq!(db.increment_zero_balance_count("base", "0xtx3:1").unwrap(), 1);
    assert_eq!(db.increment_zero_balance_count("base", "0xtx3:1").unwrap(), 2);
    db.set_sweep_tx_hash_for_keys("base", &["0xtx3:1".to_string()], "0xsweep1").unwrap();
    assert_eq!(db.increment_sweep_failure_count("base", "0xtx3:1").unwrap(), 1);
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

    assert!(db.claim_webhook_delivery("wh1", "deposit", 9_999_999_999, 5).unwrap());
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
                    .register_account_auto(&format!("concurrent-{i}"), "https://hook.example", |idx| {
                        Ok(format!("0xconcurrent{idx}"))
                    })
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
        eprintln!("skipping postgres_pool_exhaustion_maps_to_write_queue_error: Docker not available");
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
    let write_queue_err = err
        .downcast_ref::<WriteQueueError>()
        .expect("pool-exhaustion error must be a WriteQueueError (so api.rs's existing 503 \
                 mapping keeps working unchanged on the Postgres path)");
    assert!(
        matches!(write_queue_err, WriteQueueError::Timeout(_)),
        "expected WriteQueueError::Timeout, got: {write_queue_err:?}"
    );
}

#[test]
fn postgres_health_check_reflects_container_availability() {
    if !docker_available() {
        eprintln!("skipping postgres_health_check_reflects_container_availability: Docker not available");
        return;
    }
    let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let (container, url) = start_postgres();
    let db = connect_pg(&url, None, None);

    assert!(db.writer_healthy(), "writer_healthy() must be true while the container is up");

    container.stop();
    // The pool's acquire timeout (5s) plus a little slack is enough for an
    // in-flight or next `SELECT 1` to observe the outage; writer_healthy()
    // itself only waits up to its own 2s health-check timeout per call, so
    // poll briefly rather than sleeping for the full pool timeout.
    let became_unhealthy = (0..20).any(|_| {
        std::thread::sleep(Duration::from_millis(500));
        !db.writer_healthy()
    });
    assert!(became_unhealthy, "writer_healthy() must become false once the container is stopped");

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
        Some((0, "0xseed".to_string(), "https://example.com/webhook".to_string()))
    );
    assert_eq!(db1.get_last_processed_block("base").unwrap(), 12345);

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
    assert_eq!(next_idx, 7, "next_index must continue from the migrated counter, not restart at 0");

    // Second connect against the SAME (now-populated) Postgres instance,
    // with EVM_MIGRATE_FROM_SQLITE still set: must be a no-op (the
    // empty-destination guard skips migration once accounts/next_index
    // already exist), not a duplicate-insert or an overwrite.
    let db2 = connect_pg(&url, None, Some(&sqlite_path));
    assert_eq!(
        db2.get_account_by_id("post-migration-user").unwrap().map(|(idx, ..)| idx),
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
        .register_account_auto("fresh-user", "https://hook.example", |idx| Ok(format!("0xfresh{idx}")))
        .unwrap();
    assert!(created);
    assert_eq!(idx, 0, "with no source file present, next_index must start from the fresh seed (0)");
}

#[test]
fn postgres_bootstrap_migration_skips_when_env_var_unset() {
    if !docker_available() {
        eprintln!("skipping postgres_bootstrap_migration_skips_when_env_var_unset: Docker not available");
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
        evm_hot_wallet::db::apply_pragmas_for_import(&conn).unwrap();
        evm_hot_wallet::db::migrations().to_latest(&mut conn).unwrap();
    }

    set_pg_env(true, None, Some(empty_source.path().to_str().unwrap()));
    let result = Db::new(&url);
    assert!(
        result.is_err(),
        "connecting with EVM_MIGRATE_FROM_SQLITE pointed at an empty/freshly-seeded source \
         must fail startup, not silently boot against an empty Postgres"
    );
}
