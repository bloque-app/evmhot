// Library modules
pub mod config;
pub mod db;
mod faucet;
mod monitor;
mod sweeper;
pub mod traits;
mod wallet;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod e2e_tests;

use alloy::providers::{ProviderBuilder, WsConnect};
use alloy::transports::Transport;
use config::{Config, ProviderUrl};
use db::Db;
use faucet::Faucet;
use monitor::Monitor;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use sweeper::Sweeper;
use traits::Service;
use wallet::Wallet;

/// Request structure for registering a new account
#[derive(Deserialize, Clone)]
pub struct RegisterRequest {
    pub id: String,
    pub webhook_url: String,
}

/// Response structure for account registration
#[derive(Serialize, Clone)]
pub struct RegisterResponse {
    pub address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub funding_tx: Option<String>,
}

/// Core Hot Wallet Service that manages background tasks and provides account registration
pub struct HotWalletService<T>
where
    T: Transport + Clone + Send + Sync + 'static,
{
    config: Config,
    db: Db,
    wallet: Wallet,
    faucet: Arc<Faucet<alloy::providers::RootProvider<T>>>,
}

impl<T> HotWalletService<T>
where
    T: Transport + Clone + Send + Sync + 'static,
{
    /// Get a reference to the database
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Get a reference to the configuration
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Health check method - returns Ok if service is healthy
    pub async fn health(&self) -> anyhow::Result<String> {
        Ok("OK".to_string())
    }

    /// Register a new account with the hot wallet service
    /// Returns the derived address and optionally a funding transaction hash
    pub async fn register(&self, request: RegisterRequest) -> anyhow::Result<RegisterResponse> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        use tracing::{error, info};

        // Check if account already exists
        if let Ok(Some((_index, existing_address, _webhook))) =
            self.db.get_account_by_id(&request.id)
        {
            info!(
                "Account {} already exists with address {}",
                request.id, existing_address
            );
            return Ok(RegisterResponse {
                address: existing_address,
                funding_tx: None,
            });
        }

        // Derive deterministic index from account_id using hash
        let mut hasher = DefaultHasher::new();
        request.id.hash(&mut hasher);
        let hash = hasher.finish();
        let index = (hash & 0x7FFFFFFF) as u32;

        // Derive address from the deterministic index
        let address = self.wallet.derive_address(index)?;
        let address_str = address.to_string();

        // Save to DB with webhook URL
        self.db
            .register_account(&request.id, index, &address_str, &request.webhook_url)?;

        info!(
            "Registered account {} with address {} (index: {})",
            request.id, address_str, index
        );

        // Fire-and-forget: Fund the new address with existential deposit in the background
        let faucet = Arc::clone(&self.faucet);
        let db = self.db.clone();
        let account_id = request.id.clone();
        let address_for_funding = address_str.clone();

        tokio::spawn(async move {
            info!(
                "Background task: Starting faucet funding for address {}",
                address_for_funding
            );

            match faucet.fund_new_address(&address_for_funding).await {
                Ok(tx_hash) => {
                    info!(
                        "Successfully funded address {} with tx: {}",
                        address_for_funding, tx_hash
                    );

                    // Send webhook notification for successful funding
                    if let Err(e) = send_faucet_funding_webhook(
                        &db,
                        &account_id,
                        &address_for_funding,
                        &tx_hash,
                        true,
                        None,
                    )
                    .await
                    {
                        error!(
                            "Failed to send faucet funding webhook for {}: {:?}",
                            account_id, e
                        );
                    }
                }
                Err(e) => {
                    error!(
                        "Failed to fund address {}: {:?}",
                        address_for_funding, e
                    );

                    // Send webhook notification for failed funding
                    if let Err(webhook_err) = send_faucet_funding_webhook(
                        &db,
                        &account_id,
                        &address_for_funding,
                        "",
                        false,
                        Some(&e.to_string()),
                    )
                    .await
                    {
                        error!(
                            "Failed to send faucet funding error webhook for {}: {:?}",
                            account_id, webhook_err
                        );
                    }
                }
            }
        });

        Ok(RegisterResponse {
            address: address_str,
            funding_tx: None, // No longer waiting for funding - it's fire-and-forget
        })
    }
}

// HTTP Provider implementation
impl HotWalletService<alloy::transports::http::Http<reqwest::Client>> {
    /// Create a new HotWalletService with HTTP provider from configuration
    pub async fn new_http(config: Config) -> anyhow::Result<Self> {
        let db = Db::new(&config.database_url)?;
        let wallet = Wallet::new(config.mnemonic.clone());

        let url = match &config.provider_url {
            ProviderUrl::Http(url) => url,
            _ => return Err(anyhow::anyhow!("Expected HTTP provider URL")),
        };

        let provider = ProviderBuilder::new().on_http(url.parse()?);
        let faucet = Faucet::new(
            config.faucet_mnemonic.clone(),
            provider,
            &config.existential_deposit,
        )?;

        Ok(Self {
            config,
            db,
            wallet,
            faucet: Arc::new(faucet),
        })
    }

    /// Start background services (Monitor and Sweeper) for HTTP provider
    /// Returns immediately after spawning the background tasks
    pub async fn start_background_services(&self) -> anyhow::Result<()> {
        let url = match &self.config.provider_url {
            ProviderUrl::Http(url) => url,
            _ => return Err(anyhow::anyhow!("Expected HTTP provider URL")),
        };

        let provider = ProviderBuilder::new().on_http(url.parse()?);

        // Spawn Monitor
        tokio::spawn({
            let config = self.config.clone();
            let db = self.db.clone();
            let provider = provider.clone();

            async move {
                Monitor::new(config, db, provider).run().await;
            }
        });

        // Spawn Sweeper
        tokio::spawn({
            let config = self.config.clone();
            let db = self.db.clone();
            let wallet = self.wallet.clone();
            let provider = provider.clone();
            async move {
                Sweeper::new(config, db, wallet, provider).run().await;
            }
        });

        Ok(())
    }
}

// WebSocket Provider implementation
impl HotWalletService<alloy::pubsub::PubSubFrontend> {
    /// Create a new HotWalletService with WebSocket provider from configuration
    pub async fn new_ws(config: Config) -> anyhow::Result<Self> {
        let db = Db::new(&config.database_url)?;
        let wallet = Wallet::new(config.mnemonic.clone());

        let url = match &config.provider_url {
            ProviderUrl::Ws(url) => url,
            _ => return Err(anyhow::anyhow!("Expected WebSocket provider URL")),
        };

        let provider = ProviderBuilder::new().on_ws(WsConnect::new(url)).await?;
        let faucet = Faucet::new(
            config.faucet_mnemonic.clone(),
            provider,
            &config.existential_deposit,
        )?;

        Ok(Self {
            config,
            db,
            wallet,
            faucet: Arc::new(faucet),
        })
    }

    /// Start background services (Monitor and Sweeper) for WebSocket provider
    /// Returns immediately after spawning the background tasks
    pub async fn start_background_services(&self) -> anyhow::Result<()> {
        let url = match &self.config.provider_url {
            ProviderUrl::Ws(url) => url,
            _ => return Err(anyhow::anyhow!("Expected WebSocket provider URL")),
        };

        let provider = ProviderBuilder::new().on_ws(WsConnect::new(url)).await?;

        // Spawn Monitor
        tokio::spawn({
            let config = self.config.clone();
            let db = self.db.clone();
            let provider = provider.clone();
            async move {
                Monitor::new(config, db, provider).run().await;
            }
        });

        // Spawn Sweeper
        tokio::spawn({
            let config = self.config.clone();
            let db = self.db.clone();
            let wallet = self.wallet.clone();
            let provider = provider.clone();
            async move {
                Sweeper::new(config, db, wallet, provider).run().await;
            }
        });

        Ok(())
    }
}

/// Send webhook notification for faucet funding event
async fn send_faucet_funding_webhook(
    db: &Db,
    account_id: &str,
    address: &str,
    tx_hash: &str,
    success: bool,
    error_message: Option<&str>,
) -> anyhow::Result<()> {
    use tracing::{error, info};

    // Get the webhook URL for this account
    let webhook_url = match db.get_webhook_url(account_id)? {
        Some(url) => url,
        None => {
            error!("No webhook URL found for account: {}", account_id);
            return Ok(());
        }
    };

    let client = reqwest::Client::new();

    let mut payload = serde_json::json!({
        "event": "faucet_funding",
        "account_id": account_id,
        "address": address,
        "success": success,
    });

    // Add tx_hash if funding was successful
    if success && !tx_hash.is_empty() {
        payload["tx_hash"] = serde_json::json!(tx_hash);
    }

    // Add error message if funding failed
    if let Some(error) = error_message {
        payload["error"] = serde_json::json!(error);
    }

    let res = client.post(&webhook_url).json(&payload).send().await;

    match res {
        Ok(r) => info!(
            "Faucet funding webhook sent to {}: status={}",
            webhook_url,
            r.status()
        ),
        Err(e) => error!(
            "Failed to send faucet funding webhook to {}: {:?}",
            webhook_url, e
        ),
    }

    Ok(())
}

