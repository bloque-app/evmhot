/// Example showing how to use the evm_hot_wallet library programmatically
use evm_hot_wallet::{config::Config, HotWalletService, RegisterRequest};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let config = Config::from_env()?;
    let service = HotWalletService::new(config).await?;
    service.start_background_services().await?;
    example_usage(&service).await?;
    Ok(())
}

async fn example_usage(service: &HotWalletService) -> anyhow::Result<()> {
    let health = service.health().await?;
    println!("Health check: {}", health);

    let request = RegisterRequest {
        id: "example_user_123".to_string(),
        webhook_url: "https://example.com/webhook".to_string(),
    };

    let response = service.register(request).await?;
    println!("Registered address: {}", response.address);

    println!("Service is running. Background services (monitor and sweeper) are active.");
    println!("Press Ctrl+C to stop.");

    tokio::signal::ctrl_c().await?;
    println!("Shutting down...");

    Ok(())
}
