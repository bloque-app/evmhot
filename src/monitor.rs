use crate::{config::Config, db::Db};
use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::rpc::types::BlockNumberOrTag;
use anyhow::Result;
use tracing::{error, info, warn};

pub struct Monitor<P> {
    config: Config,
    db: Db,
    provider: P,
}

impl<T> Monitor<alloy::providers::RootProvider<T>>
where
    T: alloy::transports::Transport + Clone,
{
    pub fn new(config: Config, db: Db, provider: alloy::providers::RootProvider<T>) -> Self {
        Self {
            config,
            db,
            provider,
        }
    }

    async fn catch_up(&self) -> Result<()> {
        let current_block = self.provider.get_block_number().await?;
        let last_processed = self.db.get_last_processed_block()?;

        let start_block = if last_processed == 0 {
            current_block // Start from now if fresh
        } else {
            last_processed + 1
        };

        if start_block > current_block {
            return Ok(());
        }

        // Get the latest block again to ensure we don't process blocks that aren't available yet
        let latest_block = self.provider.get_block_number().await?;
        
        info!("Latest block: {}", latest_block);

        // Process max 10 blocks at a time to avoid rate limits
        // Ensure we don't exceed the latest confirmed block
        let end_block = std::cmp::min(start_block + 10, latest_block);

        for block_num in start_block..=end_block {
            self.process_single_block(block_num).await?;
        }

        Ok(())
    }

    async fn process_single_block(&self, block_num: u64) -> Result<()> {
        info!("Processing block {}", block_num);

        if let Some(block) = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Number(block_num), true)
            .await?
        {
            // Process native ETH transfers
            if let Some(txs) = block.transactions.as_transactions() {
                info!("Transactions: {:?}", txs.len());

                for tx in txs {
                    if let Some(to) = tx.to {
                        let to_address_str = to.to_string();
                        let from_address_str = tx.from.to_string();

                        // Skip deposits from the faucet address
                        if from_address_str.eq_ignore_ascii_case(&self.config.faucet_address) {
                            info!(
                                "Skipping deposit from faucet address: {:?}, Account: {}",
                                tx.hash, to_address_str
                            );
                            continue;
                        }

                        if let Some(account_id) = self.db.get_account_by_address(&to_address_str)? {
                            info!(
                                "Native ETH deposit detected! Tx: {:?}, Account: {}",
                                tx.hash, account_id
                            );
                            self.db.record_deposit(
                                &tx.hash.to_string(),
                                &account_id,
                                &tx.value.to_string(),
                            )?;

                            // Send webhook notification for deposit detection
                            if let Err(e) = self
                                .send_deposit_detected_webhook(
                                    &account_id,
                                    &tx.hash.to_string(),
                                    &tx.value.to_string(),
                                    "native",
                                    None,
                                    None,
                                )
                                .await
                            {
                                error!("Failed to send deposit detected webhook: {:?}", e);
                            }
                        }
                    }
                }
            }

            // Process ERC20 Transfer events
            self.process_erc20_transfers(block_num).await?;
        }

        self.db.set_last_processed_block(block_num)?;
        Ok(())
    }

    async fn process_erc20_transfers(&self, block_num: u64) -> Result<()> {
        use alloy::primitives::FixedBytes;
        use alloy::rpc::types::Filter;

        // ERC20 Transfer event signature: Transfer(address,address,uint256)
        let transfer_signature: FixedBytes<32> =
            alloy::primitives::keccak256("Transfer(address,address,uint256)".as_bytes());

        let filter = Filter::new()
            .from_block(block_num)
            .to_block(block_num)
            .event_signature(transfer_signature);

        let logs = self.provider.get_logs(&filter).await?;

        info!(
            "Found {} Transfer events in block {}",
            logs.len(),
            block_num
        );

        for log in logs {
            // Decode Transfer event: topic[0] = signature, topic[1] = from, topic[2] = to
            if log.topics().len() >= 3 {
                let token_address = log.address();
                let from_address = Address::from_slice(&log.topics()[1].as_slice()[12..]); // Last 20 bytes of topic[1]
                let to_address = Address::from_slice(&log.topics()[2].as_slice()[12..]); // Last 20 bytes of topic[2]

                let from_address_str = from_address.to_string();
                let to_address_str = to_address.to_string();

                // Skip deposits from the faucet address
                if from_address_str.eq_ignore_ascii_case(&self.config.faucet_address) {
                    info!(
                        "Skipping ERC20 deposit from faucet address: Token: {}, To: {}",
                        token_address, to_address_str
                    );
                    continue;
                }

                // Check if this is one of our monitored addresses
                if let Some(account_id) = self.db.get_account_by_address(&to_address_str)? {
                    // Decode the amount from data field
                    let amount = if !log.data().data.is_empty() {
                        alloy::primitives::U256::from_be_slice(&log.data().data)
                    } else {
                        alloy::primitives::U256::ZERO
                    };

                    // log::info("Detected ERC20 deposit: Token: {}, To: {}, From: {}, Amount: {}", token_address, to_address_str, from_address_str, log.data().data);

                    // Fetch token metadata (symbol, decimals, name)
                    let token_info = self.get_or_fetch_token_metadata(token_address).await?;

                    info!(
                        "ERC20 deposit detected! Token: {} ({}), Amount: {}, Account: {}, Tx: {:?}",
                        token_info.symbol, token_address, amount, account_id, log.transaction_hash
                    );

                    // Store ERC20 deposit
                    if let Some(tx_hash) = log.transaction_hash {
                        let log_index = log.log_index.unwrap_or(0);
                        self.db.record_erc20_deposit(
                            &tx_hash.to_string(),
                            log_index,
                            &account_id,
                            &amount.to_string(),
                            &token_address.to_string(),
                            &token_info.symbol,
                        )?;

                        // Send webhook notification for ERC20 deposit detection
                        if let Err(e) = self
                            .send_deposit_detected_webhook(
                                &account_id,
                                &tx_hash.to_string(),
                                &amount.to_string(),
                                "erc20",
                                Some(&token_info.symbol),
                                Some(&token_address.to_string()),
                            )
                            .await
                        {
                            error!("Failed to send ERC20 deposit detected webhook: {:?}", e);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn get_or_fetch_token_metadata(&self, token_address: Address) -> Result<TokenInfo> {
        let token_address_str = token_address.to_string();

        // Check cache first
        if let Some((symbol, decimals, name)) = self.db.get_token_metadata(&token_address_str)? {
            return Ok(TokenInfo {
                address: token_address_str,
                symbol,
                decimals,
                name,
            });
        }

        // Fetch from blockchain
        match get_token_info(&self.provider, token_address).await {
            Ok(token_info) => {
                // Cache it
                self.db.store_token_metadata(
                    &token_address_str,
                    &token_info.symbol,
                    token_info.decimals,
                    &token_info.name,
                )?;
                Ok(token_info)
            }
            Err(e) => {
                warn!(
                    "Failed to fetch token metadata for {}: {:?}",
                    token_address, e
                );
                // Return a default token info
                Ok(TokenInfo {
                    address: token_address_str.clone(),
                    symbol: "UNKNOWN".to_string(),
                    decimals: 18,
                    name: "Unknown Token".to_string(),
                })
            }
        }
    }

    async fn send_deposit_detected_webhook(
        &self,
        account_id: &str,
        tx_hash: &str,
        amount: &str,
        token_type: &str,
        token_symbol: Option<&str>,
        token_address: Option<&str>,
    ) -> Result<()> {
        // Get the webhook URL for this account
        let webhook_url = match self.db.get_webhook_url(account_id)? {
            Some(url) => url,
            None => {
                error!("No webhook URL found for account: {}", account_id);
                return Ok(());
            }
        };

        let client = reqwest::Client::new();

        let mut payload = serde_json::json!({
            "event": "deposit_detected",
            "account_id": account_id,
            "tx_hash": tx_hash,
            "amount": amount,
            "token_type": token_type
        });

        // Add ERC20-specific fields if provided
        if let Some(symbol) = token_symbol {
            payload["token_symbol"] = serde_json::json!(symbol);
        }
        if let Some(address) = token_address {
            payload["token_address"] = serde_json::json!(address);
        }

        let res = client.post(&webhook_url).json(&payload).send().await;

        match res {
            Ok(r) => info!(
                "Deposit detected webhook sent to {}: status={}",
                webhook_url,
                r.status()
            ),
            Err(e) => error!(
                "Failed to send deposit detected webhook to {}: {:?}",
                webhook_url, e
            ),
        }

        Ok(())
    }
}

// ERC20 helper types and functions
use alloy::sol;

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    contract IERC20 {
        event Transfer(address indexed from, address indexed to, uint256 value);

        function balanceOf(address account) external view returns (uint256);
        function transfer(address to, uint256 amount) external returns (bool);
        function symbol() external view returns (string memory);
        function decimals() external view returns (uint8);
        function name() external view returns (string memory);
    }
}

#[derive(Debug, Clone)]
struct TokenInfo {
    #[allow(dead_code)]
    address: String,
    symbol: String,
    decimals: u8,
    name: String,
}

async fn get_token_info<T>(
    provider: &alloy::providers::RootProvider<T>,
    token_address: Address,
) -> Result<TokenInfo>
where
    T: alloy::transports::Transport + Clone,
{
    let contract = IERC20::new(token_address, provider);

    let symbol = contract.symbol().call().await?._0;
    let decimals = contract.decimals().call().await?._0;
    let name = contract.name().call().await?._0;

    Ok(TokenInfo {
        address: token_address.to_string(),
        symbol,
        decimals,
        name,
    })
}

use crate::traits::Service;
use async_trait::async_trait;

use alloy::transports::http::Http;
use reqwest::Client;

// Implementation for HTTP Provider (Polling)
#[async_trait]
impl Service for Monitor<alloy::providers::RootProvider<Http<Client>>> {
    async fn run(&self) {
        use std::time::Duration;
        use tokio::time::sleep;

        info!("Starting Monitor in Polling mode");
        loop {
            if let Err(e) = self.catch_up().await {
                error!("Error in monitor loop: {:?}", e);
            }
            sleep(Duration::from_secs(self.config.poll_interval)).await;
        }
    }
}

// Implementation for WebSocket Provider (Streaming)
#[async_trait]
impl Service for Monitor<alloy::providers::RootProvider<alloy::pubsub::PubSubFrontend>> {
    async fn run(&self) {
        use std::time::Duration;
        use tokio::time::sleep;

        info!("Starting Monitor in Streaming mode");
        loop {
            // 1. Catch up first
            if let Err(e) = self.catch_up().await {
                error!("Error during catch-up: {:?}", e);
            }

            // 2. Subscribe
            match self.provider.subscribe_blocks().await {
                Ok(mut stream) => {
                    while let Ok(header) = stream.recv().await {
                        if let Some(block_num) = header.header.number {
                            info!("New block received via WS: {}", block_num);
                            if let Err(e) = self.process_single_block(block_num).await {
                                error!("Error processing block {}: {:?}", block_num, e);
                            }
                        }
                    }
                    error!("WebSocket stream ended");
                }
                Err(e) => error!("Failed to subscribe to blocks: {:?}", e),
            }

            // Reconnect delay
            sleep(Duration::from_secs(5)).await;
        }
    }
}
