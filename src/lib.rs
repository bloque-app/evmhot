// Library modules
pub mod config;
pub mod db;
pub(crate) mod faucet;
mod monitor;
mod sweeper;
pub mod traits;
mod wallet;

#[cfg(test)]
mod e2e_tests;
#[cfg(test)]
mod tests;

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

/// Request structure for verifying a transfer
#[derive(Deserialize, Clone, Debug)]
pub struct VerifyTransferRequest {
    /// Transaction hash to verify
    pub tx_hash: String,
    /// Expected recipient address
    pub to_address: String,
    /// Expected amount (as string to handle large numbers)
    pub amount: String,
    /// Token type: "native" for ETH/native currency, or "erc20" for ERC20 tokens
    /// Defaults to "native" if not specified
    #[serde(default = "default_token_type")]
    pub token_type: String,
    /// Token contract address (required for ERC20)
    #[serde(default)]
    pub token_address: Option<String>,
    /// Token symbol (optional, for additional validation with ERC20)
    #[serde(default)]
    pub token_symbol: Option<String>,
}

fn default_token_type() -> String {
    "native".to_string()
}

/// Response structure for transfer verification
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum VerifyTransferResponse {
    /// Transfer was successfully verified
    Success {
        /// Actual recipient address found in the transaction
        actual_to: String,
        /// Actual amount found in the transaction
        actual_amount: String,
        /// Token type ("native" or "erc20")
        token_type: String,
        /// Token symbol (for ERC20)
        #[serde(skip_serializing_if = "Option::is_none")]
        token_symbol: Option<String>,
        /// Block number where the transaction was included
        #[serde(skip_serializing_if = "Option::is_none")]
        block_number: Option<u64>,
    },
    /// Transfer verification failed
    Error {
        /// Error message describing why verification failed
        message: String,
        /// Token type ("native" or "erc20") if known
        #[serde(skip_serializing_if = "Option::is_none")]
        token_type: Option<String>,
        /// Block number where the transaction was included (if found)
        #[serde(skip_serializing_if = "Option::is_none")]
        block_number: Option<u64>,
    },
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
    provider: alloy::providers::RootProvider<T>,
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

    /// Set the last processed block number manually
    pub fn set_block_number(&self, block_number: u64) -> anyhow::Result<()> {
        self.db.set_last_processed_block(block_number)
    }

    /// Get the current last processed block number
    pub fn get_block_number(&self) -> anyhow::Result<u64> {
        self.db.get_last_processed_block()
    }

    /// Verify if a transaction contains a transfer matching the expected criteria
    pub async fn verify_transfer(
        &self,
        request: VerifyTransferRequest,
    ) -> anyhow::Result<VerifyTransferResponse> {
        use alloy::primitives::{Address, FixedBytes, U256};
        use std::str::FromStr;
        use tracing::info;

        info!("Verifying transfer: {:?}", request);

        // Parse the transaction hash
        let tx_hash: FixedBytes<32> = request
            .tx_hash
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid transaction hash format"))?;

        // Parse expected values
        let expected_to = Address::from_str(&request.to_address)
            .map_err(|_| anyhow::anyhow!("Invalid to_address format"))?;
        let expected_amount = U256::from_str(&request.amount)
            .map_err(|_| anyhow::anyhow!("Invalid amount format"))?;

        // Determine if this is a native or ERC20 transfer based on token_type
        let is_native = request.token_type.to_lowercase() == "native";

        if is_native {
            // Verify native ETH transfer
            self.verify_native_transfer(tx_hash, expected_to, expected_amount)
                .await
        } else {
            // Verify ERC20 transfer - token_address is required
            let token_address_str = request
                .token_address
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("token_address is required for ERC20 transfers"))?;

            let token_address = Address::from_str(token_address_str)
                .map_err(|_| anyhow::anyhow!("Invalid token_address format"))?;

            self.verify_erc20_transfer(
                tx_hash,
                expected_to,
                expected_amount,
                token_address,
                request.token_symbol.as_deref(),
            )
            .await
        }
    }

    async fn verify_native_transfer(
        &self,
        tx_hash: alloy::primitives::FixedBytes<32>,
        expected_to: alloy::primitives::Address,
        expected_amount: alloy::primitives::U256,
    ) -> anyhow::Result<VerifyTransferResponse> {
        use alloy::providers::Provider;
        use tracing::info;

        // Fetch the transaction
        let tx = self
            .provider
            .get_transaction_by_hash(tx_hash)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Transaction not found"))?;

        info!("Found transaction: {:?}", tx.hash);

        // Get block number from transaction receipt for confirmation
        let receipt = self.provider.get_transaction_receipt(tx_hash).await?;
        let block_number = receipt.as_ref().and_then(|r| r.block_number);

        // Check if transaction was successful
        if let Some(ref r) = receipt {
            if !r.status() {
                return Ok(VerifyTransferResponse::Error {
                    message: "Transaction failed (reverted)".to_string(),
                    token_type: Some("native".to_string()),
                    block_number,
                });
            }
        }

        // For native transfers, check the `to` field and `value` field
        let actual_to = tx.to;
        let actual_amount = tx.value;

        let to_matches = actual_to
            .map(|to| {
                to.to_string()
                    .eq_ignore_ascii_case(&expected_to.to_string())
            })
            .unwrap_or(false);
        let amount_matches = actual_amount >= expected_amount;

        if to_matches && amount_matches {
            Ok(VerifyTransferResponse::Success {
                actual_to: actual_to.map(|a| a.to_string()).unwrap_or_default(),
                actual_amount: actual_amount.to_string(),
                token_type: "native".to_string(),
                token_symbol: None,
                block_number,
            })
        } else {
            Ok(VerifyTransferResponse::Error {
                message: format!(
                    "Mismatch: to_matches={}, amount_matches={} (expected >= {})",
                    to_matches, amount_matches, expected_amount
                ),
                token_type: Some("native".to_string()),
                block_number,
            })
        }
    }

    async fn verify_erc20_transfer(
        &self,
        tx_hash: alloy::primitives::FixedBytes<32>,
        expected_to: alloy::primitives::Address,
        expected_amount: alloy::primitives::U256,
        token_address: alloy::primitives::Address,
        expected_symbol: Option<&str>,
    ) -> anyhow::Result<VerifyTransferResponse> {
        use alloy::primitives::{Address, FixedBytes, U256};
        use alloy::providers::Provider;
        use tracing::info;

        // Fetch the transaction receipt to get logs
        let receipt = self
            .provider
            .get_transaction_receipt(tx_hash)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Transaction receipt not found"))?;

        let block_number = receipt.block_number;

        // Check if transaction was successful
        if !receipt.status() {
            return Ok(VerifyTransferResponse::Error {
                message: "Transaction failed (reverted)".to_string(),
                token_type: Some("erc20".to_string()),
                block_number,
            });
        }

        // Fetch token symbol from chain if we need to validate it
        let actual_symbol = self.fetch_token_symbol(token_address).await.ok();

        // Validate token symbol if provided
        if let Some(expected) = expected_symbol {
            if let Some(ref actual) = actual_symbol {
                if !actual.eq_ignore_ascii_case(expected) {
                    return Ok(VerifyTransferResponse::Error {
                        message: format!(
                            "Token symbol mismatch: expected '{}', got '{}'",
                            expected, actual
                        ),
                        token_type: Some("erc20".to_string()),
                        block_number,
                    });
                }
            }
        }

        // ERC20 Transfer event signature: Transfer(address,address,uint256)
        let transfer_signature: FixedBytes<32> =
            alloy::primitives::keccak256("Transfer(address,address,uint256)".as_bytes());

        // Look for Transfer events from the specified token
        for log in receipt.inner.logs() {
            // Check if this is from the expected token contract
            if log.address() != token_address {
                continue;
            }

            // Check if this is a Transfer event
            if log.topics().len() < 3 || log.topics()[0] != transfer_signature {
                continue;
            }

            // Decode Transfer event: topic[1] = from, topic[2] = to
            let to_address = Address::from_slice(&log.topics()[2].as_slice()[12..]);

            // Decode amount from data
            let amount = if !log.data().data.is_empty() {
                U256::from_be_slice(&log.data().data)
            } else {
                U256::ZERO
            };

            info!(
                "Found ERC20 Transfer: to={}, amount={}, symbol={:?}",
                to_address, amount, actual_symbol
            );

            // Check if this transfer matches our criteria
            let to_matches = to_address
                .to_string()
                .eq_ignore_ascii_case(&expected_to.to_string());
            let amount_matches = amount >= expected_amount;

            if to_matches && amount_matches {
                return Ok(VerifyTransferResponse::Success {
                    actual_to: to_address.to_string(),
                    actual_amount: amount.to_string(),
                    token_type: "erc20".to_string(),
                    token_symbol: actual_symbol,
                    block_number,
                });
            }
        }

        // No matching transfer found
        Ok(VerifyTransferResponse::Error {
            message: format!(
                "No matching ERC20 Transfer event found to {} with amount >= {}",
                expected_to, expected_amount
            ),
            token_type: Some("erc20".to_string()),
            block_number,
        })
    }

    /// Fetch token symbol from the blockchain
    async fn fetch_token_symbol(
        &self,
        token_address: alloy::primitives::Address,
    ) -> anyhow::Result<String> {
        use alloy::sol;

        sol! {
            #[sol(rpc)]
            contract IERC20Symbol {
                function symbol() external view returns (string memory);
            }
        }

        let contract = IERC20Symbol::new(token_address, &self.provider);
        let symbol = contract.symbol().call().await?._0;
        Ok(symbol)
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
        let webhook_jwt_token = self.config.webhook_jwt_token.clone();

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
                        webhook_jwt_token.as_deref(),
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
                    error!("Failed to fund address {}: {:?}", address_for_funding, e);

                    // Send webhook notification for failed funding
                    if let Err(webhook_err) = send_faucet_funding_webhook(
                        &db,
                        &account_id,
                        &address_for_funding,
                        "",
                        false,
                        Some(&e.to_string()),
                        webhook_jwt_token.as_deref(),
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
            provider.clone(),
            &config.existential_deposit,
        )?;

        Ok(Self {
            config,
            db,
            wallet,
            faucet: Arc::new(faucet),
            provider,
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
                tracing::info!("Starting Monitor in Polling mode");
                Monitor::new(config, db, provider).run().await;
            }
        });

        // Create faucet for sweeper
        let sweeper_faucet = Arc::new(Faucet::new(
            self.config.faucet_mnemonic.clone(),
            provider.clone(),
            &self.config.existential_deposit,
        )?);

        // Spawn Sweeper
        tokio::spawn({
            let config = self.config.clone();
            let db = self.db.clone();
            let wallet = self.wallet.clone();
            let provider = provider.clone();
            let faucet = sweeper_faucet;
            async move {
                tracing::info!("Starting Sweeper in Polling mode");
                Sweeper::new(config, db, wallet, provider, faucet).run().await;
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
            provider.clone(),
            &config.existential_deposit,
        )?;

        Ok(Self {
            config,
            db,
            wallet,
            faucet: Arc::new(faucet),
            provider,
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
                tracing::info!("Starting Monitor in Streaming mode");
                Monitor::new(config, db, provider).run().await;
            }
        });

        // Create faucet for sweeper
        let sweeper_faucet = Arc::new(Faucet::new(
            self.config.faucet_mnemonic.clone(),
            provider.clone(),
            &self.config.existential_deposit,
        )?);

        // Spawn Sweeper
        tokio::spawn({
            let config = self.config.clone();
            let db = self.db.clone();
            let wallet = self.wallet.clone();
            let provider = provider.clone();
            let faucet = sweeper_faucet;
            async move {
                tracing::info!("Starting Sweeper in Streaming mode");
                Sweeper::new(config, db, wallet, provider, faucet).run().await;
            }
        });

        Ok(())
    }
}

/// Send webhook notification for faucet funding event
/// registration_id: The original id used when registering the account
/// address: The Polygon address (account_id in webhook)
/// jwt_token: Optional JWT token for authorization header
async fn send_faucet_funding_webhook(
    db: &Db,
    registration_id: &str,
    address: &str,
    tx_hash: &str,
    success: bool,
    error_message: Option<&str>,
    jwt_token: Option<&str>,
) -> anyhow::Result<()> {
    use tracing::{error, info};

    // Get the webhook URL using registration_id (the key in ACCOUNTS table)
    let Some(webhook_url) = db.get_webhook_url(registration_id)? else {
        error!("No webhook URL found for registration_id: {}", registration_id);
        return Ok(());
    };

    let client = reqwest::Client::new();

    let mut payload = serde_json::json!({
        "event": "faucet_funding",
        "account_id": address,
        "registration_id": registration_id,
        "success": success,
        "id": format!("{}:funding", registration_id)
    });

    // Add tx_hash if funding was successful
    if success && !tx_hash.is_empty() {
        payload["tx_hash"] = serde_json::json!(tx_hash);
    }

    // Add error message if funding failed
    if let Some(error) = error_message {
        payload["error"] = serde_json::json!(error);
    }

    let mut request = client.post(&webhook_url).json(&payload);
    
    // Add JWT authorization header if provided
    if let Some(token) = jwt_token {
        request = request.header("Authorization", format!("Bearer {}", token));
    }

    let res = request.send().await;

    match res {
        Ok(r) => info!(
            "Faucet funding webhook sent to {}: status={}, registration_id={}",
            webhook_url,
            r.status(),
            registration_id
        ),
        Err(e) => error!(
            "Failed to send faucet funding webhook to {}: {:?}",
            webhook_url, e
        ),
    }

    Ok(())
}
