mod api;
mod config;
mod db;
#[cfg(test)]
mod e2e_tests;
mod faucet;
mod monitor;
mod sweeper;
#[cfg(test)]
mod tests;
pub mod traits;
mod wallet;

use alloy::providers::{ProviderBuilder, WsConnect};
use config::{Config, ProviderUrl};
use db::Db;
use faucet::Faucet;
use monitor::Monitor;
use sweeper::Sweeper;
use traits::Service;
use wallet::Wallet;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let config = Config::from_env()?;

    // Log configuration on startup
    tracing::info!("🚀 Starting EVM Hot Wallet");
    tracing::info!("📊 Database: {}", config.database_url);

    match &config.provider_url {
        ProviderUrl::Http(url) => tracing::info!("🌐 RPC Provider (HTTP): {}", url),
        ProviderUrl::Ws(url) => tracing::info!("🌐 RPC Provider (WebSocket): {}", url),
    }
    tracing::info!("💰 Treasury Address: {}", config.treasury_address);
    tracing::info!("🚰 Faucet Address: {}", config.faucet_address);
    tracing::info!("⚡ Existential Deposit: {} wei", config.existential_deposit);
    tracing::info!("🔄 Poll Interval: {} seconds", config.poll_interval);
    tracing::info!("🌐 API Port: {}", config.port);

    let db = Db::new(&config.database_url)?;
    let wallet = Wallet::new(config.mnemonic.clone());

    match &config.provider_url {
        ProviderUrl::Http(url) => {
            let provider = ProviderBuilder::new().on_http(url.parse().expect("Invalid RPC URL"));

            let faucet = Faucet::new(
                config.faucet_mnemonic.clone(),
                provider.clone(),
                &config.existential_deposit,
            )?;

            // Spawn Monitor
            tokio::spawn({
                let config = config.clone();
                let db = db.clone();
                let provider = provider.clone();

                async move {
                    Monitor::new(config, db, provider).run().await;
                }
            });

            // Spawn Sweeper
            tokio::spawn({
                let config = config.clone();
                let db = db.clone();
                let wallet = wallet.clone();
                let provider = provider.clone();
                async move {
                    Sweeper::new(config, db, wallet, provider).run().await;
                }
            });

            // Start API (blocks forever)
            api::start_server(config, db, wallet, faucet).await;
        }

        ProviderUrl::Ws(url) => {
            let provider = ProviderBuilder::new().on_ws(WsConnect::new(url)).await?;
            let faucet = Faucet::new(
                config.faucet_mnemonic.clone(),
                provider.clone(),
                &config.existential_deposit,
            )?;

            // Spawn Monitor
            tokio::spawn({
                let config = config.clone();
                let db = db.clone();
                let provider = provider.clone();
                async move {
                    Monitor::new(config, db, provider).run().await;
                }
            });

            // Spawn Sweeper
            tokio::spawn({
                let config = config.clone();
                let db = db.clone();
                let wallet = wallet.clone();
                let provider = provider.clone();
                async move {
                    Sweeper::new(config, db, wallet, provider).run().await;
                }
            });

            // Start API (blocks forever)
            api::start_server(config, db, wallet, faucet).await;
        }
    };

    Ok(())
}
