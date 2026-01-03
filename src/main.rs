use evm_hot_wallet::{config::Config, HotWalletService};

mod api;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Enhanced logging configuration with support for RUST_LOG environment variable
    // Examples:
    //   RUST_LOG=debug                                  - all debug logs
    //   RUST_LOG=alloy=debug                           - alloy debug logs
    //   RUST_LOG=evm_hot_wallet=info,alloy=debug       - app info, alloy debug
    //   RUST_LOG=evm_hot_wallet::rpc_debug=info        - RPC request/response logs
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "evm_hot_wallet=info,alloy=trace".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

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
    tracing::info!(
        "📦 Block Offset from Head: {} blocks",
        config.block_offset_from_head
    );
    tracing::info!("🌐 API Port: {}", config.port);
    tracing::info!(
        "🔐 Webhook JWT Auth: {}",
        if config.webhook_jwt_token.is_some() { "Enabled" } else { "Disabled" }
    );

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
