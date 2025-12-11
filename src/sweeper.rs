use crate::{
    config::Config,
    db::{Db, Erc20Deposit},
    wallet::Wallet,
};
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use anyhow::Result;
use std::str::FromStr;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info};

pub struct Sweeper<P> {
    config: Config,
    db: Db,
    wallet: Wallet,
    provider: P,
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
    ) -> Self {
        Self {
            config,
            db,
            wallet,
            provider,
        }
    }

    async fn process_deposits(&self) -> Result<()> {
        // Process native ETH deposits
        let deposits = self.db.get_detected_deposits()?;

        for (tx_hash, account_id, amount_str) in deposits {
            info!(
                "Processing native ETH deposit: tx_hash={}, account={}, amount={}",
                tx_hash, account_id, amount_str
            );

            // Get account details to derive key
            let (derivation_index, address_str, _webhook_url) = self
                .db
                .get_account_by_id(&account_id)?
                .ok_or_else(|| anyhow::anyhow!("Account not found"))?;

            let signer = self.wallet.get_signer(derivation_index)?;
            let wallet = alloy::network::EthereumWallet::from(signer);

            let sweep_provider = alloy::providers::ProviderBuilder::new()
                .with_recommended_fillers()
                .wallet(wallet)
                .on_provider(&self.provider);

            self.sweep_deposit(
                &sweep_provider,
                &address_str,
                &tx_hash,
                &account_id,
                &amount_str,
            )
            .await?;
        }

        // Process ERC20 deposits
        let erc20_deposits = self.db.get_detected_erc20_deposits()?;

        for deposit in erc20_deposits {
            info!(
                "Processing ERC20 deposit: key={}, token={} ({}), account={}, amount={}",
                deposit.key,
                deposit.token_symbol,
                deposit.token_address,
                deposit.account_id,
                deposit.amount
            );

            // Get account details to derive key
            let (derivation_index, address_str, _webhook_url) = self
                .db
                .get_account_by_id(&deposit.account_id)?
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
        account_id: &str,
        amount_str: &str,
    ) -> Result<()>
    where
        SP: Provider<T, alloy::network::Ethereum>,
    {
        let from_address = Address::from_str(from_address_str)?;
        let to_address = Address::from_str(&self.config.treasury_address)?;

        // Check balance again to be sure (and to calculate gas)
        let balance = provider.get_balance(from_address).await?;

        // Simple gas estimation/reservation (leaving some dust for gas)
        let gas_price = provider.get_gas_price().await?;
        let gas_limit = 21000; // Standard transfer
        let gas_cost = U256::from(gas_limit) * U256::from(gas_price);

        if balance <= gas_cost {
            info!("Balance too low to sweep: {} <= {}", balance, gas_cost);
            return Ok(());
        }

        let value_to_send = balance - gas_cost;

        let tx = TransactionRequest::default()
            .with_to(to_address)
            .with_value(value_to_send)
            .with_gas_limit(gas_limit)
            .with_gas_price(gas_price);

        let pending_tx = provider.send_transaction(tx).await?;
        let receipt = pending_tx.get_receipt().await?;

        info!("Swept funds! Tx hash: {:?}", receipt.transaction_hash);

        // Update DB
        self.db.mark_deposit_swept(tx_hash)?;

        // Send Webhook
        self.send_webhook(account_id, tx_hash, amount_str).await?;

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

        // Check native balance first (need gas for ERC20 transfer)
        let native_balance = provider.get_balance(from_address).await?;

        info!(
            "Native balance: {} wei for address {}",
            native_balance, from_address_str
        );

        if native_balance.is_zero() {
            error!(
                "Cannot sweep ERC20 tokens from {}: no native balance for gas. Address needs to be funded first.",
                from_address_str
            );

            return Err(anyhow::anyhow!(
                "Insufficient native balance for gas. Address: {}, Balance: 0",
                from_address_str
            ));
        }

        // Estimate gas cost
        let gas_price = provider.get_gas_price().await?;
        let gas_limit = 100000u128;
        let estimated_gas_cost = U256::from(gas_limit) * U256::from(gas_price);

        if native_balance < estimated_gas_cost {
            error!(
                "Insufficient native balance for gas. Address: {}, Balance: {} wei, Estimated gas cost: {} wei",
                from_address_str, native_balance, estimated_gas_cost
            );
            return Err(anyhow::anyhow!(
                "Insufficient native balance for gas. Need at least {} wei, but only have {} wei",
                estimated_gas_cost,
                native_balance
            ));
        }

        info!(
            "Native balance check passed: {} wei (gas estimate: {} wei)",
            native_balance, estimated_gas_cost
        );

        // Check token balance
        let token_balance = get_token_balance(&self.provider, token_address, from_address).await?;

        if token_balance.is_zero() {
            info!(
                "ERC20 balance is zero for {} token at {}, skipping sweep",
                deposit.token_symbol, from_address_str
            );
            return Ok(());
        }

        info!(
            "Sweeping {} {} tokens (raw: {}) from {} to {} (native balance: {} wei)",
            token_balance,
            deposit.token_symbol,
            token_balance,
            from_address,
            to_address,
            native_balance
        );

        // Build ERC20 transfer call data
        let transfer_call = IERC20::transferCall {
            to: to_address,
            amount: token_balance,
        };

        let call_data = transfer_call.abi_encode();

        let tx = TransactionRequest::default()
            .with_to(token_address)
            .with_input(call_data)
            .with_gas_limit(gas_limit);

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

        // Send Webhook
        self.send_erc20_webhook(
            &deposit.account_id,
            &deposit.key,
            &deposit.amount,
            &deposit.token_symbol,
            &deposit.token_address,
            token_decimals,
        )
        .await?;

        Ok(())
    }

    async fn send_webhook(&self, account_id: &str, tx_hash: &str, amount: &str) -> Result<()> {
        // Get the webhook URL for this account
        let webhook_url = match self.db.get_webhook_url(account_id)? {
            Some(url) => url,
            None => {
                error!("No webhook URL found for account: {}", account_id);
                return Ok(());
            }
        };

        let client = reqwest::Client::new();
        let payload = serde_json::json!({
            "event": "deposit_swept",
            "account_id": account_id,
            "original_tx_hash": tx_hash,
            "amount": amount,
            "token_type": "native"
        });

        let res = client.post(&webhook_url).json(&payload).send().await;

        match res {
            Ok(r) => info!("Webhook sent to {}: status={}", webhook_url, r.status()),
            Err(e) => error!("Failed to send webhook to {}: {:?}", webhook_url, e),
        }

        Ok(())
    }

    async fn send_erc20_webhook(
        &self,
        account_id: &str,
        deposit_key: &str,
        amount: &str,
        token_symbol: &str,
        token_address: &str,
        token_decimals: Option<u8>,
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
            "event": "deposit_swept",
            "account_id": account_id,
            "original_tx_hash": deposit_key,
            "amount": amount,
            "token_type": "erc20",
            "token_symbol": token_symbol,
            "token_address": token_address
        });

        // Add decimals if available
        if let Some(decimals) = token_decimals {
            payload["token_decimals"] = serde_json::json!(decimals);
        }

        let res = client.post(&webhook_url).json(&payload).send().await;

        match res {
            Ok(r) => info!(
                "ERC20 Webhook sent to {}: status={}",
                webhook_url,
                r.status()
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
