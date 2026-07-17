use anyhow::{Context, Result};
use clap::Parser;
use evm_hot_wallet::sqlite_import::{
    ensure_postgres_schema, migrate_snapshot_to_postgres, read_sqlite_snapshot, verify_migration,
};
use postgres::{Client, NoTls};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "migrate_sqlite_to_postgres",
    about = "One-shot (or --verify, repeatable) migration from evmhot's SQLite wallet.db to Postgres"
)]
struct Args {
    #[arg(
        long,
        help = "Path to the source SQLite file (opened read-only, never modified)"
    )]
    from: PathBuf,
    #[arg(long, help = "Destination Postgres connection string (postgres://...)")]
    to: String,
    #[arg(
        long,
        default_value = "verify-ca",
        help = "TLS mode for the destination connection: 'verify-ca' (default, uses the bundled AWS RDS CA) or 'disable' (plain TCP, for local/Docker Postgres in dry runs)"
    )]
    tls: String,
    #[arg(
        long,
        default_value_t = false,
        help = "Re-run migration inserts even if the destination already has data (inserts are idempotent via ON CONFLICT DO NOTHING; use this to top up a partially-migrated scratch DB)"
    )]
    force: bool,
    #[arg(
        long,
        default_value_t = false,
        help = "Read-only mode: compare source vs. destination instead of migrating, and exit non-zero on any mismatch"
    )]
    verify: bool,
}

fn connect(database_url: &str, tls: &str) -> Result<Client> {
    if tls.eq_ignore_ascii_case("disable") {
        Client::connect(database_url, NoTls).context("failed to connect to Postgres (NoTls)")
    } else {
        use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
        use postgres_openssl::MakeTlsConnector;
        use std::io::Write;

        const RDS_CA_BUNDLE: &str = include_str!("../../certs/rds-global-bundle.pem");
        let mut builder = SslConnector::builder(SslMethod::tls())?;
        builder.set_verify(SslVerifyMode::PEER);
        let mut ca_file = tempfile::NamedTempFile::new()?;
        ca_file.write_all(RDS_CA_BUNDLE.as_bytes())?;
        builder.set_ca_file(ca_file.path())?;
        let connector = MakeTlsConnector::new(builder.build());
        Client::connect(database_url, connector)
            .context("failed to connect to Postgres (TLS verify-ca)")
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let mut client = connect(&args.to, &args.tls)?;

    if args.verify {
        println!("Reading source snapshot: {}", args.from.display());
        let snapshot = read_sqlite_snapshot(&args.from)?;
        println!(
            "Verifying against destination: {}",
            mask_credentials(&args.to)
        );
        let report = verify_migration(&snapshot, &mut client)?;
        if report.is_match() {
            println!("VERIFY OK: source and destination match on every checked field.");
            return Ok(());
        }
        println!("VERIFY FAILED: {} mismatch(es):", report.mismatches.len());
        for m in &report.mismatches {
            println!(
                "  {}: source={} destination={}",
                m.field, m.source, m.destination
            );
        }
        std::process::exit(1);
    }

    println!(
        "Ensuring destination schema exists: {}",
        mask_credentials(&args.to)
    );
    ensure_postgres_schema(&mut client)?;

    if !args.force {
        let accounts_count: i64 = client
            .query_one("SELECT COUNT(*) FROM accounts", &[])?
            .get(0);
        if accounts_count > 0 {
            anyhow::bail!(
                "destination already has {accounts_count} accounts; pass --force to top it up \
                 (migration inserts are idempotent via ON CONFLICT DO NOTHING) or point --to at \
                 an empty database"
            );
        }
    }

    println!("Reading source snapshot: {}", args.from.display());
    let snapshot = read_sqlite_snapshot(&args.from)?;
    println!(
        "Migrating {} accounts, {} deposits, {} erc20_deposits, {} token_metadata rows, \
         {} sweep_meta, {} sweep_failures, {} webhook_deliveries...",
        snapshot.accounts.len(),
        snapshot.deposits.len(),
        snapshot.erc20_deposits.len(),
        snapshot.token_metadata.len(),
        snapshot.sweep_meta.len(),
        snapshot.sweep_failures.len(),
        snapshot.webhook_deliveries.len(),
    );

    let summary = migrate_snapshot_to_postgres(&snapshot, &mut client)?;

    println!(
        "Migration complete: {} -> {}",
        args.from.display(),
        mask_credentials(&args.to)
    );
    println!(
        "accounts: {} sqlite -> {} inserted",
        summary.accounts.0, summary.accounts.1
    );
    println!(
        "deposits: {} sqlite -> {} inserted",
        summary.deposits.0, summary.deposits.1
    );
    println!(
        "erc20_deposits: {} sqlite -> {} inserted",
        summary.erc20_deposits.0, summary.erc20_deposits.1
    );
    println!(
        "token_metadata: {} sqlite -> {} inserted",
        summary.token_metadata.0, summary.token_metadata.1
    );
    println!(
        "sweep_meta: {} sqlite -> {} inserted",
        summary.sweep_meta.0, summary.sweep_meta.1
    );
    println!(
        "sweep_failures: {} sqlite -> {} inserted",
        summary.sweep_failures.0, summary.sweep_failures.1
    );
    println!(
        "webhook_deliveries: {} sqlite -> {} inserted",
        summary.webhook_deliveries.0, summary.webhook_deliveries.1
    );
    if let Some(next_index) = summary.next_index {
        println!("next_index: {next_index}");
    }
    println!("block cursors:");
    for (chain, block) in &summary.block_cursors {
        println!("  last_block:{chain} = {block}");
    }
    println!("\nRun with --verify to re-check source vs. destination.");

    Ok(())
}

/// Never print a connection string with a password in it to stdout/logs.
fn mask_credentials(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let (scheme, rest) = url.split_at(scheme_end + 3);
        if let Some(at) = rest.find('@') {
            let (creds, host_and_rest) = rest.split_at(at);
            if let Some(colon) = creds.find(':') {
                let user = &creds[..colon];
                return format!("{scheme}{user}:***{host_and_rest}");
            }
        }
    }
    url.to_string()
}
