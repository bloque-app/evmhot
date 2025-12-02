/// Example showing how to use the evm_hot_wallet library programmatically
///
/// This demonstrates using the HotWalletService as a library without the web server
use evm_hot_wallet::{config::Config, HotWalletService, RegisterRequest};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // Load configuration from environment
    let config = Config::from_env()?;

    // Create service based on provider type
    match &config.provider_url {
        evm_hot_wallet::config::ProviderUrl::Http(_) => {
            // Create HTTP service
            let service = HotWalletService::new_http(config).await?;

            // Start background services
            service.start_background_services().await?;

            // Use the service programmatically
            example_usage(&service).await?;
        }
        evm_hot_wallet::config::ProviderUrl::Ws(_) => {
            // Create WebSocket service
            let service = HotWalletService::new_ws(config).await?;

            // Start background services
            service.start_background_services().await?;

            // Use the service programmatically
            example_usage(&service).await?;
        }
    }

    Ok(())
}

async fn example_usage<T>(service: &HotWalletService<T>) -> anyhow::Result<()>
where
    T: alloy::transports::Transport + Clone + Send + Sync + 'static,
{
    // Check health
    let health = service.health().await?;
    println!("Health check: {}", health);

    // Register a new account
    let request = RegisterRequest {
        id: "example_user_123".to_string(),
        webhook_url: "https://example.com/webhook".to_string(),
    };

    let response = service.register(request).await?;
    println!("Registered address: {}", response.address);

    if let Some(tx) = response.funding_tx {
        println!("Funding transaction: {}", tx);
    }

    // Keep the program running to allow background services to work
    println!("Service is running. Background services (monitor and sweeper) are active.");
    println!("Press Ctrl+C to stop.");

    tokio::signal::ctrl_c().await?;
    println!("Shutting down...");

    Ok(())
}
