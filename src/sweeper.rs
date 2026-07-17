use crate::{
    config::ChainConfig,
    db::{Db, Erc20Deposit},
    faucet::Faucet,
    wallet::Wallet,
    webhook::WebhookDeliverer,
};
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, RootProvider};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use alloy::transports::BoxTransport;
use anyhow::Result;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};

struct Erc20WebhookInfo<'a> {
    id: &'a str,
    chain: &'a str,
    chain_id: u64,
    account_id: &'a str,
    registration_id: &'a str,
    deposit_key: &'a str,
    amount: &'a str,
    token_symbol: &'a str,
    token_address: &'a str,
    token_decimals: Option<u8>,
    sweep_tx_hash: &'a str,
}

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

        let chain_name = self.chain.name.clone();
        let erc20_deposits = self
            .db
            .blocking(move |db| db.get_detected_erc20_deposits(&chain_name))
            .await?;
        let mut swept_pairs: HashSet<(String, String)> = HashSet::new();

        for deposit in erc20_deposits {
            let registration_id = &deposit.account_id;

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

            if deposit.token_symbol == "UNKNOWN" {
                let chain_name = self.chain.name.clone();
                let key = deposit.key.clone();
                self.db
                    .blocking(move |db| db.mark_erc20_deposit_swept(&chain_name, &key))
                    .await?;
                continue;
            }

            let (derivation_index, address_str, _webhook_url) = {
                let reg_id = registration_id.clone();
                self.db
                    .blocking(move |db| db.get_account_by_id(&reg_id))
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("Account not found"))?
            };

            let pair_key = (address_str.clone(), deposit.token_address.clone());
            if swept_pairs.contains(&pair_key) {
                continue;
            }

            let signer = self.wallet.get_signer(derivation_index)?;
            let wallet = alloy::network::EthereumWallet::from(signer);

            let sweep_provider = alloy::providers::ProviderBuilder::new()
                .with_recommended_fillers()
                .wallet(wallet)
                .on_provider(&self.provider);

            match self
                .sweep_erc20_deposit(&sweep_provider, &address_str, &deposit)
                .await
            {
                Ok(_) => {
                    swept_pairs.insert(pair_key);
                }
                Err(e) => {
                    let err_str = format!("{:?}", e);
                    if is_transient_funding_error(&err_str) {
                        warn!(
                            "[{}] ERC20 deposit {} waiting for faucet funding: {}",
                            self.chain.name, deposit.key, err_str
                        );
                    } else {
                        error!(
                            "[{}] Failed to sweep ERC20 deposit {}: {}",
                            self.chain.name, deposit.key, err_str
                        );
                        if is_permanent_sweep_error(&err_str) {
                            let failed = {
                                let chain_name = self.chain.name.clone();
                                let reg_id = registration_id.clone();
                                let token_address = deposit.token_address.clone();
                                self.db
                                    .blocking(move |db| {
                                        db.mark_erc20_deposits_failed_for_account_token(
                                            &chain_name,
                                            &reg_id,
                                            &token_address,
                                        )
                                    })
                                    .await?
                            };
                            for key in failed {
                                warn!(
                                    "[{}] Permanently failed ERC20 deposit {} (permanent sweep error)",
                                    self.chain.name, key
                                );
                            }
                        } else {
                            let failures = {
                                let chain_name = self.chain.name.clone();
                                let key = deposit.key.clone();
                                self.db
                                    .blocking(move |db| {
                                        db.increment_sweep_failure_count(&chain_name, &key)
                                    })
                                    .await
                            };
                            if let Ok(failures) = failures {
                                if failures >= MAX_SWEEP_RETRIES {
                                    let failed = {
                                        let chain_name = self.chain.name.clone();
                                        let reg_id = registration_id.clone();
                                        let token_address = deposit.token_address.clone();
                                        self.db
                                            .blocking(move |db| {
                                                db.mark_erc20_deposits_failed_for_account_token(
                                                    &chain_name,
                                                    &reg_id,
                                                    &token_address,
                                                )
                                            })
                                            .await?
                                    };
                                    for key in failed {
                                        warn!(
                                            "[{}] Permanently failed ERC20 deposit {} after {} attempts: {}",
                                            self.chain.name, key, failures, err_str
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
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

        {
            let chain_name = self.chain.name.clone();
            let tx_hash = tx_hash.to_string();
            self.db
                .blocking(move |db| db.mark_deposit_swept(&chain_name, &tx_hash))
                .await?;
        }

        let webhook_id = format!("{}:{}", self.chain.name, tx_hash);
        if let Err(e) = self
            .enqueue_deposit_swept_webhook(
                &webhook_id,
                from_address_str,
                registration_id,
                tx_hash,
                amount_str,
                None,
            )
            .await
        {
            error!(
                "[{}] Failed to enqueue deposit_swept webhook for {webhook_id}: {e:?}",
                self.chain.name
            );
        }

        info!(
            "[{}] Swept funds! Tx hash: {:?}",
            self.chain.name, receipt.transaction_hash
        );

        Ok(())
    }

    async fn sweep_erc20_deposit<SP>(
        &self,
        provider: &SP,
        from_address_str: &str,
        deposit: &Erc20Deposit,
    ) -> Result<()>
    where
        SP: Provider<BoxTransport, alloy::network::Ethereum>,
    {
        let from_address = Address::from_str(from_address_str)?;
        let to_address = Address::from_str(&self.chain.treasury_address)?;
        let token_address = Address::from_str(&deposit.token_address)?;

        let token_balance = get_token_balance(&self.provider, token_address, from_address).await?;

        if deposit.token_symbol.len() > 5 {
            let chain_name = self.chain.name.clone();
            let key = deposit.key.clone();
            self.db
                .blocking(move |db| db.mark_erc20_deposit_swept(&chain_name, &key))
                .await?;
            return Ok(());
        }

        if token_balance.is_zero() {
            let retry_count = {
                let chain_name = self.chain.name.clone();
                let key = deposit.key.clone();
                self.db
                    .blocking(move |db| db.increment_zero_balance_count(&chain_name, &key))
                    .await?
            };
            if retry_count >= MAX_ZERO_BALANCE_RETRIES {
                let chain_name = self.chain.name.clone();
                let key = deposit.key.clone();
                self.db
                    .blocking(move |db| db.mark_erc20_deposit_swept(&chain_name, &key))
                    .await?;
            }
            return Ok(());
        }

        let amount = token_balance;
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
        let sweep_tx_hash = receipt.transaction_hash.to_string();

        let registration_id = &deposit.account_id;
        let swept = {
            let chain_name = self.chain.name.clone();
            let reg_id = registration_id.clone();
            let token_address = deposit.token_address.clone();
            self.db
                .blocking(move |db| {
                    db.mark_erc20_deposits_swept_for_account_token(
                        &chain_name,
                        &reg_id,
                        &token_address,
                    )
                })
                .await?
        };

        let keys: Vec<String> = swept.iter().map(|(k, _)| k.clone()).collect();
        {
            let chain_name = self.chain.name.clone();
            let keys = keys.clone();
            let sweep_tx_hash = sweep_tx_hash.clone();
            self.db
                .blocking(move |db| db.set_sweep_tx_hash_for_keys(&chain_name, &keys, &sweep_tx_hash))
                .await?;
        }

        let token_decimals = {
            let chain_name = self.chain.name.clone();
            let token_address = deposit.token_address.clone();
            self.db
                .blocking(move |db| db.get_token_metadata(&chain_name, &token_address))
                .await?
                .map(|(_, decimals, _)| decimals)
        };

        for (key, dep_amount) in &swept {
            let webhook_id = format!("{}:{}", self.chain.name, key);
            let webhook_info = Erc20WebhookInfo {
                id: &webhook_id,
                chain: &self.chain.name,
                chain_id: self.chain.chain_id,
                account_id: from_address_str,
                registration_id,
                deposit_key: key,
                amount: dep_amount,
                token_symbol: &deposit.token_symbol,
                token_address: &deposit.token_address,
                token_decimals,
                sweep_tx_hash: &sweep_tx_hash,
            };
            if let Err(e) = self.enqueue_erc20_webhook(&webhook_info).await {
                error!(
                    "[{}] swept webhook enqueue failed for {key}: {e:?}",
                    self.chain.name
                );
            }
        }

        Ok(())
    }

    async fn enqueue_deposit_swept_webhook(
        &self,
        id: &str,
        account_id: &str,
        registration_id: &str,
        tx_hash: &str,
        amount: &str,
        erc20_info: Option<&Erc20WebhookInfo<'_>>,
    ) -> Result<()> {
        let webhook_url = {
            let reg_id = registration_id.to_string();
            self.db
                .blocking(move |db| db.get_webhook_url(&reg_id))
                .await?
        };
        let Some(webhook_url) = webhook_url else {
            return Ok(());
        };

        let payload = if let Some(info) = erc20_info {
            let mut payload = serde_json::json!({
                "id": info.id,
                "chain": info.chain,
                "chain_id": info.chain_id,
                "event": "deposit_swept",
                "account_id": info.account_id,
                "registration_id": info.registration_id,
                "original_tx_hash": info.deposit_key.split(':').next().unwrap_or(info.deposit_key),
                "amount": info.amount,
                "token_type": "erc20",
                "token_symbol": info.token_symbol,
                "token_address": info.token_address,
                "sweep_tx_hash": info.sweep_tx_hash
            });
            if let Some(decimals) = info.token_decimals {
                payload["token_decimals"] = serde_json::json!(decimals);
            }
            payload
        } else {
            serde_json::json!({
                "id": id,
                "chain": self.chain.name,
                "chain_id": self.chain.chain_id,
                "event": "deposit_swept",
                "account_id": account_id,
                "registration_id": registration_id,
                "original_tx_hash": tx_hash,
                "amount": amount,
                "token_type": "native"
            })
        };

        self.deliverer
            .enqueue(&webhook_url, registration_id, payload)
            .await
    }

    async fn enqueue_erc20_webhook(&self, info: &Erc20WebhookInfo<'_>) -> Result<()> {
        self.enqueue_deposit_swept_webhook(
            info.id,
            info.account_id,
            info.registration_id,
            info.deposit_key,
            info.amount,
            Some(info),
        )
        .await
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
