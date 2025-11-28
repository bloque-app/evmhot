use crate::{config::Config, db::Db, wallet::Wallet};
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
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
        // Fetch detected deposits that haven't been swept
        let deposits = self.db.get_detected_deposits()?;

        for (tx_hash, account_id, amount_str) in deposits {
            info!(
                "Processing deposit: tx_hash={}, account={}, amount={}",
                tx_hash, account_id, amount_str
            );

            // Get account details to derive key
            let (derivation_index, address_str) = self
                .db
                .get_account_by_id(&account_id)?
                .ok_or_else(|| anyhow::anyhow!("Account not found"))?;

            // Note: In this refactor, the provider already has a wallet attached (FillProvider).
            // However, that wallet is likely the "root" wallet or we need to switch signers?
            // Wait, the previous implementation created a NEW provider with a specific signer for EACH deposit.
            // Because each deposit needs to be swept FROM a specific derived address.
            // The `FillProvider` with a single wallet won't work if we need to sign as different users.
            // BUT, we are sweeping FROM the derived address TO the treasury.
            // So we MUST sign with the derived private key.
            // The `provider` passed to Sweeper might be a "base" provider, but we need to sign transactions.
            // Alloy's `Provider` trait doesn't easily allow "switching" the signer if it's baked into the type.
            // We might need to construct a `WalletProvider` on the fly using the base provider's transport.
            // OR, we can just use `send_raw_transaction` if we sign manually?
            // OR, we can clone the base provider (if cheap) and wrap it?

            // Let's look at how we did it before:
            // `ProviderBuilder::new().wallet(wallet).on_http(...)`

            // If we pass a `RootProvider` to Sweeper, we can wrap it.
            // But `Sweeper` is generic over `P`.
            // If `P` is `RootProvider`, we can wrap it.
            // If `P` is `FillProvider`, it already has a wallet.

            // Issue: We need to sign with *different* keys.
            // So we probably shouldn't pass a `FillProvider` with a fixed wallet to `Sweeper`.
            // We should pass the *underlying* provider (RootProvider) and then wrap it with the specific signer for the sweep.

            // Let's assume P is the RootProvider (Http or Ws).
            // We can use `ProviderBuilder` to wrap it?
            // `ProviderBuilder::new().wallet(wallet).on_provider(&self.provider)`?

            let signer = self.wallet.get_signer(derivation_index)?;
            let wallet = alloy::network::EthereumWallet::from(signer);

            // We need to create a provider that uses this wallet.
            // Since we want to reuse the connection (P), we should check if we can wrap it.
            // Alloy allows layering.

            // Let's try to use the provider to get data, but for signing, we might need to construct a new provider wrapping the transport?
            // Or just use `ProviderBuilder` with the existing provider.

            // Actually, `ProviderBuilder` has `on_provider`.
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

    async fn send_webhook(&self, account_id: &str, tx_hash: &str, amount: &str) -> Result<()> {
        let client = reqwest::Client::new();
        let payload = serde_json::json!({
            "event": "deposit_swept",
            "account_id": account_id,
            "original_tx_hash": tx_hash,
            "amount": amount
        });

        let res = client
            .post(&self.config.webhook_url)
            .json(&payload)
            .send()
            .await;

        match res {
            Ok(r) => info!("Webhook sent: status={}", r.status()),
            Err(e) => error!("Failed to send webhook: {:?}", e),
        }

        Ok(())
    }
}
