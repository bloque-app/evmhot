use crate::{
    config::Config,
    db::{Db, Erc20Deposit},
    faucet::Faucet,
    wallet::Wallet,
};
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use anyhow::Result;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info};

/// Information about an ERC20 deposit for webhook notification
struct Erc20WebhookInfo<'a> {
    id: &'a str,
    account_id: &'a str,        // Polygon address
    registration_id: &'a str,   // Original id used when registering
    deposit_key: &'a str,
    amount: &'a str,
    token_symbol: &'a str,
    token_address: &'a str,
    token_decimals: Option<u8>,
}

pub struct Sweeper<P> {
    config: Config,
    db: Db,
    wallet: Wallet,
    provider: P,
    faucet: Arc<Faucet<P>>,
}

use crate::traits::Service;
use async_trait::async_trait;

#[async_trait]
impl<T> Service for Sweeper<alloy::providers::RootProvider<T>>
where
    T: alloy::transports::Transport + Clone,
{
    async fn run(&self) {
        loop {
            if let Err(e) = self.process_deposits().await {
                error!("Error in sweeper loop: {:?}", e);
            }
            sleep(Duration::from_secs(self.config.poll_interval)).await;
        }
    }
}

impl<T> Sweeper<alloy::providers::RootProvider<T>>
where
    T: alloy::transports::Transport + Clone,
{
    pub fn new(
        config: Config,
        db: Db,
        wallet: Wallet,
        provider: alloy::providers::RootProvider<T>,
        faucet: Arc<Faucet<alloy::providers::RootProvider<T>>>,
    ) -> Self {
        Self {
            config,
            db,
            wallet,
            provider,
            faucet,
        }
    }

    async fn process_deposits(&self) -> Result<()> {
        // Process native ETH deposits
        let deposits = self.db.get_detected_deposits()?;

        for (tx_hash, registration_id, amount_str) in deposits {
            info!(
                "Processing native ETH deposit: tx_hash={}, registration_id={}, amount={}",
                tx_hash, registration_id, amount_str
            );

            // Get account details to derive key (registration_id is the key in ACCOUNTS table)
            let (derivation_index, address_str, _webhook_url) = self
                .db
                .get_account_by_id(&registration_id)?
                .ok_or_else(|| anyhow::anyhow!("Account not found"))?;

            let signer = self.wallet.get_signer(derivation_index)?;
            let wallet = alloy::network::EthereumWallet::from(signer);

            let sweep_provider = alloy::providers::ProviderBuilder::new()
                .with_recommended_fillers()
                .wallet(wallet)
                .on_provider(&self.provider);

            match self.sweep_deposit(
                &sweep_provider,
                &address_str,
                &tx_hash,
                &registration_id,
                &amount_str,
            )
            .await {
                Ok(_) => info!("Successfully swept native ETH deposit: {}", tx_hash),
                Err(e) => {
                    error!("Failed to sweep native ETH deposit {}: {:?}", tx_hash, e);
                }
            }
        }

        // Process ERC20 deposits
        let erc20_deposits = self.db.get_detected_erc20_deposits()?;

        for deposit in erc20_deposits {
            // deposit.account_id is actually the registration_id (original id from registration)
            let registration_id = &deposit.account_id;
            
            info!(
                "Processing ERC20 deposit: key={}, token={} ({}), registration_id={}, amount={}",
                deposit.key,
                deposit.token_symbol,
                deposit.token_address,
                registration_id,
                deposit.amount
            );

            if deposit.token_symbol == "UNKNOWN" {
                error!(
                    "Skipping ERC20 deposit token symbol for deposit: {}",
                    deposit.key
                );
                self.db.mark_erc20_deposit_swept(&deposit.key)?;
                continue;
            }

            // Get account details to derive key (registration_id is the key in ACCOUNTS table)
            let (derivation_index, address_str, _webhook_url) = self
                .db
                .get_account_by_id(registration_id)?
                .ok_or_else(|| anyhow::anyhow!("Account not found"))?;

            let signer = self.wallet.get_signer(derivation_index)?;

            info!("Signer address: {}", signer.address());

            let wallet = alloy::network::EthereumWallet::from(signer);

            let sweep_provider = alloy::providers::ProviderBuilder::new()
                .with_recommended_fillers()
                .wallet(wallet)
                .on_provider(&self.provider);

            // Try to sweep, but don't fail the entire loop if one sweep fails
            match self
                .sweep_erc20_deposit(&sweep_provider, &address_str, &deposit)
                .await
            {
                Ok(_) => info!("Successfully swept ERC20 deposit: {}", deposit.key),
                Err(e) => {
                    error!("Failed to sweep ERC20 deposit {}: {:?}", deposit.key, e);
                    // Don't return error - continue processing other deposits
                    // This deposit will be retried in the next sweep cycle
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
        SP: Provider<T, alloy::network::Ethereum>,
    {
        let from_address = Address::from_str(from_address_str)?;
        let to_address = Address::from_str(&self.config.treasury_address)?;

        // Check balance again to be sure (and to calculate gas)
        let mut balance = provider.get_balance(from_address).await?;

        // Standard ETH transfer gas limit
        let gas_limit: u128 = 21000;

        // Get current fee estimates (EIP-1559 compatible)
        let fee_estimate = provider.estimate_eip1559_fees(None).await?;
        let max_fee_per_gas = fee_estimate.max_fee_per_gas;
        
        // Calculate gas cost with 50% buffer for price fluctuations
        let gas_cost = U256::from(gas_limit) * U256::from(max_fee_per_gas);
        let gas_cost_with_buffer = gas_cost + (gas_cost / U256::from(10));

        info!(
            "Gas estimation for native ETH transfer: gas_limit={}, max_fee_per_gas={}, gas_cost={} wei (with 50% buffer: {} wei)",
            gas_limit, max_fee_per_gas, gas_cost, gas_cost_with_buffer
        );

        // If balance is too low to cover gas, try to fund via faucet
        if balance <= gas_cost_with_buffer {
            info!(
                "Balance too low to sweep: {} <= {}. Attempting to fund via faucet...",
                balance, gas_cost_with_buffer
            );

            // Fund the address via faucet
            match self.faucet.fund_new_address(from_address_str).await {
                Ok(tx_hash) => {
                    info!(
                        "Successfully funded address {} via faucet with tx: {}. Waiting for balance update...",
                        from_address_str, tx_hash
                    );
                    
                    // Wait a bit for the transaction to be processed and balance to update
                    sleep(Duration::from_secs(2)).await;
                    
                    // Re-check the balance after funding
                    balance = provider.get_balance(from_address).await?;
                    info!(
                        "Updated balance after faucet funding: {} wei for address {}",
                        balance, from_address_str
                    );

                    // Final check - if still not enough, return error
                    if balance <= gas_cost_with_buffer {
                        return Err(anyhow::anyhow!(
                            "Still insufficient balance after faucet funding. Address: {}, Balance: {} wei, Gas cost: {} wei",
                            from_address_str, balance, gas_cost_with_buffer
                        ));
                    }
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Failed to fund address {} via faucet: {}",
                        from_address_str, e
                    ));
                }
            }
        }

        // Use actual gas cost (without buffer) for value calculation to maximize sweep amount
        let value_to_send = balance - gas_cost_with_buffer;

        let tx = TransactionRequest::default()
            .with_to(to_address)
            .with_value(value_to_send)
            .with_gas_limit(gas_limit);

        let pending_tx = provider.send_transaction(tx).await?;
        let receipt = pending_tx.get_receipt().await?;

        info!("Swept funds! Tx hash: {:?}", receipt.transaction_hash);

        // Update DB
        self.db.mark_deposit_swept(tx_hash)?;

        // Send Webhook (for native deposits, id = tx_hash)
        // account_id = Polygon address, registration_id = original id from registration
        self.send_webhook(tx_hash, from_address_str, registration_id, tx_hash, amount_str)
            .await?;

        Ok(())
    }

    async fn sweep_erc20_deposit<SP>(
        &self,
        provider: &SP,
        from_address_str: &str,
        deposit: &Erc20Deposit,
    ) -> Result<()>
    where
        SP: Provider<T, alloy::network::Ethereum>,
    {
        let from_address = Address::from_str(from_address_str)?;
        let to_address = Address::from_str(&self.config.treasury_address)?;
        let token_address = Address::from_str(&deposit.token_address)?;

        // Check token balance first
        let token_balance = get_token_balance(&self.provider, token_address, from_address).await?;

        if token_balance.is_zero() {
            info!(
                "ERC20 balance is zero for {} token at {}, skipping sweep",
                deposit.token_symbol, from_address_str
            );
            return Ok(());
        }

        let amount = U256::from_str(&deposit.amount).unwrap_or(token_balance);
        
        // Build ERC20 transfer call data for gas estimation
        let transfer_call = IERC20::transferCall {
            to: to_address,
            amount,
        };

        let call_data = transfer_call.abi_encode();

        // Build transaction request for gas estimation
        let tx_for_estimate = TransactionRequest::default()
            .with_from(from_address)
            .with_to(token_address)
            .with_input(call_data.clone());

        // Estimate actual gas needed for this specific transaction
        let estimated_gas = provider.estimate_gas(&tx_for_estimate).await?;
        
        let gas_limit_with_buffer = estimated_gas + (estimated_gas / 10);

        // Get current fee estimates (EIP-1559 compatible)
        let fee_estimate = provider.estimate_eip1559_fees(None).await?;
        let max_fee_per_gas = fee_estimate.max_fee_per_gas;
        
        // Calculate worst-case gas cost with safety buffer
        // Add extra 10% buffer on top for gas price fluctuations
        let estimated_gas_cost = U256::from(gas_limit_with_buffer) * U256::from(max_fee_per_gas);
        let estimated_gas_cost_with_buffer = estimated_gas_cost + (estimated_gas_cost / U256::from(10));

        info!(
            "Gas estimation for ERC20 transfer: gas={}, max_fee_per_gas={}, estimated_cost={} wei (with 50% buffer: {} wei)",
            gas_limit_with_buffer, max_fee_per_gas, estimated_gas_cost, estimated_gas_cost_with_buffer
        );

        // Check native balance (need gas for ERC20 transfer)
        let mut native_balance = provider.get_balance(from_address).await?;

        info!(
            "Native balance: {} wei for address {}",
            native_balance, from_address_str
        );

        // If insufficient balance for gas, try to fund via faucet
        if native_balance < estimated_gas_cost_with_buffer {
            info!(
                "Insufficient native balance for gas. Address: {}, Balance: {} wei, Estimated gas cost: {} wei. Attempting to fund via faucet...",
                from_address_str, native_balance, estimated_gas_cost_with_buffer
            );

            // Fund the address via faucet
            match self.faucet.fund_new_address(from_address_str).await {
                Ok(tx_hash) => {
                    info!(
                        "Successfully funded address {} via faucet with tx: {}. Waiting for balance update...",
                        from_address_str, tx_hash
                    );
                    
                    // Wait a bit for the transaction to be processed and balance to update
                    sleep(Duration::from_secs(2)).await;
                    
                    // Re-check the balance after funding
                    native_balance = provider.get_balance(from_address).await?;
                    info!(
                        "Updated native balance after faucet funding: {} wei for address {}",
                        native_balance, from_address_str
                    );

                    // Final check - if still not enough, error out
                    if native_balance < estimated_gas_cost_with_buffer {
                        error!(
                            "Still insufficient balance after faucet funding. Address: {}, Balance: {} wei, Required: {} wei",
                            from_address_str, native_balance, estimated_gas_cost_with_buffer
                        );
                        return Err(anyhow::anyhow!(
                            "Insufficient native balance for gas even after faucet funding. Need at least {} wei, but only have {} wei",
                            estimated_gas_cost_with_buffer,
                            native_balance
                        ));
                    }
                }
                Err(e) => {
                    error!(
                        "Failed to fund address {} via faucet: {:?}",
                        from_address_str, e
                    );
                    return Err(anyhow::anyhow!(
                        "Insufficient native balance for gas and faucet funding failed. Address: {}, Balance: {} wei, Error: {}",
                        from_address_str,
                        native_balance,
                        e
                    ));
                }
            }
        }

        info!(
            "Native balance check passed: {} wei (gas estimate with buffer: {} wei)",
            native_balance, estimated_gas_cost_with_buffer
        );

        info!(
            "Sweeping {} {} tokens (raw: {}) from {} to {} (native balance: {} wei)",
            token_balance,
            deposit.token_symbol,
            token_balance,
            from_address,
            to_address,
            native_balance
        );

        // Build final transaction with estimated gas limit
        let tx = TransactionRequest::default()
            .with_to(token_address)
            .with_input(call_data)
            .with_gas_limit(gas_limit_with_buffer);

        let pending_tx = provider.send_transaction(tx).await?;
        let receipt = pending_tx.get_receipt().await?;

        info!(
            "Swept ERC20 tokens! Tx hash: {:?}",
            receipt.transaction_hash
        );

        // Update DB
        self.db.mark_erc20_deposit_swept(&deposit.key)?;

        // Fetch token decimals from DB
        let token_decimals = self
            .db
            .get_token_metadata(&deposit.token_address)?
            .map(|(_, decimals, _)| decimals);

        // deposit.account_id is actually the registration_id
        let registration_id = &deposit.account_id;

        // Send Webhook (for ERC20 deposits, id = deposit.key which is tx_hash:log_index)
        // account_id = Polygon address (from_address_str), registration_id = original id from registration
        let webhook_info = Erc20WebhookInfo {
            id: &deposit.key,
            account_id: from_address_str,
            registration_id,
            deposit_key: &deposit.key,
            amount: &deposit.amount,
            token_symbol: &deposit.token_symbol,
            token_address: &deposit.token_address,
            token_decimals,
        };
        self.send_erc20_webhook(&webhook_info).await?;

        Ok(())
    }

    async fn send_webhook(
        &self,
        id: &str,
        account_id: &str,
        registration_id: &str,
        tx_hash: &str,
        amount: &str,
    ) -> Result<()> {
        // Get the webhook URL using registration_id (the key in ACCOUNTS table)
        let Some(webhook_url) = self.db.get_webhook_url(registration_id)? else {
            error!("No webhook URL found for registration_id: {}", registration_id);
            return Ok(());
        };

        let client = reqwest::Client::new();
        let payload = serde_json::json!({
            "id": id,
            "event": "deposit_swept",
            "account_id": account_id,
            "registration_id": registration_id,
            "original_tx_hash": tx_hash,
            "amount": amount,
            "token_type": "native"
        });

        let mut request = client.post(&webhook_url).json(&payload);
        
        // Add JWT authorization header if configured
        if let Some(ref token) = self.config.webhook_jwt_token {
            request = request.header("Authorization", format!("Bearer {}", token));
        }

        let res = request.send().await;

        match res {
            Ok(r) => info!(
                "Webhook sent to {}: status={}, registration_id={}",
                webhook_url, r.status(), registration_id
            ),
            Err(e) => error!("Failed to send webhook to {}: {:?}", webhook_url, e),
        }

        Ok(())
    }

    async fn send_erc20_webhook(&self, info: &Erc20WebhookInfo<'_>) -> Result<()> {
        // Get the webhook URL using registration_id (the key in ACCOUNTS table)
        let Some(webhook_url) = self.db.get_webhook_url(info.registration_id)? else {
            error!("No webhook URL found for registration_id: {}", info.registration_id);
            return Ok(());
        };

        let client = reqwest::Client::new();
        let mut payload = serde_json::json!({
            "id": info.id,
            "event": "deposit_swept",
            "account_id": info.account_id,
            "registration_id": info.registration_id,
            "original_tx_hash": info.deposit_key.split(':').nth(0).unwrap(),
            "amount": info.amount,
            "token_type": "erc20",
            "token_symbol": info.token_symbol,
            "token_address": info.token_address
        });

        // Add decimals if available
        if let Some(decimals) = info.token_decimals {
            payload["token_decimals"] = serde_json::json!(decimals);
        }

        let mut request = client.post(&webhook_url).json(&payload);
        
        // Add JWT authorization header if configured
        if let Some(ref token) = self.config.webhook_jwt_token {
            request = request.header("Authorization", format!("Bearer {}", token));
        }

        let res = request.send().await;

        match res {
            Ok(r) => info!(
                "ERC20 Webhook sent to {}: status={}, registration_id={}",
                webhook_url,
                r.status(),
                info.registration_id
            ),
            Err(e) => error!("Failed to send ERC20 webhook to {}: {:?}", webhook_url, e),
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

async fn get_token_balance<T>(
    provider: &alloy::providers::RootProvider<T>,
    token_address: Address,
    owner_address: Address,
) -> Result<U256>
where
    T: alloy::transports::Transport + Clone,
{
    let contract = IERC20::new(token_address, provider);
    let balance = contract.balanceOf(owner_address).call().await?._0;
    Ok(balance)
}
