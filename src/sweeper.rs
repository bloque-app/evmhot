use crate::{
    config::{plan_sweep_parts, ChainConfig, SweepTarget, TargetKind},
    db::{Db, Erc20Deposit, Erc20Settlement, OutboxEntry, SweepRecord},
    faucet::Faucet,
    wallet::Wallet,
    webhook::WebhookDeliverer,
};
use alloy::network::TransactionBuilder;
use alloy::primitives::{keccak256, Address, U256};
use alloy::providers::{Provider, RootProvider};
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::sol_types::SolCall;
use alloy::transports::BoxTransport;
use anyhow::Result;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};

pub struct Sweeper {
    chain: ChainConfig,
    deliverer: Arc<WebhookDeliverer>,
    db: Db,
    wallet: Wallet,
    provider: RootProvider<BoxTransport>,
    faucet: Arc<Faucet>,
}

use crate::traits::Service;
use async_trait::async_trait;

#[async_trait]
impl Service for Sweeper {
    async fn run(&self) {
        self.log_deposit_queue("startup").await;
        let mut cycle: u64 = 0;
        loop {
            if let Err(e) = self.process_deposits().await {
                error!("[{}] Error in sweeper loop: {:?}", self.chain.name, e);
            }
            cycle += 1;
            if cycle.is_multiple_of(QUEUE_LOG_INTERVAL_CYCLES) {
                self.log_deposit_queue("periodic").await;
            }
            sleep(Duration::from_secs(self.chain.poll_interval)).await;
        }
    }
}

const MAX_ZERO_BALANCE_RETRIES: u64 = 10;
const MAX_SWEEP_RETRIES: u64 = 5;
const QUEUE_LOG_INTERVAL_CYCLES: u64 = 60;

fn is_permanent_sweep_error(err_debug: &str) -> bool {
    let s = err_debug.to_ascii_lowercase();
    s.contains("buffer overrun") || s.contains("deserializ")
}

fn is_transient_funding_error(err_debug: &str) -> bool {
    let s = err_debug.to_ascii_lowercase();
    s.contains("faucet has insufficient balance")
        || s.contains("insufficient native balance for gas")
        || s.contains("still insufficient balance after faucet")
}

impl Sweeper {
    pub fn new(
        chain: ChainConfig,
        deliverer: Arc<WebhookDeliverer>,
        db: Db,
        wallet: Wallet,
        provider: RootProvider<BoxTransport>,
        faucet: Arc<Faucet>,
    ) -> Self {
        Self {
            chain,
            deliverer,
            db,
            wallet,
            provider,
            faucet,
        }
    }

    #[cfg(test)]
    pub(crate) async fn process_deposits_once(&self) -> Result<()> {
        self.process_deposits().await
    }

    async fn log_deposit_queue(&self, reason: &str) {
        let chain_name = self.chain.name.clone();
        match self
            .db
            .blocking(move |db| db.deposit_queue_counts(&chain_name))
            .await
        {
            Ok(counts) => {
                if counts.has_pending() {
                    info!(
                        "[{}] Sweeper queue ({reason}): native detected={}, native failed={}, erc20 detected={}, erc20 failed={}",
                        self.chain.name,
                        counts.native_detected,
                        counts.native_failed,
                        counts.erc20_detected,
                        counts.erc20_failed
                    );
                }
            }
            Err(e) => {
                error!(
                    "[{}] Failed to read deposit queue counts: {:?}",
                    self.chain.name, e
                );
            }
        }
    }

    async fn process_deposits(&self) -> Result<()> {
        self.process_native_deposits().await?;
        self.process_erc20_deposits().await
    }

    async fn process_native_deposits(&self) -> Result<()> {
        let chain_name = self.chain.name.clone();
        let deposits = self
            .db
            .blocking(move |db| db.get_detected_deposits(&chain_name))
            .await?;

        for (tx_hash, registration_id, amount_str) in deposits {
            info!(
                "[{}] Processing native deposit: tx_hash={}, registration_id={}, amount={}",
                self.chain.name, tx_hash, registration_id, amount_str
            );

            let (derivation_index, address_str, _webhook_url) = {
                let reg_id = registration_id.clone();
                self.db
                    .blocking(move |db| db.get_account_by_id(&reg_id))
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("Account not found"))?
            };

            let signer = self.wallet.get_signer(derivation_index)?;
            let wallet = alloy::network::EthereumWallet::from(signer);

            let sweep_provider = alloy::providers::ProviderBuilder::new()
                .with_recommended_fillers()
                .wallet(wallet)
                .on_provider(&self.provider);

            match self
                .sweep_deposit(
                    &sweep_provider,
                    &address_str,
                    &tx_hash,
                    &registration_id,
                    &amount_str,
                )
                .await
            {
                Ok(_) => info!(
                    "[{}] Successfully swept native deposit: {}",
                    self.chain.name, tx_hash
                ),
                Err(e) => {
                    let err_str = format!("{:?}", e);
                    if is_transient_funding_error(&err_str) {
                        warn!(
                            "[{}] Native deposit {} waiting for faucet funding: {}",
                            self.chain.name, tx_hash, err_str
                        );
                    } else {
                        error!(
                            "[{}] Failed to sweep native deposit {}: {}",
                            self.chain.name, tx_hash, err_str
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// Token sweeps. Detected deposits are grouped by deposit address and token
    /// (and by sender when the token's target says `per_sender`, REQ-SW-12);
    /// each group is processed once per pass.
    async fn process_erc20_deposits(&self) -> Result<()> {
        let chain_name = self.chain.name.clone();
        let erc20_deposits = self
            .db
            .blocking(move |db| db.get_detected_erc20_deposits(&chain_name))
            .await?;

        let mut groups: Vec<Vec<Erc20Deposit>> = Vec::new();
        let mut group_index: std::collections::HashMap<(String, String, Option<String>), usize> =
            std::collections::HashMap::new();

        for deposit in erc20_deposits {
            if !self.chain.is_token_allowed(&deposit.token_address) {
                info!(
                    "[{}] Skipping non-allowlisted ERC20 deposit: key={}, token={}",
                    self.chain.name, deposit.key, deposit.token_address
                );
                let chain_name = self.chain.name.clone();
                let key = deposit.key.clone();
                self.db
                    .blocking(move |db| db.mark_erc20_deposit_failed(&chain_name, &key))
                    .await?;
                continue;
            }

            let target = self.chain.target_for_token(&deposit.token_address);
            if target.kind == TargetKind::Hold {
                // REQ-SW-6: detected and notified, never swept. A later change
                // of the target's kind releases it on the next pass.
                info!(
                    "[{}] Holding ERC20 deposit {} (token {} target is hold)",
                    self.chain.name, deposit.key, deposit.token_address
                );
                continue;
            }

            let token = deposit.token_address.to_lowercase();
            let sender = if target.per_sender {
                match &deposit.from_address {
                    Some(from) => Some(from.to_lowercase()),
                    // A deposit with no recorded sender cannot be grouped:
                    // it is swept on its own.
                    None => Some(format!("unknown:{}", deposit.key)),
                }
            } else {
                None
            };
            let key = (deposit.account_id.clone(), token, sender);
            match group_index.get(&key) {
                Some(&i) => groups[i].push(deposit),
                None => {
                    group_index.insert(key, groups.len());
                    groups.push(vec![deposit]);
                }
            }
        }

        for group in groups {
            let first = &group[0];
            let registration_id = first.account_id.clone();
            let target = self.chain.target_for_token(&first.token_address);

            let (derivation_index, address_str, _webhook_url) = {
                let reg_id = registration_id.clone();
                self.db
                    .blocking(move |db| db.get_account_by_id(&reg_id))
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("Account not found"))?
            };

            let signer = self.wallet.get_signer(derivation_index)?;
            let wallet = alloy::network::EthereumWallet::from(signer);

            let sweep_provider = alloy::providers::ProviderBuilder::new()
                .with_recommended_fillers()
                .wallet(wallet)
                .on_provider(&self.provider);

            if let Err(e) = self
                .sweep_erc20_group(&sweep_provider, &address_str, &target, &group)
                .await
            {
                self.handle_group_failure(&group, &e).await?;
            }
        }

        Ok(())
    }

    /// REQ-SW-4: funding errors wait; a decoding error fails the group at once;
    /// any other failure (including a reverted sweep, REQ-SW-5) counts, and the
    /// fifth consecutive one fails the group.
    async fn handle_group_failure(&self, group: &[Erc20Deposit], e: &anyhow::Error) -> Result<()> {
        let err_str = format!("{:?}", e);
        let first_key = group[0].key.clone();
        if is_transient_funding_error(&err_str) {
            warn!(
                "[{}] ERC20 deposit {} waiting for faucet funding: {}",
                self.chain.name, first_key, err_str
            );
            return Ok(());
        }
        error!(
            "[{}] Failed to sweep ERC20 deposit {}: {}",
            self.chain.name, first_key, err_str
        );
        let keys: Vec<String> = group.iter().map(|d| d.key.clone()).collect();
        if is_permanent_sweep_error(&err_str) {
            self.fail_keys(&keys).await?;
            for key in &keys {
                warn!(
                    "[{}] Permanently failed ERC20 deposit {} (permanent sweep error)",
                    self.chain.name, key
                );
            }
            return Ok(());
        }
        let failures = {
            let chain_name = self.chain.name.clone();
            let key = first_key.clone();
            self.db
                .blocking(move |db| db.increment_sweep_failure_count(&chain_name, &key))
                .await
        };
        if let Ok(failures) = failures {
            if failures >= MAX_SWEEP_RETRIES {
                self.fail_keys(&keys).await?;
                for key in &keys {
                    warn!(
                        "[{}] Permanently failed ERC20 deposit {} after {} attempts: {}",
                        self.chain.name, key, failures, err_str
                    );
                }
            }
        }
        Ok(())
    }

    async fn fail_keys(&self, keys: &[String]) -> Result<()> {
        let chain_name = self.chain.name.clone();
        let keys = keys.to_vec();
        self.db
            .blocking(move |db| db.mark_erc20_deposits_failed_for_keys(&chain_name, &keys))
            .await
    }

    async fn sweep_deposit<SP>(
        &self,
        provider: &SP,
        from_address_str: &str,
        tx_hash: &str,
        registration_id: &str,
        amount_str: &str,
    ) -> Result<()>
    where
        SP: Provider<BoxTransport, alloy::network::Ethereum>,
    {
        let from_address = Address::from_str(from_address_str)?;
        let to_address = Address::from_str(&self.chain.treasury_address)?;

        let mut balance = provider.get_balance(from_address).await?;
        let gas_limit: u128 = 21000;
        let fee_estimate = provider.estimate_eip1559_fees(None).await?;
        let max_fee_per_gas = fee_estimate.max_fee_per_gas;
        let gas_cost = U256::from(gas_limit) * U256::from(max_fee_per_gas);
        let gas_cost_with_buffer = gas_cost + (gas_cost / U256::from(10));

        if balance <= gas_cost_with_buffer {
            match self.faucet.fund_new_address(from_address_str).await {
                Ok(_) => {
                    sleep(Duration::from_secs(2)).await;
                    balance = provider.get_balance(from_address).await?;
                    if balance <= gas_cost_with_buffer {
                        return Err(anyhow::anyhow!(
                            "Still insufficient balance after faucet funding"
                        ));
                    }
                }
                Err(e) => return Err(e),
            }
        }

        let value_to_send = balance - gas_cost_with_buffer;

        let tx = TransactionRequest::default()
            .with_to(to_address)
            .with_value(value_to_send)
            .with_gas_limit(gas_limit);

        let pending_tx = provider.send_transaction(tx).await?;
        let receipt = pending_tx.get_receipt().await?;

        // REQ-SW-5: only a successful receipt settles the deposit.
        if !receipt.inner.status() {
            return Err(anyhow::anyhow!(
                "native sweep transaction reverted (receipt status 0): {:?}",
                receipt.transaction_hash
            ));
        }

        let from = {
            let chain_name = self.chain.name.clone();
            let tx_hash = tx_hash.to_string();
            self.db
                .blocking(move |db| db.get_deposit_from_address(&chain_name, &tx_hash))
                .await?
        };
        let webhook_url = {
            let reg_id = registration_id.to_string();
            self.db
                .blocking(move |db| db.get_webhook_url(&reg_id))
                .await?
        };
        let webhook_id = format!("{}:{}", self.chain.name, tx_hash);
        let outbox = webhook_url.map(|webhook_url| OutboxEntry {
            id: webhook_id.clone(),
            event: "deposit_swept".to_string(),
            registration_id: registration_id.to_string(),
            webhook_url,
            payload: serde_json::json!({
                "id": webhook_id,
                "chain": self.chain.name,
                "chain_id": self.chain.chain_id,
                "event": "deposit_swept",
                "account_id": from_address_str,
                "registration_id": registration_id,
                "original_tx_hash": tx_hash,
                "from_address": from,
                "amount": amount_str,
                "token_type": "native"
            })
            .to_string(),
        });

        // REQ-WH-7: the swept mark and its webhook are one transaction.
        let enqueued = {
            let chain_name = self.chain.name.clone();
            let tx_hash = tx_hash.to_string();
            self.db
                .blocking(move |db| {
                    db.complete_native_sweep(&chain_name, &tx_hash, outbox.as_ref())
                })
                .await?
        };
        if enqueued {
            self.deliverer.notify_worker();
        }

        info!(
            "[{}] Swept funds! Tx hash: {:?}",
            self.chain.name, receipt.transaction_hash
        );

        Ok(())
    }

    /// Sweeps one group of deposits (one address, one token, and one sender when
    /// the target is `per_sender`). Each part is its own `transfer` call and its
    /// own coordinate (REQ-SW-9, REQ-SW-10); the deposits are marked swept, and
    /// `deposit_swept` sent, once after the last part (REQ-WH-4).
    async fn sweep_erc20_group<SP>(
        &self,
        provider: &SP,
        from_address_str: &str,
        target: &SweepTarget,
        group: &[Erc20Deposit],
    ) -> Result<()>
    where
        SP: Provider<BoxTransport, alloy::network::Ethereum>,
    {
        let first = &group[0];
        let from_address = Address::from_str(from_address_str)?;
        let to_address = Address::from_str(
            target
                .address
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("sweep target has no address"))?,
        )?;
        let token_address = Address::from_str(&first.token_address)?;
        let keys: Vec<String> = group.iter().map(|d| d.key.clone()).collect();

        let token_balance = get_token_balance(&self.provider, token_address, from_address).await?;

        // Sweeps recorded by an earlier pass that did not finish the group.
        let recorded = self.recorded_sweeps(&keys).await?;
        let sweepable = if target.per_sender {
            // REQ-SW-12: one sender's share, not the whole balance.
            let mut owed = U256::ZERO;
            for deposit in group {
                owed += U256::from_str(&deposit.amount)?;
            }
            let mut already = U256::ZERO;
            for record in &recorded {
                already += U256::from_str(&record.sweep_amount)?;
            }
            owed.saturating_sub(already).min(token_balance)
        } else {
            // REQ-SW-1: the whole balance.
            token_balance
        };

        if sweepable.is_zero() {
            if !recorded.is_empty() {
                return self.complete_group(from_address_str, group).await;
            }
            if token_balance.is_zero() {
                // REQ-SW-3
                let retry_count = {
                    let chain_name = self.chain.name.clone();
                    let key = first.key.clone();
                    self.db
                        .blocking(move |db| db.increment_zero_balance_count(&chain_name, &key))
                        .await?
                };
                if retry_count >= MAX_ZERO_BALANCE_RETRIES {
                    let chain_name = self.chain.name.clone();
                    let keys = keys.clone();
                    self.db
                        .blocking(move |db| {
                            db.mark_erc20_deposits_swept_for_keys(&chain_name, &keys)
                        })
                        .await?;
                }
            }
            return Ok(());
        }

        if let Some(min) = target.min {
            if sweepable < min {
                // REQ-SW-7: it waits for later deposits to reach the minimum.
                info!(
                    "[{}] ERC20 deposit {} waiting: {} is below the target minimum {}",
                    self.chain.name, first.key, sweepable, min
                );
                if !recorded.is_empty() {
                    // Earlier parts went out; what is left is below the minimum.
                    return self.complete_group(from_address_str, group).await;
                }
                return Ok(());
            }
        }

        let parts = plan_sweep_parts(sweepable, target.min, target.max);
        for amount in parts {
            let record = self
                .sweep_erc20_part(
                    provider,
                    from_address_str,
                    token_address,
                    to_address,
                    amount,
                )
                .await?;
            let chain_name = self.chain.name.clone();
            let keys = keys.clone();
            self.db
                .blocking(move |db| db.record_sweep(&chain_name, &keys, &record))
                .await?;
        }

        self.complete_group(from_address_str, group).await
    }

    /// Distinct sweeps recorded against any deposit of the group.
    async fn recorded_sweeps(&self, keys: &[String]) -> Result<Vec<SweepRecord>> {
        let mut seen: HashSet<(String, u64)> = HashSet::new();
        let mut out = Vec::new();
        for key in keys {
            let chain_name = self.chain.name.clone();
            let key = key.clone();
            let records = self
                .db
                .blocking(move |db| db.get_sweeps(&chain_name, &key))
                .await?;
            for record in records {
                if seen.insert((record.sweep_tx_hash.clone(), record.sweep_log_index)) {
                    out.push(record);
                }
            }
        }
        Ok(out)
    }

    /// One `transfer(target, amount)` call: fund gas, send, check the receipt,
    /// and read the coordinate from the `Transfer` log it emitted.
    async fn sweep_erc20_part<SP>(
        &self,
        provider: &SP,
        from_address_str: &str,
        token_address: Address,
        to_address: Address,
        amount: U256,
    ) -> Result<SweepRecord>
    where
        SP: Provider<BoxTransport, alloy::network::Ethereum>,
    {
        let from_address = Address::from_str(from_address_str)?;
        let transfer_call = IERC20::transferCall {
            to: to_address,
            amount,
        };
        let call_data = transfer_call.abi_encode();

        let tx_for_estimate = TransactionRequest::default()
            .with_from(from_address)
            .with_to(token_address)
            .with_input(call_data.clone());

        let estimated_gas = provider.estimate_gas(&tx_for_estimate).await?;
        let gas_limit_with_buffer = estimated_gas + (estimated_gas / 10);
        let fee_estimate = provider.estimate_eip1559_fees(None).await?;
        let max_fee_per_gas = fee_estimate.max_fee_per_gas;
        let estimated_gas_cost = U256::from(gas_limit_with_buffer) * U256::from(max_fee_per_gas);
        let estimated_gas_cost_with_buffer =
            estimated_gas_cost + (estimated_gas_cost / U256::from(10));

        let mut native_balance = provider.get_balance(from_address).await?;

        if native_balance < estimated_gas_cost_with_buffer {
            match self.faucet.fund_new_address(from_address_str).await {
                Ok(_) => {
                    sleep(Duration::from_secs(2)).await;
                    native_balance = provider.get_balance(from_address).await?;
                    if native_balance < estimated_gas_cost_with_buffer {
                        return Err(anyhow::anyhow!(
                            "Insufficient native balance for gas even after faucet funding"
                        ));
                    }
                }
                Err(e) => return Err(e),
            }
        }

        let tx = TransactionRequest::default()
            .with_to(token_address)
            .with_input(call_data)
            .with_gas_limit(gas_limit_with_buffer);

        let pending_tx = provider.send_transaction(tx).await?;
        let receipt = pending_tx.get_receipt().await?;

        // REQ-SW-5, INV-5: a reverted sweep is a failure, never a sweep.
        if !receipt.inner.status() {
            return Err(anyhow::anyhow!(
                "sweep transaction reverted (receipt status 0): {:?}",
                receipt.transaction_hash
            ));
        }

        sweep_record_from_receipt(&receipt, token_address, from_address, to_address)
    }

    /// Marks the group's deposits swept and enqueues one `deposit_swept` per
    /// deposit, in one transaction (REQ-WH-7). Only reached once every part of
    /// the group has succeeded.
    async fn complete_group(&self, from_address_str: &str, group: &[Erc20Deposit]) -> Result<()> {
        let first = &group[0];
        let registration_id = &first.account_id;
        let webhook_url = {
            let reg_id = registration_id.clone();
            self.db
                .blocking(move |db| db.get_webhook_url(&reg_id))
                .await?
        };
        let token_decimals = {
            let chain_name = self.chain.name.clone();
            let token_address = first.token_address.clone();
            let stored = self
                .db
                .blocking(move |db| db.get_token_metadata(&chain_name, &token_address))
                .await?
                .map(|(_, decimals, _)| decimals);
            // The monitor reports an unreadable token as 18 decimals.
            stored.or((first.token_symbol == "UNKNOWN").then_some(18))
        };

        // Only deposits with a recorded sweep are settled. A deposit that joined
        // the group after the sweeps went out has none; it stays detected and
        // is swept on a later pass, so it cannot hold the others back.
        let mut settlements = Vec::with_capacity(group.len());
        for deposit in group {
            let sweeps = {
                let chain_name = self.chain.name.clone();
                let key = deposit.key.clone();
                self.db
                    .blocking(move |db| db.get_sweeps(&chain_name, &key))
                    .await?
            };
            if sweeps.is_empty() {
                continue;
            }
            let outbox = webhook_url.as_ref().map(|webhook_url| {
                let webhook_id = format!("{}:{}", self.chain.name, deposit.key);
                OutboxEntry {
                    id: webhook_id.clone(),
                    event: "deposit_swept".to_string(),
                    registration_id: registration_id.clone(),
                    webhook_url: webhook_url.clone(),
                    payload: self
                        .erc20_swept_payload(
                            &webhook_id,
                            from_address_str,
                            deposit,
                            token_decimals,
                            &sweeps,
                        )
                        .to_string(),
                }
            });
            settlements.push(Erc20Settlement {
                key: deposit.key.clone(),
                outbox,
            });
        }
        if settlements.is_empty() {
            return Err(anyhow::anyhow!(
                "no sweep recorded for deposit {}",
                first.key
            ));
        }

        let enqueued = {
            let chain_name = self.chain.name.clone();
            self.db
                .blocking(move |db| db.complete_erc20_sweep(&chain_name, &settlements))
                .await?
        };
        if enqueued > 0 {
            self.deliverer.notify_worker();
        }
        Ok(())
    }

    /// `deposit_swept` for a token deposit (REQ-WH-3, REQ-WH-4, REQ-WH-9).
    fn erc20_swept_payload(
        &self,
        id: &str,
        account_id: &str,
        deposit: &Erc20Deposit,
        token_decimals: Option<u8>,
        sweeps: &[SweepRecord],
    ) -> serde_json::Value {
        let entries: Vec<serde_json::Value> = sweeps
            .iter()
            .map(|s| {
                serde_json::json!({
                    "sweep_tx_hash": s.sweep_tx_hash,
                    "sweep_block_number": s.sweep_block_number,
                    "sweep_block_hash": s.sweep_block_hash,
                    "sweep_log_index": s.sweep_log_index,
                    "sweep_amount": s.sweep_amount,
                })
            })
            .collect();
        let mut payload = serde_json::json!({
            "id": id,
            "chain": self.chain.name,
            "chain_id": self.chain.chain_id,
            "event": "deposit_swept",
            "account_id": account_id,
            "registration_id": deposit.account_id,
            "original_tx_hash": deposit.key.split(':').next().unwrap_or(&deposit.key),
            "from_address": deposit.from_address,
            "amount": deposit.amount,
            "token_type": "erc20",
            "token_symbol": deposit.token_symbol,
            "token_address": deposit.token_address,
            "sweep_tx_hash": sweeps[0].sweep_tx_hash,
            "sweeps": entries,
        });
        if let Some(decimals) = token_decimals {
            payload["token_decimals"] = serde_json::json!(decimals);
        }
        payload
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

async fn get_token_balance(
    provider: &RootProvider<BoxTransport>,
    token_address: Address,
    owner_address: Address,
) -> Result<U256> {
    let contract = IERC20::new(token_address, provider);
    let balance = contract.balanceOf(owner_address).call().await?._0;
    Ok(balance)
}

/// Finds the token's `Transfer` from the deposit address to the target in a
/// successful sweep receipt and returns the sweep coordinate (REQ-SW-10). The
/// log index is the log's block-level index from the receipt, not its position
/// within the transaction. A receipt with no such log is an error: a sweep
/// whose coordinate cannot be shown is not reported.
fn sweep_record_from_receipt(
    receipt: &TransactionReceipt,
    token: Address,
    from: Address,
    to: Address,
) -> Result<SweepRecord> {
    let signature = keccak256("Transfer(address,address,uint256)".as_bytes());
    let topic_address = |topic: &alloy::primitives::B256| Address::from_slice(&topic[12..]);
    let log = receipt
        .inner
        .logs()
        .iter()
        .find(|log| {
            let topics = log.topics();
            log.address() == token
                && topics.len() == 3
                && topics[0] == signature
                && topic_address(&topics[1]) == from
                && topic_address(&topics[2]) == to
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "sweep receipt {:?} has no Transfer log from the deposit address to the target",
                receipt.transaction_hash
            )
        })?;
    let log_index = log
        .log_index
        .ok_or_else(|| anyhow::anyhow!("sweep Transfer log carries no log index"))?;
    let data = &log.data().data;
    let amount = if data.len() >= 32 {
        U256::from_be_slice(&data[..32])
    } else {
        U256::from_be_slice(data)
    };
    let block_number = receipt
        .block_number
        .ok_or_else(|| anyhow::anyhow!("sweep receipt carries no block number"))?;
    let block_hash = receipt
        .block_hash
        .ok_or_else(|| anyhow::anyhow!("sweep receipt carries no block hash"))?;
    Ok(SweepRecord {
        sweep_tx_hash: receipt.transaction_hash.to_string(),
        sweep_block_number: block_number,
        sweep_block_hash: block_hash.to_string(),
        sweep_log_index: log_index,
        sweep_amount: amount.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::{is_permanent_sweep_error, is_transient_funding_error};

    #[test]
    fn test_is_permanent_sweep_error() {
        assert!(is_permanent_sweep_error(
            "buffer overrun while deserializing"
        ));
        assert!(is_permanent_sweep_error(
            "ABI decode failed: Deserialization error"
        ));
        assert!(!is_permanent_sweep_error("execution reverted"));
        assert!(!is_permanent_sweep_error("network timeout"));
    }

    #[test]
    fn test_is_transient_funding_error() {
        assert!(is_transient_funding_error(
            "Faucet has insufficient balance to fund new address"
        ));
        assert!(is_transient_funding_error(
            "Insufficient native balance for gas even after faucet funding"
        ));
        assert!(is_transient_funding_error(
            "Still insufficient balance after faucet funding"
        ));
        assert!(!is_transient_funding_error("execution reverted"));
        assert!(!is_transient_funding_error(
            "buffer overrun while deserializing"
        ));
    }
}
