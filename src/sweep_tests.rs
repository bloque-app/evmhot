//! F-002a tests: receipt check, sweep coordinates, senders, sweep targets,
//! atomic outbox, derivation ceiling and the symbol filter's withdrawal.
//!
//! Each test is named for the requirement it verifies. The RPC node is a
//! wiremock server; addresses are placeholders and the mnemonic is the
//! well-known development one.

use crate::config::{parse_chains_toml, plan_sweep_parts, SweepTarget, TargetKind};
use crate::db::{Db, NewErc20Deposit, NewNativeDeposit, OutboxEntry, SweepRecord};
use crate::faucet::Faucet;
use crate::monitor::Monitor;
use crate::sweeper::Sweeper;
use crate::test_support::{
    self, hash32, http_provider_boxed, receipt_json, test_config, test_webhook_deliverer,
    transfer_log_json, TEST_CHAIN, TEST_MNEMONIC, TEST_TREASURY,
};
use crate::tests::{body_json_contains, empty_block_rpc_response, native_tx_block_response};
use crate::wallet::Wallet;
use alloy::primitives::U256;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::NamedTempFile;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Placeholder token contracts (arbitrary addresses, not tied to any deployment).
const TOKEN: &str = "0x00000000000000000000000000000000000c0de1";
/// A second placeholder token.
const OTHER_TOKEN: &str = "0x00000000000000000000000000000000000c0de2";
/// Placeholder sweep targets.
const TARGET_A: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
/// Placeholder senders.
const SENDER_A: &str = "0x00000000000000000000000000000000000a11ce";
const SENDER_B: &str = "0x0000000000000000000000000000000000000b0b";
const UNREACHABLE_WEBHOOK: &str = "http://127.0.0.1:9/hook";

fn checksum(address: &str) -> String {
    use std::str::FromStr;
    alloy::primitives::Address::from_str(address)
        .unwrap()
        .to_string()
}

fn sweep_tx(n: u64) -> String {
    hash32(0xf000 + n)
}

/// A sweep receipt for `amount` tokens moved from `from` to `to`, with the
/// matching `Transfer` at block-level `log_index`.
fn ok_receipt(
    tx: &str,
    block: u64,
    log_index: u64,
    token: &str,
    from: &str,
    to: &str,
    amount: u128,
) -> Value {
    receipt_json(
        tx,
        block,
        2,
        from,
        token,
        true,
        vec![transfer_log_json(
            token, from, to, amount, log_index, tx, block, 2,
        )],
    )
}

struct Rpc {
    server: MockServer,
    /// What `balanceOf` answers, changeable between passes.
    balance: Arc<Mutex<u128>>,
    /// The address's latest (mined) nonce.
    latest_nonce: Arc<Mutex<u64>>,
    /// Real hashes of the distinct transactions the node accepted, in order.
    accepted: Arc<Mutex<Vec<String>>>,
    /// Broadcasts still to refuse (a failing public RPC).
    fail_sends: Arc<AtomicUsize>,
    /// Every `eth_sendRawTransaction` call, accepted or not.
    send_calls: Arc<AtomicUsize>,
}

impl Rpc {
    /// Standard node mocks for sweeping. Sweeps are signed locally, so their
    /// hashes are real: the node learns each distinct raw transaction's hash on
    /// first broadcast and maps it to the next name in `sends`; `receipts` are
    /// keyed by those names (or by a literal hash) and answered with the real
    /// hash. `hash(i)` is the real hash of the i-th accepted transaction.
    async fn start(balance: u128, sends: Vec<String>, receipts: Vec<(String, Value)>) -> Self {
        let server = MockServer::start().await;
        let balance = Arc::new(Mutex::new(balance));
        let latest_nonce = Arc::new(Mutex::new(0u64));
        let accepted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let fail_sends = Arc::new(AtomicUsize::new(0));
        let send_calls = Arc::new(AtomicUsize::new(0));
        let learned: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));

        let bal = Arc::clone(&balance);
        Mock::given(method("POST"))
            .and(body_json_contains("eth_call"))
            .respond_with(move |_: &wiremock::Request| {
                let value = *bal.lock().unwrap();
                ResponseTemplate::new(200).set_body_json(json!({
                    "jsonrpc": "2.0", "id": 1, "result": format!("0x{value:064x}")
                }))
            })
            .mount(&server)
            .await;
        for (needle, result) in [
            ("eth_estimateGas", json!("0x186a0")),
            ("eth_getBalance", json!("0x0DE0B6B3A7640000")),
            ("eth_chainId", json!("0x89")),
        ] {
            Mock::given(method("POST"))
                .and(body_json_contains(needle))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "jsonrpc": "2.0", "id": 1, "result": result
                })))
                .mount(&server)
                .await;
        }
        // The pending nonce counts the transactions the node accepted; the latest
        // (mined) nonce is settable.
        let acc_for_nonce = Arc::clone(&accepted);
        Mock::given(method("POST"))
            .and(body_json_contains("eth_getTransactionCount"))
            .and(body_json_contains("pending"))
            .respond_with(move |_: &wiremock::Request| {
                let n = acc_for_nonce.lock().unwrap().len();
                ResponseTemplate::new(200).set_body_json(json!({
                    "jsonrpc": "2.0", "id": 1, "result": format!("0x{n:x}")
                }))
            })
            .mount(&server)
            .await;
        let latest = Arc::clone(&latest_nonce);
        Mock::given(method("POST"))
            .and(body_json_contains("eth_getTransactionCount"))
            .respond_with(move |_: &wiremock::Request| {
                let n = *latest.lock().unwrap();
                ResponseTemplate::new(200).set_body_json(json!({
                    "jsonrpc": "2.0", "id": 1, "result": format!("0x{n:x}")
                }))
            })
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_json_contains("eth_feeHistory"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1,
                "result": {
                    "baseFeePerGas": ["0x3B9ACA00", "0x3B9ACA00"],
                    "gasUsedRatio": [0.5],
                    "oldestBlock": "0x9",
                    "reward": [["0x3B9ACA00"]]
                }
            })))
            .mount(&server)
            .await;

        let (acc, fails, calls, learn) = (
            Arc::clone(&accepted),
            Arc::clone(&fail_sends),
            Arc::clone(&send_calls),
            Arc::clone(&learned),
        );
        Mock::given(method("POST"))
            .and(body_json_contains("eth_sendRawTransaction"))
            .respond_with(move |req: &wiremock::Request| {
                calls.fetch_add(1, Ordering::SeqCst);
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                let raw = body["params"][0].as_str().unwrap();
                let bytes = alloy::primitives::hex::decode(raw).unwrap();
                let real = alloy::primitives::keccak256(&bytes)
                    .to_string()
                    .to_lowercase();
                let mut learned = learn.lock().unwrap();
                if !learned.contains_key(&real) {
                    if fails
                        .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                        .is_ok()
                    {
                        return ResponseTemplate::new(200).set_body_json(json!({
                            "jsonrpc": "2.0", "id": body["id"],
                            "error": {"code": -32005, "message": "rate limited"}
                        }));
                    }
                    let mut acc = acc.lock().unwrap();
                    let name = sends[acc.len().min(sends.len() - 1)].clone();
                    learned.insert(real.clone(), name);
                    acc.push(real.clone());
                }
                ResponseTemplate::new(200)
                    .set_body_json(json!({"jsonrpc": "2.0", "id": body["id"], "result": real}))
            })
            .mount(&server)
            .await;

        let by_name: HashMap<String, Value> = receipts
            .into_iter()
            .map(|(hash, receipt)| (hash.to_lowercase(), receipt))
            .collect();
        let learn = Arc::clone(&learned);
        Mock::given(method("POST"))
            .and(body_json_contains("eth_getTransactionReceipt"))
            .respond_with(move |req: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                let wanted = body["params"][0].as_str().unwrap_or("").to_lowercase();
                let name = learn
                    .lock()
                    .unwrap()
                    .get(&wanted)
                    .cloned()
                    .unwrap_or_else(|| wanted.clone())
                    .to_lowercase();
                let result = by_name
                    .get(&name)
                    .cloned()
                    .map(|mut r| {
                        r["transactionHash"] = json!(wanted);
                        r
                    })
                    .unwrap_or(Value::Null);
                ResponseTemplate::new(200)
                    .set_body_json(json!({"jsonrpc": "2.0", "id": body["id"], "result": result}))
            })
            .mount(&server)
            .await;

        Self {
            server,
            balance,
            latest_nonce,
            accepted,
            fail_sends,
            send_calls,
        }
    }

    fn set_balance(&self, value: u128) {
        *self.balance.lock().unwrap() = value;
    }

    fn set_latest_nonce(&self, value: u64) {
        *self.latest_nonce.lock().unwrap() = value;
    }

    fn fail_next_sends(&self, n: usize) {
        self.fail_sends.store(n, Ordering::SeqCst);
    }

    /// Distinct transactions the node accepted.
    fn sent(&self) -> usize {
        self.accepted.lock().unwrap().len()
    }

    /// Every broadcast call, accepted or refused.
    fn send_calls(&self) -> usize {
        self.send_calls.load(Ordering::SeqCst)
    }

    /// Real hash of the i-th accepted transaction.
    fn hash(&self, i: usize) -> String {
        self.accepted.lock().unwrap()[i].clone()
    }

    /// Calldata of every `eth_estimateGas` request (one per sweep part).
    async fn estimate_bodies(&self) -> Vec<String> {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).to_lowercase())
            .filter(|b| b.contains("eth_estimategas"))
            .collect()
    }
}

struct Fixture {
    _db_file: NamedTempFile,
    db: Db,
    /// Deposit address of registration `user_1`.
    addr: String,
}

fn fixture() -> Fixture {
    let db_file = NamedTempFile::new().unwrap();
    let db = Db::new(db_file.path().to_str().unwrap()).unwrap();
    let wallet = Wallet::new(TEST_MNEMONIC.to_string());
    let addr = wallet.derive_address(0).unwrap().to_string();
    db.register_account("user_1", 0, &addr, UNREACHABLE_WEBHOOK)
        .unwrap();
    // The monitor looks metadata up by the checksummed address.
    db.store_token_metadata(TEST_CHAIN, &checksum(TOKEN), "USDT", 6, "Test token")
        .unwrap();
    Fixture {
        _db_file: db_file,
        db,
        addr,
    }
}

impl Fixture {
    fn token_deposit(&self, tx: &str, log_index: u64, amount: &str, from: Option<&str>) {
        self.token_deposit_of(TOKEN, tx, log_index, amount, from, "USDT");
    }

    fn token_deposit_of(
        &self,
        token: &str,
        tx: &str,
        log_index: u64,
        amount: &str,
        from: Option<&str>,
        symbol: &str,
    ) {
        let is_new = self
            .db
            .record_erc20_deposit_atomic(
                &NewErc20Deposit {
                    chain: TEST_CHAIN.into(),
                    tx_hash: tx.into(),
                    log_index,
                    account_id: "user_1".into(),
                    amount: amount.into(),
                    token_address: token.into(),
                    token_symbol: symbol.into(),
                    from_address: from.map(String::from),
                },
                None,
            )
            .unwrap();
        assert!(is_new);
    }

    fn sweeper(
        &self,
        rpc: &Rpc,
        configure: impl FnOnce(&mut crate::config::ChainConfig),
    ) -> Sweeper {
        let mut config = test_config("unused", rpc.server.uri());
        configure(&mut config.chains[0]);
        let provider = http_provider_boxed(&rpc.server.uri());
        let faucet = Arc::new(
            Faucet::new(
                config.faucet_mnemonic.clone(),
                provider.clone(),
                &test_support::chain_existential(&config),
            )
            .unwrap(),
        );
        Sweeper::new(
            config.chains[0].clone(),
            test_webhook_deliverer(self.db.clone()),
            self.db.clone(),
            Wallet::new(TEST_MNEMONIC.to_string()),
            provider,
            faucet,
        )
    }

    fn swept_webhook(&self, deposit_key: &str) -> Option<Value> {
        self.db
            .get_webhook_delivery(&format!("{TEST_CHAIN}:{deposit_key}"), "deposit_swept")
            .unwrap()
            .map(|row| serde_json::from_str(&row.payload).unwrap())
    }

    fn detected(&self) -> usize {
        self.db
            .get_detected_erc20_deposits(TEST_CHAIN)
            .unwrap()
            .len()
    }
}

fn target_for(
    token: &str,
    build: impl FnOnce(&mut SweepTarget),
) -> impl FnOnce(&mut crate::config::ChainConfig) {
    let token = token.to_lowercase();
    move |chain| {
        let mut target = SweepTarget {
            kind: TargetKind::Transfer,
            address: Some(TARGET_A.to_string()),
            min: None,
            max: None,
            admits_only: false,
            per_sender: false,
            per_transfer: false,
        };
        build(&mut target);
        chain.token_targets.insert(token, target);
    }
}

// ---------------------------------------------------------------- REQ-SW-5

#[tokio::test]
async fn req_sw_5_reverted_sweep_is_not_marked_swept() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "1000", Some(SENDER_A));
    let tx = sweep_tx(1);
    let rpc = Rpc::start(
        1000,
        vec![tx.clone()],
        vec![(
            tx.clone(),
            receipt_json(&tx, 11, 2, &fx.addr, TOKEN, false, vec![]),
        )],
    )
    .await;
    let sweeper = fx.sweeper(&rpc, |_| {});

    sweeper.process_deposits_once().await.unwrap();

    // Not swept, no webhook, no sweep recorded, and the failure is counted.
    assert_eq!(fx.detected(), 1);
    assert!(fx.swept_webhook("0xaaa:1").is_none());
    assert!(fx.db.get_sweeps(TEST_CHAIN, "0xaaa:1").unwrap().is_empty());
    assert_eq!(
        fx.db
            .get_sweep_failure_count(TEST_CHAIN, "0xaaa:1")
            .unwrap(),
        1
    );

    // REQ-SW-4: five consecutive failures fail the deposit; still no webhook.
    for _ in 0..4 {
        sweeper.process_deposits_once().await.unwrap();
    }
    assert_eq!(fx.detected(), 0);
    assert_eq!(
        fx.db.deposit_queue_counts(TEST_CHAIN).unwrap().erc20_failed,
        1
    );
    assert!(fx.swept_webhook("0xaaa:1").is_none());
}

#[tokio::test]
async fn req_sw_5_reverted_native_sweep_is_not_marked_swept() {
    let fx = fixture();
    fx.db
        .record_native_deposit_atomic(
            &NewNativeDeposit {
                chain: TEST_CHAIN.into(),
                tx_hash: "0xnative".into(),
                account_id: "user_1".into(),
                amount: "1000000000000000000".into(),
                from_address: Some(SENDER_A.into()),
            },
            None,
        )
        .unwrap();
    let tx = sweep_tx(2);
    let rpc = Rpc::start(
        0,
        vec![tx.clone()],
        vec![(
            tx.clone(),
            receipt_json(&tx, 11, 0, &fx.addr, TEST_TREASURY, false, vec![]),
        )],
    )
    .await;
    let sweeper = fx.sweeper(&rpc, |_| {});

    sweeper.process_deposits_once().await.unwrap();

    assert_eq!(rpc.sent(), 1);
    assert_eq!(fx.db.get_detected_deposits(TEST_CHAIN).unwrap().len(), 1);
    assert!(fx
        .db
        .get_webhook_delivery(&format!("{TEST_CHAIN}:0xnative"), "deposit_swept")
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn req_sw_5_successful_native_sweep_sends_swept_webhook_with_from_address() {
    let fx = fixture();
    fx.db
        .record_native_deposit_atomic(
            &NewNativeDeposit {
                chain: TEST_CHAIN.into(),
                tx_hash: "0xnative".into(),
                account_id: "user_1".into(),
                amount: "1000000000000000000".into(),
                from_address: Some(SENDER_A.into()),
            },
            None,
        )
        .unwrap();
    let tx = sweep_tx(3);
    let rpc = Rpc::start(
        0,
        vec![tx.clone()],
        vec![(
            tx.clone(),
            receipt_json(&tx, 11, 0, &fx.addr, TEST_TREASURY, true, vec![]),
        )],
    )
    .await;
    fx.sweeper(&rpc, |_| {})
        .process_deposits_once()
        .await
        .unwrap();

    assert!(fx.db.get_detected_deposits(TEST_CHAIN).unwrap().is_empty());
    let row = fx
        .db
        .get_webhook_delivery(&format!("{TEST_CHAIN}:0xnative"), "deposit_swept")
        .unwrap()
        .expect("deposit_swept is in the outbox");
    let payload: Value = serde_json::from_str(&row.payload).unwrap();
    assert_eq!(payload["token_type"], "native");
    // REQ-WH-9, and REQ-WH-4: a native event carries no sweep fields.
    assert_eq!(payload["from_address"], SENDER_A);
    assert!(payload.get("sweeps").is_none());
    assert!(payload.get("sweep_tx_hash").is_none());
}

// --------------------------------------------------------------- REQ-SW-10

#[tokio::test]
async fn req_sw_10_coordinate_comes_from_receipt_log_index_not_tx_position() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "1000", Some(SENDER_A));
    let tx = sweep_tx(4);
    // Transaction index 2 in block 11; the Transfer into the target is log 17
    // of the block. An unrelated token's log and a same-token transfer to some
    // other address come first and must be ignored.
    let receipt = receipt_json(
        &tx,
        11,
        2,
        &fx.addr,
        TOKEN,
        true,
        vec![
            transfer_log_json(OTHER_TOKEN, &fx.addr, TARGET_A, 1000, 15, &tx, 11, 2),
            transfer_log_json(TOKEN, &fx.addr, SENDER_B, 1000, 16, &tx, 11, 2),
            transfer_log_json(TOKEN, &fx.addr, TARGET_A, 1000, 17, &tx, 11, 2),
        ],
    );
    let rpc = Rpc::start(1000, vec![tx.clone()], vec![(tx.clone(), receipt)]).await;
    let sweeper = fx.sweeper(&rpc, target_for(TOKEN, |_| {}));

    sweeper.process_deposits_once().await.unwrap();

    assert_eq!(fx.detected(), 0);
    let stored = fx.db.get_sweeps(TEST_CHAIN, "0xaaa:1").unwrap();
    assert_eq!(
        stored,
        vec![SweepRecord {
            sweep_tx_hash: rpc.hash(0),
            sweep_block_number: 11,
            sweep_block_hash: hash32(11),
            sweep_log_index: 17,
            sweep_amount: "1000".into(),
            sweep_target: Some(checksum(TARGET_A)),
        }]
    );
    let payload = fx.swept_webhook("0xaaa:1").expect("deposit_swept enqueued");
    assert_eq!(payload["sweep_tx_hash"], rpc.hash(0));
    assert_eq!(payload["sweeps"].as_array().unwrap().len(), 1);
    assert_eq!(payload["sweeps"][0]["sweep_tx_hash"], rpc.hash(0));
    assert_eq!(payload["sweeps"][0]["sweep_block_number"], 11);
    assert_eq!(payload["sweeps"][0]["sweep_block_hash"], hash32(11));
    assert_eq!(payload["sweeps"][0]["sweep_log_index"], 17);
    assert_eq!(payload["sweeps"][0]["sweep_amount"], "1000");
    assert_eq!(payload["sweeps"][0]["sweep_target"], checksum(TARGET_A));
    assert_eq!(payload["original_tx_hash"], "0xaaa");
    assert_eq!(payload["from_address"], SENDER_A);
}

#[tokio::test]
async fn req_sw_10_receipt_without_a_transfer_into_the_target_goes_to_review_with_an_alert() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "1000", Some(SENDER_A));
    let tx = sweep_tx(5);
    let rpc = Rpc::start(
        1000,
        vec![tx.clone()],
        vec![(
            tx.clone(),
            receipt_json(&tx, 11, 2, &fx.addr, TOKEN, true, vec![]),
        )],
    )
    .await;
    let sweeper = fx.sweeper(&rpc, |_| {});

    sweeper.process_deposits_once().await.unwrap();

    // Not settled, not retried, no webhook: an operator looks.
    assert_eq!(fx.detected(), 0);
    assert_eq!(
        fx.db
            .get_review_kind(TEST_CHAIN, "0xaaa:1")
            .unwrap()
            .as_deref(),
        Some("sweep_review")
    );
    assert!(fx.swept_webhook("0xaaa:1").is_none());
    assert_eq!(
        fx.db
            .deposit_queue_counts(TEST_CHAIN)
            .unwrap()
            .erc20_needs_review,
        1
    );
    sweeper.process_deposits_once().await.unwrap();
    assert_eq!(rpc.sent(), 1, "a deposit in review is never swept again");
}

#[tokio::test]
async fn req_sw_10_matched_transfer_with_a_different_value_goes_to_review() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "1000", Some(SENDER_A));
    let tx = sweep_tx(6);
    // The Transfer into the target moved 900, not the planned 1000.
    let rpc = Rpc::start(
        1000,
        vec![tx.clone()],
        vec![(
            tx.clone(),
            ok_receipt(&tx, 11, 3, TOKEN, &fx.addr, TARGET_A, 900),
        )],
    )
    .await;
    let sweeper = fx.sweeper(&rpc, target_for(TOKEN, |_| {}));
    sweeper.process_deposits_once().await.unwrap();
    assert_eq!(fx.detected(), 0);
    assert_eq!(
        fx.db
            .get_review_kind(TEST_CHAIN, "0xaaa:1")
            .unwrap()
            .as_deref(),
        Some("sweep_review")
    );
    assert!(fx.swept_webhook("0xaaa:1").is_none());
    assert!(fx.db.get_sweeps(TEST_CHAIN, "0xaaa:1").unwrap().is_empty());
}

#[tokio::test]
async fn req_sw_10_old_rows_without_coordinates_stay_valid() {
    // A deposit recorded by the old calls (no sender, no sweeps) still reads.
    let fx = fixture();
    fx.db
        .record_erc20_deposit(TEST_CHAIN, "0xold", 3, "user_1", "5", TOKEN, "USDT")
        .unwrap();
    let deposits = fx.db.get_detected_erc20_deposits(TEST_CHAIN).unwrap();
    assert_eq!(deposits.len(), 1);
    assert_eq!(deposits[0].from_address, None);
    assert!(fx.db.get_sweeps(TEST_CHAIN, "0xold:3").unwrap().is_empty());
}

// ---------------------------------------------------------------- REQ-WH-9

fn log_block_rpc_mocks_native_and_token(
    to_addr: &str,
    native_tx: &str,
    token_tx: &str,
) -> (Value, Value) {
    let block = native_tx_block_response(10, to_addr, native_tx);
    let logs = json!({
        "jsonrpc": "2.0", "id": 1,
        "result": [transfer_log_json(TOKEN, SENDER_A, to_addr, 2_000_000, 7, token_tx, 10, 1)]
    });
    (block, logs)
}

async fn monitor_rpc(block: Value, logs: Value, token_calls_fail: bool) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_json_contains("eth_blockNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0xA"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getBlockByNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(block))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getLogs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(logs))
        .mount(&server)
        .await;
    if token_calls_fail {
        Mock::given(method("POST"))
            .and(body_json_contains("eth_call"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1,
                "error": {"code": 3, "message": "execution reverted"}
            })))
            .mount(&server)
            .await;
    }
    server
}

fn monitor_for(fx: &Fixture, server: &MockServer) -> Monitor {
    let mut config = test_config("unused", server.uri());
    config.chains[0].get_logs_max_retries = 1;
    config.chains[0].get_logs_delay_ms = 1;
    Monitor::new(
        config.chains[0].clone(),
        test_webhook_deliverer(fx.db.clone()),
        fx.db.clone(),
        http_provider_boxed(&server.uri()),
    )
}

/// Addresses the monitor derives are checksummed; compare ignoring case.
fn assert_addr(actual: &Value, expected: &str) {
    assert!(
        actual
            .as_str()
            .is_some_and(|a| a.eq_ignore_ascii_case(expected)),
        "{actual} != {expected}"
    );
}

#[tokio::test]
async fn req_wh_9_from_address_is_stored_and_sent_in_both_events() {
    let fx = fixture();
    let (block, logs) =
        log_block_rpc_mocks_native_and_token(&fx.addr, &hash32(0xa1), &hash32(0xa2));
    let server = monitor_rpc(block, logs, false).await;
    monitor_for(&fx, &server).catch_up_once().await.unwrap();

    // deposit_detected, token: sender is the Transfer's `from`.
    let token_id = format!("{TEST_CHAIN}:{}:7", hash32(0xa2));
    let row = fx
        .db
        .get_webhook_delivery(&token_id, "deposit_detected")
        .unwrap()
        .expect("token deposit_detected is in the outbox");
    let detected: Value = serde_json::from_str(&row.payload).unwrap();
    assert_addr(&detected["from_address"], SENDER_A);
    assert_eq!(detected["token_type"], "erc20");

    // deposit_detected, native: sender is the transaction's `from`.
    let native_id = format!("{TEST_CHAIN}:{}", hash32(0xa1));
    let row = fx
        .db
        .get_webhook_delivery(&native_id, "deposit_detected")
        .unwrap()
        .expect("native deposit_detected is in the outbox");
    let detected: Value = serde_json::from_str(&row.payload).unwrap();
    assert_addr(
        &detected["from_address"],
        "0x0000000000000000000000000000000000000001",
    );
    assert_eq!(
        fx.db
            .get_deposit_from_address(TEST_CHAIN, &hash32(0xa1))
            .unwrap()
            .as_deref(),
        Some("0x0000000000000000000000000000000000000001")
    );

    // deposit_swept, token: the stored sender comes back.
    let key = format!("{}:7", hash32(0xa2));
    let tx = sweep_tx(6);
    let rpc = Rpc::start(
        2_000_000,
        vec![tx.clone()],
        vec![(
            tx.clone(),
            ok_receipt(&tx, 11, 4, TOKEN, &fx.addr, TEST_TREASURY, 2_000_000),
        )],
    )
    .await;
    fx.sweeper(&rpc, |_| {})
        .process_deposits_once()
        .await
        .unwrap();
    let swept = fx.swept_webhook(&key).expect("token deposit_swept");
    assert_addr(&swept["from_address"], SENDER_A);
}

// ---------------------------------------------------------------- REQ-SW-6

#[tokio::test]
async fn req_sw_6_hold_detects_and_notifies_but_never_sweeps_then_releases() {
    let fx = fixture();
    let (block, logs) = (
        empty_block_rpc_response(),
        json!({
            "jsonrpc": "2.0", "id": 1,
            "result": [transfer_log_json(TOKEN, SENDER_A, &fx.addr, 900, 3, &hash32(0xb1), 10, 0)]
        }),
    );
    let server = monitor_rpc(block, logs, false).await;
    monitor_for(&fx, &server).catch_up_once().await.unwrap();

    // Detected and notified.
    assert_eq!(fx.detected(), 1);
    let id = format!("{TEST_CHAIN}:{}:3", hash32(0xb1));
    assert!(fx
        .db
        .get_webhook_delivery(&id, "deposit_detected")
        .unwrap()
        .is_some());

    // Held: any number of passes, nothing is sent, nothing is counted.
    let key = format!("{}:3", hash32(0xb1));
    let tx = sweep_tx(7);
    let rpc = Rpc::start(
        900,
        vec![tx.clone()],
        vec![(
            tx.clone(),
            ok_receipt(&tx, 11, 5, TOKEN, &fx.addr, TARGET_A, 900),
        )],
    )
    .await;
    let hold = |chain: &mut crate::config::ChainConfig| {
        chain.token_targets.insert(
            TOKEN.to_lowercase(),
            SweepTarget {
                kind: TargetKind::Hold,
                address: None,
                min: None,
                max: None,
                admits_only: false,
                per_sender: false,
                per_transfer: false,
            },
        );
    };
    let held = fx.sweeper(&rpc, hold);
    for _ in 0..3 {
        held.process_deposits_once().await.unwrap();
    }
    assert_eq!(rpc.sent(), 0);
    assert_eq!(fx.detected(), 1);
    assert_eq!(fx.db.get_sweep_failure_count(TEST_CHAIN, &key).unwrap(), 0);
    assert!(fx.swept_webhook(&key).is_none());

    // The kind changes: the held deposit is swept on the next pass.
    let released = fx.sweeper(&rpc, target_for(TOKEN, |_| {}));
    released.process_deposits_once().await.unwrap();
    assert_eq!(rpc.sent(), 1);
    assert_eq!(fx.detected(), 0);
    let payload = fx.swept_webhook(&key).expect("released deposit is swept");
    assert_eq!(payload["sweeps"][0]["sweep_amount"], "900");
}

// ---------------------------------------------------------------- REQ-SW-7

#[tokio::test]
async fn req_sw_7_below_minimum_waits_until_later_deposits_reach_it() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "400", Some(SENDER_A));
    let tx = sweep_tx(8);
    let rpc = Rpc::start(
        400,
        vec![tx.clone()],
        vec![(
            tx.clone(),
            ok_receipt(&tx, 11, 1, TOKEN, &fx.addr, TARGET_A, 1100),
        )],
    )
    .await;
    let sweeper = fx.sweeper(
        &rpc,
        target_for(TOKEN, |t| t.min = Some(U256::from(1000u64))),
    );

    sweeper.process_deposits_once().await.unwrap();
    assert_eq!(rpc.sent(), 0, "below the minimum, nothing is swept");
    assert_eq!(fx.detected(), 1);
    assert_eq!(
        fx.db
            .get_sweep_failure_count(TEST_CHAIN, "0xaaa:1")
            .unwrap(),
        0
    );

    // More arrives; the balance reaches the minimum; it is swept whole.
    fx.token_deposit("0xbbb", 2, "700", Some(SENDER_B));
    rpc.set_balance(1100);
    sweeper.process_deposits_once().await.unwrap();
    assert_eq!(rpc.sent(), 1);
    assert_eq!(fx.detected(), 0);
    let first = fx.swept_webhook("0xaaa:1").unwrap();
    let second = fx.swept_webhook("0xbbb:2").unwrap();
    assert_eq!(first["sweeps"], second["sweeps"], "one sweep settled both");
    assert_eq!(first["sweeps"][0]["sweep_amount"], "1100");
}

#[tokio::test]
async fn req_sw_7_above_maximum_splits_into_sweeps_with_one_webhook() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "250", Some(SENDER_A));
    let (t1, t2, t3) = (sweep_tx(11), sweep_tx(12), sweep_tx(13));
    let rpc = Rpc::start(
        250,
        vec![t1.clone(), t2.clone(), t3.clone()],
        vec![
            (
                t1.clone(),
                ok_receipt(&t1, 20, 3, TOKEN, &fx.addr, TARGET_A, 100),
            ),
            (
                t2.clone(),
                ok_receipt(&t2, 21, 8, TOKEN, &fx.addr, TARGET_A, 100),
            ),
            (
                t3.clone(),
                ok_receipt(&t3, 22, 1, TOKEN, &fx.addr, TARGET_A, 50),
            ),
        ],
    )
    .await;
    let sweeper = fx.sweeper(
        &rpc,
        target_for(TOKEN, |t| t.max = Some(U256::from(100u64))),
    );

    sweeper.process_deposits_once().await.unwrap();

    assert_eq!(rpc.sent(), 3, "250 with a maximum of 100 is three sweeps");
    assert_eq!(fx.detected(), 0);
    let payload = fx.swept_webhook("0xaaa:1").expect("one deposit_swept");
    let sweeps = payload["sweeps"].as_array().unwrap();
    assert_eq!(sweeps.len(), 3);
    let amounts: Vec<&str> = sweeps
        .iter()
        .map(|s| s["sweep_amount"].as_str().unwrap())
        .collect();
    assert_eq!(amounts, ["100", "100", "50"]);
    let blocks: Vec<u64> = sweeps
        .iter()
        .map(|s| s["sweep_block_number"].as_u64().unwrap())
        .collect();
    assert_eq!(blocks, [20, 21, 22]);
    assert_eq!(
        payload["sweep_tx_hash"],
        rpc.hash(0),
        "top level is the first entry's"
    );
    assert_eq!(sweeps[0]["sweep_tx_hash"], rpc.hash(0));
    // One webhook for the deposit, not one per sweep.
    let rows = fx
        .db
        .get_pending_webhook_delivery_keys(5, 50)
        .unwrap()
        .into_iter()
        .filter(|(_, event)| event == "deposit_swept")
        .count();
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn req_sw_7_split_waits_for_its_last_sweep_and_resumes_after_a_failure() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "250", Some(SENDER_A));
    let (t1, t2, t3, t4) = (sweep_tx(21), sweep_tx(22), sweep_tx(23), sweep_tx(24));
    let rpc = Rpc::start(
        250,
        vec![t1.clone(), t2.clone(), t3.clone(), t4.clone()],
        vec![
            (
                t1.clone(),
                ok_receipt(&t1, 20, 3, TOKEN, &fx.addr, TARGET_A, 100),
            ),
            // The second sweep reverts.
            (
                t2.clone(),
                receipt_json(&t2, 21, 0, &fx.addr, TOKEN, false, vec![]),
            ),
            (
                t3.clone(),
                ok_receipt(&t3, 22, 4, TOKEN, &fx.addr, TARGET_A, 100),
            ),
            (
                t4.clone(),
                ok_receipt(&t4, 23, 5, TOKEN, &fx.addr, TARGET_A, 50),
            ),
        ],
    )
    .await;
    let sweeper = fx.sweeper(
        &rpc,
        target_for(TOKEN, |t| t.max = Some(U256::from(100u64))),
    );

    sweeper.process_deposits_once().await.unwrap();
    // One part went out; the deposit is not settled and nothing is sent yet.
    assert_eq!(fx.detected(), 1);
    assert!(fx.swept_webhook("0xaaa:1").is_none());
    assert_eq!(fx.db.get_sweeps(TEST_CHAIN, "0xaaa:1").unwrap().len(), 1);

    // 150 is left; the next pass sweeps it in two parts and settles the deposit.
    rpc.set_balance(150);
    sweeper.process_deposits_once().await.unwrap();
    assert_eq!(fx.detected(), 0);
    let payload = fx.swept_webhook("0xaaa:1").unwrap();
    let hashes: Vec<&str> = payload["sweeps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["sweep_tx_hash"].as_str().unwrap())
        .collect();
    assert_eq!(
        hashes,
        [
            rpc.hash(0).as_str(),
            rpc.hash(2).as_str(),
            rpc.hash(3).as_str()
        ]
    );
}

#[test]
fn req_sw_7_plan_sweep_parts_respects_both_bounds() {
    let u = |n: u64| U256::from(n);
    // No bounds: one sweep. Zero: none.
    assert_eq!(plan_sweep_parts(u(250), None, None, 10), vec![u(250)]);
    assert!(plan_sweep_parts(u(0), None, None, 10).is_empty());
    // Below the minimum waits.
    assert!(plan_sweep_parts(u(99), Some(u(100)), None, 10).is_empty());
    // At the maximum is one sweep; above it splits.
    assert_eq!(
        plan_sweep_parts(u(100), None, Some(u(100)), 10),
        vec![u(100)]
    );
    assert_eq!(
        plan_sweep_parts(u(250), None, Some(u(100)), 10),
        vec![u(100), u(100), u(50)]
    );
    // A remainder under the minimum is topped up from the previous part.
    assert_eq!(
        plan_sweep_parts(u(210), Some(u(50)), Some(u(100)), 10),
        vec![u(100), u(60), u(50)]
    );
    // When it cannot be, the dust stays in the address rather than being swept
    // outside the bounds.
    assert_eq!(
        plan_sweep_parts(u(110), Some(u(90)), Some(u(100)), 10),
        vec![u(100)]
    );
    for part in plan_sweep_parts(u(1234), Some(u(40)), Some(u(100)), 10) {
        assert!(part >= u(40) && part <= u(100));
    }
}

// ---------------------------------------------------------------- REQ-SW-8

fn chains_toml(extra_chain_lines: &str, default_target: &str) -> String {
    format!(
        r#"
[[chains]]
name = "polygon"
chain_id = 137
rpc_url = "http://localhost:8545"
treasury_address = "{default_target}"
faucet_address = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
allowed_token_addresses = ["{TOKEN}", "{OTHER_TOKEN}"]
{extra_chain_lines}
"#
    )
}

#[test]
fn req_sw_8_admits_only_refused_when_another_token_routes_to_the_address() {
    let toml = chains_toml(
        &format!(
            r#"
[chains.token_treasuries]
"{OTHER_TOKEN}" = "{TARGET_A}"

[chains.token_targets."{TOKEN}"]
kind = "transfer"
address = "{TARGET_A}"
admits_only = true
"#
        ),
        TEST_TREASURY,
    );
    let err = parse_chains_toml(&toml).unwrap_err().to_string();
    assert!(err.contains("admits only"), "{err}");
    assert!(
        err.to_lowercase().contains(&OTHER_TOKEN.to_lowercase()),
        "{err}"
    );
}

#[test]
fn req_sw_8_admits_only_refused_when_a_token_target_table_shares_the_address() {
    let toml = chains_toml(
        &format!(
            r#"
[chains.token_targets."{TOKEN}"]
kind = "transfer"
address = "{TARGET_A}"
admits_only = true

[chains.token_targets."{OTHER_TOKEN}"]
kind = "transfer"
address = "{TARGET_A}"
"#
        ),
        TEST_TREASURY,
    );
    assert!(parse_chains_toml(&toml).is_err());
}

#[test]
fn req_sw_8_admits_only_refused_when_the_default_target_or_native_coin_routes_there() {
    let toml = chains_toml(
        &format!(
            r#"
[chains.token_targets."{TOKEN}"]
kind = "transfer"
address = "{TARGET_A}"
admits_only = true
"#
        ),
        // The network default target is also where the native coin goes.
        TARGET_A,
    );
    let err = parse_chains_toml(&toml).unwrap_err().to_string();
    assert!(err.contains("default target"), "{err}");
    assert!(err.contains("native coin"), "{err}");
}

#[test]
fn req_sw_8_admits_only_accepted_when_nothing_else_routes_there() {
    let toml = chains_toml(
        &format!(
            r#"
[chains.token_targets."{TOKEN}"]
kind = "transfer"
address = "{TARGET_A}"
admits_only = true
min = "1000000"
max = "50000000000"
per_sender = true
"#
        ),
        TEST_TREASURY,
    );
    let chains = parse_chains_toml(&toml).unwrap();
    let target = chains[0].target_for_token(TOKEN);
    assert_eq!(target.kind, TargetKind::Transfer);
    assert_eq!(target.address.as_deref(), Some(TARGET_A));
    assert_eq!(target.min, Some(U256::from(1_000_000u64)));
    assert_eq!(target.max, Some(U256::from(50_000_000_000u64)));
    assert!(target.admits_only && target.per_sender);
    // The other token still goes to the default target.
    let other = chains[0].target_for_token(OTHER_TOKEN);
    assert_eq!(other.address.as_deref(), Some(TEST_TREASURY));
    assert!(!other.admits_only);
}

#[test]
fn req_sw_6_existing_token_treasuries_map_keeps_working() {
    let toml = chains_toml(
        &format!(
            r#"
[chains.token_treasuries]
"{TOKEN}" = "{TARGET_A}"
"#
        ),
        TEST_TREASURY,
    );
    let chains = parse_chains_toml(&toml).unwrap();
    let target = chains[0].target_for_token(TOKEN);
    assert_eq!(target.kind, TargetKind::Transfer);
    assert_eq!(target.address.as_deref(), Some(TARGET_A));
    assert_eq!(chains[0].treasury_for_token(TOKEN), TARGET_A);
}

#[test]
fn req_sw_6_token_target_validation() {
    let table = |body: &str| {
        chains_toml(
            &format!("[chains.token_targets.\"{TOKEN}\"]\n{body}\n"),
            TEST_TREASURY,
        )
    };
    // hold parses and routes nowhere.
    let chains = parse_chains_toml(&table("kind = \"hold\"")).unwrap();
    let target = chains[0].target_for_token(TOKEN);
    assert_eq!(target.kind, TargetKind::Hold);
    assert_eq!(target.address, None);
    // directed_deposit is not built.
    assert!(parse_chains_toml(&table("kind = \"directed_deposit\"")).is_err());
    // transfer needs an address; hold takes no address or bounds.
    assert!(parse_chains_toml(&table("kind = \"transfer\"")).is_err());
    assert!(parse_chains_toml(&table(&format!(
        "kind = \"hold\"\naddress = \"{TARGET_A}\""
    )))
    .is_err());
    // Bounds: min must not exceed max; max must not be zero.
    let bounded = |bounds: &str| {
        table(&format!(
            "kind = \"transfer\"\naddress = \"{TARGET_A}\"\n{bounds}"
        ))
    };
    assert!(parse_chains_toml(&bounded("min = \"10\"\nmax = \"5\"")).is_err());
    assert!(parse_chains_toml(&bounded("max = \"0\"")).is_err());
    assert!(parse_chains_toml(&bounded("min = \"5\"\nmax = \"5\"")).is_ok());
    // A token is in token_treasuries or token_targets, not both.
    let both = chains_toml(
        &format!(
            "[chains.token_treasuries]\n\"{TOKEN}\" = \"{TARGET_A}\"\n[chains.token_targets.\"{TOKEN}\"]\nkind = \"hold\"\n"
        ),
        TEST_TREASURY,
    );
    assert!(parse_chains_toml(&both).is_err());
    // A target for a token that is not admitted is refused.
    let stranger = chains_toml(
        "[chains.token_targets.\"0x1111111111111111111111111111111111111111\"]\nkind = \"hold\"\n",
        TEST_TREASURY,
    );
    assert!(parse_chains_toml(&stranger).is_err());
}

// ---------------------------------------------------------------- REQ-SW-12

#[tokio::test]
async fn req_sw_12_per_sender_two_senders_get_two_sweeps_never_mixed() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "500", Some(SENDER_A));
    fx.token_deposit("0xbbb", 2, "300", Some(SENDER_B));
    let (t1, t2) = (sweep_tx(31), sweep_tx(32));
    // The address holds 800; each sweep must move only one sender's share.
    let rpc = Rpc::start(
        800,
        vec![t1.clone(), t2.clone()],
        vec![
            (
                t1.clone(),
                ok_receipt(&t1, 30, 2, TOKEN, &fx.addr, TARGET_A, 500),
            ),
            (
                t2.clone(),
                ok_receipt(&t2, 31, 6, TOKEN, &fx.addr, TARGET_A, 300),
            ),
        ],
    )
    .await;
    let sweeper = fx.sweeper(&rpc, target_for(TOKEN, |t| t.per_sender = true));

    sweeper.process_deposits_once().await.unwrap();

    assert_eq!(rpc.sent(), 2, "two senders, two sweeps");
    let calldata = rpc.estimate_bodies().await;
    let amount_hex = |n: u64| format!("{n:064x}");
    assert!(calldata.iter().any(|b| b.contains(&amount_hex(500))));
    assert!(calldata.iter().any(|b| b.contains(&amount_hex(300))));
    assert!(calldata.iter().all(|b| !b.contains(&amount_hex(800))));

    let a = fx.swept_webhook("0xaaa:1").unwrap();
    let b = fx.swept_webhook("0xbbb:2").unwrap();
    assert_eq!(a["sweeps"].as_array().unwrap().len(), 1);
    assert_eq!(b["sweeps"].as_array().unwrap().len(), 1);
    assert_eq!(a["sweeps"][0]["sweep_amount"], "500");
    assert_eq!(b["sweeps"][0]["sweep_amount"], "300");
    assert_ne!(a["sweep_tx_hash"], b["sweep_tx_hash"]);
    assert_eq!(a["from_address"], SENDER_A);
    assert_eq!(b["from_address"], SENDER_B);
}

#[tokio::test]
async fn req_sw_12_per_sender_deposits_of_one_sender_accumulate_to_the_minimum() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "400", Some(SENDER_A));
    fx.token_deposit("0xbbb", 2, "400", Some(SENDER_B));
    let t1 = sweep_tx(41);
    let rpc = Rpc::start(
        800,
        vec![t1.clone()],
        vec![(
            t1.clone(),
            ok_receipt(&t1, 40, 2, TOKEN, &fx.addr, TARGET_A, 600),
        )],
    )
    .await;
    let sweeper = fx.sweeper(
        &rpc,
        target_for(TOKEN, |t| {
            t.per_sender = true;
            t.min = Some(U256::from(500u64));
        }),
    );

    // 400 from each sender: both below the minimum, even though the address
    // holds 800 (more than the minimum). Nothing is swept.
    sweeper.process_deposits_once().await.unwrap();
    assert_eq!(rpc.sent(), 0);
    assert_eq!(fx.detected(), 2);

    // Sender A sends 200 more: A's 600 reaches the minimum; B's 400 still waits.
    fx.token_deposit("0xccc", 3, "200", Some(SENDER_A));
    rpc.set_balance(1000);
    sweeper.process_deposits_once().await.unwrap();
    assert_eq!(rpc.sent(), 1);
    assert_eq!(fx.detected(), 1, "sender B's deposit is still waiting");
    let first = fx.swept_webhook("0xaaa:1").unwrap();
    let third = fx.swept_webhook("0xccc:3").unwrap();
    assert_eq!(first["sweeps"], third["sweeps"]);
    assert_eq!(first["sweeps"][0]["sweep_amount"], "600");
    assert!(fx.swept_webhook("0xbbb:2").is_none());
}

#[tokio::test]
async fn no_sender_on_per_sender_target_goes_to_review() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "100", None);
    fx.token_deposit("0xbbb", 2, "200", Some(SENDER_A));
    let t1 = sweep_tx(51);
    let rpc = Rpc::start(
        300,
        vec![t1.clone()],
        vec![(
            t1.clone(),
            ok_receipt(&t1, 50, 1, TOKEN, &fx.addr, TARGET_A, 200),
        )],
    )
    .await;
    let sweeper = fx.sweeper(&rpc, target_for(TOKEN, |t| t.per_sender = true));
    sweeper.process_deposits_once().await.unwrap();
    // The sender-less deposit has no refund address: never swept, for an operator.
    assert_eq!(
        fx.db
            .get_review_kind(TEST_CHAIN, "0xaaa:1")
            .unwrap()
            .as_deref(),
        Some("no_sender")
    );
    assert!(fx.swept_webhook("0xaaa:1").is_none());
    // The sender that is known is still swept (its own 200 only).
    assert_eq!(rpc.sent(), 1);
    assert!(fx.swept_webhook("0xbbb:2").is_some());
}

#[tokio::test]
async fn req_sw_7_a_deposit_that_joined_after_the_sweeps_does_not_block_settling_the_others() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "100", Some(SENDER_A));
    // A sweep for the first deposit went out in an earlier pass (crash before settling).
    fx.db
        .record_sweep(
            TEST_CHAIN,
            &["0xaaa:1".to_string()],
            &SweepRecord {
                sweep_tx_hash: sweep_tx(90),
                sweep_block_number: 5,
                sweep_block_hash: hash32(5),
                sweep_log_index: 2,
                sweep_amount: "100".into(),
                sweep_target: None,
            },
        )
        .unwrap();
    // A second deposit arrived since; the address now holds nothing.
    fx.token_deposit("0xbbb", 2, "50", Some(SENDER_B));
    let rpc = Rpc::start(0, vec![sweep_tx(91)], vec![]).await;
    fx.sweeper(&rpc, |_| {})
        .process_deposits_once()
        .await
        .unwrap();

    assert!(
        fx.swept_webhook("0xaaa:1").is_some(),
        "the swept deposit is settled"
    );
    assert_eq!(fx.detected(), 1, "the newcomer waits for a later pass");
    assert!(fx.swept_webhook("0xbbb:2").is_none());
    assert_eq!(
        fx.db
            .get_sweep_failure_count(TEST_CHAIN, "0xaaa:1")
            .unwrap(),
        0
    );
}

// ---------------------------------------------------------------- REQ-SW-9

#[tokio::test]
async fn req_sw_9_each_sweep_is_a_single_transfer_into_the_target() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "250", Some(SENDER_A));
    let (t1, t2, t3) = (sweep_tx(61), sweep_tx(62), sweep_tx(63));
    let rpc = Rpc::start(
        250,
        vec![t1.clone(), t2.clone(), t3.clone()],
        vec![
            (
                t1.clone(),
                ok_receipt(&t1, 60, 1, TOKEN, &fx.addr, TARGET_A, 100),
            ),
            (
                t2.clone(),
                ok_receipt(&t2, 61, 1, TOKEN, &fx.addr, TARGET_A, 100),
            ),
            (
                t3.clone(),
                ok_receipt(&t3, 62, 1, TOKEN, &fx.addr, TARGET_A, 50),
            ),
        ],
    )
    .await;
    let sweeper = fx.sweeper(
        &rpc,
        target_for(TOKEN, |t| t.max = Some(U256::from(100u64))),
    );
    sweeper.process_deposits_once().await.unwrap();

    // One transaction and one `transfer(target, part)` per part, to the target.
    assert_eq!(rpc.sent(), 3);
    let calldata = rpc.estimate_bodies().await;
    assert_eq!(calldata.len(), 3);
    let selector = "a9059cbb"; // transfer(address,uint256)
    let target_word = format!("{:0>64}", TARGET_A.trim_start_matches("0x"));
    for body in &calldata {
        assert!(
            body.contains(selector) && body.contains(&target_word),
            "{body}"
        );
    }
}

// ---------------------------------------------------------------- REQ-SW-3

#[tokio::test]
async fn req_sw_3_zero_balance_deposit_is_marked_swept_after_ten_passes_without_a_webhook() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "100", Some(SENDER_A));
    let rpc = Rpc::start(0, vec![sweep_tx(71)], vec![]).await;
    let sweeper = fx.sweeper(&rpc, |_| {});
    for _ in 0..9 {
        sweeper.process_deposits_once().await.unwrap();
    }
    assert_eq!(fx.detected(), 1);
    sweeper.process_deposits_once().await.unwrap();
    assert_eq!(fx.detected(), 0);
    assert_eq!(rpc.sent(), 0);
    assert!(fx.swept_webhook("0xaaa:1").is_none());
}

// --------------------------------------------------------------- REQ-DT-10

#[tokio::test]
async fn req_dt_10_unknown_symbol_deposit_is_detected_and_really_swept() {
    let fx = fixture();
    // The token's metadata is not stored and `symbol()` reverts.
    let token = OTHER_TOKEN;
    let block = empty_block_rpc_response();
    let logs = json!({
        "jsonrpc": "2.0", "id": 1,
        "result": [transfer_log_json(token, SENDER_A, &fx.addr, 777, 2, &hash32(0xc1), 10, 0)]
    });
    let server = monitor_rpc(block, logs, true).await;
    monitor_for(&fx, &server).catch_up_once().await.unwrap();

    let deposits = fx.db.get_detected_erc20_deposits(TEST_CHAIN).unwrap();
    assert_eq!(
        deposits.len(),
        1,
        "an unreadable symbol no longer drops the deposit"
    );
    assert_eq!(deposits[0].token_symbol, "UNKNOWN");
    let key = deposits[0].key.clone();
    let id = format!("{TEST_CHAIN}:{key}");
    let detected: Value = serde_json::from_str(
        &fx.db
            .get_webhook_delivery(&id, "deposit_detected")
            .unwrap()
            .unwrap()
            .payload,
    )
    .unwrap();
    assert_eq!(detected["token_symbol"], "UNKNOWN");

    // And it is swept like any admitted token: a real sweep, a real coordinate.
    let tx = sweep_tx(81);
    let rpc = Rpc::start(
        777,
        vec![tx.clone()],
        vec![(
            tx.clone(),
            ok_receipt(&tx, 12, 9, token, &fx.addr, TEST_TREASURY, 777),
        )],
    )
    .await;
    fx.sweeper(&rpc, |_| {})
        .process_deposits_once()
        .await
        .unwrap();
    assert_eq!(rpc.sent(), 1);
    assert_eq!(fx.detected(), 0);
    let swept = fx
        .swept_webhook(&key)
        .expect("deposit_swept for UNKNOWN token");
    assert_eq!(swept["token_symbol"], "UNKNOWN");
    assert_eq!(swept["sweeps"][0]["sweep_tx_hash"], rpc.hash(0));
    assert_eq!(swept["sweeps"][0]["sweep_log_index"], 9);
}

#[tokio::test]
async fn req_dt_10_long_symbol_token_is_detected_and_swept() {
    let fx = fixture();
    fx.db
        .store_token_metadata(
            TEST_CHAIN,
            &checksum(TOKEN),
            "A_VERY_LONG_SYMBOL",
            6,
            "Long",
        )
        .unwrap();
    let logs = json!({
        "jsonrpc": "2.0", "id": 1,
        "result": [transfer_log_json(TOKEN, SENDER_A, &fx.addr, 55, 2, &hash32(0xc2), 10, 0)]
    });
    let server = monitor_rpc(empty_block_rpc_response(), logs, false).await;
    monitor_for(&fx, &server).catch_up_once().await.unwrap();
    let deposits = fx.db.get_detected_erc20_deposits(TEST_CHAIN).unwrap();
    assert_eq!(deposits.len(), 1);
    assert_eq!(deposits[0].token_symbol, "A_VERY_LONG_SYMBOL");

    let tx = sweep_tx(82);
    let rpc = Rpc::start(
        55,
        vec![tx.clone()],
        vec![(
            tx.clone(),
            ok_receipt(&tx, 12, 1, TOKEN, &fx.addr, TEST_TREASURY, 55),
        )],
    )
    .await;
    fx.sweeper(&rpc, |_| {})
        .process_deposits_once()
        .await
        .unwrap();
    assert_eq!(rpc.sent(), 1);
    assert_eq!(fx.detected(), 0);
}

// ---------------------------------------------------------------- REQ-WH-7

fn outbox(id: &str, event: &str) -> OutboxEntry {
    OutboxEntry {
        id: id.into(),
        event: event.into(),
        registration_id: "user_1".into(),
        webhook_url: UNREACHABLE_WEBHOOK.into(),
        payload: json!({"id": id, "event": event}).to_string(),
    }
}

/// Makes every insert into the outbox fail, on the same SQLite file.
fn break_outbox(path: &str) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_outbox BEFORE INSERT ON webhook_deliveries
         BEGIN SELECT RAISE(ABORT, 'outbox unavailable'); END;",
    )
    .unwrap();
}

fn repair_outbox(path: &str) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch("DROP TRIGGER fail_outbox;").unwrap();
}

#[test]
fn req_wh_7_failed_outbox_write_rolls_back_the_detected_deposit() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_str().unwrap().to_string();
    let db = Db::new(&path).unwrap();
    db.register_account("user_1", 0, "0xaddr", UNREACHABLE_WEBHOOK)
        .unwrap();
    let native = NewNativeDeposit {
        chain: TEST_CHAIN.into(),
        tx_hash: "0xnative".into(),
        account_id: "user_1".into(),
        amount: "5".into(),
        from_address: Some(SENDER_A.into()),
    };
    let token = NewErc20Deposit {
        chain: TEST_CHAIN.into(),
        tx_hash: "0xtoken".into(),
        log_index: 4,
        account_id: "user_1".into(),
        amount: "9".into(),
        token_address: TOKEN.into(),
        token_symbol: "USDT".into(),
        from_address: Some(SENDER_A.into()),
    };

    break_outbox(&path);
    assert!(db
        .record_native_deposit_atomic(&native, Some(&outbox("n", "deposit_detected")))
        .is_err());
    assert!(db
        .record_erc20_deposit_atomic(&token, Some(&outbox("t", "deposit_detected")))
        .is_err());
    // Neither deposit was stored: a stored deposit always has its webhook.
    assert!(db.get_detected_deposits(TEST_CHAIN).unwrap().is_empty());
    assert!(db
        .get_detected_erc20_deposits(TEST_CHAIN)
        .unwrap()
        .is_empty());

    // Once the outbox works the same deposits are recorded, with their rows.
    repair_outbox(&path);
    assert!(db
        .record_native_deposit_atomic(&native, Some(&outbox("n", "deposit_detected")))
        .unwrap());
    assert!(db
        .record_erc20_deposit_atomic(&token, Some(&outbox("t", "deposit_detected")))
        .unwrap());
    assert!(db
        .get_webhook_delivery("n", "deposit_detected")
        .unwrap()
        .is_some());
    assert!(db
        .get_webhook_delivery("t", "deposit_detected")
        .unwrap()
        .is_some());
    // Recording again is a no-op and writes no second row.
    assert!(!db
        .record_erc20_deposit_atomic(&token, Some(&outbox("t", "deposit_detected")))
        .unwrap());
}

#[test]
fn req_wh_7_failed_outbox_write_rolls_back_the_swept_mark() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_str().unwrap().to_string();
    let db = Db::new(&path).unwrap();
    db.register_account("user_1", 0, "0xaddr", UNREACHABLE_WEBHOOK)
        .unwrap();
    db.record_native_deposit_atomic(
        &NewNativeDeposit {
            chain: TEST_CHAIN.into(),
            tx_hash: "0xnative".into(),
            account_id: "user_1".into(),
            amount: "5".into(),
            from_address: None,
        },
        None,
    )
    .unwrap();
    for (tx, idx) in [("0xt1", 1u64), ("0xt2", 2)] {
        db.record_erc20_deposit_atomic(
            &NewErc20Deposit {
                chain: TEST_CHAIN.into(),
                tx_hash: tx.into(),
                log_index: idx,
                account_id: "user_1".into(),
                amount: "9".into(),
                token_address: TOKEN.into(),
                token_symbol: "USDT".into(),
                from_address: None,
            },
            None,
        )
        .unwrap();
    }
    let settlements = vec![
        crate::db::Erc20Settlement {
            key: "0xt1:1".into(),
            outbox: Some(outbox("c:0xt1:1", "deposit_swept")),
        },
        crate::db::Erc20Settlement {
            key: "0xt2:2".into(),
            outbox: Some(outbox("c:0xt2:2", "deposit_swept")),
        },
    ];

    break_outbox(&path);
    assert!(db
        .complete_native_sweep(
            TEST_CHAIN,
            "0xnative",
            Some(&outbox("c:n", "deposit_swept"))
        )
        .is_err());
    assert!(db.complete_erc20_sweep(TEST_CHAIN, &settlements).is_err());
    // Still detected: neither the swept marks nor any of the rows leaked.
    assert_eq!(db.get_detected_deposits(TEST_CHAIN).unwrap().len(), 1);
    assert_eq!(db.get_detected_erc20_deposits(TEST_CHAIN).unwrap().len(), 2);

    repair_outbox(&path);
    assert_eq!(
        db.complete_erc20_sweep(TEST_CHAIN, &settlements).unwrap(),
        2
    );
    assert!(db
        .complete_native_sweep(
            TEST_CHAIN,
            "0xnative",
            Some(&outbox("c:n", "deposit_swept"))
        )
        .unwrap());
    assert!(db.get_detected_deposits(TEST_CHAIN).unwrap().is_empty());
    assert!(db
        .get_detected_erc20_deposits(TEST_CHAIN)
        .unwrap()
        .is_empty());
    assert!(db
        .get_webhook_delivery("c:0xt2:2", "deposit_swept")
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn req_wh_7_monitor_stores_no_deposit_when_the_outbox_write_fails() {
    let fx = fixture();
    let path = fx._db_file.path().to_str().unwrap().to_string();
    let logs = json!({
        "jsonrpc": "2.0", "id": 1,
        "result": [transfer_log_json(TOKEN, SENDER_A, &fx.addr, 55, 2, &hash32(0xc3), 10, 0)]
    });
    let server = monitor_rpc(empty_block_rpc_response(), logs, false).await;
    break_outbox(&path);
    let monitor = monitor_for(&fx, &server);
    assert!(monitor.catch_up_once().await.is_err());
    assert_eq!(fx.detected(), 0);
    // The cursor did not advance past the block either (REQ-DT-1), so the
    // deposit is found again once the outbox is back.
    repair_outbox(&path);
    monitor.catch_up_once().await.unwrap();
    assert_eq!(fx.detected(), 1);
}

// ---------------------------------------------------------------- REQ-RG-5

#[test]
fn req_rg_5_registration_at_the_32_bit_ceiling_is_refused_on_sqlite() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_str().unwrap().to_string();
    let db = Db::new(&path).unwrap();
    let derive = |i: u32| -> anyhow::Result<String> { Ok(format!("0xaddr{i}")) };

    // The last index, 2^32 - 1, is still issued, once.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO state (key, value) VALUES ('next_index', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [u32::MAX.to_string()],
    )
    .unwrap();
    let (index, _, created) = db
        .register_account_auto("last", "https://x", derive)
        .unwrap();
    assert_eq!((index, created), (u32::MAX, true));

    // The counter is now 2^32: refused, not wrapped, nothing written.
    let err = db
        .register_account_auto("one_too_many", "https://x", derive)
        .unwrap_err();
    assert!(err.to_string().contains("REQ-RG-5"), "{err}");
    assert!(db.get_account_by_id("one_too_many").unwrap().is_none());
    let counter: String = conn
        .query_row(
            "SELECT value FROM state WHERE key = 'next_index'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(counter, (1u64 << 32).to_string());

    // A repeated registration of an existing id still answers.
    let (index, _, created) = db
        .register_account_auto("last", "https://x", derive)
        .unwrap();
    assert_eq!((index, created), (u32::MAX, false));
}

// ------------------------------------------------------------------ store

#[test]
fn req_sw_10_store_records_sweeps_idempotently_and_in_order() {
    let file = NamedTempFile::new().unwrap();
    let db = Db::new(file.path().to_str().unwrap()).unwrap();
    let rec = |tx: &str, log: u64| SweepRecord {
        sweep_tx_hash: tx.into(),
        sweep_block_number: 7,
        sweep_block_hash: hash32(7),
        sweep_log_index: log,
        sweep_amount: "10".into(),
        sweep_target: Some(TARGET_A.into()),
    };
    let keys = vec!["0xd1:1".to_string(), "0xd2:2".to_string()];
    db.record_sweep(TEST_CHAIN, &keys, &rec("0xs1", 3)).unwrap();
    db.record_sweep(TEST_CHAIN, &keys, &rec("0xs2", 4)).unwrap();
    db.record_sweep(TEST_CHAIN, &keys, &rec("0xs1", 3)).unwrap();
    for key in &keys {
        let sweeps = db.get_sweeps(TEST_CHAIN, key).unwrap();
        assert_eq!(sweeps, vec![rec("0xs1", 3), rec("0xs2", 4)]);
    }
}

// ---------------------------------------------------------- review fixes

fn pending_intent(fx: &Fixture, key: &str, amount: &str, tx: Option<&str>) -> i64 {
    let id = fx
        .db
        .create_sweep_intent(&crate::db::NewSweepIntent {
            chain: TEST_CHAIN.into(),
            account_id: "user_1".into(),
            token_address: checksum(TOKEN),
            group_key: "gk".into(),
            keys: vec![key.to_string()],
            part_index: 0,
            amount: amount.into(),
            target: checksum(TARGET_A),
        })
        .unwrap();
    if let Some(tx) = tx {
        fx.db.set_sweep_intent_tx(id, tx).unwrap();
    }
    id
}

#[tokio::test]
async fn review_landed_but_unrecorded_is_never_silently_settled() {
    // A sweep was sent (intent has its hash) but the process stopped before
    // recording it. The address is now empty, so the zero-balance rule would
    // settle the deposit silently. It must wait for the receipt instead.
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "100", Some(SENDER_A));
    let tx = sweep_tx(101);
    pending_intent(&fx, "0xaaa:1", "100", Some(&tx));
    let rpc = Rpc::start(0, vec![sweep_tx(102)], vec![]).await; // no receipt yet
    let sweeper = fx.sweeper(&rpc, target_for(TOKEN, |_| {}));
    for _ in 0..9 {
        sweeper.process_deposits_once().await.unwrap();
    }
    assert_eq!(fx.detected(), 1, "not settled by the zero-balance rule");
    assert!(fx.swept_webhook("0xaaa:1").is_none());
    // Still no receipt after the tenth pass: an operator looks.
    sweeper.process_deposits_once().await.unwrap();
    assert_eq!(fx.detected(), 0);
    assert_eq!(
        fx.db
            .get_review_kind(TEST_CHAIN, "0xaaa:1")
            .unwrap()
            .as_deref(),
        Some("sweep_review")
    );
    assert_eq!(rpc.sent(), 0);
}

#[tokio::test]
async fn review_pending_intent_is_resolved_from_its_receipt_and_settled() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "100", Some(SENDER_A));
    let tx = sweep_tx(103);
    pending_intent(&fx, "0xaaa:1", "100", Some(&tx));
    let rpc = Rpc::start(
        0,
        vec![sweep_tx(104)],
        vec![(
            tx.clone(),
            ok_receipt(&tx, 30, 6, TOKEN, &fx.addr, TARGET_A, 100),
        )],
    )
    .await;
    fx.sweeper(&rpc, target_for(TOKEN, |_| {}))
        .process_deposits_once()
        .await
        .unwrap();
    // Recovered: recorded from the receipt, settled by the recorded sweep, not re-sent.
    assert_eq!(rpc.sent(), 0);
    assert_eq!(fx.detected(), 0);
    let payload = fx.swept_webhook("0xaaa:1").expect("deposit_swept");
    assert_eq!(payload["sweeps"][0]["sweep_tx_hash"], tx);
    assert_eq!(payload["sweeps"][0]["sweep_log_index"], 6);
}

#[tokio::test]
async fn crash_before_sign_is_dropped_and_the_group_replans_without_review() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "100", Some(SENDER_A));
    // An intent with no nonce, hash or bytes: the process stopped before signing.
    pending_intent(&fx, "0xaaa:1", "100", None);
    let tx = sweep_tx(105);
    let rpc = Rpc::start(
        100,
        vec![tx.clone()],
        vec![(
            tx.clone(),
            ok_receipt(&tx, 30, 2, TOKEN, &fx.addr, TARGET_A, 100),
        )],
    )
    .await;
    fx.sweeper(&rpc, target_for(TOKEN, |_| {}))
        .process_deposits_once()
        .await
        .unwrap();
    // Nothing was ever sent, so no review: it is simply swept, once.
    assert_eq!(rpc.sent(), 1);
    assert_eq!(fx.detected(), 0);
    assert!(fx
        .db
        .get_review_kind(TEST_CHAIN, "0xaaa:1")
        .unwrap()
        .is_none());
    assert!(fx.swept_webhook("0xaaa:1").is_some());
}

#[tokio::test]
async fn review_a_sweep_leaves_a_succeeded_intent_and_a_sweep_row() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "100", Some(SENDER_A));
    let tx = sweep_tx(106);
    let rpc = Rpc::start(
        100,
        vec![tx.clone()],
        vec![(
            tx.clone(),
            ok_receipt(&tx, 30, 6, TOKEN, &fx.addr, TARGET_A, 100),
        )],
    )
    .await;
    fx.sweeper(&rpc, target_for(TOKEN, |_| {}))
        .process_deposits_once()
        .await
        .unwrap();
    assert!(fx
        .db
        .pending_sweep_intents(TEST_CHAIN, &["0xaaa:1".to_string()])
        .unwrap()
        .is_empty());
    assert_eq!(fx.db.get_sweeps(TEST_CHAIN, "0xaaa:1").unwrap().len(), 1);
}

#[tokio::test]
async fn per_sender_never_spends_other_senders_funds() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "500", Some(SENDER_A));
    fx.token_deposit("0xbbb", 2, "300", Some(SENDER_B));
    // The address holds 600, less than the 800 owed to the two senders.
    let rpc = Rpc::start(600, vec![sweep_tx(111)], vec![]).await;
    fx.sweeper(&rpc, target_for(TOKEN, |t| t.per_sender = true))
        .process_deposits_once()
        .await
        .unwrap();
    assert_eq!(rpc.sent(), 0, "nothing is swept for the pair");
    for key in ["0xaaa:1", "0xbbb:2"] {
        assert_eq!(
            fx.db.get_review_kind(TEST_CHAIN, key).unwrap().as_deref(),
            Some("balance_shortfall")
        );
    }
}

#[tokio::test]
async fn split_plan_is_bounded_per_pass() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "1500", Some(SENDER_A));
    let hashes: Vec<String> = (0..15).map(|i| sweep_tx(200 + i)).collect();
    let receipts = hashes
        .iter()
        .enumerate()
        .map(|(i, h)| {
            (
                h.clone(),
                ok_receipt(h, 40 + i as u64, i as u64, TOKEN, &fx.addr, TARGET_A, 100),
            )
        })
        .collect();
    let rpc = Rpc::start(1500, hashes.clone(), receipts).await;
    let sweeper = fx.sweeper(
        &rpc,
        target_for(TOKEN, |t| t.max = Some(U256::from(100u64))),
    );
    sweeper.process_deposits_once().await.unwrap();
    // 15 parts are owed; a pass sends at most 10 and settles nothing.
    assert_eq!(rpc.sent(), 10);
    assert_eq!(fx.detected(), 1);
    assert!(fx.swept_webhook("0xaaa:1").is_none());
    // The next pass re-reads the balance and sends the rest.
    rpc.set_balance(500);
    sweeper.process_deposits_once().await.unwrap();
    assert_eq!(rpc.sent(), 15);
    assert_eq!(fx.detected(), 0);
    let payload = fx.swept_webhook("0xaaa:1").unwrap();
    assert_eq!(payload["sweeps"].as_array().unwrap().len(), 15);
}

#[tokio::test]
async fn dust_after_a_split_stays_owed_and_is_lifted_by_a_later_deposit() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "110", Some(SENDER_A));
    let (t1, t2) = (sweep_tx(301), sweep_tx(302));
    let rpc = Rpc::start(
        110,
        vec![t1.clone(), t2.clone()],
        vec![
            (
                t1.clone(),
                ok_receipt(&t1, 50, 1, TOKEN, &fx.addr, TARGET_A, 100),
            ),
            (
                t2.clone(),
                ok_receipt(&t2, 51, 1, TOKEN, &fx.addr, TARGET_A, 100),
            ),
        ],
    )
    .await;
    let sweeper = fx.sweeper(
        &rpc,
        target_for(TOKEN, |t| {
            t.min = Some(U256::from(90u64));
            t.max = Some(U256::from(100u64));
        }),
    );
    sweeper.process_deposits_once().await.unwrap();
    // 100 went out; the 10 left is below the minimum: owed, not settled.
    assert_eq!(rpc.sent(), 1);
    assert_eq!(fx.detected(), 1);
    assert!(fx.swept_webhook("0xaaa:1").is_none());
    rpc.set_balance(10);
    sweeper.process_deposits_once().await.unwrap();
    assert_eq!(rpc.sent(), 1);
    assert_eq!(fx.detected(), 1);
    // The dust alert was raised once and recorded, not on every pass.
    assert!(!fx.db.alert_once(TEST_CHAIN, "0xaaa:1", "dust").unwrap());
    // A later deposit lifts the dust over the minimum; both are settled.
    fx.token_deposit("0xbbb", 2, "90", Some(SENDER_B));
    rpc.set_balance(100);
    sweeper.process_deposits_once().await.unwrap();
    assert_eq!(rpc.sent(), 2);
    assert_eq!(fx.detected(), 0);
    let first = fx.swept_webhook("0xaaa:1").unwrap();
    assert_eq!(first["sweeps"].as_array().unwrap().len(), 2);
}

#[test]
fn split_plan_limit_never_allocates_more_than_the_limit() {
    let u = |n: u64| U256::from(n);
    // A tiny maximum on a huge amount is capped, not expanded.
    let huge = U256::from(10u64).pow(U256::from(30u64));
    assert_eq!(plan_sweep_parts(huge, None, Some(u(1)), 10).len(), 10);
    assert_eq!(plan_sweep_parts(u(1500), None, Some(u(100)), 10).len(), 10);
    assert_eq!(plan_sweep_parts(u(1000), None, Some(u(100)), 10).len(), 10);
    assert_eq!(plan_sweep_parts(u(1010), None, Some(u(100)), 10).len(), 10);
}

#[test]
fn review_admin_retry_moves_needs_review_back_to_detected() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "100", Some(SENDER_A));
    let id = pending_intent(&fx, "0xaaa:1", "100", None);
    fx.db.finish_sweep_intent(id, "review").unwrap();
    fx.db
        .mark_erc20_deposits_needs_review_for_keys(
            TEST_CHAIN,
            &["0xaaa:1".to_string()],
            "sweep_review",
        )
        .unwrap();
    assert_eq!(fx.detected(), 0);
    // Only the retry route (this call) moves it back.
    assert!(fx.db.retry_erc20_deposit(TEST_CHAIN, "0xaaa", 1).unwrap());
    assert_eq!(fx.detected(), 1);
    assert_eq!(fx.db.get_review_kind(TEST_CHAIN, "0xaaa:1").unwrap(), None);
    // A deposit that is not in review or failed is untouched.
    assert!(!fx.db.retry_erc20_deposit(TEST_CHAIN, "0xaaa", 1).unwrap());
}

#[tokio::test]
async fn review_deposits_are_not_picked_up_by_the_sweeper() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "100", Some(SENDER_A));
    fx.db
        .mark_erc20_deposits_needs_review_for_keys(TEST_CHAIN, &["0xaaa:1".to_string()], "dust")
        .unwrap();
    let rpc = Rpc::start(100, vec![sweep_tx(401)], vec![]).await;
    fx.sweeper(&rpc, |_| {})
        .process_deposits_once()
        .await
        .unwrap();
    assert_eq!(rpc.sent(), 0);
}

// ------------------------------------------------- broadcast failures

#[tokio::test]
async fn broadcast_error_then_next_pass_rebroadcasts_the_same_tx_and_settles() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "100", Some(SENDER_A));
    let tx = sweep_tx(501);
    let rpc = Rpc::start(
        100,
        vec![tx.clone()],
        vec![(
            tx.clone(),
            ok_receipt(&tx, 60, 4, TOKEN, &fx.addr, TARGET_A, 100),
        )],
    )
    .await;
    rpc.fail_next_sends(1); // a rate-limited public RPC refuses the first broadcast
    let sweeper = fx.sweeper(&rpc, target_for(TOKEN, |_| {}));

    sweeper.process_deposits_once().await.unwrap();
    // Not sent, not in review, not failed-out: the signed tx is stored.
    assert_eq!(rpc.sent(), 0);
    assert_eq!(fx.detected(), 1);
    let pending = fx
        .db
        .pending_sweep_intents(TEST_CHAIN, &["0xaaa:1".to_string()])
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].tx_hash.is_some() && pending[0].raw_tx.is_some());
    assert_eq!(pending[0].nonce, Some(0));
    let signed_hash = pending[0].tx_hash.clone().unwrap();

    // Next pass: no receipt, nonce still free -> rebroadcast the same bytes.
    sweeper.process_deposits_once().await.unwrap();
    assert_eq!(rpc.sent(), 1);
    assert_eq!(rpc.hash(0), signed_hash, "same bytes, same hash");
    assert_eq!(fx.detected(), 1);
    // Then the receipt is there (the tokens have left the address) and it
    // settles; nothing was swept twice.
    rpc.set_balance(0);
    sweeper.process_deposits_once().await.unwrap();
    assert_eq!(rpc.sent(), 1);
    assert_eq!(fx.detected(), 0);
    let payload = fx.swept_webhook("0xaaa:1").unwrap();
    assert_eq!(payload["sweeps"].as_array().unwrap().len(), 1);
    assert_eq!(payload["sweeps"][0]["sweep_tx_hash"], signed_hash);
}

#[tokio::test]
async fn nonce_consumed_by_another_tx_goes_to_review() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "100", Some(SENDER_A));
    let rpc = Rpc::start(100, vec![sweep_tx(502)], vec![]).await;
    rpc.fail_next_sends(1);
    let sweeper = fx.sweeper(&rpc, target_for(TOKEN, |_| {}));
    sweeper.process_deposits_once().await.unwrap();
    assert_eq!(fx.detected(), 1);
    // Something else used the nonce; our tx has no receipt and can never land.
    rpc.set_latest_nonce(1);
    sweeper.process_deposits_once().await.unwrap();
    assert_eq!(fx.detected(), 0);
    assert_eq!(
        fx.db
            .get_review_kind(TEST_CHAIN, "0xaaa:1")
            .unwrap()
            .as_deref(),
        Some("sweep_review")
    );
    assert_eq!(rpc.sent(), 0);
    assert!(fx.swept_webhook("0xaaa:1").is_none());
}

#[tokio::test]
async fn unmined_intent_is_dropped_after_the_wait_passes_and_replans_once() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "100", Some(SENDER_A));
    let tx = sweep_tx(503);
    let rpc = Rpc::start(
        100,
        vec![tx.clone()],
        vec![(
            tx.clone(),
            ok_receipt(&tx, 61, 1, TOKEN, &fx.addr, TARGET_A, 100),
        )],
    )
    .await;
    // The first broadcast and the nine rebroadcasts all fail: it never lands.
    rpc.fail_next_sends(10);
    let sweeper = fx.sweeper(&rpc, target_for(TOKEN, |_| {}));
    for _ in 0..10 {
        sweeper.process_deposits_once().await.unwrap();
    }
    assert_eq!(rpc.sent(), 0);
    assert_eq!(fx.detected(), 1);
    // The tenth wait pass drops the intent; the group re-plans and sends once.
    sweeper.process_deposits_once().await.unwrap();
    assert_eq!(rpc.sent(), 1);
    assert!(rpc.send_calls() >= 11);
    assert_eq!(fx.detected(), 0);
    assert!(fx
        .db
        .get_review_kind(TEST_CHAIN, "0xaaa:1")
        .unwrap()
        .is_none());
    let payload = fx.swept_webhook("0xaaa:1").unwrap();
    assert_eq!(payload["sweeps"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn retry_leaves_in_flight_intents_alone_and_names_the_tx() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "100", Some(SENDER_A));
    let tx = sweep_tx(504);
    let id = pending_intent(&fx, "0xaaa:1", "100", Some(&tx));
    fx.db
        .mark_erc20_deposits_failed_for_keys(TEST_CHAIN, &["0xaaa:1".to_string()])
        .unwrap();
    // The sweep could still land: retry refuses, naming the transaction.
    let err = fx
        .db
        .retry_erc20_deposit(TEST_CHAIN, "0xaaa", 1)
        .unwrap_err();
    assert!(err.to_string().contains(&tx), "{err}");
    assert_eq!(fx.detected(), 0);
    assert_eq!(
        fx.db
            .pending_sweep_intents(TEST_CHAIN, &["0xaaa:1".to_string()])
            .unwrap()
            .len(),
        1,
        "the in-flight intent is untouched"
    );
    // Once the intent is resolved (here: review), retry releases the deposit.
    fx.db.finish_sweep_intent(id, "review").unwrap();
    assert!(fx.db.retry_erc20_deposit(TEST_CHAIN, "0xaaa", 1).unwrap());
    assert_eq!(fx.detected(), 1);
}

// ------------------------------------------------------------ per_transfer

#[tokio::test]
async fn per_transfer_two_deposits_from_the_same_sender_get_two_sweeps() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "100", Some(SENDER_A));
    fx.token_deposit("0xbbb", 2, "200", Some(SENDER_A));
    let (t1, t2) = (sweep_tx(601), sweep_tx(602));
    let rpc = Rpc::start(
        300,
        vec![t1.clone(), t2.clone()],
        vec![
            (
                t1.clone(),
                ok_receipt(&t1, 70, 1, TOKEN, &fx.addr, TARGET_A, 100),
            ),
            (
                t2.clone(),
                ok_receipt(&t2, 71, 1, TOKEN, &fx.addr, TARGET_A, 200),
            ),
        ],
    )
    .await;
    let sweeper = fx.sweeper(&rpc, target_for(TOKEN, |t| t.per_transfer = true));
    sweeper.process_deposits_once().await.unwrap();
    assert_eq!(
        rpc.sent(),
        2,
        "one sweep per inbound transfer, same sender or not"
    );
    let calldata = rpc.estimate_bodies().await;
    let amount_hex = |n: u64| format!("{n:064x}");
    assert!(calldata.iter().any(|b| b.contains(&amount_hex(100))));
    assert!(calldata.iter().any(|b| b.contains(&amount_hex(200))));
    assert!(calldata.iter().all(|b| !b.contains(&amount_hex(300))));
    let a = fx.swept_webhook("0xaaa:1").unwrap();
    let b = fx.swept_webhook("0xbbb:2").unwrap();
    assert_eq!(a["sweeps"].as_array().unwrap().len(), 1);
    assert_eq!(b["sweeps"].as_array().unwrap().len(), 1);
    assert_eq!(a["sweeps"][0]["sweep_amount"], "100");
    assert_eq!(b["sweeps"][0]["sweep_amount"], "200");
    assert_ne!(a["sweep_tx_hash"], b["sweep_tx_hash"]);
}

#[tokio::test]
async fn per_transfer_split_deposit_keeps_all_parts_for_that_deposit() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "250", Some(SENDER_A));
    fx.token_deposit("0xbbb", 2, "50", Some(SENDER_A));
    let hs: Vec<String> = (0..4).map(|i| sweep_tx(610 + i)).collect();
    let amounts = [100u128, 100, 50, 50];
    let receipts = hs
        .iter()
        .zip(amounts)
        .enumerate()
        .map(|(i, (h, a))| {
            (
                h.clone(),
                ok_receipt(h, 80 + i as u64, 1, TOKEN, &fx.addr, TARGET_A, a),
            )
        })
        .collect();
    let rpc = Rpc::start(300, hs, receipts).await;
    let sweeper = fx.sweeper(
        &rpc,
        target_for(TOKEN, |t| {
            t.per_transfer = true;
            t.max = Some(U256::from(100u64));
        }),
    );
    sweeper.process_deposits_once().await.unwrap();
    assert_eq!(rpc.sent(), 4);
    let first = fx.swept_webhook("0xaaa:1").unwrap();
    let second = fx.swept_webhook("0xbbb:2").unwrap();
    let amounts_of = |p: &Value| -> Vec<String> {
        p["sweeps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["sweep_amount"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(amounts_of(&first), ["100", "100", "50"]);
    assert_eq!(amounts_of(&second), ["50"]);
    assert_ne!(first["sweeps"][0], second["sweeps"][0]);
}

#[tokio::test]
async fn per_transfer_shortfall_goes_to_review() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "100", Some(SENDER_A));
    fx.token_deposit("0xbbb", 2, "200", Some(SENDER_A));
    let rpc = Rpc::start(100, vec![sweep_tx(620)], vec![]).await; // owes 300, holds 100
    fx.sweeper(&rpc, target_for(TOKEN, |t| t.per_transfer = true))
        .process_deposits_once()
        .await
        .unwrap();
    assert_eq!(rpc.sent(), 0);
    for key in ["0xaaa:1", "0xbbb:2"] {
        assert_eq!(
            fx.db.get_review_kind(TEST_CHAIN, key).unwrap().as_deref(),
            Some("balance_shortfall")
        );
    }
}

#[tokio::test]
async fn per_transfer_no_sender_goes_to_review() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "100", None);
    let rpc = Rpc::start(100, vec![sweep_tx(621)], vec![]).await;
    fx.sweeper(&rpc, target_for(TOKEN, |t| t.per_transfer = true))
        .process_deposits_once()
        .await
        .unwrap();
    assert_eq!(rpc.sent(), 0);
    assert_eq!(
        fx.db
            .get_review_kind(TEST_CHAIN, "0xaaa:1")
            .unwrap()
            .as_deref(),
        Some("no_sender")
    );
}

#[test]
fn per_transfer_min_above_the_detection_minimum_is_refused_at_load() {
    let cfg = |min_line: &str, detection: &str| {
        chains_toml(
            &format!(
                r#"
min_deposit_default = "{detection}"

[chains.token_targets."{TOKEN}"]
kind = "transfer"
address = "{TARGET_A}"
per_transfer = true
{min_line}
"#
            ),
            TEST_TREASURY,
        )
    };
    // A deposit of 500 would be stored but could never be swept alone.
    let err = parse_chains_toml(&cfg("min = \"1000\"", "500"))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("per_transfer") && err.contains("detection"),
        "{err}"
    );
    // At or below the detection minimum is fine, and so is no minimum.
    assert!(parse_chains_toml(&cfg("min = \"500\"", "500")).is_ok());
    assert!(parse_chains_toml(&cfg("", "0")).is_ok());
    // A per-token detection minimum counts too.
    let per_token = chains_toml(
        &format!(
            r#"
[chains.min_deposits]
"{TOKEN}" = "2000"

[chains.token_targets."{TOKEN}"]
kind = "transfer"
address = "{TARGET_A}"
per_transfer = true
min = "1500"
"#
        ),
        TEST_TREASURY,
    );
    let chains = parse_chains_toml(&per_token).unwrap();
    assert!(chains[0].target_for_token(TOKEN).per_transfer);
    // per_transfer and per_sender together are refused.
    let both = chains_toml(
        &format!(
            "[chains.token_targets.\"{TOKEN}\"]\nkind = \"transfer\"\naddress = \"{TARGET_A}\"\nper_transfer = true\nper_sender = true\n"
        ),
        TEST_TREASURY,
    );
    assert!(parse_chains_toml(&both).is_err());
}

// ------------------------------------------------ second review's findings

#[tokio::test]
async fn failed_deposit_with_a_pending_intent_still_has_it_resolved() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "100", Some(SENDER_A));
    let tx = sweep_tx(701);
    pending_intent(&fx, "0xaaa:1", "100", Some(&tx));
    // The deposit went failed (say, a "deserializ" error) with the sweep in flight.
    fx.db
        .mark_erc20_deposits_failed_for_keys(TEST_CHAIN, &["0xaaa:1".to_string()])
        .unwrap();
    assert!(fx.db.retry_erc20_deposit(TEST_CHAIN, "0xaaa", 1).is_err());
    let rpc = Rpc::start(
        0,
        vec![sweep_tx(702)],
        vec![(
            tx.clone(),
            ok_receipt(&tx, 90, 3, TOKEN, &fx.addr, TARGET_A, 100),
        )],
    )
    .await;
    fx.sweeper(&rpc, target_for(TOKEN, |_| {}))
        .process_deposits_once()
        .await
        .unwrap();
    // Resolved although its deposit is not detected: the sweep is recorded and
    // the retry is no longer refused.
    assert_eq!(fx.db.get_sweeps(TEST_CHAIN, "0xaaa:1").unwrap().len(), 1);
    assert!(fx.db.retry_erc20_deposit(TEST_CHAIN, "0xaaa", 1).unwrap());
    fx.sweeper(&rpc, target_for(TOKEN, |_| {}))
        .process_deposits_once()
        .await
        .unwrap();
    assert!(
        fx.swept_webhook("0xaaa:1").is_some(),
        "settled by its recorded sweep"
    );
}

#[tokio::test]
async fn a_deposit_with_a_sweep_in_flight_is_not_failed_out_from_under_it() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "100", Some(SENDER_A));
    let tx = sweep_tx(703);
    // In flight: hash but no receipt, and nothing to rebroadcast.
    pending_intent(&fx, "0xaaa:1", "100", Some(&tx));
    let rpc = Rpc::start(100, vec![sweep_tx(704)], vec![]).await;
    // The token is not on the allowlist, which would fail the deposit.
    let sweeper = fx.sweeper(&rpc, |c| {
        c.allowed_token_addresses.insert(OTHER_TOKEN.to_lowercase());
    });
    sweeper.process_deposits_once().await.unwrap();
    assert_eq!(
        fx.detected(),
        1,
        "left alone while its sweep may still land"
    );
}

#[tokio::test]
async fn whole_balance_pair_is_blocked_while_a_deposit_is_in_review() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "100", Some(SENDER_A));
    fx.db
        .mark_erc20_deposits_needs_review_for_keys(
            TEST_CHAIN,
            &["0xaaa:1".to_string()],
            "sweep_review",
        )
        .unwrap();
    // D1's tokens are still at the address when D3 arrives.
    fx.token_deposit("0xccc", 3, "50", Some(SENDER_B));
    let t = sweep_tx(711);
    let rpc = Rpc::start(
        150,
        vec![t.clone()],
        vec![(
            t.clone(),
            ok_receipt(&t, 91, 1, TOKEN, &fx.addr, TARGET_A, 150),
        )],
    )
    .await;
    let sweeper = fx.sweeper(&rpc, target_for(TOKEN, |_| {}));
    sweeper.process_deposits_once().await.unwrap();
    sweeper.process_deposits_once().await.unwrap();
    assert_eq!(
        rpc.sent(),
        0,
        "nothing is swept on a pair with a deposit in review"
    );
    assert_eq!(fx.detected(), 1);
    // The alert naming the blocker was raised once.
    assert!(!fx
        .db
        .alert_once(TEST_CHAIN, "0xaaa:1", "pair_blocked")
        .unwrap());
    // After the operator's retry both are swept together.
    assert!(fx.db.retry_erc20_deposit(TEST_CHAIN, "0xaaa", 1).unwrap());
    sweeper.process_deposits_once().await.unwrap();
    assert_eq!(rpc.sent(), 1);
    assert!(fx.swept_webhook("0xaaa:1").is_some());
    assert!(fx.swept_webhook("0xccc:3").is_some());
}

#[tokio::test]
async fn per_transfer_pair_is_blocked_while_a_deposit_is_failed_or_in_review() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "100", Some(SENDER_A));
    fx.token_deposit("0xbbb", 2, "200", Some(SENDER_A));
    fx.db
        .mark_erc20_deposits_failed_for_keys(TEST_CHAIN, &["0xaaa:1".to_string()])
        .unwrap();
    // The balance covers only the live deposit's 200, not the failed one's 100.
    let rpc = Rpc::start(200, vec![sweep_tx(721)], vec![]).await;
    fx.sweeper(&rpc, target_for(TOKEN, |t| t.per_transfer = true))
        .process_deposits_once()
        .await
        .unwrap();
    assert_eq!(rpc.sent(), 0);
    assert_eq!(fx.detected(), 1);
}

#[tokio::test]
async fn retried_review_whose_transfer_landed_is_checked_again_not_zero_balance_settled() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "100", Some(SENDER_A));
    let tx = sweep_tx(731);
    let id = pending_intent(&fx, "0xaaa:1", "100", Some(&tx));
    fx.db.finish_sweep_intent(id, "review").unwrap();
    fx.db
        .mark_erc20_deposits_needs_review_for_keys(
            TEST_CHAIN,
            &["0xaaa:1".to_string()],
            "sweep_review",
        )
        .unwrap();
    // The operator retries; the transfer really did move the funds.
    assert!(fx.db.retry_erc20_deposit(TEST_CHAIN, "0xaaa", 1).unwrap());
    assert_eq!(
        fx.db
            .pending_sweep_intents(TEST_CHAIN, &["0xaaa:1".to_string()])
            .unwrap()
            .len(),
        1,
        "the review intent is pending again, so its receipt is re-checked"
    );
    let rpc = Rpc::start(
        0,
        vec![sweep_tx(732)],
        vec![(
            tx.clone(),
            ok_receipt(&tx, 92, 2, TOKEN, &fx.addr, TARGET_A, 100),
        )],
    )
    .await;
    fx.sweeper(&rpc, target_for(TOKEN, |_| {}))
        .process_deposits_once()
        .await
        .unwrap();
    assert_eq!(fx.db.get_sweeps(TEST_CHAIN, "0xaaa:1").unwrap().len(), 1);
    assert!(fx.swept_webhook("0xaaa:1").is_some());
}

#[tokio::test]
async fn zero_balance_rule_never_fires_when_an_intent_could_have_moved_funds() {
    let fx = fixture();
    fx.token_deposit("0xaaa", 1, "100", Some(SENDER_A));
    // A review intent without a hash (so retry could not re-check it): the
    // deposit is detected again, the address is empty, and 10+ passes must not
    // mark it swept.
    let id = pending_intent(&fx, "0xaaa:1", "100", None);
    fx.db.finish_sweep_intent(id, "review").unwrap();
    let rpc = Rpc::start(0, vec![sweep_tx(741)], vec![]).await;
    let sweeper = fx.sweeper(&rpc, target_for(TOKEN, |_| {}));
    for _ in 0..12 {
        sweeper.process_deposits_once().await.unwrap();
    }
    assert_eq!(fx.detected(), 1);
    assert!(fx.swept_webhook("0xaaa:1").is_none());
}

#[test]
fn split_plan_leaves_no_permanent_dust_at_the_part_limit() {
    let u = |n: u64| U256::from(n);
    // max=100, min=10, amount=1005: nine full parts now, then 105 -> 95 + 10.
    let first = plan_sweep_parts(u(1005), Some(u(10)), Some(u(100)), 10);
    assert_eq!(first, vec![u(100); 9]);
    let next = plan_sweep_parts(u(105), Some(u(10)), Some(u(100)), 10);
    assert_eq!(next, vec![u(95), u(10)]);
    let total = first
        .iter()
        .chain(next.iter())
        .fold(U256::ZERO, |a, p| a + *p);
    assert_eq!(total, u(1005), "everything is swept across the two passes");
}
