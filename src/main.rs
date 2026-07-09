use evm_hot_wallet::{config::Config, HotWalletService};

mod api;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "evm_hot_wallet=info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = Config::from_env()?;

    tracing::info!("Starting EVM Hot Wallet (multi-chain)");
    tracing::info!("Database: {}", config.database_url);
    tracing::info!("API Port: {}", config.port);
    tracing::info!(
        "Webhook JWT Auth: {}",
        if config.webhook_jwt_token.is_some() {
            "Enabled"
        } else {
            "Disabled"
        }
    );
    tracing::info!(
        "Webhook retries: max={}, delay_ms={}, poll_interval_s={}, batch_size={}, lease_s={}",
        config.webhook_max_retries,
        config.webhook_retry_delay_ms,
        config.webhook_retry_poll_interval_secs,
        config.webhook_retry_batch_size,
        config.webhook_lease_seconds
    );

    let faucet_address = config.derived_faucet_address()?;
    tracing::info!("Faucet address: {}", faucet_address);

    for chain in &config.chains {
        if !chain.faucet_address.eq_ignore_ascii_case(&faucet_address) {
            tracing::warn!(
                "Chain {}: chains.toml faucet_address ({}) does not match derived faucet ({})",
                chain.name,
                chain.faucet_address,
                faucet_address
            );
        }
        tracing::info!(
            "Chain: {} (id={}) rpc={} treasury={}",
            chain.name,
            chain.chain_id,
            chain.rpc_url,
            chain.treasury_address
        );
    }

    let port = config.port;
    let service = HotWalletService::new(config).await?;
    service.start_background_services().await?;
    api::start_server(service, port).await;

    Ok(())
}
