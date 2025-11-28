use crate::{config::Config, db::Db};
use alloy::providers::Provider;
use alloy::rpc::types::BlockNumberOrTag;
use anyhow::Result;
use tracing::{error, info};

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

        // Process max 10 blocks at a time to avoid rate limits
        let end_block = std::cmp::min(start_block + 10, current_block);

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
            if let Some(txs) = block.transactions.as_transactions() {
                for tx in txs {
                    if let Some(to) = tx.to {
                        let to_address_str = to.to_string();
                        if let Some(account_id) = self.db.get_account_by_address(&to_address_str)? {
                            info!(
                                "Deposit detected! Tx: {:?}, Account: {}",
                                tx.hash, account_id
                            );
                            self.db.record_deposit(
                                &tx.hash.to_string(),
                                &account_id,
                                &tx.value.to_string(),
                            )?;
                        }
                    }
                }
            }
        }

        self.db.set_last_processed_block(block_num)?;
        Ok(())
    }
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
