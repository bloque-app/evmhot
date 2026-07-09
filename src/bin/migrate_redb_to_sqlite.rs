use clap::Parser;
use evm_hot_wallet::redb_import::migrate_redb_file_to_sqlite;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "migrate_redb_to_sqlite",
    about = "One-shot migration from redb to SQLite"
)]
struct Args {
    #[arg(long, help = "Path to existing redb database file")]
    from: PathBuf,
    #[arg(long, help = "Path for new SQLite database file")]
    to: PathBuf,
    #[arg(
        long,
        default_value = "polygon",
        help = "Legacy chain name for v1 key namespacing"
    )]
    legacy_chain: String,
    #[arg(long, default_value_t = false, help = "Overwrite existing SQLite file")]
    force: bool,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    let summary =
        migrate_redb_file_to_sqlite(&args.from, &args.to, &args.legacy_chain, args.force)?;

    println!(
        "Migration complete: {} -> {}",
        args.from.display(),
        args.to.display()
    );
    println!(
        "accounts: {} redb -> {} inserted",
        summary.accounts.0, summary.accounts.1
    );
    println!(
        "deposits: {} redb -> {} inserted",
        summary.deposits.0, summary.deposits.1
    );
    println!(
        "erc20_deposits: {} redb -> {} inserted",
        summary.erc20_deposits.0, summary.erc20_deposits.1
    );
    println!(
        "token_metadata: {} redb -> {} inserted",
        summary.token_metadata.0, summary.token_metadata.1
    );
    println!(
        "state: {} redb -> {} inserted",
        summary.state.0, summary.state.1
    );
    println!(
        "sweep_meta: {} redb -> {} inserted",
        summary.sweep_meta.0, summary.sweep_meta.1
    );
    println!(
        "sweep_failures: {} redb -> {} inserted",
        summary.sweep_failures.0, summary.sweep_failures.1
    );
    if summary.orphan_address_mappings > 0 {
        println!(
            "orphan address_to_id mappings (no account row): {}",
            summary.orphan_address_mappings
        );
    }
    println!("block cursors:");
    for (chain, block) in &summary.block_cursors {
        println!("  last_block:{chain} = {block}");
    }

    Ok(())
}
