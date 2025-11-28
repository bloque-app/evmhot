mod api;
mod config;
mod db;
#[cfg(test)]
mod e2e_tests;
mod monitor;
mod sweeper;
#[cfg(test)]
mod tests;
pub mod traits;
mod wallet;

use alloy::providers::{ProviderBuilder, WsConnect};
use config::{Config, ProviderUrl};
use db::Db;
use monitor::Monitor;
use sweeper::Sweeper;
use traits::Service;
use wallet::Wallet;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let config = Config::from_env()?;
    let db = Db::new(&config.database_url)?;
    let wallet = Wallet::new(config.mnemonic.clone());

    let (monitor, sweeper): (Box<dyn Service>, Box<dyn Service>) = match &config.provider_url {
        ProviderUrl::Http(url) => {
            let provider = ProviderBuilder::new().on_http(url.parse().expect("Invalid RPC URL"));

            (
                Box::new(Monitor::new(config.clone(), db.clone(), provider.clone())),
                Box::new(Sweeper::new(
                    config.clone(),
                    db.clone(),
                    wallet.clone(),
                    provider.clone(),
                )),
            )
        }
        ProviderUrl::Ws(url) => {
            let provider = ProviderBuilder::new().on_ws(WsConnect::new(url)).await?;

            (
                Box::new(Monitor::new(config.clone(), db.clone(), provider.clone())),
                Box::new(Sweeper::new(
                    config.clone(),
                    db.clone(),
                    wallet.clone(),
                    provider.clone(),
                )),
            )
        }
    };

    // Spawn Services
    tokio::spawn(async move {
        monitor.run().await;
    });

    tokio::spawn(async move {
        sweeper.run().await;
    });

    // Start API
    api::start_server(config, db, wallet).await;

    Ok(())
}
