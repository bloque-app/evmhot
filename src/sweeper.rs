use crate::{
    config::{next_sweep_part, ChainConfig, SweepTarget, TargetKind},
    db::{
        Db, Erc20Deposit, Erc20Settlement, IntentTx, NativeSettlement, NewSweepIntent, OutboxEntry,
        SweepIntent, SweepRecord,
    },
    faucet::Faucet,
    wallet::Wallet,
    webhook::WebhookDeliverer,
};
use alloy::eips::eip2718::Encodable2718;
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
use tracing::{debug, error, info, warn};

pub struct Sweeper {
    chain: ChainConfig,
    deliverer: Arc<WebhookDeliverer>,
    db: Db,
    wallet: Wallet,
    provider: RootProvider<BoxTransport>,
    faucet: Arc<Faucet>,
    /// Test-only: stop the pass with an error at a chosen point, to model a
    /// process that dies mid-pass (the store is then picked up by a new sweeper).
    #[cfg(test)]
    crash_at: std::sync::Mutex<Option<CrashPoint>>,
}

/// Where a test may stop a pass, as if the process died there.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CrashPoint {
    /// After the intent row is written, before anything is signed.
    AfterIntentCreated,
    /// After the signed transaction is stored, before it is broadcast.
    AfterSigned,
}

/// How long to wait after a faucet top-up before reading the balance again.
/// A static so tests can set it to zero.
pub(crate) static FUNDING_SETTLE_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(2000);

fn funding_settle() -> Duration {
    Duration::from_millis(FUNDING_SETTLE_MS.load(std::sync::atomic::Ordering::Relaxed))
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
/// Waiting passes before an underpriced sweep is re-signed at its nonce.
const REPRICE_AFTER_PASSES: u64 = 10;
/// Passes in which a sweep's nonce is seen used, with no receipt of ours, before
/// the deposits go to review (receipt indexing can lag the nonce).
const GONE_PASSES_TO_REVIEW: u64 = 2;
/// An intent's `token_address` for the native coin.
const NATIVE_TOKEN: &str = "native";
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
            #[cfg(test)]
            crash_at: std::sync::Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_crash(&self, point: Option<CrashPoint>) {
        *self.crash_at.lock().unwrap() = point;
    }

    /// Fails once, at the armed point, in tests; free in production.
    #[cfg(test)]
    fn crash_if(&self, point: CrashPoint) -> Result<()> {
        let mut armed = self.crash_at.lock().unwrap();
        if *armed == Some(point) {
            *armed = None;
            return Err(anyhow::anyhow!("test crash at {point:?}"));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn process_deposits_once(&self) -> Result<()> {
        self.process_deposits().await
    }

    /// Test helper: several passes in a row (a sweep is sent in one pass and its
    /// receipt read in the next).
    #[cfg(test)]
    pub(crate) async fn process_deposits_n(&self, n: usize) -> Result<()> {
        for _ in 0..n {
            self.process_deposits().await?;
        }
        Ok(())
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
                        "[{}] Sweeper queue ({reason}): native detected={}, native failed={}, erc20 detected={}, erc20 failed={}, erc20 needs_review={}",
                        self.chain.name,
                        counts.native_detected,
                        counts.native_failed,
                        counts.erc20_detected,
                        counts.erc20_failed,
                        counts.erc20_needs_review
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
        let (erc20, native, open) = {
            let c = chain_name.clone();
            let erc20 = self
                .db
                .blocking(move |db| db.get_detected_erc20_deposits(&c))
                .await?;
            let c = chain_name.clone();
            let native = self
                .db
                .blocking(move |db| db.get_detected_deposits(&c))
                .await?;
            let c = chain_name.clone();
            let open = self
                .db
                .blocking(move |db| db.all_pending_sweep_intents(&c))
                .await?;
            (erc20, native, open)
        };
        // Everything is planned per deposit address: nonces are per address, so
        // is the rule of one open intent. Addresses with only an open intent (its
        // deposits failed, say) are included so that intent still gets resolved.
        let mut regs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        regs.extend(erc20.iter().map(|d| d.account_id.clone()));
        regs.extend(native.iter().map(|(_, reg, _)| reg.clone()));
        regs.extend(open.iter().map(|i| i.account_id.clone()));
        for reg in regs {
            if let Err(e) = self.process_address(&reg).await {
                #[cfg(test)]
                if e.to_string().contains("test crash") {
                    return Err(e);
                }
                warn!(
                    "[{}] address {reg} skipped this pass: {:?}",
                    self.chain.name, e
                );
            }
        }
        Ok(())
    }

    /// One address, one pass: resolve its open intent, and only if there is none
    /// left plan at most one new send (an ERC-20 part or the native coin).
    async fn process_address(&self, reg: &str) -> Result<()> {
        let (derivation_index, address_str, _webhook_url) = {
            let r = reg.to_string();
            self.db
                .blocking(move |db| db.get_account_by_id(&r))
                .await?
                .ok_or_else(|| anyhow::anyhow!("Account not found"))?
        };
        let address = Address::from_str(&address_str)?;

        // 1. An open intent owns the address (and its nonce) until resolved.
        let open = {
            let (c, r) = (self.chain.name.clone(), reg.to_string());
            self.db
                .blocking(move |db| db.open_sweep_intent(&c, &r))
                .await?
        };
        if let Some(intent) = open {
            if self.resolve_intent(&intent, &address_str, address).await? == Resolution::Open {
                return Ok(());
            }
        }

        // 2. Plan from what is detected now (resolution may have settled or
        // sent deposits to review).
        let chain_name = self.chain.name.clone();
        let erc20: Vec<Erc20Deposit> = {
            let c = chain_name.clone();
            self.db
                .blocking(move |db| db.get_detected_erc20_deposits(&c))
                .await?
                .into_iter()
                .filter(|d| d.account_id == reg)
                .collect()
        };
        let native: Vec<(String, String, String)> = {
            let c = chain_name.clone();
            self.db
                .blocking(move |db| db.get_detected_deposits(&c))
                .await?
                .into_iter()
                .filter(|(_, r, _)| r == reg)
                .collect()
        };

        let signer = self.wallet.get_signer(derivation_index)?;
        let wallet = alloy::network::EthereumWallet::from(signer);

        let mut pairs: Vec<Vec<Erc20Deposit>> = Vec::new();
        for deposit in erc20 {
            if !self.chain.is_token_allowed(&deposit.token_address) {
                info!(
                    "[{}] Skipping non-allowlisted ERC20 deposit: key={}, token={}",
                    self.chain.name, deposit.key, deposit.token_address
                );
                self.fail_keys(std::slice::from_ref(&deposit.key)).await?;
                continue;
            }
            if self.chain.target_for_token(&deposit.token_address).kind == TargetKind::Hold {
                // REQ-SW-6: detected and notified, never swept; a later change of
                // the target's kind releases it.
                info!(
                    "[{}] Holding ERC20 deposit {} (token {} target is hold)",
                    self.chain.name, deposit.key, deposit.token_address
                );
                continue;
            }
            let token = deposit.token_address.to_lowercase();
            match pairs
                .iter_mut()
                .find(|p| p[0].token_address.to_lowercase() == token)
            {
                Some(pair) => pair.push(deposit),
                None => pairs.push(vec![deposit]),
            }
        }

        // Tokens first, in the order their deposits arrived, then the native
        // coin. The first send ends the pass for this address: a long split of
        // one token holds back the address's other tokens and its native sweep.
        for pair in pairs {
            if self
                .plan_pair(reg, &address_str, address, &wallet, pair)
                .await?
                == Outcome::Sent
            {
                return Ok(());
            }
        }
        if !native.is_empty() {
            self.plan_native(reg, &address_str, address, &wallet, native)
                .await?;
        }
        Ok(())
    }

    /// Whether an intent is still open for the address (after an error mid-send).
    async fn address_has_open_intent(&self, reg: &str) -> Result<bool> {
        let (c, r) = (self.chain.name.clone(), reg.to_string());
        Ok(self
            .db
            .blocking(move |db| db.open_sweep_intent(&c, &r))
            .await?
            .is_some())
    }

    async fn plan_pair(
        &self,
        reg: &str,
        address_str: &str,
        address: Address,
        wallet: &alloy::network::EthereumWallet,
        pair: Vec<Erc20Deposit>,
    ) -> Result<Outcome> {
        let first = &pair[0];
        let target = self.chain.target_for_token(&first.token_address);
        // per_sender and per_transfer sweep what a group is owed, not the balance.
        let owed_mode = target.per_sender || target.per_transfer;
        let token_address = Address::from_str(&first.token_address)?;

        // Deposits of this address and token waiting for a person (review or
        // failed). Their tokens are still at the address.
        let blocked = {
            let (c, r, t) = (
                self.chain.name.clone(),
                reg.to_string(),
                first.token_address.clone(),
            );
            self.db
                .blocking(move |db| db.blocking_erc20_deposits(&c, &r, &t))
                .await?
        };
        if !owed_mode && !blocked.is_empty() {
            // A whole-balance sweep would spend their tokens: nothing is swept.
            self.alert_once(&blocked[0].key, "pair_blocked").await?;
            return Ok(Outcome::Idle);
        }

        let mut subgroups: Vec<Vec<Erc20Deposit>> = Vec::new();
        if owed_mode {
            let mut by_sender: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            let mut no_sender: Vec<String> = Vec::new();
            for deposit in pair {
                match &deposit.from_address {
                    // REQ-SW-12: no recorded sender, no refund address: never swept
                    // by this target. Its amount stays reserved.
                    None => no_sender.push(deposit.key.clone()),
                    Some(_) if target.per_transfer => subgroups.push(vec![deposit]),
                    Some(from) => {
                        let sender = from.to_lowercase();
                        match by_sender.get(&sender) {
                            Some(&i) => subgroups[i].push(deposit),
                            None => {
                                by_sender.insert(sender, subgroups.len());
                                subgroups.push(vec![deposit]);
                            }
                        }
                    }
                }
            }
            if !no_sender.is_empty() {
                self.send_to_review(&no_sender, "no_sender", None, false)
                    .await?;
            }
            if subgroups.is_empty() {
                return Ok(Outcome::Idle);
            }
            // The balance must cover everything owed on the pair: the live
            // deposits and the ones waiting for a person (reserved). Below that,
            // nothing is swept and nothing is capped: an operator looks.
            let balance = get_token_balance(&self.provider, token_address, address).await?;
            let mut owed_total = U256::ZERO;
            for subgroup in &subgroups {
                owed_total += self.remaining_owed(subgroup).await?;
            }
            owed_total += self.remaining_owed(&blocked).await?;
            if balance < owed_total {
                let keys: Vec<String> = subgroups.iter().flatten().map(|d| d.key.clone()).collect();
                self.send_to_review(&keys, "balance_shortfall", None, false)
                    .await?;
                return Ok(Outcome::Idle);
            }
        } else {
            subgroups.push(pair);
        }

        for subgroup in subgroups {
            match self
                .sweep_subgroup(reg, wallet, address_str, &target, &subgroup)
                .await
            {
                Ok(Outcome::Sent) => return Ok(Outcome::Sent),
                Ok(Outcome::Idle) => {}
                Err(e) => {
                    #[cfg(test)]
                    if e.to_string().contains("test crash") {
                        return Err(e);
                    }
                    self.handle_group_failure(&subgroup, &e).await?;
                    // An error after the intent was written leaves it open: the
                    // address is not planned again until it resolves.
                    if self.address_has_open_intent(reg).await? {
                        return Ok(Outcome::Sent);
                    }
                }
            }
        }
        Ok(Outcome::Idle)
    }

    /// The native coin: one sweep of the whole balance (REQ-SW-11), covering
    /// every detected native deposit of the address.
    async fn plan_native(
        &self,
        reg: &str,
        address_str: &str,
        address: Address,
        wallet: &alloy::network::EthereumWallet,
        deposits: Vec<(String, String, String)>,
    ) -> Result<Outcome> {
        let blocked = {
            let (c, r) = (self.chain.name.clone(), reg.to_string());
            self.db
                .blocking(move |db| db.blocking_native_deposits(&c, &r))
                .await?
        };
        if let Some(key) = blocked.first() {
            self.alert_once(key, "pair_blocked").await?;
            return Ok(Outcome::Idle);
        }
        let result = self
            .send_native(reg, address_str, address, wallet, &deposits)
            .await;
        match result {
            Ok(()) => Ok(Outcome::Sent),
            Err(e) => {
                #[cfg(test)]
                if e.to_string().contains("test crash") {
                    return Err(e);
                }
                let err_str = format!("{e:?}");
                if is_transient_funding_error(&err_str) {
                    warn!(
                        "[{}] Native deposits of {reg} waiting for faucet funding: {err_str}",
                        self.chain.name
                    );
                } else {
                    error!(
                        "[{}] Failed to sweep native deposits of {reg}: {err_str}",
                        self.chain.name
                    );
                }
                if self.address_has_open_intent(reg).await? {
                    return Ok(Outcome::Sent);
                }
                Ok(Outcome::Idle)
            }
        }
    }

    async fn send_native(
        &self,
        reg: &str,
        address_str: &str,
        address: Address,
        wallet: &alloy::network::EthereumWallet,
        deposits: &[(String, String, String)],
    ) -> Result<()> {
        let to_address = Address::from_str(&self.chain.treasury_address)?;
        let mut balance = self.provider.get_balance(address).await?;
        let gas_limit: u128 = 21000;
        let fees = self.provider.estimate_eip1559_fees(None).await?;
        let gas_cost = U256::from(gas_limit) * U256::from(fees.max_fee_per_gas);
        let gas_cost_with_buffer = gas_cost + (gas_cost / U256::from(10));

        if balance <= gas_cost_with_buffer {
            match self.faucet.fund_new_address(address_str).await {
                Ok(_) => {
                    sleep(funding_settle()).await;
                    balance = self.provider.get_balance(address).await?;
                    if balance <= gas_cost_with_buffer {
                        return Err(anyhow::anyhow!(
                            "Still insufficient balance after faucet funding"
                        ));
                    }
                }
                Err(e) => return Err(e),
            }
        }
        let value = balance - gas_cost_with_buffer;
        let keys: Vec<String> = deposits.iter().map(|(tx, _, _)| tx.clone()).collect();
        let intent_id = self
            .create_intent(
                reg,
                address_str,
                NATIVE_TOKEN,
                &keys,
                0,
                &value.to_string(),
                &to_address.to_string(),
            )
            .await?;
        #[cfg(test)]
        self.crash_if(CrashPoint::AfterIntentCreated)?;
        let nonce = self.next_nonce(reg, address).await?;
        let request = TransactionRequest::default()
            .with_from(address)
            .with_to(to_address)
            .with_value(value)
            .with_gas_limit(gas_limit)
            .with_nonce(nonce)
            .with_chain_id(self.chain.chain_id)
            .with_max_fee_per_gas(fees.max_fee_per_gas)
            .with_max_priority_fee_per_gas(fees.max_priority_fee_per_gas);
        self.sign_store_broadcast(intent_id, nonce, request, wallet)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_intent(
        &self,
        reg: &str,
        address_str: &str,
        token: &str,
        keys: &[String],
        part_index: u64,
        amount: &str,
        target: &str,
    ) -> Result<i64> {
        let new = NewSweepIntent {
            chain: self.chain.name.clone(),
            account_id: reg.to_string(),
            address: address_str.to_lowercase(),
            token_address: token.to_string(),
            group_key: format!("{reg}|{}", token.to_lowercase()),
            keys: keys.to_vec(),
            part_index,
            amount: amount.to_string(),
            target: target.to_string(),
        };
        self.db
            .blocking(move |db| db.create_sweep_intent(&new))
            .await
    }

    /// The next nonce for an address: its pending count, but never at or below
    /// the nonce of an intent that already landed (a public RPC's pending count
    /// can be stale).
    async fn next_nonce(&self, reg: &str, address: Address) -> Result<u64> {
        let pending = self
            .provider
            .get_transaction_count(address)
            .block_id(alloy::eips::BlockNumberOrTag::Pending.into())
            .await?;
        let highest = {
            let (c, r) = (self.chain.name.clone(), reg.to_string());
            self.db
                .blocking(move |db| db.highest_landed_intent_nonce(&c, &r))
                .await?
        };
        Ok(highest.map_or(pending, |h| pending.max(h + 1)))
    }

    /// Signs locally, stores the signed transaction on the intent, then
    /// broadcasts it. A broadcast error is not an error here: the intent is open
    /// with its hash and bytes, and the next pass rebroadcasts or resolves it. No
    /// receipt is waited for.
    async fn sign_store_broadcast(
        &self,
        intent_id: i64,
        nonce: u64,
        request: TransactionRequest,
        wallet: &alloy::network::EthereumWallet,
    ) -> Result<()> {
        let max_fee = request.max_fee_per_gas.unwrap_or_default();
        let max_priority = request.max_priority_fee_per_gas.unwrap_or_default();
        let envelope = request
            .build(wallet)
            .await
            .map_err(|e| anyhow::anyhow!("failed to sign the sweep: {e}"))?;
        let tx = IntentTx {
            hash: envelope.tx_hash().to_string(),
            raw: alloy::primitives::hex::encode_prefixed(envelope.encoded_2718()),
            max_fee: max_fee.to_string(),
            max_priority: max_priority.to_string(),
        };
        {
            let tx = tx.clone();
            self.db
                .blocking(move |db| db.set_sweep_intent_signed(intent_id, nonce, &tx))
                .await?;
        }
        #[cfg(test)]
        self.crash_if(CrashPoint::AfterSigned)?;
        self.broadcast(&tx).await;
        Ok(())
    }

    /// Broadcasts; returns the node's error, if any. "already known" and a lost
    /// answer both land here: the intent resolves by receipt either way.
    async fn broadcast(&self, tx: &IntentTx) -> Option<String> {
        match alloy::primitives::hex::decode(&tx.raw) {
            Ok(bytes) => match self.provider.send_raw_transaction(&bytes).await {
                Ok(_) => None,
                Err(e) => {
                    warn!(
                        "[{}] broadcast of {} failed: {e:?}",
                        self.chain.name, tx.hash
                    );
                    Some(e.to_string())
                }
            },
            Err(e) => {
                warn!("[{}] stored raw tx unreadable: {e}", self.chain.name);
                Some(e.to_string())
            }
        }
    }

    /// Sends the same signed bytes again. A node that refuses them for lack of gas
    /// (the address paid for earlier transactions since) gets a faucet top-up and
    /// one more try, for a token sweep.
    async fn rebroadcast(
        &self,
        intent: &SweepIntent,
        tx: &IntentTx,
        address_str: &str,
        address: Address,
    ) -> Result<()> {
        let Some(err) = self.broadcast(tx).await else {
            return Ok(());
        };
        if err.to_lowercase().contains("insufficient funds") && intent.token_address != NATIVE_TOKEN
        {
            use alloy::consensus::TxEnvelope;
            use alloy::eips::eip2718::Decodable2718;
            let bytes = alloy::primitives::hex::decode(&tx.raw)?;
            if let TxEnvelope::Eip1559(signed) = TxEnvelope::decode_2718(&mut &bytes[..])? {
                let t = signed.tx();
                self.ensure_gas(address_str, address, t.gas_limit, t.max_fee_per_gas)
                    .await?;
                self.broadcast(tx).await;
            }
        }
        Ok(())
    }

    /// Resolves an open intent. It keeps its nonce for good: it is never
    /// dropped once signed, only settled, failed, or sent to review.
    async fn resolve_intent(
        &self,
        intent: &SweepIntent,
        address_str: &str,
        address: Address,
    ) -> Result<Resolution> {
        if intent.txs.is_empty() {
            // Stopped between writing the intent and signing: nothing exists on
            // any network, so there is nothing to wait for.
            warn!(
                "[{}] sweep intent {} was never signed; dropping it",
                self.chain.name, intent.id
            );
            self.finish_intent(intent.id, "dropped").await?;
            return Ok(Resolution::Done);
        }
        if let Some((tx, receipt)) = self.find_receipt(intent).await? {
            self.apply_receipt(intent, &tx, &receipt, address_str, address)
                .await?;
            return Ok(Resolution::Done);
        }
        let nonce = intent
            .nonce
            .ok_or_else(|| anyhow::anyhow!("signed intent {} has no nonce", intent.id))?;
        let newest = intent.txs.last().expect("checked non-empty").clone();
        let latest = self.provider.get_transaction_count(address).await?;
        let id = intent.id;
        if latest > nonce {
            // The nonce is used and none of our hashes has a receipt. Receipt
            // indexing can lag the nonce: look again, and only after a second
            // pass conclude that something outside evmhot used this key.
            if let Some((tx, receipt)) = self.find_receipt(intent).await? {
                self.apply_receipt(intent, &tx, &receipt, address_str, address)
                    .await?;
                return Ok(Resolution::Done);
            }
            let gone = self
                .db
                .blocking(move |db| db.bump_sweep_intent_gone_passes(id))
                .await?;
            if gone >= GONE_PASSES_TO_REVIEW {
                self.send_to_review(
                    &intent.keys,
                    "sweep_review",
                    Some(&newest.hash),
                    intent.token_address == NATIVE_TOKEN,
                )
                .await?;
                self.finish_intent(id, "review").await?;
                return Ok(Resolution::Done);
            }
            return Ok(Resolution::Open);
        }

        // Not mined, nonce free. Reprice only a transaction the network will not
        // take at its price, after it has waited; otherwise send the same bytes
        // again (an evicted transaction comes back, a known one is a no-op).
        let passes = self
            .db
            .blocking(move |db| db.bump_sweep_intent_passes(id))
            .await?;
        let estimate = self.provider.estimate_eip1559_fees(None).await?;
        let held_fee: u128 = newest.max_fee.parse().unwrap_or(0);
        if passes >= REPRICE_AFTER_PASSES && held_fee < estimate.max_fee_per_gas {
            self.reprice(intent, &newest, &estimate, address_str, address)
                .await?;
        } else {
            self.rebroadcast(intent, &newest, address_str, address)
                .await?;
        }
        Ok(Resolution::Open)
    }

    /// Re-signs the intent's transaction at the same nonce with both fee fields
    /// raised by at least 10% (or to the current estimate, whichever is higher),
    /// stores it next to the earlier ones, and broadcasts it. Both can land, but
    /// only one can: they share the nonce.
    async fn reprice(
        &self,
        intent: &SweepIntent,
        newest: &IntentTx,
        estimate: &alloy::providers::utils::Eip1559Estimation,
        address_str: &str,
        address: Address,
    ) -> Result<()> {
        use alloy::consensus::TxEnvelope;
        use alloy::eips::eip2718::Decodable2718;
        let bytes = alloy::primitives::hex::decode(&newest.raw)?;
        let TxEnvelope::Eip1559(signed) = TxEnvelope::decode_2718(&mut &bytes[..])? else {
            return Err(anyhow::anyhow!(
                "stored sweep is not an EIP-1559 transaction"
            ));
        };
        let old = signed.tx().clone();
        let bump = |old: u128| (old * 11).div_ceil(10);
        let max_fee = bump(old.max_fee_per_gas).max(estimate.max_fee_per_gas);
        let priority = bump(old.max_priority_fee_per_gas)
            .max(estimate.max_priority_fee_per_gas)
            .min(max_fee);
        let signer = {
            let r = intent.account_id.clone();
            let (index, _, _) = self
                .db
                .blocking(move |db| db.get_account_by_id(&r))
                .await?
                .ok_or_else(|| anyhow::anyhow!("Account not found"))?;
            self.wallet.get_signer(index)?
        };
        let wallet = alloy::network::EthereumWallet::from(signer);
        let mut value = old.value;
        if intent.token_address == NATIVE_TOKEN {
            // The dearer fee must still fit in the balance the value was cut from.
            let extra = U256::from(old.gas_limit) * U256::from(max_fee - old.max_fee_per_gas);
            value = value.saturating_sub(extra);
        } else {
            // A higher fee needs more gas in the address.
            self.ensure_gas(address_str, address, old.gas_limit, max_fee)
                .await?;
        }
        let alloy::primitives::TxKind::Call(to) = old.to else {
            return Err(anyhow::anyhow!("stored sweep has no recipient"));
        };
        let request = TransactionRequest::default()
            .with_from(address)
            .with_to(to)
            .with_value(value)
            .with_input(old.input.clone())
            .with_gas_limit(old.gas_limit)
            .with_nonce(old.nonce)
            .with_chain_id(self.chain.chain_id)
            .with_max_fee_per_gas(max_fee)
            .with_max_priority_fee_per_gas(priority);
        info!(
            "[{}] repricing sweep intent {} at nonce {} (max fee {} -> {})",
            self.chain.name, intent.id, old.nonce, old.max_fee_per_gas, max_fee
        );
        self.sign_store_broadcast(intent.id, old.nonce, request, &wallet)
            .await
    }

    /// The first stored transaction (newest first) that has a receipt. At most
    /// one of an intent's transactions can land: they share a nonce.
    async fn find_receipt(
        &self,
        intent: &SweepIntent,
    ) -> Result<Option<(IntentTx, TransactionReceipt)>> {
        for tx in intent.txs.iter().rev() {
            let hash = alloy::primitives::TxHash::from_str(&tx.hash)?;
            if let Some(receipt) = self.provider.get_transaction_receipt(hash).await? {
                return Ok(Some((tx.clone(), receipt)));
            }
        }
        Ok(None)
    }

    async fn apply_receipt(
        &self,
        intent: &SweepIntent,
        tx: &IntentTx,
        receipt: &TransactionReceipt,
        address_str: &str,
        address: Address,
    ) -> Result<()> {
        if intent.token_address == NATIVE_TOKEN {
            if !receipt.inner.status() {
                // Reverted: the deposits stay detected and are planned again.
                warn!(
                    "[{}] native sweep {} reverted; planning again",
                    self.chain.name, tx.hash
                );
                return self.finish_intent(intent.id, "failed").await;
            }
            return self.settle_native(intent, address_str).await;
        }
        let token = Address::from_str(&intent.token_address)?;
        let to = Address::from_str(&intent.target)?;
        let planned = U256::from_str(&intent.amount)?;
        match evaluate_receipt(receipt, token, address, to, planned) {
            Verdict::Reverted => {
                self.finish_intent(intent.id, "failed").await?;
                self.record_failure(
                    &intent.keys,
                    &format!("sweep transaction reverted (receipt status 0): {}", tx.hash),
                )
                .await
            }
            Verdict::Review(why) => {
                warn!(
                    "[{}] sweep {} needs review: {why}",
                    self.chain.name, tx.hash
                );
                self.send_to_review(&intent.keys, "sweep_review", Some(&tx.hash), false)
                    .await?;
                self.finish_intent(intent.id, "review").await
            }
            Verdict::Success(record) => {
                let chain_name = self.chain.name.clone();
                let keys = intent.keys.clone();
                let id = intent.id;
                self.db
                    .blocking(move |db| {
                        db.settle_sweep_intent(&chain_name, id, &keys, &record, &[])
                    })
                    .await?;
                Ok(())
            }
        }
    }

    /// A native sweep landed: its deposits are swept, with their webhooks, in
    /// one transaction.
    async fn settle_native(&self, intent: &SweepIntent, address_str: &str) -> Result<()> {
        let detected = {
            let c = self.chain.name.clone();
            self.db
                .blocking(move |db| db.get_detected_deposits(&c))
                .await?
        };
        let webhook_url = {
            let r = intent.account_id.clone();
            self.db.blocking(move |db| db.get_webhook_url(&r)).await?
        };
        let mut settlements = Vec::new();
        for key in &intent.keys {
            let amount = detected
                .iter()
                .find(|(tx, _, _)| tx == key)
                .map(|(_, _, a)| a.clone())
                .unwrap_or_default();
            let from = {
                let (c, k) = (self.chain.name.clone(), key.clone());
                self.db
                    .blocking(move |db| db.get_deposit_from_address(&c, &k))
                    .await?
            };
            let id = format!("{}:{}", self.chain.name, key);
            let outbox = webhook_url.as_ref().map(|url| OutboxEntry {
                id: id.clone(),
                event: "deposit_swept".to_string(),
                registration_id: intent.account_id.clone(),
                webhook_url: url.clone(),
                payload: serde_json::json!({
                    "id": id,
                    "chain": self.chain.name,
                    "chain_id": self.chain.chain_id,
                    "event": "deposit_swept",
                    "account_id": address_str,
                    "registration_id": intent.account_id,
                    "original_tx_hash": key,
                    "from_address": from,
                    "amount": amount,
                    "token_type": "native"
                })
                .to_string(),
            });
            settlements.push(NativeSettlement {
                tx_hash: key.clone(),
                outbox,
            });
        }
        let (c, id) = (self.chain.name.clone(), intent.id);
        let enqueued = self
            .db
            .blocking(move |db| db.settle_native_intent(&c, id, &settlements))
            .await?;
        if enqueued > 0 {
            self.deliverer.notify_worker();
        }
        Ok(())
    }

    /// Native gas for a token transfer: top the address up from the faucet when
    /// it cannot pay `gas_limit` at `max_fee` (with the usual buffers).
    async fn ensure_gas(
        &self,
        address_str: &str,
        address: Address,
        gas_limit: u128,
        max_fee: u128,
    ) -> Result<()> {
        let cost = U256::from(gas_limit) * U256::from(max_fee);
        let needed = cost + (cost / U256::from(10));
        let mut balance = self.provider.get_balance(address).await?;
        if balance < needed {
            match self.faucet.fund_new_address(address_str).await {
                Ok(_) => {
                    sleep(funding_settle()).await;
                    balance = self.provider.get_balance(address).await?;
                    if balance < needed {
                        return Err(anyhow::anyhow!(
                            "Insufficient native balance for gas even after faucet funding"
                        ));
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Sweeps one group (one address, one token, and one sender or one deposit
    /// when the target says so): sends at most the next part (REQ-SW-9,
    /// REQ-SW-10). A group is settled only by its recorded sweeps, and only when
    /// nothing is left owed (REQ-WH-4).
    async fn sweep_subgroup(
        &self,
        reg: &str,
        wallet: &alloy::network::EthereumWallet,
        from_address_str: &str,
        target: &SweepTarget,
        group: &[Erc20Deposit],
    ) -> Result<Outcome> {
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
        let owed_mode = target.per_sender || target.per_transfer;

        let token_balance = get_token_balance(&self.provider, token_address, from_address).await?;
        let recorded = self.recorded_sweeps(&keys).await?;
        let sweepable = if owed_mode {
            // What this group is still owed. The pair's balance covers it: the
            // shortfall check ran before.
            self.remaining_owed(group).await?
        } else {
            // REQ-SW-1: the whole balance.
            token_balance
        };

        if sweepable.is_zero() {
            if !recorded.is_empty() {
                // Nothing left owed and sweeps are on record: settle by them.
                self.finish_group(from_address_str, group).await?;
                return Ok(Outcome::Idle);
            }
            // REQ-SW-3 applies only to a group with no sweep on record and no
            // intent that could have moved funds. Failed and dropped intents did
            // not; a closed-by-operator one is the operator's word that it did
            // not matter.
            let history = {
                let chain_name = self.chain.name.clone();
                let keys = keys.clone();
                self.db
                    .blocking(move |db| db.sweep_intent_history_for_keys(&chain_name, &keys))
                    .await?
            };
            let could_have_moved = history
                .iter()
                .any(|(st, _)| st != "failed" && st != "dropped" && st != "closed_by_operator");
            if token_balance.is_zero() && !owed_mode && !could_have_moved {
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
                    // Settled with no sweep on record. With no intent history that
                    // is the plain REQ-SW-3 case; with any (a closed-by-operator
                    // review, say) the funds may really have moved, and nobody
                    // is told by a `deposit_swept`: so say it, once.
                    if !history.is_empty() {
                        let hashes: Vec<&str> =
                            history.iter().filter_map(|(_, h)| h.as_deref()).collect();
                        let tx = if hashes.is_empty() {
                            "-".to_string()
                        } else {
                            hashes.join(",")
                        };
                        for key in group.iter().map(|d| &d.key) {
                            self.alert_once_with_tx(key, "settled_without_sweep", &tx)
                                .await?;
                        }
                    }
                }
            }
            return Ok(Outcome::Idle);
        }

        let Some(amount) = next_sweep_part(sweepable, target.min, target.max, target.per_transfer)
        else {
            // REQ-SW-7: below the minimum it waits for later deposits.
            let min = target.min.unwrap_or_default();
            if recorded.is_empty() {
                info!(
                    "[{}] ERC20 deposit {} waiting: {} is below the target minimum {}",
                    self.chain.name, first.key, sweepable, min
                );
            } else {
                // Dust after a split: it stays owed, the deposits are not
                // settled, and a later deposit lifts it above the minimum.
                self.alert_once(&first.key, "dust").await?;
            }
            return Ok(Outcome::Idle);
        };

        self.send_erc20_part(
            reg,
            wallet,
            from_address_str,
            token_address,
            to_address,
            amount,
            &PartCtx {
                token: &first.token_address,
                keys: &keys,
                part_index: recorded.len() as u64,
            },
        )
        .await?;
        Ok(Outcome::Sent)
    }

    /// One `transfer(target, amount)` call: intent first, then signed with an
    /// explicit nonce, stored, broadcast. The receipt is read on a later pass.
    #[allow(clippy::too_many_arguments)]
    async fn send_erc20_part(
        &self,
        reg: &str,
        wallet: &alloy::network::EthereumWallet,
        from_address_str: &str,
        token_address: Address,
        to_address: Address,
        amount: U256,
        ctx: &PartCtx<'_>,
    ) -> Result<()> {
        let provider = &self.provider;
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
        let fees = provider.estimate_eip1559_fees(None).await?;
        self.ensure_gas(
            from_address_str,
            from_address,
            gas_limit_with_buffer,
            fees.max_fee_per_gas,
        )
        .await?;

        let intent_id = self
            .create_intent(
                reg,
                from_address_str,
                ctx.token,
                ctx.keys,
                ctx.part_index,
                &amount.to_string(),
                &to_address.to_string(),
            )
            .await?;
        #[cfg(test)]
        self.crash_if(CrashPoint::AfterIntentCreated)?;
        let nonce = self.next_nonce(reg, from_address).await?;
        let request = TransactionRequest::default()
            .with_from(from_address)
            .with_to(token_address)
            .with_input(call_data)
            .with_gas_limit(gas_limit_with_buffer)
            .with_nonce(nonce)
            .with_chain_id(self.chain.chain_id)
            .with_max_fee_per_gas(fees.max_fee_per_gas)
            .with_max_priority_fee_per_gas(fees.max_priority_fee_per_gas);
        self.sign_store_broadcast(intent_id, nonce, request, wallet)
            .await
    }

    /// REQ-SW-4: funding errors wait; a decoding error fails the group at once;
    /// any other failure (including a reverted sweep, REQ-SW-5) counts, and the
    /// fifth consecutive one fails the group.
    async fn handle_group_failure(&self, group: &[Erc20Deposit], e: &anyhow::Error) -> Result<()> {
        let keys: Vec<String> = group.iter().map(|d| d.key.clone()).collect();
        self.record_failure(&keys, &format!("{e:?}")).await
    }

    async fn record_failure(&self, keys: &[String], err_str: &str) -> Result<()> {
        let first_key = keys[0].clone();
        if is_transient_funding_error(err_str) {
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
        if is_permanent_sweep_error(err_str) {
            self.fail_keys(keys).await?;
            for key in keys {
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
                self.fail_keys(keys).await?;
                for key in keys {
                    warn!(
                        "[{}] Permanently failed ERC20 deposit {} after {} attempts: {}",
                        self.chain.name, key, failures, err_str
                    );
                }
            }
        }
        Ok(())
    }

    /// Moves deposits to `needs_review` and raises the alert operators watch for.
    async fn send_to_review(
        &self,
        keys: &[String],
        kind: &str,
        tx: Option<&str>,
        native: bool,
    ) -> Result<()> {
        {
            let chain_name = self.chain.name.clone();
            let keys = keys.to_vec();
            let kind = kind.to_string();
            self.db
                .blocking(move |db| {
                    if native {
                        db.mark_native_deposits_needs_review_for_keys(&chain_name, &keys, &kind)
                    } else {
                        db.mark_erc20_deposits_needs_review_for_keys(&chain_name, &keys, &kind)
                    }
                })
                .await?;
        }
        for key in keys {
            error!(
                "EVMHOT_ALERT kind={kind} network={} deposit={}:{key} tx={}",
                self.chain.name,
                self.chain.name,
                tx.unwrap_or("-")
            );
        }
        Ok(())
    }

    /// Logs an alert marker once per (deposit, kind); later passes only debug.
    async fn alert_once(&self, key: &str, kind: &str) -> Result<()> {
        self.alert_once_with_tx(key, kind, "-").await
    }

    async fn alert_once_with_tx(&self, key: &str, kind: &str, tx: &str) -> Result<()> {
        let first = {
            let chain_name = self.chain.name.clone();
            let (key, kind) = (key.to_string(), kind.to_string());
            self.db
                .blocking(move |db| db.alert_once(&chain_name, &key, &kind))
                .await?
        };
        if first {
            error!(
                "EVMHOT_ALERT kind={kind} network={} deposit={}:{key} tx={tx}",
                self.chain.name, self.chain.name
            );
        } else {
            debug!("[{}] {kind} still pending for {key}", self.chain.name);
        }
        Ok(())
    }

    /// Keys with no sweep in flight (those must not be failed out from under it).
    async fn keys_without_pending(&self, keys: &[String]) -> Result<Vec<String>> {
        let chain_name = self.chain.name.clone();
        let pending = self
            .db
            .blocking(move |db| db.all_pending_sweep_intents(&chain_name))
            .await?;
        Ok(keys
            .iter()
            .filter(|k| !pending.iter().any(|i| i.keys.contains(k)))
            .cloned()
            .collect())
    }

    async fn fail_keys(&self, keys: &[String]) -> Result<()> {
        // A deposit with a sweep in flight is not failed out from under it.
        let keys = self.keys_without_pending(keys).await?;
        if keys.is_empty() {
            return Ok(());
        }
        let chain_name = self.chain.name.clone();
        self.db
            .blocking(move |db| db.mark_erc20_deposits_failed_for_keys(&chain_name, &keys))
            .await
    }

    /// What a group still owes: its deposits' amounts minus the sweeps already
    /// recorded for them (`per_sender` only; whole-balance sweeps use the balance).
    async fn remaining_owed(&self, group: &[Erc20Deposit]) -> Result<U256> {
        let keys: Vec<String> = group.iter().map(|d| d.key.clone()).collect();
        let mut owed = U256::ZERO;
        for deposit in group {
            owed += U256::from_str(&deposit.amount)?;
        }
        let mut swept = U256::ZERO;
        for record in self.recorded_sweeps(&keys).await? {
            swept += U256::from_str(&record.sweep_amount)?;
        }
        Ok(owed.saturating_sub(swept))
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

    /// Settlements for the group's deposits that have a recorded sweep, with
    /// `extra` (the part just sent, not yet stored) counted for its keys. A
    /// deposit that joined after the sweeps went out has none and stays detected.
    async fn build_settlements(
        &self,
        from_address_str: &str,
        group: &[Erc20Deposit],
        extra: Option<(&[String], &SweepRecord)>,
    ) -> Result<Vec<Erc20Settlement>> {
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

        let mut settlements = Vec::with_capacity(group.len());
        for deposit in group {
            let mut sweeps = {
                let chain_name = self.chain.name.clone();
                let key = deposit.key.clone();
                self.db
                    .blocking(move |db| db.get_sweeps(&chain_name, &key))
                    .await?
            };
            if let Some((keys, record)) = extra {
                let known = sweeps.iter().any(|s| {
                    s.sweep_tx_hash == record.sweep_tx_hash
                        && s.sweep_log_index == record.sweep_log_index
                });
                if keys.contains(&deposit.key) && !known {
                    sweeps.push(record.clone());
                }
            }
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
        Ok(settlements)
    }

    /// Settles a group from its recorded sweeps alone (nothing left owed).
    async fn finish_group(&self, from_address_str: &str, group: &[Erc20Deposit]) -> Result<()> {
        let settlements = self
            .build_settlements(from_address_str, group, None)
            .await?;
        if settlements.is_empty() {
            return Err(anyhow::anyhow!(
                "no sweep recorded for deposit {}",
                group[0].key
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
    pub(crate) fn erc20_swept_payload(
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
                    "sweep_target": s.sweep_target,
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

    async fn finish_intent(&self, id: i64, state: &str) -> Result<()> {
        let state = state.to_string();
        self.db
            .blocking(move |db| db.finish_sweep_intent(id, &state))
            .await
    }
}

/// An address's pass: whether a transaction was sent for it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Sent,
    Idle,
}

/// Whether an intent is still waiting.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Resolution {
    Open,
    Done,
}

struct PartCtx<'a> {
    token: &'a str,
    keys: &'a [String],
    part_index: u64,
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

enum Verdict {
    Reverted,
    /// Success receipt that does not show the planned transfer.
    Review(String),
    Success(SweepRecord),
}

/// Reads a sweep receipt (REQ-SW-5, REQ-SW-10). A revert is a failure. On
/// success the first `Transfer` of the token from the deposit address to the
/// target is the sweep's coordinate (its block-level log index, from the
/// receipt); if there is none, or its value is not the planned amount, the
/// sweep needs an operator and is never reported.
fn evaluate_receipt(
    receipt: &TransactionReceipt,
    token: Address,
    from: Address,
    to: Address,
    planned: U256,
) -> Verdict {
    if !receipt.inner.status() {
        return Verdict::Reverted;
    }
    let signature = keccak256("Transfer(address,address,uint256)".as_bytes());
    let topic_address = |topic: &alloy::primitives::B256| Address::from_slice(&topic[12..]);
    let Some(log) = receipt.inner.logs().iter().find(|log| {
        let topics = log.topics();
        log.address() == token
            && topics.len() == 3
            && topics[0] == signature
            && topic_address(&topics[1]) == from
            && topic_address(&topics[2]) == to
    }) else {
        return Verdict::Review("no Transfer from the deposit address to the target".into());
    };
    let data = &log.data().data;
    let value = if data.len() >= 32 {
        U256::from_be_slice(&data[..32])
    } else {
        U256::from_be_slice(data)
    };
    if value != planned {
        return Verdict::Review(format!(
            "Transfer value {value} differs from planned {planned}"
        ));
    }
    let (Some(log_index), Some(block_number), Some(block_hash)) =
        (log.log_index, receipt.block_number, receipt.block_hash)
    else {
        return Verdict::Review("receipt lacks a log index, block number or block hash".into());
    };
    Verdict::Success(SweepRecord {
        sweep_tx_hash: receipt.transaction_hash.to_string(),
        sweep_block_number: block_number,
        sweep_block_hash: block_hash.to_string(),
        sweep_log_index: log_index,
        sweep_amount: value.to_string(),
        sweep_target: Some(to.to_string()),
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
