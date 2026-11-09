//! Offline gap scan: finds token deposits evmhot missed while it was down.
//! Dry-run by default; see the README ("Gap scan") for the runbook.

use clap::Parser;
use evmhot::config::Config;
use evmhot::db::Db;
use evmhot::gapscan::{self, RunOptions, ScanOptions, MULTICALL3};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "evmhot-gapscan",
    about = "Find token deposits evmhot missed while it was down (balance-first, then targeted logs)"
)]
struct Cli {
    /// Network to scan (as named in chains.toml). Repeatable; default: all.
    #[arg(long = "chain")]
    chains: Vec<String>,
    /// First block of the gap. Default: the network's stored cursor.
    #[arg(long)]
    from: Option<u64>,
    /// Block B to read balances at and scan up to. Default: head minus the
    /// network's block_offset_from_head. Needs exactly one --chain.
    #[arg(long)]
    block: Option<u64>,
    /// Addresses per Multicall3 call.
    #[arg(long, default_value_t = 500)]
    batch_size: usize,
    /// Blocks per eth_getLogs range before any shrinking.
    #[arg(long, default_value_t = 50_000)]
    log_range: u64,
    /// Recipient addresses per eth_getLogs filter.
    #[arg(long, default_value_t = 20)]
    log_addresses: usize,
    /// Attempts per RPC call before a query counts as refused.
    #[arg(long, default_value_t = 3)]
    retries: u32,
    /// Refused log queries tolerated per network before giving up.
    #[arg(long, default_value_t = 500)]
    max_refusals: u64,
    /// Store the deposits found (default is a dry run that writes nothing).
    #[arg(long)]
    apply: bool,
    /// With --apply: afterwards move the network's cursor to B. Needs --block
    /// and exactly one --chain.
    #[arg(long)]
    set_cursor: bool,
    /// Reserved for the native coin. Not implemented (scan_native is off).
    #[arg(long)]
    include_native: bool,
    /// Multicall3 address.
    #[arg(long, default_value_t = MULTICALL3)]
    multicall: alloy::primitives::Address,
    /// Where to write the CSV report.
    #[arg(long, default_value = "gapscan-report.csv")]
    csv: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "evmhot=info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();
    // Same chains.toml / DATABASE_URL as the main binary; no mnemonics needed.
    let config = Config::from_env_without_mnemonics()?;

    let db_url = config.database_url.clone();
    let pool = config.db_read_pool_size;
    // Db init drives a nested runtime for Postgres: keep it off the async threads.
    let db = tokio::task::spawn_blocking(move || Db::with_pool_size(&db_url, pool)).await??;

    let opts = RunOptions {
        scan: ScanOptions {
            chains: cli.chains,
            from_block: cli.from,
            block: cli.block,
            batch_size: cli.batch_size,
            log_range: cli.log_range,
            log_addresses: cli.log_addresses,
            retries: cli.retries,
            max_refusals: cli.max_refusals,
            apply: cli.apply,
            set_cursor: cli.set_cursor,
            include_native: cli.include_native,
            multicall: cli.multicall,
        },
        csv: cli.csv.clone(),
    };
    let reports = gapscan::run(&config, &db, &opts).await?;
    println!("{}", gapscan::render_summary(&reports, cli.apply));
    println!("CSV written to {}", cli.csv.display());
    Ok(())
}
