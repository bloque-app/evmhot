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
/// How long a confirmed top-up does. A node that lags the one that confirmed it
/// can still show the old balance; a second top-up for the same need would be a
/// double fund. Passes in this window wait ("faucet top-up pending").
const CONFIRMED_HOLD: Duration = Duration::from_secs(120);
/// A reservation with no hash: the send itself is in progress, or a send answered
/// "nonce too low" (possibly a stale nonce cache with nothing in flight; the
/// provider is rebuilt, so a retry on a later pass may succeed).
const RESERVATION_HOLD: Duration = Duration::from_secs(90);

/// A top-up this faucet sent to an address. `hash` is `None` while the
/// transaction is being sent (the reservation that keeps two concurrent calls
/// from both funding).
#[derive(Clone, Copy)]
struct Sent {
    hash: Option<TxHash>,
    at: Instant,
    confirmed: bool,
    /// How long this record blocks another top-up to the same address.
    hold: Duration,
}

pub struct Faucet {
    root: RootProvider<BoxTransport>,
    wallet: EthereumWallet,
    faucet_address: Address,
    existential_deposit: U256,
    provider: RwLock<FaucetProvider>,
    /// Every top-up sent, confirmed or not, per address (amendment 0006: once
    /// `send_transaction` succeeds, the top-up is sent). In process only.
    sent: Mutex<HashMap<Address, Sent>>,
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
            sent: Mutex::new(HashMap::new()),
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

        // A top-up already sent to this address: wait for it, do not send
        // another because a balance read still looks low. Checked and reserved
        // in one step so two concurrent calls cannot both fund.
        let earlier = {
            let mut sent = self.sent.lock().unwrap();
            let earlier = sent.get(&to).copied();
            let blocking = earlier.is_some_and(|s| s.at.elapsed() < s.hold);
            if !blocking {
                sent.insert(
                    to,
                    Sent {
                        hash: None,
                        at: Instant::now(),
                        confirmed: false,
                        hold: RESERVATION_HOLD,
                    },
                );
            }
            blocking.then_some(earlier).flatten()
        };
        if let Some(s) = earlier {
            // Still unconfirmed: it may have landed since. It still counts as
            // recent, but is no longer in doubt.
            if let (false, Some(hash)) = (s.confirmed, s.hash) {
                if matches!(self.root.get_transaction_receipt(hash).await, Ok(Some(_))) {
                    if let Some(e) = self.sent.lock().unwrap().get_mut(&to) {
                        e.confirmed = true;
                        e.hold = CONFIRMED_HOLD;
                    }
                }
            }
            let what = s
                .hash
                .map(|h| h.to_string())
                .unwrap_or_else(|| "a top-up".into());
            return Err(anyhow::anyhow!(
                "faucet top-up pending: {what} was sent to {to_address} recently; not funding it again yet"
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
                    // After a failover, one endpoint may have taken the
                    // transaction and the next one says so ("already known",
                    // "nonce too low"). The top-up may be on its way: keep the
                    // reservation as unconfirmed rather than funding again.
                    let text = e.to_string().to_ascii_lowercase();
                    let maybe_sent =
                        text.contains("already known") || text.contains("nonce too low");
                    if maybe_sent {
                        warn!(
                            "Faucet send to {to_address} answered \"{e}\": an earlier attempt may have been accepted, holding the top-up as unconfirmed"
                        );
                        self.sent.lock().unwrap().insert(
                            to,
                            Sent {
                                hash: None,
                                at: Instant::now(),
                                confirmed: false,
                                // "already known": it is in a mempool, hold long.
                                // "nonce too low" may be only a stale nonce
                                // cache: hold briefly.
                                hold: if text.contains("already known") {
                                    UNCONFIRMED_HOLD
                                } else {
                                    RESERVATION_HOLD
                                },
                            },
                        );
                    } else {
                        error!("Faucet send failed, resetting nonce cache: {e}");
                        self.sent.lock().unwrap().remove(&to);
                    }
                    *self.provider.write().await = Self::build_provider(&self.root, &self.wallet);
                    if maybe_sent {
                        return Err(anyhow::anyhow!(
                            "faucet top-up pending: sending to {to_address} answered \"{e}\"; it may already be in flight"
                        ));
                    }
                    return Err(e.into());
                }
            };
            let sent_hash = *pending.tx_hash();
            self.sent.lock().unwrap().insert(
                to,
                Sent {
                    hash: Some(sent_hash),
                    at: Instant::now(),
                    confirmed: false,
                    hold: UNCONFIRMED_HOLD,
                },
            );
            // alloy 0.1 has no receipt timeout: bound the wait so one stuck top-up
            // cannot freeze the sweeper.
            let waited = tokio::time::timeout(Duration::from_secs(60), pending.get_receipt()).await;
            match waited {
                Ok(Ok(receipt)) => {
                    if let Some(e) = self.sent.lock().unwrap().get_mut(&to) {
                        e.confirmed = true;
                        e.hold = CONFIRMED_HOLD;
                    }
                    receipt
                }
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

    /// How long the faucet's record for `address` blocks another top-up.
    #[cfg(test)]
    pub(crate) fn hold_for(&self, address: &str) -> Option<Duration> {
        let a = Address::from_str(address).ok()?;
        self.sent.lock().unwrap().get(&a).map(|s| s.hold)
    }

    /// Check if an address already has sufficient balance (skip funding if it does)
    #[allow(dead_code)]
    pub async fn needs_funding(&self, address: &str) -> Result<bool> {
        let addr = Address::from_str(address)?;
        let balance = self.root.get_balance(addr).await?;
        Ok(balance < self.existential_deposit)
    }
}
