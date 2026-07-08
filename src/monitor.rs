use crate::{config::ChainConfig, db::Db, webhook::WebhookDeliverer};
use alloy::primitives::Address;
use alloy::providers::{Provider, RootProvider};
use alloy::rpc::types::{Block, BlockNumberOrTag, Filter, Log};
use alloy::transports::BoxTransport;
use anyhow::Result;
use futures::future::BoxFuture;
use futures::StreamExt;
use std::str::FromStr;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

/// Below this gap (in blocks) between last-processed and head, `catch_up` scans
/// block-by-block. Above it, it switches to the batched path (ranged `eth_getLogs`
/// plus concurrent block fetches) so a large backlog actually drains instead of
/// perpetually trailing a fast-moving chain.
const BATCH_CATCHUP_MIN_GAP: u64 = 10;

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
    /// Allowlisted token contracts parsed as addresses, used to narrow
    /// eth_getLogs to the tokens we actually sweep. Empty (tests only) means
    /// no address filter is applied.
    allowed_token_filter: Vec<Address>,
}

impl Monitor {
    pub fn new(
        chain: ChainConfig,
        deliverer: Arc<WebhookDeliverer>,
        db: Db,
        provider: RootProvider<BoxTransport>,
    ) -> Self {
        let allowed_token_filter = chain
            .allowed_token_addresses
            .iter()
            .filter_map(|a| Address::from_str(a).ok())
            .collect();
        Self {
            chain,
            deliverer,
            db,
            provider,
            allowed_token_filter,
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

        let gap = current_block - start_block;
        if gap > BATCH_CATCHUP_MIN_GAP && !self.allowed_token_filter.is_empty() {
            self.catch_up_batch(start_block, current_block).await
        } else {
            for block_num in start_block..=current_block {
                self.process_single_block(block_num).await?;
            }
            Ok(())
        }
    }

    /// Drains a large backlog in chunks of `catch_up_chunk_size` blocks: one
    /// address-filtered ranged `eth_getLogs` call per chunk for ERC20 transfers
    /// (bisecting on provider range/response-size errors), plus concurrent
    /// `eth_getBlockByNumber` fetches for native transfers. Checkpoints
    /// `last_processed_block` once per chunk, which is safe to replay because
    /// deposit recording (and the webhook it triggers) is idempotent.
    ///
    /// Requires a non-empty `allowed_token_filter`: an unfiltered ranged
    /// `eth_getLogs` across all Transfer events would be the same unbounded-response
    /// problem this path exists to avoid. Callers gate on this before invoking.
    async fn catch_up_batch(&self, start_block: u64, end_block: u64) -> Result<()> {
        let chunk_size = self.chain.catch_up_chunk_size.max(1);
        let mut chunk_start = start_block;

        while chunk_start <= end_block {
            let chunk_end = chunk_start.saturating_add(chunk_size - 1).min(end_block);
            info!(
                "[{}] Catch-up chunk {}..={} ({} blocks remaining after this chunk)",
                self.chain.name,
                chunk_start,
                chunk_end,
                end_block.saturating_sub(chunk_end)
            );

            self.process_erc20_transfers_ranged(chunk_start, chunk_end)
                .await?;
            self.process_native_range_concurrent(chunk_start, chunk_end)
                .await?;

            self.db
                .set_last_processed_block(&self.chain.name, chunk_end)?;
            chunk_start = chunk_end + 1;
        }

        Ok(())
    }

    /// Fetches and scans `[from_block, to_block]` for native deposits, up to
    /// `block_fetch_concurrency` blocks in flight at once. Uses `buffered` (not
    /// `buffer_unordered`) so results are collected in block order, keeping
    /// per-chunk checkpointing straightforward.
    async fn process_native_range_concurrent(&self, from_block: u64, to_block: u64) -> Result<()> {
        let concurrency = self.chain.block_fetch_concurrency.max(1) as usize;

        let results: Vec<Result<()>> = futures::stream::iter(from_block..=to_block)
            .map(|block_num| async move {
                if let Some(block) = self
                    .provider
                    .get_block_by_number(BlockNumberOrTag::Number(block_num), true)
                    .await?
                {
                    self.handle_native_txs(&block).await?;
                }
                Ok(())
            })
            .buffered(concurrency)
            .collect()
            .await;

        for result in results {
            result?;
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
            self.handle_native_txs(&block).await?;
            self.process_erc20_transfers(block_num).await?;
        }

        self.db
            .set_last_processed_block(&self.chain.name, block_num)?;
        Ok(())
    }

    /// Scans a hydrated block's transactions for native deposits to registered
    /// addresses. Shared by the per-block steady-state path and the batch
    /// catch-up path's concurrent block fetches.
    async fn handle_native_txs(&self, block: &Block) -> Result<()> {
        let Some(txs) = block.transactions.as_transactions() else {
            return Ok(());
        };

        for tx in txs {
            let Some(to) = tx.to else {
                continue;
            };
            let to_address_str = to.to_string();
            let from_address_str = tx.from.to_string();

            if from_address_str.eq_ignore_ascii_case(&self.chain.faucet_address) {
                info!(
                    "[{}] Skipping deposit from faucet: {:?}, Account: {}",
                    self.chain.name, tx.hash, to_address_str
                );
                continue;
            }

            let Some(registration_id) = self.db.get_registration_id_by_address(&to_address_str)?
            else {
                continue;
            };

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
                if let Err(e) = self.send_deposit_detected_webhook(&deposit_info).await {
                    error!(
                        "[{}] Failed to send deposit detected webhook: {:?}",
                        self.chain.name, e
                    );
                }
            }
        }

        Ok(())
    }

    /// Builds the ERC20 Transfer filter for `[from_block, to_block]`, narrowed to
    /// `allowed_token_filter` when non-empty (empty means no filter — tests only,
    /// see the field doc on `Monitor`).
    fn erc20_transfer_filter(&self, from_block: u64, to_block: u64) -> Filter {
        use alloy::primitives::FixedBytes;

        let transfer_signature: FixedBytes<32> =
            alloy::primitives::keccak256("Transfer(address,address,uint256)".as_bytes());

        let mut filter = Filter::new()
            .from_block(from_block)
            .to_block(to_block)
            .event_signature(transfer_signature);

        if !self.allowed_token_filter.is_empty() {
            filter = filter.address(self.allowed_token_filter.clone());
        }

        filter
    }

    async fn process_erc20_transfers(&self, block_num: u64) -> Result<()> {
        let filter = self.erc20_transfer_filter(block_num, block_num);

        let logs = self
            .get_logs_with_retry(
                &filter,
                self.chain.get_logs_max_retries,
                self.chain.get_logs_delay_ms,
            )
            .await?;

        for log in &logs {
            self.handle_erc20_log(log).await?;
        }

        Ok(())
    }

    /// Fetches ERC20 Transfer logs for `[from_block, to_block]` in one ranged
    /// `eth_getLogs` call. Used by the batch catch-up path.
    async fn process_erc20_transfers_ranged(&self, from_block: u64, to_block: u64) -> Result<()> {
        let logs = self.get_logs_ranged_bisect(from_block, to_block).await?;
        for log in &logs {
            self.handle_erc20_log(log).await?;
        }
        Ok(())
    }

    /// Ranged `eth_getLogs` that bisects on error instead of retrying the identical
    /// request. Providers cap `eth_getLogs` responses (e.g. Alchemy rejects a range
    /// with "Log response size exceeded" rather than returning a partial result), so
    /// a busy-token range that overflows the cap would otherwise fail the same way on
    /// every retry and wedge catch-up on that chunk forever. Splitting the range in
    /// half and recursing (down to a single block) makes `catch_up_chunk_size` a
    /// soft performance hint rather than a correctness requirement.
    fn get_logs_ranged_bisect(
        &self,
        from_block: u64,
        to_block: u64,
    ) -> BoxFuture<'_, Result<Vec<Log>>> {
        Box::pin(async move {
            let filter = self.erc20_transfer_filter(from_block, to_block);
            match self
                .get_logs_with_retry(
                    &filter,
                    self.chain.get_logs_max_retries,
                    self.chain.get_logs_delay_ms,
                )
                .await
            {
                Ok(logs) => Ok(logs),
                Err(e) => {
                    if from_block >= to_block {
                        return Err(e);
                    }
                    warn!(
                        "[{}] get_logs failed for range {}..={} ({:?}), bisecting",
                        self.chain.name, from_block, to_block, e
                    );
                    let mid = from_block + (to_block - from_block) / 2;
                    let mut left = self.get_logs_ranged_bisect(from_block, mid).await?;
                    let right = self.get_logs_ranged_bisect(mid + 1, to_block).await?;
                    left.extend(right);
                    Ok(left)
                }
            }
        })
    }

    /// Handles a single ERC20 Transfer log: allowlist/faucet/min-deposit checks,
    /// idempotent recording, and webhook dispatch. Shared by the per-block
    /// steady-state path and the batch catch-up path's ranged log queries.
    async fn handle_erc20_log(&self, log: &Log) -> Result<()> {
        if log.topics().len() < 3 {
            return Ok(());
        }

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
            return Ok(());
        }

        let Some(registration_id) = self.db.get_registration_id_by_address(&to_address_str)? else {
            return Ok(());
        };

        if !self.chain.is_token_allowed(&token_address.to_string()) {
            debug!(
                "[{}] Skipping non-allowlisted ERC20 token: {}",
                self.chain.name, token_address
            );
            return Ok(());
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
            return Ok(());
        }

        let token_info = self.get_or_fetch_token_metadata(token_address).await?;

        if token_info.symbol.len() > 5 {
            info!(
                "[{}] Skipping ERC20 deposit: token symbol '{}' exceeds 5 characters",
                self.chain.name, token_info.symbol
            );
            return Ok(());
        }

        let Some(tx_hash) = log.transaction_hash else {
            return Ok(());
        };
        let log_index = log.log_index.unwrap_or(0);
        let tx_hash_str = tx_hash.to_string();
        let deposit_id = format!("{}:{}:{}", self.chain.name, tx_hash_str, log_index);

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
            if let Err(e) = self.send_deposit_detected_webhook(&deposit_info).await {
                error!(
                    "[{}] Failed to send ERC20 deposit detected webhook: {:?}",
                    self.chain.name, e
                );
            }
        }

        Ok(())
    }

    async fn get_logs_with_retry(
        &self,
        filter: &Filter,
        max_retries: u32,
        delay_ms: u64,
    ) -> Result<Vec<Log>> {
        use std::time::Duration;
        use tokio::time::sleep;

        let attempts = max_retries.max(1);
        let mut last_error: Option<anyhow::Error> = None;

        for attempt in 1..=attempts {
            match self.provider.get_logs(filter).await {
                Ok(logs) => {
                    if attempt > 1 {
                        info!(
                            "[{}] get_logs succeeded with {} logs on attempt {}",
                            self.chain.name,
                            logs.len(),
                            attempt
                        );
                    }
                    return Ok(logs);
                }
                Err(e) => {
                    warn!(
                        "[{}] get_logs failed on attempt {}/{}: {:?}",
                        self.chain.name, attempt, attempts, e
                    );
                    last_error = Some(e.into());
                }
            }

            if attempt < attempts {
                sleep(Duration::from_millis(delay_ms)).await;
            }
        }

        Err(last_error.expect("loop runs at least once and only exits here after an Err"))
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
