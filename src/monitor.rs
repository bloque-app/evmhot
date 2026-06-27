use crate::{config::ChainConfig, db::Db, webhook::WebhookDeliverer};
use alloy::primitives::Address;
use alloy::providers::{Provider, RootProvider};
use alloy::rpc::types::BlockNumberOrTag;
use alloy::transports::BoxTransport;
use anyhow::Result;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

/// Information about a detected deposit for webhook notification
struct DepositInfo<'a> {
    id: &'a str,
    chain: &'a str,
    chain_id: u64,
    account_id: &'a str,
    registration_id: &'a str,
    tx_hash: &'a str,
    amount: &'a str,
    token_type: &'a str,
    token_symbol: Option<&'a str>,
    token_address: Option<&'a str>,
    token_decimals: Option<u8>,
}

pub struct Monitor {
    chain: ChainConfig,
    deliverer: Arc<WebhookDeliverer>,
    db: Db,
    provider: RootProvider<BoxTransport>,
}

impl Monitor {
    pub fn new(
        chain: ChainConfig,
        deliverer: Arc<WebhookDeliverer>,
        db: Db,
        provider: RootProvider<BoxTransport>,
    ) -> Self {
        Self {
            chain,
            deliverer,
            db,
            provider,
        }
    }

    async fn catch_up(&self) -> Result<()> {
        let latest_block = self.provider.get_block_number().await?;
        let current_block = latest_block.saturating_sub(self.chain.block_offset_from_head);
        let last_processed = self.db.get_last_processed_block(&self.chain.name)?;

        let start_block = if last_processed == 0 {
            current_block
        } else {
            last_processed
        };

        info!("--------------------------------");
        info!(
            "[{}] Offset from head: {}",
            self.chain.name, self.chain.block_offset_from_head
        );
        info!("[{}] Start block: {}", self.chain.name, start_block);
        info!("[{}] Current block: {}", self.chain.name, current_block);
        info!(
            "[{}] Last processed block: {}",
            self.chain.name, last_processed
        );

        if start_block > current_block {
            return Ok(());
        }

        info!("--------------------------------");
        info!(
            "[{}] Processing blocks from {} to {}",
            self.chain.name, start_block, current_block
        );

        for block_num in start_block..=current_block {
            self.process_single_block(block_num).await?;
        }

        Ok(())
    }

    async fn process_single_block(&self, block_num: u64) -> Result<()> {
        info!("[{}] Processing block {}", self.chain.name, block_num);

        if let Some(block) = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Number(block_num), true)
            .await?
        {
            if let Some(txs) = block.transactions.as_transactions() {
                for tx in txs {
                    if let Some(to) = tx.to {
                        let to_address_str = to.to_string();
                        let from_address_str = tx.from.to_string();

                        if from_address_str.eq_ignore_ascii_case(&self.chain.faucet_address) {
                            info!(
                                "[{}] Skipping deposit from faucet: {:?}, Account: {}",
                                self.chain.name, tx.hash, to_address_str
                            );
                            continue;
                        }

                        if let Some(registration_id) =
                            self.db.get_registration_id_by_address(&to_address_str)?
                        {
                            if tx.value < self.chain.min_deposits.native {
                                info!(
                                    "[{}] Skipping native deposit below minimum: tx={:?}, amount={}, min={}",
                                    self.chain.name, tx.hash, tx.value, self.chain.min_deposits.native
                                );
                                continue;
                            }

                            info!(
                                "[{}] Native deposit detected! Tx: {:?}, Address: {}, Registration ID: {}",
                                self.chain.name, tx.hash, to_address_str, registration_id
                            );

                            let tx_hash_str = tx.hash.to_string();
                            let is_new_deposit = self.db.record_deposit(
                                &self.chain.name,
                                &tx_hash_str,
                                &registration_id,
                                &tx.value.to_string(),
                            )?;

                            if is_new_deposit {
                                let amount_str = tx.value.to_string();
                                let deposit_id = format!("{}:{}", self.chain.name, tx_hash_str);
                                let deposit_info = DepositInfo {
                                    id: &deposit_id,
                                    chain: &self.chain.name,
                                    chain_id: self.chain.chain_id,
                                    account_id: &to_address_str,
                                    registration_id: &registration_id,
                                    tx_hash: &tx_hash_str,
                                    amount: &amount_str,
                                    token_type: "native",
                                    token_symbol: None,
                                    token_address: None,
                                    token_decimals: None,
                                };
                                if let Err(e) =
                                    self.send_deposit_detected_webhook(&deposit_info).await
                                {
                                    error!(
                                        "[{}] Failed to send deposit detected webhook: {:?}",
                                        self.chain.name, e
                                    );
                                }
                            }
                        }
                    }
                }
            }

            self.process_erc20_transfers(block_num).await?;
        }

        self.db
            .set_last_processed_block(&self.chain.name, block_num)?;
        Ok(())
    }

    async fn process_erc20_transfers(&self, block_num: u64) -> Result<()> {
        use alloy::primitives::FixedBytes;
        use alloy::rpc::types::Filter;

        let transfer_signature: FixedBytes<32> =
            alloy::primitives::keccak256("Transfer(address,address,uint256)".as_bytes());

        let filter = Filter::new()
            .from_block(block_num)
            .to_block(block_num)
            .event_signature(transfer_signature);

        let logs = self
            .get_logs_with_retry(
                &filter,
                self.chain.get_logs_max_retries,
                self.chain.get_logs_delay_ms,
            )
            .await?;

        for log in logs {
            if log.topics().len() >= 3 {
                let token_address = log.address();
                let from_address = Address::from_slice(&log.topics()[1].as_slice()[12..]);
                let to_address = Address::from_slice(&log.topics()[2].as_slice()[12..]);

                let from_address_str = from_address.to_string();
                let to_address_str = to_address.to_string();

                if from_address_str.eq_ignore_ascii_case(&self.chain.faucet_address) {
                    info!(
                        "[{}] Skipping ERC20 deposit from faucet: Token: {}, To: {}",
                        self.chain.name, token_address, to_address_str
                    );
                    continue;
                }

                if let Some(registration_id) =
                    self.db.get_registration_id_by_address(&to_address_str)?
                {
                    if !self.chain.is_token_allowed(&token_address.to_string()) {
                        debug!(
                            "[{}] Skipping non-allowlisted ERC20 token: {}",
                            self.chain.name, token_address
                        );
                        continue;
                    }

                    let amount = if log.data().data.len() >= 32 {
                        let amount_bytes: [u8; 32] = log.data().data[..32]
                            .try_into()
                            .expect("slice length is 32");
                        alloy::primitives::U256::from_be_bytes(amount_bytes)
                    } else if !log.data().data.is_empty() {
                        alloy::primitives::U256::from_be_slice(&log.data().data)
                    } else {
                        alloy::primitives::U256::ZERO
                    };

                    let token_address_lc = token_address.to_string().to_lowercase();
                    let min_deposit = self.chain.min_deposits.for_token(&token_address_lc);
                    if amount < min_deposit {
                        info!(
                            "[{}] Skipping ERC20 deposit below minimum: token={}, amount={}, min={}",
                            self.chain.name, token_address, amount, min_deposit
                        );
                        continue;
                    }

                    let token_info = self.get_or_fetch_token_metadata(token_address).await?;

                    if token_info.symbol.len() > 5 {
                        info!(
                            "[{}] Skipping ERC20 deposit: token symbol '{}' exceeds 5 characters",
                            self.chain.name, token_info.symbol
                        );
                        continue;
                    }

                    if let Some(tx_hash) = log.transaction_hash {
                        let log_index = log.log_index.unwrap_or(0);
                        let tx_hash_str = tx_hash.to_string();
                        let deposit_id =
                            format!("{}:{}:{}", self.chain.name, tx_hash_str, log_index);

                        let is_new_deposit = self.db.record_erc20_deposit(
                            &self.chain.name,
                            &tx_hash_str,
                            log_index,
                            &registration_id,
                            &amount.to_string(),
                            &token_address.to_string(),
                            &token_info.symbol,
                        )?;

                        if is_new_deposit {
                            let token_addr_str = token_address.to_string();
                            let amount_str = amount.to_string();
                            let deposit_info = DepositInfo {
                                id: &deposit_id,
                                chain: &self.chain.name,
                                chain_id: self.chain.chain_id,
                                account_id: &to_address_str,
                                registration_id: &registration_id,
                                tx_hash: &tx_hash_str,
                                amount: &amount_str,
                                token_type: "erc20",
                                token_symbol: Some(&token_info.symbol),
                                token_address: Some(&token_addr_str),
                                token_decimals: Some(token_info.decimals),
                            };
                            if let Err(e) = self.send_deposit_detected_webhook(&deposit_info).await
                            {
                                error!(
                                    "[{}] Failed to send ERC20 deposit detected webhook: {:?}",
                                    self.chain.name, e
                                );
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn get_logs_with_retry(
        &self,
        filter: &alloy::rpc::types::Filter,
        max_retries: u32,
        delay_ms: u64,
    ) -> Result<Vec<alloy::rpc::types::Log>> {
        use std::time::Duration;
        use tokio::time::sleep;

        let mut last_result = Ok(Vec::new());

        for attempt in 1..=max_retries {
            last_result = self.provider.get_logs(filter).await.map_err(|e| e.into());

            match &last_result {
                Ok(logs) if !logs.is_empty() => {
                    if attempt > 1 {
                        info!(
                            "[{}] get_logs succeeded with {} logs on attempt {}",
                            self.chain.name,
                            logs.len(),
                            attempt
                        );
                    }
                    return last_result;
                }
                Ok(_) => warn!(
                    "[{}] get_logs returned empty on attempt {}/{}",
                    self.chain.name, attempt, max_retries
                ),
                Err(e) => warn!(
                    "[{}] get_logs failed on attempt {}/{}: {:?}",
                    self.chain.name, attempt, max_retries, e
                ),
            }

            if attempt < max_retries {
                sleep(Duration::from_millis(delay_ms)).await;
            }
        }

        last_result
    }

    async fn get_or_fetch_token_metadata(&self, token_address: Address) -> Result<TokenInfo> {
        let token_address_str = token_address.to_string();

        if let Some((symbol, decimals, name)) = self
            .db
            .get_token_metadata(&self.chain.name, &token_address_str)?
        {
            return Ok(TokenInfo {
                address: token_address_str,
                symbol,
                decimals,
                name,
            });
        }

        match get_token_info(&self.provider, token_address).await {
            Ok(token_info) => {
                self.db.store_token_metadata(
                    &self.chain.name,
                    &token_address_str,
                    &token_info.symbol,
                    token_info.decimals,
                    &token_info.name,
                )?;
                Ok(token_info)
            }
            Err(e) => {
                warn!(
                    "[{}] Failed to fetch token metadata for {}: {:?}",
                    self.chain.name, token_address, e
                );
                Ok(TokenInfo {
                    address: token_address_str.clone(),
                    symbol: "UNKNOWN".to_string(),
                    decimals: 18,
                    name: "Unknown Token".to_string(),
                })
            }
        }
    }

    async fn send_deposit_detected_webhook(&self, info: &DepositInfo<'_>) -> Result<()> {
        let Some(webhook_url) = self.db.get_webhook_url(info.registration_id)? else {
            error!(
                "No webhook URL found for registration_id: {}",
                info.registration_id
            );
            return Ok(());
        };

        let mut payload = serde_json::json!({
            "id": info.id,
            "chain": info.chain,
            "chain_id": info.chain_id,
            "event": "deposit_detected",
            "account_id": info.account_id,
            "registration_id": info.registration_id,
            "tx_hash": info.tx_hash,
            "amount": info.amount,
            "token_type": info.token_type
        });

        if let Some(symbol) = info.token_symbol {
            payload["token_symbol"] = serde_json::json!(symbol);
        }
        if let Some(address) = info.token_address {
            payload["token_address"] = serde_json::json!(address);
        }
        if let Some(decimals) = info.token_decimals {
            payload["token_decimals"] = serde_json::json!(decimals);
        }

        self.deliverer
            .enqueue(&webhook_url, info.registration_id, payload)
            .await?;

        info!(
            "[{}] Deposit detected webhook enqueued for {} (registration_id={})",
            info.chain, webhook_url, info.registration_id
        );

        Ok(())
    }
}

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

async fn get_token_info(
    provider: &RootProvider<BoxTransport>,
    token_address: Address,
) -> Result<TokenInfo> {
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

#[async_trait]
impl Service for Monitor {
    async fn run(&self) {
        use std::time::Duration;
        use tokio::time::sleep;

        info!("[{}] Starting Monitor in Polling mode", self.chain.name);
        loop {
            if let Err(e) = self.catch_up().await {
                error!("[{}] Error in monitor loop: {:?}", self.chain.name, e);
            }
            sleep(Duration::from_secs(self.chain.poll_interval)).await;
        }
    }
}
