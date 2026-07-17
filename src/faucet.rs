use alloy::network::{Ethereum, EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, U256};
use alloy::providers::fillers::{FillProvider, JoinFill, RecommendedFiller, WalletFiller};
use alloy::providers::{Provider, ProviderBuilder, RootProvider};
use alloy::rpc::types::TransactionRequest;
use alloy::transports::{BoxTransport, Transport};
use anyhow::Result;
use std::str::FromStr;
use tokio::sync::RwLock;
use tracing::{error, info};

use crate::wallet::Wallet;

type FaucetProvider = FillProvider<
    JoinFill<RecommendedFiller, WalletFiller<EthereumWallet>>,
    RootProvider<BoxTransport>,
    BoxTransport,
    Ethereum,
>;

pub struct Faucet {
    root: RootProvider<BoxTransport>,
    wallet: EthereumWallet,
    faucet_address: Address,
    existential_deposit: U256,
    provider: RwLock<FaucetProvider>,
}

impl Faucet {
    pub fn new<T: Transport + Clone>(
        faucet_mnemonic: String,
        provider: RootProvider<T>,
        existential_deposit_str: &str,
    ) -> Result<Self> {
        let signer = Wallet::new(faucet_mnemonic).get_signer(0)?;
        let faucet_address = signer.address();
        let wallet = EthereumWallet::from(signer);
        let root = provider.boxed();
        let existential_deposit = U256::from_str(existential_deposit_str)?;
        let provider = RwLock::new(Self::build_provider(&root, &wallet));

        Ok(Self {
            root,
            wallet,
            faucet_address,
            existential_deposit,
            provider,
        })
    }

    fn build_provider(
        root: &RootProvider<BoxTransport>,
        wallet: &EthereumWallet,
    ) -> FaucetProvider {
        ProviderBuilder::new()
            .with_recommended_fillers()
            .wallet(wallet.clone())
            .on_provider(root.clone())
    }

    /// Send existential deposit to a newly created address
    pub async fn fund_new_address(&self, to_address: &str) -> Result<String> {
        let to = Address::from_str(to_address)?;

        info!(
            "Funding new address {} with {} wei",
            to_address, self.existential_deposit
        );
        info!("Faucet address: {}", self.faucet_address);

        let balance = self.root.get_balance(self.faucet_address).await?;
        if balance < self.existential_deposit {
            error!(
                "Faucet has insufficient balance: {} < {}",
                balance, self.existential_deposit
            );
            return Err(anyhow::anyhow!(
                "Faucet has insufficient balance to fund new address"
            ));
        }

        let tx = TransactionRequest::default()
            .with_from(self.faucet_address)
            .with_to(to)
            .with_value(self.existential_deposit);

        let receipt = {
            let provider = self.provider.read().await;
            let pending = match provider.send_transaction(tx).await {
                Ok(p) => p,
                Err(e) => {
                    drop(provider);
                    error!("Faucet send failed, resetting nonce cache: {e}");
                    *self.provider.write().await = Self::build_provider(&self.root, &self.wallet);
                    return Err(e.into());
                }
            };
            pending.get_receipt().await?
        };

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
        let balance = self.root.get_balance(addr).await?;
        Ok(balance < self.existential_deposit)
    }
}
