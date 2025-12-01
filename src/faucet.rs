use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use anyhow::Result;
use std::str::FromStr;
use tracing::{error, info};

use crate::wallet::Wallet;

pub struct Faucet<P> {
    wallet: Wallet,
    provider: P,
    existential_deposit: U256,
}

impl<T> Faucet<alloy::providers::RootProvider<T>>
where
    T: alloy::transports::Transport + Clone,
{
    pub fn new(
        faucet_mnemonic: String,
        provider: alloy::providers::RootProvider<T>,
        existential_deposit_str: &str,
    ) -> Result<Self> {
        let wallet = Wallet::new(faucet_mnemonic);
        let existential_deposit = U256::from_str(existential_deposit_str)?;

        Ok(Self {
            wallet,
            provider,
            existential_deposit,
        })
    }

    /// Send existential deposit to a newly created address
    pub async fn fund_new_address(&self, to_address: &str) -> Result<String> {
        let to = Address::from_str(to_address)?;

        info!(
            "Funding new address {} with {} wei",
            to_address, self.existential_deposit
        );

        // Get the faucet signer (using index 0 from the faucet mnemonic)
        let signer = self.wallet.get_signer(0)?;
        let faucet_address = signer.address();

        info!("Faucet address: {}", faucet_address);

        // Check faucet balance
        let balance = self.provider.get_balance(faucet_address).await?;
        if balance < self.existential_deposit {
            error!(
                "Faucet has insufficient balance: {} < {}",
                balance, self.existential_deposit
            );
            return Err(anyhow::anyhow!(
                "Faucet has insufficient balance to fund new address"
            ));
        }

        // Create a provider with the faucet wallet
        let wallet = alloy::network::EthereumWallet::from(signer);
        let faucet_provider = alloy::providers::ProviderBuilder::new()
            .with_recommended_fillers()
            .wallet(wallet)
            .on_provider(&self.provider);

        // Build and send transaction
        let tx = TransactionRequest::default()
            .with_to(to)
            .with_value(self.existential_deposit);

        let pending_tx = faucet_provider.send_transaction(tx).await?;
        let receipt = pending_tx.get_receipt().await?;

        let tx_hash = receipt.transaction_hash.to_string();
        info!(
            "Successfully funded address {} with tx: {}",
            to_address, tx_hash
        );

        Ok(tx_hash)
    }

    /// Check if an address already has sufficient balance (skip funding if it does)
    #[allow(dead_code)]
    pub async fn needs_funding(&self, address: &str) -> Result<bool> {
        let addr = Address::from_str(address)?;
        let balance = self.provider.get_balance(addr).await?;

        // If balance is less than existential deposit, it needs funding
        Ok(balance < self.existential_deposit)
    }
}
