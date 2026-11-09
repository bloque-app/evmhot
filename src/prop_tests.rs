//! Property test for sweep sending, against the fake chain in `fake_chain.rs`.
//!
//! Each case (a seed) builds a store, a fake chain and a sweeper with one of
//! several target shapes, then runs a random interleaving of: token and native
//! deposits, sweeper passes (some of which die mid-pass, after which a fresh
//! sweeper takes over the same store), blocks being mined (held transactions
//! landing late or not at all), fee spikes, refused broadcasts, operator
//! retries and restarts. After every step it checks the invariants below; at
//! the end it lets the chain settle and checks nothing is left stuck without a
//! reason. Cases are deterministic: a failure prints its seed and trace.
//!
//! Invariants, after every step:
//!  1. every transaction mined from a deposit address is a recorded sweep, or
//!     belongs to an intent that may still resolve (never to a dropped one);
//!  2. no intent has two mined transactions (nothing is swept twice);
//!  3. recorded sweeps match the chain's transfers, and per deposit (or sender
//!     or address, by target shape) never exceed what was deposited;
//!  4. a deposit is settled only with recorded sweeps behind it, with exactly
//!     one `deposit_swept` whose `sweeps` are its recorded sweeps, and the
//!     zero-balance rule (REQ-SW-3), which needs a deposit whose tokens vanished,
//!     never fires because every deposit here is funded;
//!  5. an address has at most one open intent.

use crate::config::{ChainConfig, SweepTarget, TargetKind};
use crate::db::{Db, NewErc20Deposit, NewNativeDeposit};
use crate::fake_chain::{serve, Chain, MinedKind, Rng, SharedChain};
use crate::faucet::Faucet;
use crate::sweeper::{CrashPoint, Sweeper};
use crate::test_support::{
    self, http_provider_boxed, test_chain_config, test_webhook_deliverer, TEST_CHAIN, TEST_MNEMONIC,
};
use crate::wallet::Wallet;
use alloy::primitives::{Address, U256};
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tempfile::NamedTempFile;

const TOKEN: &str = "0x00000000000000000000000000000000000c0de1";
const TARGET: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SENDERS: [&str; 2] = [
    "0x00000000000000000000000000000000000a11ce",
    "0x0000000000000000000000000000000000000b0b",
];

#[derive(Clone, Copy, Debug)]
enum Shape {
    Plain,
    PlainMax,
    PerSender,
    PerSenderMax,
    PerTransfer,
    PerTransferMax,
}

impl Shape {
    fn pick(rng: &mut Rng) -> Self {
        match rng.below(6) {
            0 => Shape::Plain,
            1 => Shape::PlainMax,
            2 => Shape::PerSender,
            3 => Shape::PerSenderMax,
            4 => Shape::PerTransfer,
            _ => Shape::PerTransferMax,
        }
    }
    fn owed_mode(self) -> bool {
        matches!(
            self,
            Shape::PerSender | Shape::PerSenderMax | Shape::PerTransfer | Shape::PerTransferMax
        )
    }
    fn target(self) -> SweepTarget {
        let max = match self {
            Shape::PlainMax | Shape::PerSenderMax | Shape::PerTransferMax => {
                Some(U256::from(100u64))
            }
            _ => None,
        };
        SweepTarget {
            kind: TargetKind::Transfer,
            address: Some(TARGET.to_string()),
            min: None,
            max,
            admits_only: false,
            per_sender: matches!(self, Shape::PerSender | Shape::PerSenderMax),
            per_transfer: matches!(self, Shape::PerTransfer | Shape::PerTransferMax),
        }
    }
}

struct Case {
    seed: u64,
    shape: Shape,
    _file: NamedTempFile,
    path: String,
    db: Db,
    chain: SharedChain,
    _server: wiremock::MockServer,
    uri: String,
    /// (registration id, deposit address)
    accounts: Vec<(String, Address)>,
    rng: Rng,
    next_id: u64,
    trace: Vec<String>,
    sweeper: Sweeper,
}

fn build_sweeper(case_uri: &str, db: &Db, shape: Shape) -> Sweeper {
    let mut cfg: ChainConfig = test_chain_config(case_uri.to_string());
    cfg.token_targets
        .insert(TOKEN.to_lowercase(), shape.target());
    let provider = http_provider_boxed(case_uri);
    let faucet = Arc::new(
        Faucet::new(
            TEST_MNEMONIC.to_string(),
            provider.clone(),
            test_support::TEST_EXISTENTIAL,
        )
        .unwrap(),
    );
    Sweeper::new(
        cfg,
        test_webhook_deliverer(db.clone()),
        db.clone(),
        Wallet::new(TEST_MNEMONIC.to_string()),
        provider,
        faucet,
    )
}

async fn new_case(seed: u64) -> Case {
    let mut rng = Rng::new(seed);
    let shape = Shape::pick(&mut rng);
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_str().unwrap().to_string();
    let db = Db::new(&path).unwrap();
    let wallet = Wallet::new(TEST_MNEMONIC.to_string());
    let faucet_addr = wallet.derive_address(0).unwrap();
    let mut accounts = Vec::new();
    let mut chain = Chain::new(
        Address::from_str(TOKEN).unwrap(),
        faucet_addr,
        seed ^ 0xABCD,
    );
    for i in 1..=2u32 {
        let addr = wallet.derive_address(i).unwrap();
        let id = format!("u{i}");
        db.register_account(&id, i, &addr.to_string(), "http://127.0.0.1:9/hook")
            .unwrap();
        chain.native_bal.insert(addr, 20_000_000_000_000_000);
        accounts.push((id, addr));
    }
    db.store_token_metadata(TEST_CHAIN, &checksum(TOKEN), "TKN", 6, "Test token")
        .unwrap();
    chain.receipt_lag = rng.chance(40);
    chain.p_delay = 20 + rng.below(30);
    chain.p_evict = rng.below(25);
    chain.accept_on_error = rng.below(100);
    let chain: SharedChain = Arc::new(Mutex::new(chain));
    let server = serve(chain.clone()).await;
    let uri = server.uri();
    let sweeper = build_sweeper(&uri, &db, shape);
    Case {
        seed,
        shape,
        _file: file,
        path,
        db,
        chain,
        _server: server,
        uri,
        accounts,
        rng,
        next_id: 1,
        trace: Vec::new(),
        sweeper,
    }
}

fn checksum(a: &str) -> String {
    Address::from_str(a).unwrap().to_string()
}

impl Case {
    fn note(&mut self, s: String) {
        self.trace.push(s);
    }

    fn fail(&self, what: &str) -> ! {
        let tail: Vec<&String> = self.trace.iter().rev().take(40).rev().collect();
        panic!(
            "property violated (seed {}, shape {:?}): {what}\nlast steps:\n{}",
            self.seed,
            self.shape,
            tail.iter()
                .enumerate()
                .map(|(i, s)| format!("  {i:>2}. {s}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    fn deposit_token(&mut self) {
        let acct = self.rng.below(2) as usize;
        let amount = 10 + self.rng.below(290) as u128;
        let sender = if self.rng.chance(10) {
            None
        } else {
            Some(SENDERS[self.rng.below(2) as usize].to_string())
        };
        let id = self.next_id;
        self.next_id += 1;
        let (reg, addr) = self.accounts[acct].clone();
        let tx = format!("0x{id:064x}");
        self.db
            .record_erc20_deposit_atomic(
                &NewErc20Deposit {
                    chain: TEST_CHAIN.into(),
                    tx_hash: tx.clone(),
                    log_index: id % 5,
                    account_id: reg.clone(),
                    amount: amount.to_string(),
                    token_address: checksum(TOKEN),
                    token_symbol: "TKN".into(),
                    from_address: sender.clone(),
                },
                None,
            )
            .unwrap();
        *self
            .chain
            .lock()
            .unwrap()
            .token_bal
            .entry(addr)
            .or_insert(0) += amount;
        self.note(format!(
            "token deposit {reg} {amount} from {sender:?} ({tx})"
        ));
    }

    fn deposit_native(&mut self) {
        let acct = self.rng.below(2) as usize;
        let value = 1_000_000_000_000_000u128 + self.rng.below(1000) as u128;
        let id = self.next_id;
        self.next_id += 1;
        let (reg, addr) = self.accounts[acct].clone();
        let tx = format!("0x{:064x}", 0x1000_0000 + id);
        self.db
            .record_native_deposit_atomic(
                &NewNativeDeposit {
                    chain: TEST_CHAIN.into(),
                    tx_hash: tx.clone(),
                    account_id: reg.clone(),
                    amount: value.to_string(),
                    from_address: Some(SENDERS[0].to_string()),
                },
                None,
            )
            .unwrap();
        *self
            .chain
            .lock()
            .unwrap()
            .native_bal
            .entry(addr)
            .or_insert(0) += value;
        self.note(format!("native deposit {reg} {value} ({tx})"));
    }

    async fn pass(&mut self, crash: Option<CrashPoint>) {
        self.sweeper.set_crash(crash);
        // alloy 0.1 has no receipt timeout: a pass that waits on a transaction that
        // never lands hangs the whole sweeper. Bound it, and call that a failure.
        let r = match tokio::time::timeout(
            std::time::Duration::from_secs(8),
            self.sweeper.process_deposits_once(),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => self.fail("a sweeper pass hung (waiting on a receipt that never comes?)"),
        };
        match (&r, crash) {
            (Ok(()), _) => self.note(format!("pass (crash armed {crash:?}) ok")),
            (Err(e), Some(_)) if e.to_string().contains("test crash") => {
                self.note(format!("pass crashed: {e}"));
                // The process died: a fresh sweeper takes over the same store.
                self.sweeper = build_sweeper(&self.uri, &self.db, self.shape);
            }
            (Err(e), _) => self.fail(&format!("sweeper pass failed: {e:?}")),
        }
        self.sweeper.set_crash(None);
    }

    fn retry_one(&mut self) {
        let conn = Connection::open(&self.path).unwrap();
        let mut keys: Vec<(String, i64, bool)> = Vec::new();
        let mut stmt = conn
            .prepare("SELECT tx_hash, log_index FROM erc20_deposits WHERE status IN ('needs_review','failed')")
            .unwrap();
        for r in stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .unwrap()
        {
            let (t, l) = r.unwrap();
            keys.push((t, l, false));
        }
        if let Ok(mut stmt) =
            conn.prepare("SELECT tx_hash FROM deposits WHERE status IN ('needs_review','failed')")
        {
            for r in stmt.query_map([], |r| r.get::<_, String>(0)).unwrap() {
                keys.push((r.unwrap(), 0, true));
            }
        }
        if keys.is_empty() {
            return;
        }
        let (tx, log, native) = keys[self.rng.below(keys.len() as u64) as usize].clone();
        let res = if native {
            self.db.retry_native_deposit(TEST_CHAIN, &tx)
        } else {
            self.db.retry_erc20_deposit(TEST_CHAIN, &tx, log as u64)
        };
        self.note(format!(
            "operator retry {tx}:{log} native={native} -> {res:?}"
        ));
    }

    /// Reads the store and the chain and checks the invariants.
    fn check(&self) {
        let conn = Connection::open(&self.path).unwrap();
        let chain = self.chain.lock().unwrap();
        let deposit_addrs: HashMap<Address, String> =
            self.accounts.iter().map(|(r, a)| (*a, r.clone())).collect();

        // Recorded sweeps: key -> [(hash, amount)] in order.
        let mut recorded: HashMap<String, Vec<(String, u128)>> = HashMap::new();
        let mut recorded_hashes: HashSet<String> = HashSet::new();
        {
            let mut stmt = conn
                .prepare("SELECT tx_hash, log_index, sweep_tx_hash, sweep_amount FROM sweeps ORDER BY seq")
                .unwrap();
            for r in stmt
                .query_map([], |r| {
                    Ok((
                        format!("{}:{}", r.get::<_, String>(0)?, r.get::<_, i64>(1)?),
                        r.get::<_, String>(2)?.to_lowercase(),
                        r.get::<_, String>(3)?,
                    ))
                })
                .unwrap()
            {
                let (key, hash, amount) = r.unwrap();
                recorded_hashes.insert(hash.clone());
                recorded
                    .entry(key)
                    .or_default()
                    .push((hash, amount.parse().unwrap()));
            }
        }

        // Intents: hashes by state, and the open ones per account.
        let has_txs = conn
            .prepare("SELECT txs FROM sweep_intents LIMIT 1")
            .is_ok();
        let mut intent_hashes: HashMap<i64, Vec<String>> = HashMap::new();
        let mut live_intent_hashes: HashSet<String> = HashSet::new();
        let mut open_per_account: HashMap<String, u32> = HashMap::new();
        {
            let sql = if has_txs {
                "SELECT id, account_id, state, tx_hash, txs FROM sweep_intents"
            } else {
                "SELECT id, account_id, state, tx_hash, NULL FROM sweep_intents"
            };
            let mut stmt = conn.prepare(sql).unwrap();
            for r in stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                })
                .unwrap()
            {
                let (id, account, state, hash, txs) = r.unwrap();
                let mut hashes: Vec<String> = Vec::new();
                if let Some(txs) = txs {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txs) {
                        for t in v.as_array().cloned().unwrap_or_default() {
                            if let Some(h) = t["hash"].as_str() {
                                hashes.push(h.to_lowercase());
                            }
                        }
                    }
                }
                if hashes.is_empty() {
                    if let Some(h) = hash {
                        hashes.push(h.to_lowercase());
                    }
                }
                if state == "pending" {
                    *open_per_account.entry(account.clone()).or_insert(0) += 1;
                }
                // A dropped intent can no longer account for a transaction.
                if state != "dropped" {
                    live_intent_hashes.extend(hashes.iter().cloned());
                }
                intent_hashes.insert(id, hashes);
            }
        }
        // 5. one open intent per address.
        for (account, n) in &open_per_account {
            if *n > 1 {
                self.fail(&format!("{n} open intents for {account}"));
            }
        }

        // 1 + 2 + 3. Every mined transaction from a deposit address is accounted for.
        let mut mined_by_hash: HashMap<String, &crate::fake_chain::Mined> = HashMap::new();
        for m in &chain.mined {
            mined_by_hash.insert(m.hash.to_lowercase(), m);
        }
        for m in &chain.mined {
            if !deposit_addrs.contains_key(&m.from) {
                continue;
            }
            let h = m.hash.to_lowercase();
            let ok = recorded_hashes.contains(&h) || live_intent_hashes.contains(&h);
            if !ok {
                self.fail(&format!(
                    "mined tx {h} from {} (nonce {}, {:?}) is neither a recorded sweep nor a live intent's",
                    m.from, m.nonce, m.kind
                ));
            }
        }
        for (id, hashes) in &intent_hashes {
            let landed = hashes
                .iter()
                .filter(|h| mined_by_hash.contains_key(*h))
                .count();
            if landed > 1 {
                self.fail(&format!("intent {id} has {landed} mined transactions"));
            }
        }
        // Recorded sweeps match what the chain moved.
        for (key, sweeps) in &recorded {
            for (hash, amount) in sweeps {
                match mined_by_hash.get(hash) {
                    Some(m) => match &m.kind {
                        MinedKind::Token {
                            amount: a,
                            success: true,
                            ..
                        } if a == amount => {}
                        other => self.fail(&format!(
                            "recorded sweep {hash} of {key} says {amount}, the chain has {other:?}"
                        )),
                    },
                    None => self.fail(&format!("recorded sweep {hash} of {key} never mined")),
                }
            }
        }

        // Deposits.
        struct Dep {
            key: String,
            reg: String,
            amount: u128,
            status: String,
            sender: Option<String>,
        }
        let mut deps: Vec<Dep> = Vec::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT tx_hash, log_index, account_id, amount, status, from_address FROM erc20_deposits",
                )
                .unwrap();
            for r in stmt
                .query_map([], |r| {
                    Ok(Dep {
                        key: format!("{}:{}", r.get::<_, String>(0)?, r.get::<_, i64>(1)?),
                        reg: r.get(2)?,
                        amount: r.get::<_, String>(3)?.parse().unwrap(),
                        status: r.get(4)?,
                        sender: r.get(5)?,
                    })
                })
                .unwrap()
            {
                deps.push(r.unwrap());
            }
        }
        let swept_webhooks: HashMap<String, serde_json::Value> = {
            let mut stmt = conn
                .prepare("SELECT id, payload FROM webhook_deliveries WHERE event = 'deposit_swept'")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .unwrap()
                .map(|r| {
                    let (id, p) = r.unwrap();
                    (id, serde_json::from_str(&p).unwrap())
                })
                .collect()
        };
        for d in &deps {
            let id = format!("{TEST_CHAIN}:{}", d.key);
            let rec = recorded.get(&d.key);
            if d.status == "swept" {
                let Some(rec) = rec else {
                    self.fail(&format!("deposit {} settled with no recorded sweep", d.key));
                };
                let Some(w) = swept_webhooks.get(&id) else {
                    self.fail(&format!("deposit {} settled with no deposit_swept", d.key));
                };
                let hashes: Vec<String> = w["sweeps"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| s["sweep_tx_hash"].as_str().unwrap().to_lowercase())
                    .collect();
                let want: Vec<String> = rec.iter().map(|(h, _)| h.clone()).collect();
                if hashes != want {
                    self.fail(&format!(
                        "deposit {} event sweeps {hashes:?} differ from recorded {want:?}",
                        d.key
                    ));
                }
                if self.shape_per_transfer() {
                    let sum: u128 = rec.iter().map(|(_, a)| a).sum();
                    if sum != d.amount {
                        self.fail(&format!(
                            "per_transfer deposit {} settled with {sum} swept of {}",
                            d.key, d.amount
                        ));
                    }
                }
            } else if swept_webhooks.contains_key(&id) {
                self.fail(&format!(
                    "deposit {} has deposit_swept but status {}",
                    d.key, d.status
                ));
            }
        }
        // 3 (caps): nothing recorded beyond what was deposited.
        if self.shape_per_transfer() {
            for d in &deps {
                let sum: u128 = recorded
                    .get(&d.key)
                    .map(|r| r.iter().map(|(_, a)| a).sum())
                    .unwrap_or(0);
                if sum > d.amount {
                    self.fail(&format!("deposit {} swept {sum} of {}", d.key, d.amount));
                }
            }
        }
        // Per address (distinct sweeps) never beyond all deposits on it.
        for (reg, _) in &self.accounts {
            let mut seen: HashSet<String> = HashSet::new();
            let mut total = 0u128;
            for d in deps.iter().filter(|d| &d.reg == reg) {
                for (h, a) in recorded.get(&d.key).cloned().unwrap_or_default() {
                    if seen.insert(h) {
                        total += a;
                    }
                }
            }
            let deposited: u128 = deps
                .iter()
                .filter(|d| &d.reg == reg)
                .map(|d| d.amount)
                .sum();
            if total > deposited {
                self.fail(&format!(
                    "{reg} swept {total} but only {deposited} was deposited"
                ));
            }
            if self.shape_per_sender() {
                for s in SENDERS {
                    let mut seen: HashSet<String> = HashSet::new();
                    let mut sent = 0u128;
                    let mut owed = 0u128;
                    for d in deps
                        .iter()
                        .filter(|d| &d.reg == reg && d.sender.as_deref() == Some(s))
                    {
                        owed += d.amount;
                        for (h, a) in recorded.get(&d.key).cloned().unwrap_or_default() {
                            if seen.insert(h) {
                                sent += a;
                            }
                        }
                    }
                    if sent > owed {
                        self.fail(&format!(
                            "{reg} swept {sent} for sender {s} who deposited {owed}"
                        ));
                    }
                }
            }
        }
        // Native: a settled native deposit has exactly one event.
        let mut stmt = conn
            .prepare("SELECT tx_hash, status FROM deposits")
            .unwrap();
        for r in stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
        {
            let (tx, status) = r.unwrap();
            let id = format!("{TEST_CHAIN}:{tx}");
            if status == "swept" && !swept_webhooks.contains_key(&id) {
                self.fail(&format!(
                    "native deposit {tx} settled without deposit_swept"
                ));
            }
            if status != "swept" && swept_webhooks.contains_key(&id) {
                self.fail(&format!(
                    "native deposit {tx} has deposit_swept but status {status}"
                ));
            }
        }
    }

    fn shape_per_transfer(&self) -> bool {
        matches!(self.shape, Shape::PerTransfer | Shape::PerTransferMax)
    }

    fn shape_per_sender(&self) -> bool {
        matches!(self.shape, Shape::PerSender | Shape::PerSenderMax)
    }

    /// Lets the chain settle (fees normal, no failures) and checks nothing is
    /// left stuck without a reason.
    async fn settle_and_check_liveness(&mut self) {
        {
            let mut c = self.chain.lock().unwrap();
            c.base_fee = 1_000_000_000;
            c.fail_broadcasts = 0;
            c.p_delay = 0;
            c.p_evict = 0;
        }
        for _ in 0..80 {
            self.pass(None).await;
            self.chain.lock().unwrap().mine();
            self.chain.lock().unwrap().step += 1;
            self.check();
        }
        let conn = Connection::open(&self.path).unwrap();
        let open: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sweep_intents WHERE state = 'pending'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if open != 0 {
            let mut stmt = conn
                .prepare("SELECT id, account_id, token_address, keys, nonce, passes, gone_passes, tx_hash FROM sweep_intents WHERE state = 'pending'")
                .unwrap();
            let rows: Vec<String> = stmt
                .query_map([], |r| {
                    Ok(format!(
                        "intent {} acct {} token {} keys {} nonce {:?} passes {} gone {} tx {:?}",
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, Option<String>>(7)?
                    ))
                })
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            let c = self.chain.lock().unwrap();
            let nonces: Vec<String> = self
                .accounts
                .iter()
                .map(|(r, a)| format!("{r} latest nonce {}", c.latest_nonce(*a)))
                .collect();
            let held = c.held_hashes();
            drop(c);
            self.fail(&format!(
                "{open} intents still open after settling: {rows:?}; {nonces:?}; held {held:?}"
            ));
        }
        // A deposit still detected must be blocked by a deposit in review/failed
        // on its address (whole-balance targets); owed modes reserve instead.
        let mut stmt = conn
            .prepare("SELECT tx_hash, log_index, account_id FROM erc20_deposits WHERE status = 'detected'")
            .unwrap();
        let detected: Vec<(String, i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        for (tx, log, reg) in detected {
            let blockers: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM erc20_deposits WHERE account_id = ?1 AND status IN ('needs_review','failed')",
                    [&reg],
                    |r| r.get(0),
                )
                .unwrap();
            if self.shape.owed_mode() || blockers == 0 {
                self.fail(&format!(
                    "deposit {tx}:{log} ({reg}) still detected after settling"
                ));
            }
        }
        let mut stmt = conn
            .prepare("SELECT tx_hash, account_id FROM deposits WHERE status = 'detected'")
            .unwrap();
        let native: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        for (tx, reg) in native {
            let blockers: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM deposits WHERE account_id = ?1 AND status IN ('needs_review','failed')",
                    [&reg],
                    |r| r.get(0),
                )
                .unwrap();
            if blockers == 0 {
                self.fail(&format!(
                    "native deposit {tx} ({reg}) still detected after settling"
                ));
            }
        }
    }
}

async fn run_case(seed: u64, steps: usize) {
    let mut case = new_case(seed).await;
    for step in 0..steps {
        let roll = case.rng.below(100);
        match roll {
            0..=24 => case.deposit_token(),
            25..=33 => case.deposit_native(),
            34..=63 => {
                let crash = if case.rng.chance(12) {
                    Some(if case.rng.chance(50) {
                        CrashPoint::AfterIntentCreated
                    } else {
                        CrashPoint::AfterSigned
                    })
                } else {
                    None
                };
                case.pass(crash).await;
            }
            64..=83 => {
                case.chain.lock().unwrap().mine();
                case.note("mine".into());
            }
            84..=88 => {
                let mult = 1 + case.rng.below(8) as u128;
                let mut c = case.chain.lock().unwrap();
                c.base_fee = 1_000_000_000 * mult;
                drop(c);
                case.note(format!("base fee x{mult}"));
            }
            89..=93 => {
                let n = 1 + case.rng.below(3) as u32;
                case.chain.lock().unwrap().fail_broadcasts = n;
                case.note(format!("next {n} broadcasts fail"));
            }
            94..=97 => case.retry_one(),
            _ => {
                case.sweeper = build_sweeper(&case.uri, &case.db, case.shape);
                case.note("restart".into());
            }
        }
        case.chain.lock().unwrap().step += 1;
        let _ = step;
        case.check();
    }
    case.settle_and_check_liveness().await;
}

/// 120 deterministic cases (about two minutes). `PROP_SEEDS=n` runs n instead; the
/// ignored test below runs 1000. `PROP_SEED=k` runs only case k; `PROP_TRACE=1` prints
/// every RPC call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sweep_sending_keeps_its_invariants_on_a_fake_chain() {
    crate::sweeper::FUNDING_SETTLE_MS.store(0, std::sync::atomic::Ordering::Relaxed);
    let n: u64 = std::env::var("PROP_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    if let Some(seed) = std::env::var("PROP_SEED").ok().and_then(|v| v.parse().ok()) {
        run_case(seed, 30).await;
        return;
    }
    for seed in 0..n {
        run_case(seed, 30).await;
    }
}

/// The long run: 1000 cases. `cargo test -- --ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "long: 1000 cases"]
async fn sweep_sending_keeps_its_invariants_on_a_fake_chain_long_run() {
    crate::sweeper::FUNDING_SETTLE_MS.store(0, std::sync::atomic::Ordering::Relaxed);
    for seed in 1000..2000 {
        run_case(seed, 40).await;
    }
}
