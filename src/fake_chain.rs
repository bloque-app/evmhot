//! A small fake EVM chain behind a JSON-RPC responder, for the sweeper's
//! property test. It enforces the rules that sweep sending depends on:
//!
//! - one transaction per (sender, nonce); a lower nonce is "nonce too low";
//! - a replacement at the same nonce needs both fee fields at least 1.1x the
//!   held one, and identical bytes are "already known" (unless evicted);
//! - a held transaction lands only when it is next in its sender's nonce order,
//!   its landing step has come, and its max fee covers the current base fee;
//!   it may also be evicted from the mempool, and a rebroadcast of the same
//!   bytes brings it back;
//! - native and token transfers from one address share that address's nonces;
//! - a token transfer larger than the balance fails `eth_estimateGas`, and
//!   reverts if it still gets mined;
//! - mined transactions give receipts (a step later, if the case says so) with
//!   block-level log indices, preceded by an unrelated log.
//!
//! Everything random comes from the case's seeded generator.

use alloy::consensus::TxEnvelope;
use alloy::eips::eip2718::Decodable2718;
use alloy::primitives::{keccak256, Address, TxKind, B256};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// splitmix64: tiny, seedable, deterministic.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in `0..n`.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

const TRANSFER_TOPIC: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
const OTHER_TOPIC: &str = "0x8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925";

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub enum MinedKind {
    Token {
        to: Address,
        amount: u128,
        success: bool,
    },
    Native {
        to: Address,
        value: u128,
        success: bool,
    },
}

#[derive(Clone, Debug)]
pub struct Mined {
    pub hash: String,
    pub from: Address,
    pub nonce: u64,
    pub kind: MinedKind,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
struct Held {
    raw: Vec<u8>,
    hash: B256,
    from: Address,
    nonce: u64,
    max_fee: u128,
    prio: u128,
    to: Address,
    value: u128,
    input: Vec<u8>,
    land_at_step: u64,
}

pub struct Chain {
    pub token: Address,
    pub faucet: Address,
    pub step: u64,
    pub block: u64,
    pub base_fee: u128,
    pub prio: u128,
    pub token_bal: HashMap<Address, u128>,
    pub native_bal: HashMap<Address, u128>,
    mined_nonce: HashMap<Address, u64>,
    mempool: Vec<Held>,
    evicted: Vec<Held>,
    receipts: HashMap<B256, (Value, u64)>,
    pub mined: Vec<Mined>,
    log_in_block: u64,
    /// Broadcasts to refuse; `accept_on_error` percent of them still enter the
    /// mempool (the answer was lost, the transaction was not).
    pub fail_broadcasts: u32,
    pub accept_on_error: u64,
    /// Percent chance a new transaction is delayed, evicted, or lands now.
    pub p_delay: u64,
    pub p_evict: u64,
    /// Whether receipts show up one step after mining.
    pub receipt_lag: bool,
    pub rng: Rng,
    pub broadcast_calls: u64,
}

impl Chain {
    pub fn new(token: Address, faucet: Address, seed: u64) -> Self {
        let mut native_bal = HashMap::new();
        native_bal.insert(faucet, 1_000_000_000_000_000_000_000u128);
        Self {
            token,
            faucet,
            step: 0,
            block: 100,
            base_fee: 1_000_000_000,
            prio: 1_000_000_000,
            token_bal: HashMap::new(),
            native_bal,
            mined_nonce: HashMap::new(),
            mempool: Vec::new(),
            evicted: Vec::new(),
            receipts: HashMap::new(),
            mined: Vec::new(),
            log_in_block: 0,
            fail_broadcasts: 0,
            accept_on_error: 50,
            p_delay: 30,
            p_evict: 15,
            receipt_lag: false,
            rng: Rng::new(seed),
            broadcast_calls: 0,
        }
    }

    pub fn latest_nonce(&self, a: Address) -> u64 {
        *self.mined_nonce.get(&a).unwrap_or(&0)
    }

    fn pending_nonce(&self, a: Address) -> u64 {
        let mut n = self.latest_nonce(a);
        for h in &self.mempool {
            if h.from == a && h.nonce >= n {
                n = h.nonce + 1;
            }
        }
        n
    }

    #[allow(dead_code)]
    /// Hashes of transactions currently held (not mined, not evicted).
    pub fn held_hashes(&self) -> Vec<String> {
        self.mempool
            .iter()
            .map(|h| h.hash.to_string().to_lowercase())
            .collect()
    }

    fn admit(&mut self, held: Held) {
        // Replace whatever is held at (from, nonce).
        self.mempool
            .retain(|h| !(h.from == held.from && h.nonce == held.nonce));
        self.evicted
            .retain(|h| !(h.from == held.from && h.nonce == held.nonce));
        let mut held = held;
        if held.from == self.faucet {
            held.land_at_step = 0; // the faucet's top-ups are mined at once
            self.mempool.push(held);
            self.mine_ready();
            return;
        }
        if self.rng.chance(self.p_evict) {
            self.evicted.push(held);
            return;
        }
        held.land_at_step = if self.rng.chance(self.p_delay) {
            self.step + 1 + self.rng.below(3)
        } else {
            self.step
        };
        self.mempool.push(held);
    }

    pub fn send_raw(&mut self, raw: &[u8]) -> Result<String, (i64, String)> {
        self.broadcast_calls += 1;
        let env = TxEnvelope::decode_2718(&mut &raw[..])
            .map_err(|e| (-32602, format!("invalid raw transaction: {e}")))?;
        let TxEnvelope::Eip1559(signed) = &env else {
            return Err((-32602, "only EIP-1559 transactions".into()));
        };
        let tx = signed.tx();
        let from = env
            .recover_signer()
            .map_err(|e| (-32602, format!("bad signature: {e}")))?;
        let hash = *signed.hash();
        let TxKind::Call(to) = tx.to else {
            return Err((-32602, "contract creation not supported".into()));
        };
        let held = Held {
            raw: raw.to_vec(),
            hash,
            from,
            nonce: tx.nonce,
            max_fee: tx.max_fee_per_gas,
            prio: tx.max_priority_fee_per_gas,
            to,
            value: u128::try_from(tx.value).unwrap_or(u128::MAX),
            input: tx.input.to_vec(),
            land_at_step: self.step,
        };

        let injected = self.fail_broadcasts > 0 && from != self.faucet;
        if injected {
            self.fail_broadcasts -= 1;
        }

        // An evicted transaction comes back on a rebroadcast of the same bytes.
        if let Some(pos) = self.evicted.iter().position(|h| h.hash == hash) {
            if injected {
                if self.rng.chance(self.accept_on_error) {
                    let h = self.evicted.remove(pos);
                    self.admit(h);
                }
                return Err((-32005, "rate limited".into()));
            }
            let h = self.evicted.remove(pos);
            self.admit(h);
            return Ok(hash.to_string());
        }
        if self.mempool.iter().any(|h| h.hash == hash)
            || self
                .mined
                .iter()
                .any(|m| m.hash.eq_ignore_ascii_case(&hash.to_string()))
        {
            return Err((-32000, "already known".into()));
        }
        if held.nonce < self.latest_nonce(from) {
            return Err((-32000, "nonce too low".into()));
        }
        if let Some(old) = self
            .mempool
            .iter()
            .chain(self.evicted.iter())
            .find(|h| h.from == from && h.nonce == held.nonce)
        {
            let enough = |new: u128, old: u128| new * 10 >= old * 11;
            if !(enough(held.max_fee, old.max_fee) && enough(held.prio, old.prio)) {
                return Err((-32000, "replacement transaction underpriced".into()));
            }
        }
        // Enough native balance for the value and the worst-case fee.
        let bal = *self.native_bal.get(&from).unwrap_or(&0);
        let is_token = to == self.token && !held.input.is_empty();
        let gas = if is_token { 150_000u128 } else { 21_000 };
        let need = held.value.saturating_add(gas * held.max_fee);
        if bal < need {
            return Err((-32000, "insufficient funds for gas * price + value".into()));
        }

        if injected {
            // The answer is lost; the node may or may not have the transaction.
            if self.rng.chance(self.accept_on_error) {
                self.admit(held);
            }
            return Err((-32005, "rate limited".into()));
        }
        self.admit(held);
        Ok(hash.to_string())
    }

    /// Lands whatever can land now (in nonce order per sender).
    fn mine_ready(&mut self) {
        loop {
            let mut landed = false;
            let mut i = 0;
            while i < self.mempool.len() {
                let h = &self.mempool[i];
                if h.nonce == self.latest_nonce(h.from)
                    && h.land_at_step <= self.step
                    && h.max_fee >= self.base_fee
                {
                    let h = self.mempool.remove(i);
                    self.execute(h);
                    landed = true;
                } else {
                    i += 1;
                }
            }
            if !landed {
                break;
            }
        }
    }

    /// One block: some held transactions may be evicted, the rest land if ready.
    pub fn mine(&mut self) {
        self.block += 1;
        self.log_in_block = 0;
        let mut i = 0;
        while i < self.mempool.len() {
            if self.mempool[i].from != self.faucet && self.rng.chance(self.p_evict / 3) {
                let h = self.mempool.remove(i);
                self.evicted.push(h);
            } else {
                i += 1;
            }
        }
        self.mine_ready();
    }

    fn execute(&mut self, h: Held) {
        *self.mined_nonce.entry(h.from).or_insert(0) += 1;
        let is_token = h.to == self.token && h.input.len() == 4 + 64;
        let mut logs = Vec::new();
        let (kind, success, gas) = if is_token {
            let to = Address::from_slice(&h.input[4 + 12..4 + 32]);
            let amount = u128::from_be_bytes(h.input[4 + 32 + 16..4 + 64].try_into().unwrap());
            let bal = *self.token_bal.get(&h.from).unwrap_or(&0);
            let ok = bal >= amount;
            if ok {
                *self.token_bal.entry(h.from).or_insert(0) -= amount;
                *self.token_bal.entry(to).or_insert(0) += amount;
                // An unrelated log first, so block-level and transaction-level
                // positions differ.
                logs.push(self.log_json(self.token, OTHER_TOPIC, h.from, h.from, 0, &h));
                logs.push(self.log_json(self.token, TRANSFER_TOPIC, h.from, to, amount, &h));
            }
            (
                MinedKind::Token {
                    to,
                    amount,
                    success: ok,
                },
                ok,
                100_000u128,
            )
        } else {
            let bal = *self.native_bal.get(&h.from).unwrap_or(&0);
            let ok = bal >= h.value;
            if ok {
                *self.native_bal.entry(h.from).or_insert(0) -= h.value;
                *self.native_bal.entry(h.to).or_insert(0) += h.value;
            }
            (
                MinedKind::Native {
                    to: h.to,
                    value: h.value,
                    success: ok,
                },
                ok,
                21_000u128,
            )
        };
        let fee = gas * (self.base_fee + h.prio.min(h.max_fee.saturating_sub(self.base_fee)));
        let b = self.native_bal.entry(h.from).or_insert(0);
        *b = b.saturating_sub(fee);
        let hash_s = h.hash.to_string();
        let receipt = crate::test_support::receipt_json(
            &hash_s,
            self.block,
            self.mined.len() as u64 % 7,
            &h.from.to_string(),
            &h.to.to_string(),
            success,
            logs,
        );
        let visible = if self.receipt_lag {
            self.step + 1
        } else {
            self.step
        };
        self.receipts.insert(h.hash, (receipt, visible));
        self.mined.push(Mined {
            hash: hash_s,
            from: h.from,
            nonce: h.nonce,
            kind,
        });
    }

    fn log_json(
        &mut self,
        token: Address,
        topic: &str,
        from: Address,
        to: Address,
        amount: u128,
        h: &Held,
    ) -> Value {
        let idx = self.log_in_block;
        self.log_in_block += 1;
        let mut log = crate::test_support::transfer_log_json(
            &token.to_string(),
            &from.to_string(),
            &to.to_string(),
            amount,
            idx,
            &h.hash.to_string(),
            self.block,
            0,
        );
        log["topics"][0] = json!(topic);
        log
    }

    fn receipt(&self, hash: B256) -> Value {
        match self.receipts.get(&hash) {
            Some((r, visible)) if *visible <= self.step => r.clone(),
            _ => Value::Null,
        }
    }

    pub fn handle(&mut self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        let addr = |v: &Value| -> Result<Address, (i64, String)> {
            Address::from_str(v.as_str().unwrap_or("")).map_err(|e| (-32602, e.to_string()))
        };
        match method {
            "eth_chainId" => Ok(json!("0x89")),
            "eth_blockNumber" => Ok(json!(format!("0x{:x}", self.block))),
            "eth_feeHistory" => Ok(json!({
                "baseFeePerGas": [format!("0x{:x}", self.base_fee), format!("0x{:x}", self.base_fee)],
                "gasUsedRatio": [0.5],
                "oldestBlock": "0x9",
                "reward": [[format!("0x{:x}", self.prio)]]
            })),
            "eth_getBalance" => {
                let a = addr(&params[0])?;
                Ok(json!(format!(
                    "0x{:x}",
                    self.native_bal.get(&a).unwrap_or(&0)
                )))
            }
            "eth_getTransactionCount" => {
                let a = addr(&params[0])?;
                let n = if params[1].as_str() == Some("pending") {
                    self.pending_nonce(a)
                } else {
                    self.latest_nonce(a)
                };
                Ok(json!(format!("0x{n:x}")))
            }
            "eth_call" => {
                let data = params[0]["data"]
                    .as_str()
                    .or_else(|| params[0]["input"].as_str())
                    .unwrap_or("");
                if data.starts_with("0x70a08231") && data.len() >= 10 + 64 {
                    let a = Address::from_str(&format!("0x{}", &data[10 + 24..10 + 64]))
                        .map_err(|e| (-32602, e.to_string()))?;
                    let bal = *self.token_bal.get(&a).unwrap_or(&0);
                    return Ok(json!(format!("0x{bal:064x}")));
                }
                Err((3, "execution reverted".into()))
            }
            "eth_estimateGas" => {
                let input = params[0]["input"]
                    .as_str()
                    .or_else(|| params[0]["data"].as_str())
                    .unwrap_or("0x");
                if input.starts_with("0xa9059cbb") && input.len() == 2 + 8 + 128 {
                    let from = addr(&params[0]["from"])?;
                    let amount =
                        u128::from_str_radix(&input[2 + 8 + 64 + 32..], 16).unwrap_or(u128::MAX);
                    if *self.token_bal.get(&from).unwrap_or(&0) < amount {
                        return Err((3, "execution reverted".into()));
                    }
                    return Ok(json!("0x186a0"));
                }
                Ok(json!("0x5208"))
            }
            "eth_sendRawTransaction" => {
                let raw = alloy::primitives::hex::decode(params[0].as_str().unwrap_or(""))
                    .map_err(|e| (-32602, e.to_string()))?;
                self.send_raw(&raw).map(Value::from)
            }
            "eth_getTransactionReceipt" => {
                let h = B256::from_str(params[0].as_str().unwrap_or(""))
                    .map_err(|e| (-32602, e.to_string()))?;
                Ok(self.receipt(h))
            }
            other => Err((-32601, format!("method not found: {other}"))),
        }
    }
}

pub type SharedChain = Arc<Mutex<Chain>>;

/// Serves `chain` over HTTP JSON-RPC. One responder, dispatching on the method.
pub async fn serve(chain: SharedChain) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::method("POST"))
        .respond_with(move |req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let respond = |item: &Value| -> Value {
                let method = item["method"].as_str().unwrap_or("");
                let result = chain.lock().unwrap().handle(method, &item["params"]);
                match result {
                    Ok(v) => json!({"jsonrpc": "2.0", "id": item["id"], "result": v}),
                    Err((code, message)) => json!({
                        "jsonrpc": "2.0", "id": item["id"],
                        "error": {"code": code, "message": message}
                    }),
                }
            };
            let out = match &body {
                Value::Array(items) => Value::Array(items.iter().map(respond).collect()),
                single => respond(single),
            };
            ResponseTemplate::new(200).set_body_json(out)
        })
        .mount(&server)
        .await;
    server
}

/// Keccak of a label, for deterministic fixture hashes.
#[allow(dead_code)]
pub fn label_hash(label: &str) -> String {
    keccak256(label.as_bytes()).to_string()
}
