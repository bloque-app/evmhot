use evm_hot_wallet::{config::Config, HotWalletService};

mod api;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let config = Config::from_env()?;

    // Log configuration on startup
    tracing::info!("🚀 Starting EVM Hot Wallet");
    tracing::info!("📊 Database: {}", config.database_url);

    match &config.provider_url {
        evm_hot_wallet::config::ProviderUrl::Http(url) => {
            tracing::info!("🌐 RPC Provider (HTTP): {}", url)
        }
        evm_hot_wallet::config::ProviderUrl::Ws(url) => {
            tracing::info!("🌐 RPC Provider (WebSocket): {}", url)
        }
    }
    tracing::info!("💰 Treasury Address: {}", config.treasury_address);
    tracing::info!("🚰 Faucet Address: {}", config.faucet_address);
    tracing::info!("⚡ Existential Deposit: {} wei", config.existential_deposit);
    tracing::info!("🔄 Poll Interval: {} seconds", config.poll_interval);
    tracing::info!("🌐 API Port: {}", config.port);

    let port = config.port;

    match &config.provider_url {
        evm_hot_wallet::config::ProviderUrl::Http(_) => {
            // Create the service with HTTP provider
            let service = HotWalletService::new_http(config).await?;

            // Start background services
            service.start_background_services().await?;

            // Start API server (blocks forever)
            api::start_server(service, port).await;
        }

        evm_hot_wallet::config::ProviderUrl::Ws(_) => {
            // Create the service with WebSocket provider
            let service = HotWalletService::new_ws(config).await?;

            // Start background services
            service.start_background_services().await?;

            // Start API server (blocks forever)
            api::start_server(service, port).await;
        }
    };

    Ok(())
}
