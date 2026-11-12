use alloy::network::{Ethereum, EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, TxHash, U256};
use alloy::providers::fillers::{FillProvider, JoinFill, RecommendedFiller, WalletFiller};
use alloy::providers::{Provider, ProviderBuilder, RootProvider};
use alloy::rpc::types::TransactionRequest;
use alloy::transports::{BoxTransport, Transport};
use anyhow::Result;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::wallet::Wallet;

type FaucetProvider = FillProvider<
    JoinFill<RecommendedFiller, WalletFiller<EthereumWallet>>,
    RootProvider<BoxTransport>,
    BoxTransport,
    Ethereum,
>;

/// How long a top-up that was broadcast but not seen confirmed blocks a second
/// top-up for the same address.
const UNCONFIRMED_HOLD: Duration = Duration::from_secs(600);

pub struct Faucet {
    root: RootProvider<BoxTransport>,
    wallet: EthereumWallet,
    faucet_address: Address,
    existential_deposit: U256,
    provider: RwLock<FaucetProvider>,
    /// Top-ups that were broadcast but whose receipt could not be read (amendment
    /// 0006: once `send_transaction` succeeds, the top-up is sent).
    unconfirmed: Mutex<HashMap<Address, (TxHash, Instant)>>,
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
            unconfirmed: Mutex::new(HashMap::new()),
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

        // A top-up broadcast earlier whose receipt we could not read: do not
        // send a second one while the first may still land.
        let earlier = self.unconfirmed.lock().unwrap().get(&to).copied();
        if let Some((hash, at)) = earlier {
            let landed = matches!(self.root.get_transaction_receipt(hash).await, Ok(Some(_)));
            if landed || at.elapsed() > UNCONFIRMED_HOLD {
                self.unconfirmed.lock().unwrap().remove(&to);
            } else {
                return Err(anyhow::anyhow!(
                    "faucet top-up pending: {hash} was sent to {to_address} and is not confirmed yet"
                ));
            }
        }

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
            let sent_hash = *pending.tx_hash();
            // alloy 0.1 has no receipt timeout: bound the wait so one stuck top-up
            // cannot freeze the sweeper.
            let waited = tokio::time::timeout(Duration::from_secs(60), pending.get_receipt()).await;
            match waited {
                Ok(Ok(receipt)) => receipt,
                other => {
                    // The transaction went out, so the top-up counts as sent. The
                    // nonce cache may be stale; rebuild it. The caller re-checks
                    // the balance on its next pass.
                    let why = match other {
                        Ok(Err(e)) => e.to_string(),
                        _ => "receipt wait timed out".to_string(),
                    };
                    drop(provider);
                    warn!(
                        "Faucet top-up of {to_address} was broadcast as {sent_hash} but its receipt could not be read ({why}); counting it as sent"
                    );
                    *self.provider.write().await = Self::build_provider(&self.root, &self.wallet);
                    self.unconfirmed
                        .lock()
                        .unwrap()
                        .insert(to, (sent_hash, Instant::now()));
                    return Ok(sent_hash.to_string());
                }
            }
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
