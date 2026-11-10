//! Offline gap scan: finds token deposits evmhot missed while it was down, so
//! they can be stored before the cursor jumps to the present (ruling R4).
//!
//! It is balance-first, to stay cheap on public RPCs (ruling R16):
//!
//! 1. every registered address is read for every admitted token at one fixed
//!    block `B`, through Multicall3 `aggregate3` (a few hundred addresses per
//!    call);
//! 2. only addresses with a non-zero balance get their inbound `Transfer` logs
//!    fetched, from the cursor to `B`, in ranges that shrink when the provider
//!    refuses them;
//! 3. logs are judged by the same rules the monitor uses (admitted token,
//!    sender is not the faucet, amount meets the detection minimum);
//! 4. with `apply`, each deposit goes through the monitor's own
//!    `handle_erc20_log`, so identity (chain, tx hash, block-level log index),
//!    INV-2 (at most once per identity) and the `deposit_detected` outbox row
//!    are exactly what live detection would have produced.
//!
//! Nothing is written unless `apply` is set. The cursor moves only with
//! `set_cursor`, after the deposits are stored.

use crate::config::{ChainConfig, Config};
use crate::db::Db;
use crate::monitor::{transfer_amount, Monitor};
use crate::webhook::WebhookDeliverer;
use alloy::eips::BlockId;
use alloy::primitives::{address, keccak256, Address, B256, U256};
use alloy::providers::{Provider, ProviderBuilder, RootProvider};
use alloy::rpc::types::{Filter, Log, TransactionRequest};
use alloy::sol;
use alloy::sol_types::SolCall;
use alloy::transports::BoxTransport;
use anyhow::{anyhow, bail, Context, Result};
use futures::future::BoxFuture;
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// Multicall3, deployed at the same address on every network we use.
pub const MULTICALL3: Address = address!("cA11bde05977b3631167028862bE2a173976CA11");

sol! {
    #[allow(missing_docs)]
    interface IMulticall3 {
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }
        struct CallResult {
            bool success;
            bytes returnData;
        }
        function aggregate3(Call3[] calldata calls) external payable returns (CallResult[] memory returnData);
    }

    #[allow(missing_docs)]
    interface IBalance {
        function balanceOf(address account) external view returns (uint256);
    }
}

#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// Networks to scan (config names). Empty means every configured network.
    pub chains: Vec<String>,
    /// First block of the gap. Default: the stored cursor of each network.
    pub from_block: Option<u64>,
    /// Block `B` to read balances at and to scan up to. Default: the head
    /// minus the network's `block_offset_from_head`. Needs exactly one network.
    pub block: Option<u64>,
    /// Addresses per Multicall3 call.
    pub batch_size: usize,
    /// Blocks per `eth_getLogs` range, before any shrinking.
    pub log_range: u64,
    /// Recipient addresses ORed into one `eth_getLogs` filter.
    pub log_addresses: usize,
    /// Attempts per RPC call before treating it as refused.
    pub retries: u32,
    /// Refused log queries tolerated per network before giving up.
    pub max_refusals: u64,
    /// Store the deposits found. Without it nothing is written to the store.
    pub apply: bool,
    /// After applying, move the network's cursor to `B`.
    pub set_cursor: bool,
    /// Reserved for the native coin; `scan_native` is off in production.
    pub include_native: bool,
    pub multicall: Address,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            chains: Vec::new(),
            from_block: None,
            block: None,
            batch_size: 500,
            log_range: 50_000,
            log_addresses: 20,
            retries: 3,
            max_refusals: 500,
            apply: false,
            set_cursor: false,
            include_native: false,
            multicall: MULTICALL3,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Registration {
    pub id: String,
    pub index: u32,
    pub address: Address,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Would be stored as a deposit.
    Deposit,
    /// Sent by the faucet; the monitor skips these.
    Faucet,
    /// Below the detection minimum for the token.
    BelowMinimum,
}

impl Verdict {
    fn label(self) -> &'static str {
        match self {
            Verdict::Deposit => "deposit",
            Verdict::Faucet => "faucet",
            Verdict::BelowMinimum => "below_minimum",
        }
    }
}

#[derive(Clone, Debug)]
pub struct FoundTransfer {
    pub from: Address,
    pub tx_hash: B256,
    pub log_index: u64,
    pub block: u64,
    pub amount: U256,
    pub verdict: Verdict,
    log: Log,
}

/// One (address, token) pair with a non-zero balance at `B`.
#[derive(Clone, Debug)]
pub struct HolderReport {
    pub registration: Registration,
    pub token: Address,
    pub balance: U256,
    pub transfers: Vec<FoundTransfer>,
}

impl HolderReport {
    /// Sum of every inbound transfer found, whatever its verdict: they all
    /// explain part of the balance.
    pub fn explained(&self) -> U256 {
        self.transfers
            .iter()
            .fold(U256::ZERO, |a, t| a.saturating_add(t.amount))
    }

    /// Balance not explained by the transfers found. Non-zero means transfers
    /// before `from`, or a leftover.
    pub fn residue(&self) -> U256 {
        self.balance.saturating_sub(self.explained())
    }

    /// Inbound transfers found exceed the balance: something left the address.
    pub fn overexplained(&self) -> U256 {
        self.explained().saturating_sub(self.balance)
    }
}

#[derive(Clone, Debug, Default)]
pub struct RpcCounters {
    pub balance_calls: u64,
    pub log_calls: u64,
    pub log_refusals: u64,
}

#[derive(Clone, Debug)]
pub struct ChainReport {
    pub chain: String,
    pub chain_id: u64,
    pub head: u64,
    pub from_block: u64,
    pub block: u64,
    pub addresses: usize,
    pub tokens: Vec<Address>,
    pub holders: Vec<HolderReport>,
    pub counters: RpcCounters,
    /// Filled by `apply_chain`.
    pub applied: Option<ApplyOutcome>,
}

#[derive(Clone, Debug, Default)]
pub struct ApplyOutcome {
    pub new_deposits: u64,
    pub already_known: u64,
    pub cursor_set_to: Option<u64>,
}

impl ChainReport {
    pub fn deposits(&self) -> impl Iterator<Item = (&HolderReport, &FoundTransfer)> {
        self.holders.iter().flat_map(|h| {
            h.transfers
                .iter()
                .filter(|t| t.verdict == Verdict::Deposit)
                .map(move |t| (h, t))
        })
    }

    pub fn deposit_count(&self) -> usize {
        self.deposits().count()
    }

    pub fn has_residue(&self) -> bool {
        self.holders.iter().any(|h| !h.residue().is_zero())
    }
}

/// `Transfer(address,address,uint256)`.
fn transfer_topic() -> B256 {
    keccak256("Transfer(address,address,uint256)".as_bytes())
}

fn address_topic(a: Address) -> B256 {
    a.into_word()
}

/// Everything one network's scan needs, with RPC accounting.
struct Scanner<'a> {
    chain: &'a ChainConfig,
    provider: &'a RootProvider<BoxTransport>,
    opts: &'a ScanOptions,
    balance_calls: AtomicU64,
    log_calls: AtomicU64,
    log_refusals: AtomicU64,
}

impl<'a> Scanner<'a> {
    /// `balanceOf` for every address at `block`, through Multicall3. A batch
    /// the provider refuses is halved; a single refused call is an error.
    fn balances<'s>(
        &'s self,
        token: Address,
        addrs: &'s [Address],
        block: u64,
    ) -> BoxFuture<'s, Result<Vec<U256>>> {
        Box::pin(async move {
            let mut out = Vec::with_capacity(addrs.len());
            for chunk in addrs.chunks(self.opts.batch_size.max(1)) {
                out.extend(self.balances_batch(token, chunk, block).await?);
            }
            Ok(out)
        })
    }

    fn balances_batch<'s>(
        &'s self,
        token: Address,
        addrs: &'s [Address],
        block: u64,
    ) -> BoxFuture<'s, Result<Vec<U256>>> {
        Box::pin(async move {
            let calls: Vec<IMulticall3::Call3> = addrs
                .iter()
                .map(|a| IMulticall3::Call3 {
                    target: token,
                    allowFailure: true,
                    callData: IBalance::balanceOfCall { account: *a }.abi_encode().into(),
                })
                .collect();
            let data = IMulticall3::aggregate3Call { calls }.abi_encode();
            let request = TransactionRequest::default()
                .to(self.opts.multicall)
                .input(data.into());

            let mut last: Option<anyhow::Error> = None;
            for attempt in 1..=self.opts.retries.max(1) {
                self.balance_calls.fetch_add(1, Ordering::Relaxed);
                match self
                    .provider
                    .call(&request)
                    .block(BlockId::number(block))
                    .await
                {
                    Ok(bytes) => {
                        let decoded = IMulticall3::aggregate3Call::abi_decode_returns(&bytes, true)
                            .map_err(|e| {
                                anyhow!(
                                    "Multicall3 at {} returned undecodable data ({e}); \
                                     is it deployed on {}?",
                                    self.opts.multicall,
                                    self.chain.name
                                )
                            })?;
                        let results = decoded.returnData;
                        if results.len() != addrs.len() {
                            bail!(
                                "Multicall3 returned {} results for {} calls",
                                results.len(),
                                addrs.len()
                            );
                        }
                        let mut out = Vec::with_capacity(results.len());
                        for (addr, r) in addrs.iter().zip(results) {
                            if !r.success || r.returnData.len() < 32 {
                                bail!("balanceOf({addr}) failed on token {token} at block {block}");
                            }
                            out.push(U256::from_be_slice(&r.returnData[..32]));
                        }
                        return Ok(out);
                    }
                    Err(e) => {
                        warn!(
                            "[{}] multicall of {} addresses failed (attempt {attempt}): {e}",
                            self.chain.name,
                            addrs.len()
                        );
                        last = Some(e.into());
                        if attempt < self.opts.retries.max(1) {
                            tokio::time::sleep(Duration::from_millis(self.chain.get_logs_delay_ms))
                                .await;
                        }
                    }
                }
            }
            let err = last.expect("at least one attempt");
            if addrs.len() <= 1 {
                return Err(err.context(format!("balanceOf batch of 1 failed at block {block}")));
            }
            let (l, r) = addrs.split_at(addrs.len() / 2);
            let mut out = self.balances_batch(token, l, block).await?;
            out.extend(self.balances_batch(token, r, block).await?);
            Ok(out)
        })
    }

    /// Inbound `Transfer` logs of `token` to any of `group` in `[from, to]`.
    /// A refused query splits the range; at one block it splits the address
    /// group instead.
    fn logs<'s>(
        &'s self,
        token: Address,
        group: &'s [Address],
        from: u64,
        to: u64,
    ) -> BoxFuture<'s, Result<Vec<Log>>> {
        Box::pin(async move {
            let filter = Filter::new()
                .address(token)
                .event_signature(transfer_topic())
                .topic2(group.iter().map(|a| address_topic(*a)).collect::<Vec<_>>())
                .from_block(from)
                .to_block(to);

            let mut last: Option<anyhow::Error> = None;
            for attempt in 1..=self.opts.retries.max(1) {
                self.log_calls.fetch_add(1, Ordering::Relaxed);
                match self.provider.get_logs(&filter).await {
                    Ok(logs) => return Ok(logs),
                    Err(e) => {
                        last = Some(e.into());
                        if attempt < self.opts.retries.max(1) {
                            tokio::time::sleep(Duration::from_millis(self.chain.get_logs_delay_ms))
                                .await;
                        }
                    }
                }
            }
            let err = last.expect("at least one attempt");
            let refusals = self.log_refusals.fetch_add(1, Ordering::Relaxed) + 1;
            if refusals > self.opts.max_refusals {
                return Err(err.context(format!(
                    "more than {} refused log queries on {}; giving up",
                    self.opts.max_refusals, self.chain.name
                )));
            }
            if from < to {
                warn!(
                    "[{}] get_logs refused {from}..={to} ({} addresses): splitting range",
                    self.chain.name,
                    group.len()
                );
                let mid = from + (to - from) / 2;
                let mut out = self.logs(token, group, from, mid).await?;
                out.extend(self.logs(token, group, mid + 1, to).await?);
                Ok(out)
            } else if group.len() > 1 {
                warn!(
                    "[{}] get_logs refused block {from} ({} addresses): splitting addresses",
                    self.chain.name,
                    group.len()
                );
                let (l, r) = group.split_at(group.len() / 2);
                let mut out = self.logs(token, l, from, to).await?;
                out.extend(self.logs(token, r, from, to).await?);
                Ok(out)
            } else {
                Err(err.context(format!(
                    "get_logs refused a single block ({from}) for a single address"
                )))
            }
        })
    }
}

/// Judges one inbound log with the monitor's rules. `None` means the monitor
/// would ignore it silently (malformed, removed, token not admitted).
fn judge(chain: &ChainConfig, log: &Log) -> Option<(Address, FoundTransfer)> {
    if log.removed || log.topics().len() < 3 {
        return None;
    }
    let token = log.address();
    if !chain.is_token_allowed(&token.to_string()) {
        return None;
    }
    let from = Address::from_slice(&log.topics()[1].as_slice()[12..]);
    let to = Address::from_slice(&log.topics()[2].as_slice()[12..]);
    let amount = transfer_amount(log);
    let tx_hash = log.transaction_hash?;
    let verdict = if from.to_string().eq_ignore_ascii_case(&chain.faucet_address) {
        Verdict::Faucet
    } else if amount
        < chain
            .min_deposits
            .for_token(&token.to_string().to_lowercase())
    {
        Verdict::BelowMinimum
    } else {
        Verdict::Deposit
    };
    Some((
        to,
        FoundTransfer {
            from,
            tx_hash,
            log_index: log.log_index.unwrap_or(0),
            block: log.block_number.unwrap_or(0),
            amount,
            verdict,
            log: log.clone(),
        },
    ))
}

/// Admitted tokens of a network, sorted, as addresses.
pub fn admitted_tokens(chain: &ChainConfig) -> Result<Vec<Address>> {
    let mut tokens: Vec<Address> = chain
        .allowed_token_addresses
        .iter()
        .map(|t| {
            Address::from_str(t)
                .with_context(|| format!("bad allowed token address {t} on {}", chain.name))
        })
        .collect::<Result<_>>()?;
    tokens.sort();
    Ok(tokens)
}

/// Scans one network: balances at `B`, then logs for the holders. Writes
/// nothing.
pub async fn scan_chain(
    db: &Db,
    chain: &ChainConfig,
    provider: &RootProvider<BoxTransport>,
    regs: &[Registration],
    opts: &ScanOptions,
) -> Result<ChainReport> {
    let head = provider
        .get_block_number()
        .await
        .with_context(|| format!("eth_blockNumber on {}", chain.name))?;
    let block = opts
        .block
        .unwrap_or_else(|| head.saturating_sub(chain.block_offset_from_head));
    let from_block = match opts.from_block {
        Some(f) => f,
        None => {
            let name = chain.name.clone();
            let cursor = db
                .blocking(move |db| db.get_last_processed_block(&name))
                .await?;
            if cursor == 0 {
                bail!(
                    "{}: no stored cursor; pass --from with the first block of the gap",
                    chain.name
                );
            }
            cursor
        }
    };
    let tokens = admitted_tokens(chain)?;
    let mut report = ChainReport {
        chain: chain.name.clone(),
        chain_id: chain.chain_id,
        head,
        from_block,
        block,
        addresses: regs.len(),
        tokens: tokens.clone(),
        holders: Vec::new(),
        counters: RpcCounters::default(),
        applied: None,
    };
    if tokens.is_empty() {
        warn!(
            "[{}] no allowed_token_addresses: nothing to scan (an unfiltered scan is not supported)",
            chain.name
        );
        return Ok(report);
    }
    if from_block > block {
        info!(
            "[{}] cursor {from_block} is beyond block {block}: empty gap",
            chain.name
        );
        return Ok(report);
    }

    let scanner = Scanner {
        chain,
        provider,
        opts,
        balance_calls: AtomicU64::new(0),
        log_calls: AtomicU64::new(0),
        log_refusals: AtomicU64::new(0),
    };
    let addrs: Vec<Address> = regs.iter().map(|r| r.address).collect();
    let by_address: BTreeMap<Address, &Registration> =
        regs.iter().map(|r| (r.address, r)).collect();

    for token in tokens {
        info!(
            "[{}] reading balanceOf for {} addresses of {token} at block {block}",
            chain.name,
            addrs.len()
        );
        let balances = scanner.balances(token, &addrs, block).await?;
        let holders: Vec<(Address, U256)> = addrs
            .iter()
            .copied()
            .zip(balances)
            .filter(|(_, b)| !b.is_zero())
            .collect();
        info!(
            "[{}] {} addresses hold {token}; fetching their inbound logs {from_block}..={block}",
            chain.name,
            holders.len()
        );

        let holder_addrs: Vec<Address> = holders.iter().map(|(a, _)| *a).collect();
        let mut found: BTreeMap<Address, Vec<FoundTransfer>> = BTreeMap::new();
        let mut seen: HashSet<(B256, u64)> = HashSet::new();
        for group in holder_addrs.chunks(opts.log_addresses.max(1)) {
            let mut start = from_block;
            while start <= block {
                let end = start.saturating_add(opts.log_range.max(1) - 1).min(block);
                for log in scanner.logs(token, group, start, end).await? {
                    let Some((to, t)) = judge(chain, &log) else {
                        continue;
                    };
                    if log.address() != token || !group.contains(&to) {
                        continue;
                    }
                    if !seen.insert((t.tx_hash, t.log_index)) {
                        continue;
                    }
                    found.entry(to).or_default().push(t);
                }
                start = end + 1;
            }
        }

        for (address, balance) in holders {
            let mut transfers = found.remove(&address).unwrap_or_default();
            transfers.sort_by_key(|t| (t.block, t.log_index));
            report.holders.push(HolderReport {
                registration: (*by_address[&address]).clone(),
                token,
                balance,
                transfers,
            });
        }
    }

    report.counters = RpcCounters {
        balance_calls: scanner.balance_calls.load(Ordering::Relaxed),
        log_calls: scanner.log_calls.load(Ordering::Relaxed),
        log_refusals: scanner.log_refusals.load(Ordering::Relaxed),
    };
    Ok(report)
}

/// Stores the deposits of a report through the monitor's own code path, then
/// optionally moves the cursor to `B`. Idempotent: a deposit already stored
/// (INV-2) is counted as known, not written again.
pub async fn apply_chain(
    db: &Db,
    chain: &ChainConfig,
    provider: &RootProvider<BoxTransport>,
    deliverer: Arc<WebhookDeliverer>,
    report: &mut ChainReport,
    set_cursor: bool,
) -> Result<()> {
    let monitor = Monitor::new(chain.clone(), deliverer, db.clone(), provider.clone());
    let mut outcome = ApplyOutcome::default();
    for holder in &report.holders {
        for t in holder
            .transfers
            .iter()
            .filter(|t| t.verdict == Verdict::Deposit)
        {
            if monitor.handle_erc20_log(&t.log).await? {
                outcome.new_deposits += 1;
            } else {
                outcome.already_known += 1;
            }
        }
    }
    if set_cursor {
        let name = chain.name.clone();
        let block = report.block;
        db.blocking(move |db| db.set_last_processed_block(&name, block))
            .await?;
        outcome.cursor_set_to = Some(block);
    }
    report.applied = Some(outcome);
    Ok(())
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// One row per found transfer; a holder with none gets one row with the
/// transfer columns empty.
pub fn render_csv(reports: &[ChainReport]) -> String {
    let mut out = String::from(
        "chain,block,address,registration_id,token,balance_at_block,residue,\
         tx_hash,log_index,transfer_block,from,amount,verdict\n",
    );
    for r in reports {
        for h in &r.holders {
            let head = format!(
                "{},{},{},{},{},{},{}",
                csv_field(&r.chain),
                r.block,
                h.registration.address,
                csv_field(&h.registration.id),
                h.token,
                h.balance,
                h.residue()
            );
            if h.transfers.is_empty() {
                let _ = writeln!(out, "{head},,,,,,");
            }
            for t in &h.transfers {
                let _ = writeln!(
                    out,
                    "{head},{},{},{},{},{},{}",
                    t.tx_hash,
                    t.log_index,
                    t.block,
                    t.from,
                    t.amount,
                    t.verdict.label()
                );
            }
        }
    }
    out
}

pub fn write_csv(path: &Path, reports: &[ChainReport]) -> Result<()> {
    std::fs::write(path, render_csv(reports))
        .with_context(|| format!("writing CSV {}", path.display()))
}

/// Human-readable summary. Unexplained balance is printed loudly.
pub fn render_summary(reports: &[ChainReport], applied_mode: bool) -> String {
    let mut out = String::new();
    let mode = if applied_mode {
        "APPLY"
    } else {
        "DRY RUN (nothing written to the store)"
    };
    let _ = writeln!(out, "=== evmhot gap scan: {mode} ===");
    for r in reports {
        let _ = writeln!(
            out,
            "\n[{}] chain_id={} head={} scanned blocks {}..={} ({} addresses, {} tokens)",
            r.chain,
            r.chain_id,
            r.head,
            r.from_block,
            r.block,
            r.addresses,
            r.tokens.len()
        );
        let _ = writeln!(
            out,
            "  rpc calls: {} multicall, {} getLogs ({} refused and split)",
            r.counters.balance_calls, r.counters.log_calls, r.counters.log_refusals
        );
        for token in &r.tokens {
            let hs: Vec<&HolderReport> = r.holders.iter().filter(|h| h.token == *token).collect();
            let total = hs
                .iter()
                .fold(U256::ZERO, |a, h| a.saturating_add(h.balance));
            let (mut deposits, mut faucet, mut minimum) = (0usize, 0usize, 0usize);
            let mut deposit_total = U256::ZERO;
            for t in hs.iter().flat_map(|h| h.transfers.iter()) {
                match t.verdict {
                    Verdict::Deposit => {
                        deposits += 1;
                        deposit_total = deposit_total.saturating_add(t.amount);
                    }
                    Verdict::Faucet => faucet += 1,
                    Verdict::BelowMinimum => minimum += 1,
                }
            }
            let residue_total = hs
                .iter()
                .fold(U256::ZERO, |a, h| a.saturating_add(h.residue()));
            let _ = writeln!(
                out,
                "  token {token}: {} addresses with balance, total {total}; \
                 {deposits} deposits found (sum {deposit_total}), \
                 {faucet} from faucet and {minimum} below minimum skipped; \
                 unexplained balance {residue_total}",
                hs.len()
            );
        }
        let _ = writeln!(
            out,
            "  deposits found on {}: {}",
            r.chain,
            r.deposit_count()
        );
        if let Some(a) = &r.applied {
            let _ = writeln!(
                out,
                "  applied: {} new, {} already stored{}",
                a.new_deposits,
                a.already_known,
                match a.cursor_set_to {
                    Some(b) => format!("; cursor set to {b}"),
                    None => "; cursor NOT moved".to_string(),
                }
            );
        }
        let over: Vec<&HolderReport> = r
            .holders
            .iter()
            .filter(|h| !h.overexplained().is_zero())
            .collect();
        if !over.is_empty() {
            let _ = writeln!(
                out,
                "  note: {} (address, token) pairs received more than they hold \
                 (something left them, e.g. an earlier sweep)",
                over.len()
            );
        }
        let bad: Vec<&HolderReport> = r
            .holders
            .iter()
            .filter(|h| !h.residue().is_zero())
            .collect();
        if !bad.is_empty() {
            let _ = writeln!(
                out,
                "\n  !!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
            );
            let _ = writeln!(
                out,
                "  !!! UNEXPLAINED BALANCE on {}: {} (address, token) pairs hold more",
                r.chain,
                bad.len()
            );
            let _ = writeln!(
                out,
                "  !!! than the transfers found in {}..={}. Transfers before --from, or a leftover.",
                r.from_block, r.block
            );
            let _ = writeln!(
                out,
                "  !!! Do NOT move the cursor until this is understood."
            );
            let _ = writeln!(
                out,
                "  !!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
            );
            for h in bad.iter().take(20) {
                let _ = writeln!(
                    out,
                    "    {} (registration {}) token {}: balance {}, explained {}, residue {}",
                    h.registration.address,
                    h.registration.id,
                    h.token,
                    h.balance,
                    h.explained(),
                    h.residue()
                );
            }
            if bad.len() > 20 {
                let _ = writeln!(out, "    ... and {} more (see the CSV)", bad.len() - 20);
            }
        }
    }
    out
}

#[derive(Clone, Debug)]
pub struct RunOptions {
    pub scan: ScanOptions,
    pub csv: PathBuf,
}

/// Validates the flags against the configuration before any RPC call.
pub fn validate(config: &Config, opts: &ScanOptions) -> Result<Vec<ChainConfig>> {
    if opts.include_native {
        bail!("--include-native is reserved: the native coin is not scanned (scan_native is off)");
    }
    if opts.set_cursor && !opts.apply {
        bail!("--set-cursor writes to the store, so it needs --apply");
    }
    let chains: Vec<ChainConfig> = if opts.chains.is_empty() {
        config.chains.clone()
    } else {
        opts.chains
            .iter()
            .map(|n| {
                config.chain(n).cloned().ok_or_else(|| {
                    anyhow!(
                        "unknown network {n}; configured: {}",
                        config
                            .chains
                            .iter()
                            .map(|c| c.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })
            })
            .collect::<Result<_>>()?
    };
    if chains.is_empty() {
        bail!("no networks configured");
    }
    if opts.block.is_some() && chains.len() != 1 {
        bail!("--block is per network: select exactly one with --chain");
    }
    if opts.set_cursor && opts.block.is_none() {
        bail!(
            "--set-cursor needs an explicit --block, the same one the deposits were scanned \
             and stored at, so the cursor cannot skip a part of the chain"
        );
    }
    if opts.set_cursor && chains.len() != 1 {
        bail!("--set-cursor moves one network's cursor: select it with --chain");
    }
    Ok(chains)
}

/// Runs the whole scan for the selected networks. Returns one report each.
pub async fn run(config: &Config, db: &Db, opts: &RunOptions) -> Result<Vec<ChainReport>> {
    let chains = validate(config, &opts.scan)?;

    let regs: Vec<Registration> = db
        .blocking(|db| db.list_registrations())
        .await?
        .into_iter()
        .map(|(id, index, address)| {
            Ok(Registration {
                id,
                index,
                address: Address::from_str(&address)
                    .with_context(|| format!("bad stored address {address}"))?,
            })
        })
        .collect::<Result<_>>()?;
    info!("{} registered addresses", regs.len());

    let deliverer = Arc::new(WebhookDeliverer::new(db.clone(), config)?);
    let mut reports = Vec::new();
    for chain in &chains {
        let provider = ProviderBuilder::new()
            .on_builtin(&chain.rpc_url)
            .await
            .map_err(|e| anyhow!("connecting to {} at {}: {e}", chain.name, chain.rpc_url))?;
        let mut report = scan_chain(db, chain, &provider, &regs, &opts.scan).await?;
        if opts.scan.apply {
            apply_chain(
                db,
                chain,
                &provider,
                deliverer.clone(),
                &mut report,
                opts.scan.set_cursor,
            )
            .await?;
        }
        reports.push(report);
    }
    write_csv(&opts.csv, &reports)?;
    Ok(reports)
}
