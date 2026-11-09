use crate::{
    config::{plan_sweep_parts, ChainConfig, SweepTarget, TargetKind},
    db::{
        Db, Erc20Deposit, Erc20Settlement, NewSweepIntent, OutboxEntry, SweepIntent, SweepRecord,
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
/// Most parts of a split sent in one pass; the balance is re-read on the next.
const MAX_PARTS_PER_PASS: usize = 10;
/// Passes a sent sweep may go without a receipt before it is sent to review.
const MAX_INTENT_WAIT_PASSES: u64 = 10;
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
    /// (and by sender when the token's target says `per_sender`, REQ-SW-12).
    /// Pending sweep intents are resolved first; a pair with an intent still
    /// waiting for its receipt is left alone this pass.
    async fn process_erc20_deposits(&self) -> Result<()> {
        self.resolve_all_pending().await?;
        let chain_name = self.chain.name.clone();
        let erc20_deposits = self
            .db
            .blocking(move |db| db.get_detected_erc20_deposits(&chain_name))
            .await?;

        // Deposits come back in insertion order on both backends, so the first
        // deposit of a group is stable: it keys the failure counters.
        let mut pairs: Vec<Vec<Erc20Deposit>> = Vec::new();
        let mut pair_index: std::collections::HashMap<(String, String), usize> =
            std::collections::HashMap::new();

        for deposit in erc20_deposits {
            if !self.chain.is_token_allowed(&deposit.token_address) {
                info!(
                    "[{}] Skipping non-allowlisted ERC20 deposit: key={}, token={}",
                    self.chain.name, deposit.key, deposit.token_address
                );
                self.fail_keys(std::slice::from_ref(&deposit.key)).await?;
                continue;
            }

            let target = self.chain.target_for_token(&deposit.token_address);
            if target.kind == TargetKind::Hold {
                // REQ-SW-6: detected and notified, never swept. A later change
                // of the target's kind releases it.
                info!(
                    "[{}] Holding ERC20 deposit {} (token {} target is hold)",
                    self.chain.name, deposit.key, deposit.token_address
                );
                continue;
            }

            let key = (
                deposit.account_id.clone(),
                deposit.token_address.to_lowercase(),
            );
            match pair_index.get(&key) {
                Some(&i) => pairs[i].push(deposit),
                None => {
                    pair_index.insert(key, pairs.len());
                    pairs.push(vec![deposit]);
                }
            }
        }

        for pair in pairs {
            if let Err(e) = self.process_pair(pair).await {
                warn!(
                    "[{}] ERC20 pair skipped this pass: {:?}",
                    self.chain.name, e
                );
            }
        }
        Ok(())
    }

    async fn process_pair(&self, pair: Vec<Erc20Deposit>) -> Result<()> {
        let first = &pair[0];
        let registration_id = first.account_id.clone();
        if let Some(blocker) = self
            .pair_blocker(&registration_id, &first.token_address)
            .await?
        {
            // Nothing is swept here while a deposit on the pair is unresolved.
            self.alert_once(&blocker, "pair_blocked").await?;
            return Ok(());
        }
        let target = self.chain.target_for_token(&first.token_address);
        // per_sender and per_transfer both sweep what a group is owed, not the
        // balance: a sender's deposits, or one deposit.
        let owed_mode = target.per_sender || target.per_transfer;

        let (derivation_index, address_str, _webhook_url) = {
            let reg_id = registration_id.clone();
            self.db
                .blocking(move |db| db.get_account_by_id(&reg_id))
                .await?
                .ok_or_else(|| anyhow::anyhow!("Account not found"))?
        };
        let signer = self.wallet.get_signer(derivation_index)?;
        let wallet = alloy::network::EthereumWallet::from(signer);
        let token_address = Address::from_str(&first.token_address)?;
        let from_address = Address::from_str(&address_str)?;

        let mut subgroups: Vec<Vec<Erc20Deposit>> = Vec::new();
        if owed_mode {
            let mut by_sender: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            let mut no_sender: Vec<String> = Vec::new();
            for deposit in pair {
                match &deposit.from_address {
                    // REQ-SW-12: a deposit with no recorded sender has no
                    // refund address, so it is never swept by this target.
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
                self.send_to_review(&no_sender, "no_sender", None).await?;
            }
        } else {
            subgroups.push(pair);
        }

        let live = subgroups;
        if live.is_empty() {
            return Ok(());
        }

        if owed_mode {
            // One sender's sweep must never spend another sender's funds: if the
            // address cannot cover what every sender is still owed, nothing is
            // swept and an operator looks.
            let balance = get_token_balance(&self.provider, token_address, from_address).await?;
            let mut owed_total = U256::ZERO;
            for subgroup in &live {
                owed_total += self.remaining_owed(subgroup).await?;
            }
            if balance < owed_total {
                let keys: Vec<String> = live.iter().flatten().map(|d| d.key.clone()).collect();
                self.send_to_review(&keys, "balance_shortfall", None)
                    .await?;
                return Ok(());
            }
        }

        for subgroup in live {
            if let Err(e) = self
                .sweep_subgroup(&wallet, &address_str, &target, &subgroup)
                .await
            {
                self.handle_group_failure(&subgroup, &e).await?;
            }
        }
        Ok(())
    }

    /// Moves deposits to `needs_review` and raises the alert operators watch for.
    async fn send_to_review(&self, keys: &[String], kind: &str, tx: Option<&str>) -> Result<()> {
        {
            let chain_name = self.chain.name.clone();
            let keys = keys.to_vec();
            let kind = kind.to_string();
            self.db
                .blocking(move |db| {
                    db.mark_erc20_deposits_needs_review_for_keys(&chain_name, &keys, &kind)
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

    /// Resolves every pending intent on the chain, whatever its deposits' status
    /// now is: a deposit can have gone `failed` or been dropped from the
    /// allowlist with a sweep still in flight, and that sweep must still be
    /// checked (and a retry of the deposit stays refused until it is).
    async fn resolve_all_pending(&self) -> Result<()> {
        let pending = {
            let chain_name = self.chain.name.clone();
            self.db
                .blocking(move |db| db.all_pending_sweep_intents(&chain_name))
                .await?
        };
        let mut by_pair: Vec<((String, String), Vec<SweepIntent>)> = Vec::new();
        for intent in pending {
            let key = (
                intent.account_id.clone(),
                intent.token_address.to_lowercase(),
            );
            match by_pair.iter_mut().find(|(k, _)| *k == key) {
                Some((_, list)) => list.push(intent),
                None => by_pair.push((key, vec![intent])),
            }
        }
        for ((account_id, token), intents) in by_pair {
            let reg = account_id.clone();
            let Some((_, address, _)) = self
                .db
                .blocking(move |db| db.get_account_by_id(&reg))
                .await?
            else {
                warn!(
                    "[{}] pending sweep for unknown account {account_id}",
                    self.chain.name
                );
                continue;
            };
            let result = self
                .resolve_intents(
                    intents,
                    Address::from_str(&token)?,
                    Address::from_str(&address)?,
                )
                .await;
            if let Err(e) = result {
                warn!(
                    "[{}] could not resolve sweeps for {account_id}: {e:?}",
                    self.chain.name
                );
            }
        }
        Ok(())
    }

    /// While any deposit on an (address, token) pair is in review, failed, or has
    /// a sweep in flight, nothing is swept on that pair: its funds may be
    /// unaccounted for, and a sweep would spend them. Returns the blocking
    /// deposit's key.
    async fn pair_blocker(&self, account_id: &str, token: &str) -> Result<Option<String>> {
        let pending = {
            let chain_name = self.chain.name.clone();
            self.db
                .blocking(move |db| db.all_pending_sweep_intents(&chain_name))
                .await?
        };
        if let Some(intent) = pending
            .iter()
            .find(|i| i.account_id == account_id && i.token_address.eq_ignore_ascii_case(token))
        {
            return Ok(intent.keys.first().cloned());
        }
        let chain_name = self.chain.name.clone();
        let (account, token) = (account_id.to_string(), token.to_string());
        let blocking = self
            .db
            .blocking(move |db| db.blocking_erc20_deposits(&chain_name, &account, &token))
            .await?;
        Ok(blocking.into_iter().next())
    }

    /// Logs an alert marker once per (deposit, kind); later passes only debug.
    async fn alert_once(&self, key: &str, kind: &str) -> Result<()> {
        let first = {
            let chain_name = self.chain.name.clone();
            let (key, kind) = (key.to_string(), kind.to_string());
            self.db
                .blocking(move |db| db.alert_once(&chain_name, &key, &kind))
                .await?
        };
        if first {
            error!(
                "EVMHOT_ALERT kind={kind} network={} deposit={}:{key} tx=-",
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

    /// Resolves the intents an earlier pass left pending, by their transaction
    /// hash's receipt (REQ-SW-5). No hash, or no receipt after
    /// `MAX_INTENT_WAIT_PASSES` passes, sends the deposits to review.
    async fn resolve_intents(
        &self,
        pending: Vec<SweepIntent>,
        token: Address,
        from: Address,
    ) -> Result<IntentState> {
        let mut waiting = false;
        for intent in pending {
            let Some(hash) = intent.tx_hash.clone() else {
                // Stopped between storing the intent and signing: nothing was
                // ever signed or sent, so the group simply re-plans.
                warn!(
                    "[{}] sweep intent {} was never signed; dropping it",
                    self.chain.name, intent.id
                );
                self.finish_intent(intent.id, "dropped").await?;
                continue;
            };
            let tx_hash = alloy::primitives::TxHash::from_str(&hash)?;
            let mut receipt = self.provider.get_transaction_receipt(tx_hash).await?;
            if receipt.is_none() {
                let id = intent.id;
                match (intent.nonce, intent.raw_tx.as_deref()) {
                    (Some(nonce), Some(raw)) => {
                        let latest = self.provider.get_transaction_count(from).await?;
                        if latest > nonce {
                            // It may have been mined between the two calls.
                            receipt = self.provider.get_transaction_receipt(tx_hash).await?;
                            if receipt.is_none() {
                                // The nonce was used by another transaction.
                                self.send_to_review(&intent.keys, "sweep_review", Some(&hash))
                                    .await?;
                                self.finish_intent(id, "review").await?;
                                return Ok(IntentState::Review);
                            }
                        } else {
                            let passes = self
                                .db
                                .blocking(move |db| db.bump_sweep_intent_passes(id))
                                .await?;
                            if passes >= MAX_INTENT_WAIT_PASSES {
                                // Still unmined and the nonce is free: a new
                                // sweep at this nonce replaces it, which is safe.
                                warn!(
                                    "[{}] sweep {hash} never landed after {passes} passes; dropping its intent",
                                    self.chain.name
                                );
                                self.finish_intent(id, "dropped").await?;
                                continue;
                            }
                            // Same bytes, same hash: rebroadcasting is idempotent.
                            match alloy::primitives::hex::decode(raw) {
                                Ok(bytes) => {
                                    if let Err(e) = self.provider.send_raw_transaction(&bytes).await
                                    {
                                        warn!(
                                            "[{}] rebroadcast of {hash} failed: {e:?}",
                                            self.chain.name
                                        );
                                    }
                                }
                                Err(e) => {
                                    warn!("[{}] stored raw tx unreadable: {e}", self.chain.name)
                                }
                            }
                            waiting = true;
                            continue;
                        }
                    }
                    _ => {
                        // Signed by an earlier version, without its raw bytes or nonce:
                        // it cannot be rebroadcast or judged, so wait, then review.
                        let passes = self
                            .db
                            .blocking(move |db| db.bump_sweep_intent_passes(id))
                            .await?;
                        if passes >= MAX_INTENT_WAIT_PASSES {
                            self.send_to_review(&intent.keys, "sweep_review", Some(&hash))
                                .await?;
                            self.finish_intent(id, "review").await?;
                            return Ok(IntentState::Review);
                        }
                        waiting = true;
                        continue;
                    }
                }
            }
            let Some(receipt) = receipt else { continue };
            let to = Address::from_str(&intent.target)?;
            let planned = U256::from_str(&intent.amount)?;
            match evaluate_receipt(&receipt, token, from, to, planned) {
                Verdict::Reverted => self.finish_intent(intent.id, "failed").await?,
                Verdict::Review(why) => {
                    warn!("[{}] sweep {hash} needs review: {why}", self.chain.name);
                    self.send_to_review(&intent.keys, "sweep_review", Some(&hash))
                        .await?;
                    self.finish_intent(intent.id, "review").await?;
                    return Ok(IntentState::Review);
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
                }
            }
        }
        Ok(if waiting {
            IntentState::Waiting
        } else {
            IntentState::Clear
        })
    }

    async fn finish_intent(&self, id: i64, state: &str) -> Result<()> {
        let state = state.to_string();
        self.db
            .blocking(move |db| db.finish_sweep_intent(id, &state))
            .await
    }

    /// Sweeps one group (one address, one token, and one sender when the target
    /// is `per_sender`). Parts are planned lazily, at most `MAX_PARTS_PER_PASS`
    /// per pass, and each is its own `transfer` call with its own coordinate
    /// (REQ-SW-9, REQ-SW-10). A group is settled only by its recorded sweeps,
    /// and only when nothing is left owed (REQ-WH-4).
    async fn sweep_subgroup(
        &self,
        wallet: &alloy::network::EthereumWallet,
        from_address_str: &str,
        target: &SweepTarget,
        group: &[Erc20Deposit],
    ) -> Result<()> {
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
        let recorded = self.recorded_sweeps(&keys).await?;
        let sweepable = if target.per_sender || target.per_transfer {
            // REQ-SW-12: one sender's share, not the whole balance.
            self.remaining_owed(group).await?.min(token_balance)
        } else {
            // REQ-SW-1: the whole balance.
            token_balance
        };

        if sweepable.is_zero() {
            if !recorded.is_empty() {
                // Nothing left owed and sweeps are on record: settle by them.
                return self.finish_group(from_address_str, group).await;
            }
            // REQ-SW-3 applies only to a group with no sweep on record and no
            // intent that could have moved funds (failed or dropped ones did not):
            // a review, pending or succeeded intent means an operator or a
            // receipt must say where the tokens went.
            let intent_states = {
                let chain_name = self.chain.name.clone();
                let keys = keys.clone();
                self.db
                    .blocking(move |db| db.sweep_intent_states_for_keys(&chain_name, &keys))
                    .await?
            };
            let could_have_moved = intent_states
                .iter()
                .any(|st| st != "failed" && st != "dropped");
            if token_balance.is_zero() && !could_have_moved {
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
                return Ok(());
            }
        }

        let parts = plan_sweep_parts(sweepable, target.min, target.max, MAX_PARTS_PER_PASS);
        let planned_total = parts.iter().fold(U256::ZERO, |a, p| a + *p);
        let group_key = format!(
            "{}|{}|{}",
            first.account_id,
            first.token_address.to_lowercase(),
            first.from_address.as_deref().unwrap_or("*").to_lowercase()
        );
        let first_part = recorded.len() as u64;
        let count = parts.len();
        for (i, amount) in parts.into_iter().enumerate() {
            let result = self
                .sweep_part(
                    wallet,
                    from_address_str,
                    token_address,
                    to_address,
                    amount,
                    &PartCtx {
                        account_id: &first.account_id,
                        token: &first.token_address,
                        keys: &keys,
                        group_key: &group_key,
                        part_index: first_part + i as u64,
                    },
                )
                .await?;
            let PartResult::Sent { intent_id, record } = result else {
                // Sent to review: the group is out of play.
                return Ok(());
            };
            // Last part of an amount that is now fully swept: settle in the same
            // transaction as the sweep record and the outbox (REQ-WH-7).
            let finishing = i + 1 == count && planned_total == sweepable;
            let settlements = if finishing {
                self.build_settlements(from_address_str, group, Some((&keys, &record)))
                    .await?
            } else {
                Vec::new()
            };
            let enqueued = {
                let chain_name = self.chain.name.clone();
                let keys = keys.clone();
                self.db
                    .blocking(move |db| {
                        db.settle_sweep_intent(&chain_name, intent_id, &keys, &record, &settlements)
                    })
                    .await?
            };
            if enqueued > 0 {
                self.deliverer.notify_worker();
            }
        }
        if planned_total < sweepable {
            let left = sweepable - planned_total;
            if target.min.is_some_and(|m| left < m) {
                self.alert_once(&first.key, "dust").await?;
            }
        }
        Ok(())
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

    /// One `transfer(target, amount)` call. The intent is stored before the
    /// transaction is signed, and its hash as soon as it is sent; the receipt is
    /// then checked (status, a matching `Transfer`, and its value).
    #[allow(clippy::too_many_arguments)]
    async fn sweep_part(
        &self,
        wallet: &alloy::network::EthereumWallet,
        from_address_str: &str,
        token_address: Address,
        to_address: Address,
        amount: U256,
        ctx: &PartCtx<'_>,
    ) -> Result<PartResult> {
        let keys = ctx.keys;
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

        // The intent exists before anything is signed. If the process stops
        // before the signed transaction is stored, nothing was sent and the next
        // pass drops the intent and re-plans.
        let intent_id = {
            let new = NewSweepIntent {
                chain: self.chain.name.clone(),
                account_id: ctx.account_id.to_string(),
                token_address: ctx.token.to_string(),
                group_key: ctx.group_key.to_string(),
                keys: ctx.keys.to_vec(),
                part_index: ctx.part_index,
                amount: amount.to_string(),
                target: to_address.to_string(),
            };
            self.db
                .blocking(move |db| db.create_sweep_intent(&new))
                .await?
        };

        // Sign locally with an explicit nonce, and store the nonce, hash and raw
        // bytes before broadcasting: whatever happens to the broadcast, the next
        // pass can rebroadcast the same transaction or see that its nonce was used.
        let nonce = provider
            .get_transaction_count(from_address)
            .block_id(alloy::eips::BlockNumberOrTag::Pending.into())
            .await?;
        let request = TransactionRequest::default()
            .with_from(from_address)
            .with_to(token_address)
            .with_input(call_data)
            .with_gas_limit(gas_limit_with_buffer)
            .with_nonce(nonce)
            .with_chain_id(self.chain.chain_id)
            .with_max_fee_per_gas(fee_estimate.max_fee_per_gas)
            .with_max_priority_fee_per_gas(fee_estimate.max_priority_fee_per_gas);
        let envelope = request
            .build(wallet)
            .await
            .map_err(|e| anyhow::anyhow!("failed to sign the sweep: {e}"))?;
        let tx_hash = envelope.tx_hash().to_string();
        let raw = envelope.encoded_2718();
        {
            let hash = tx_hash.clone();
            let raw_hex = alloy::primitives::hex::encode_prefixed(&raw);
            self.db
                .blocking(move |db| db.set_sweep_intent_signed(intent_id, nonce, &hash, &raw_hex))
                .await?;
        }

        // A broadcast error leaves the intent pending, with its hash and bytes;
        // the next pass resolves it.
        let pending_tx = provider.send_raw_transaction(&raw).await?;
        let receipt = pending_tx.get_receipt().await?;

        match evaluate_receipt(&receipt, token_address, from_address, to_address, amount) {
            // REQ-SW-5, INV-5: a reverted sweep is a failure, never a sweep.
            Verdict::Reverted => {
                self.finish_intent(intent_id, "failed").await?;
                Err(anyhow::anyhow!(
                    "sweep transaction reverted (receipt status 0): {tx_hash}"
                ))
            }
            Verdict::Review(why) => {
                warn!("[{}] sweep {tx_hash} needs review: {why}", self.chain.name);
                self.send_to_review(keys, "sweep_review", Some(&tx_hash))
                    .await?;
                self.finish_intent(intent_id, "review").await?;
                Ok(PartResult::Review)
            }
            Verdict::Success(record) => Ok(PartResult::Sent { intent_id, record }),
        }
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
}

struct PartCtx<'a> {
    account_id: &'a str,
    token: &'a str,
    keys: &'a [String],
    group_key: &'a str,
    part_index: u64,
}

enum IntentState {
    Clear,
    Waiting,
    Review,
}

enum PartResult {
    Sent { intent_id: i64, record: SweepRecord },
    Review,
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
