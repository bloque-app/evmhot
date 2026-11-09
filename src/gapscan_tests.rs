//! Gap scan tests against a mocked JSON-RPC: Multicall3 balances, filtered
//! `eth_getLogs`, and the real store (SQLite) for apply.

use crate::config::Config;
use crate::db::Db;
use crate::gapscan::*;
use crate::test_support::{
    hash32, http_provider_boxed, test_config, transfer_log_json, TEST_CHAIN, TEST_FAUCET,
};
use alloy::primitives::{Address, U256};
use alloy::sol_types::SolCall;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tempfile::{NamedTempFile, TempDir};
use wiremock::{Mock, MockServer, ResponseTemplate};

const HEAD: u64 = 1_000;

fn addr(n: u8) -> Address {
    Address::from([n; 20])
}

fn token() -> Address {
    addr(0xA1)
}

#[derive(Default)]
struct Seen {
    /// Size of each Multicall3 batch, with the block it was read at.
    multicalls: Vec<(usize, String)>,
    /// (from, to, topic2 recipients) of each getLogs request.
    log_queries: Vec<(u64, u64, Vec<String>)>,
}

#[derive(Clone)]
struct Chain {
    balances: HashMap<(Address, Address), U256>,
    logs: Vec<Value>,
    /// getLogs refuses ranges wider than this many blocks.
    max_span: Option<u64>,
    seen: Arc<Mutex<Seen>>,
}

impl Chain {
    fn new() -> Self {
        Self {
            balances: HashMap::new(),
            logs: Vec::new(),
            max_span: None,
            seen: Arc::new(Mutex::new(Seen::default())),
        }
    }

    fn handle(&self, body: &Value) -> Value {
        let id = body["id"].clone();
        let params = &body["params"];
        let result = match body["method"].as_str().unwrap() {
            "eth_blockNumber" => json!(format!("0x{HEAD:x}")),
            "eth_call" => {
                let call = &params[0];
                let data = call["input"]
                    .as_str()
                    .or_else(|| call["data"].as_str())
                    .unwrap();
                let data = hex::decode(data.trim_start_matches("0x")).unwrap();
                let agg = IMulticall3::aggregate3Call::abi_decode(&data, true).unwrap();
                self.seen
                    .lock()
                    .unwrap()
                    .multicalls
                    .push((agg.calls.len(), params[1].to_string()));
                let results: Vec<IMulticall3::CallResult> = agg
                    .calls
                    .iter()
                    .map(|c| {
                        let who = IBalance::balanceOfCall::abi_decode(&c.callData, true)
                            .unwrap()
                            .account;
                        let bal = self
                            .balances
                            .get(&(c.target, who))
                            .copied()
                            .unwrap_or(U256::ZERO);
                        IMulticall3::CallResult {
                            success: true,
                            returnData: bal.to_be_bytes::<32>().to_vec().into(),
                        }
                    })
                    .collect();
                let out = IMulticall3::aggregate3Call::abi_encode_returns(&(results,));
                json!(format!("0x{}", hex::encode(out)))
            }
            "eth_getLogs" => {
                let f = &params[0];
                let from = u64::from_str_radix(f["fromBlock"].as_str().unwrap()[2..].as_ref(), 16)
                    .unwrap();
                let to =
                    u64::from_str_radix(f["toBlock"].as_str().unwrap()[2..].as_ref(), 16).unwrap();
                let topic2: Vec<String> = match &f["topics"][2] {
                    Value::String(s) => vec![s.to_lowercase()],
                    Value::Array(a) => a
                        .iter()
                        .map(|v| v.as_str().unwrap().to_lowercase())
                        .collect(),
                    _ => vec![],
                };
                self.seen
                    .lock()
                    .unwrap()
                    .log_queries
                    .push((from, to, topic2.clone()));
                if let Some(max) = self.max_span {
                    if to - from + 1 > max {
                        return json!({"jsonrpc":"2.0","id":id,
                            "error":{"code":-32005,"message":"query exceeds max block range"}});
                    }
                }
                let hits: Vec<&Value> = self
                    .logs
                    .iter()
                    .filter(|l| {
                        let b = u64::from_str_radix(
                            l["blockNumber"].as_str().unwrap()[2..].as_ref(),
                            16,
                        )
                        .unwrap();
                        b >= from
                            && b <= to
                            && topic2.contains(&l["topics"][2].as_str().unwrap().to_lowercase())
                    })
                    .collect();
                json!(hits)
            }
            other => panic!("unexpected rpc method {other}"),
        };
        json!({"jsonrpc":"2.0","id":id,"result":result})
    }

    async fn serve(&self) -> MockServer {
        let server = MockServer::start().await;
        let me = self.clone();
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(move |req: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                ResponseTemplate::new(200).set_body_json(me.handle(&body))
            })
            .mount(&server)
            .await;
        server
    }
}

fn log(token: Address, from: &str, to: Address, amount: u128, block: u64, idx: u64) -> Value {
    transfer_log_json(
        &token.to_string(),
        from,
        &to.to_string(),
        amount,
        idx,
        &hash32(block * 100 + idx),
        block,
        0,
    )
}

struct Fixture {
    db: Db,
    _file: NamedTempFile,
    dir: TempDir,
    config: Config,
    regs: Vec<Registration>,
}

fn fixture(rpc_url: &str, holders: u8) -> Fixture {
    let file = NamedTempFile::new().unwrap();
    let mut config = test_config(file.path().to_str().unwrap(), rpc_url);
    config.chains[0]
        .allowed_token_addresses
        .insert(token().to_string().to_lowercase());
    config.chains[0]
        .min_deposits
        .per_token
        .insert(token().to_string().to_lowercase(), U256::from(100u64));
    let db = Db::new(file.path().to_str().unwrap()).unwrap();
    db.store_token_metadata(TEST_CHAIN, &token().to_string(), "USDT", 6, "Tether")
        .unwrap();
    let mut regs = Vec::new();
    for i in 1..=holders {
        let a = addr(i);
        db.register_account(
            &format!("reg-{i}"),
            i as u32,
            &a.to_string(),
            "http://hook.test/h",
        )
        .unwrap();
        regs.push(Registration {
            id: format!("reg-{i}"),
            index: i as u32,
            address: a,
        });
    }
    Fixture {
        db,
        _file: file,
        dir: TempDir::new().unwrap(),
        config,
        regs,
    }
}

fn opts(from: u64) -> ScanOptions {
    ScanOptions {
        from_block: Some(from),
        retries: 1,
        ..ScanOptions::default()
    }
}

fn run_opts(fx: &Fixture, scan: ScanOptions) -> RunOptions {
    RunOptions {
        scan,
        csv: fx.dir.path().join("report.csv"),
    }
}

type LogQuery = (u64, u64, Vec<String>);

fn seen_of(c: &Chain) -> (Vec<(usize, String)>, Vec<LogQuery>) {
    let s = c.seen.lock().unwrap();
    (s.multicalls.clone(), s.log_queries.clone())
}

#[tokio::test]
async fn balances_are_batched_at_one_block_and_zero_balances_are_skipped() {
    let mut chain = Chain::new();
    chain
        .balances
        .insert((token(), addr(2)), U256::from(500u64));
    chain
        .balances
        .insert((token(), addr(5)), U256::from(700u64));
    chain
        .logs
        .push(log(token(), &addr(0xEE).to_string(), addr(2), 500, 50, 0));
    chain
        .logs
        .push(log(token(), &addr(0xEE).to_string(), addr(5), 700, 60, 1));
    let server = chain.serve().await;
    let fx = fixture(&server.uri(), 7);
    let provider = http_provider_boxed(&server.uri());
    let mut o = opts(1);
    o.batch_size = 3;

    let report = scan_chain(&fx.db, &fx.config.chains[0], &provider, &fx.regs, &o)
        .await
        .unwrap();

    let (multicalls, queries) = seen_of(&chain);
    let sizes: Vec<usize> = multicalls.iter().map(|m| m.0).collect();
    assert_eq!(sizes, vec![3, 3, 1]);
    let blocks: std::collections::HashSet<_> = multicalls.iter().map(|m| m.1.clone()).collect();
    assert_eq!(blocks.len(), 1, "every balance read uses the same block");
    assert!(blocks
        .iter()
        .next()
        .unwrap()
        .contains(&format!("{:#x}", HEAD)));

    assert_eq!(report.block, HEAD);
    assert_eq!(report.holders.len(), 2);
    // Only the two holders appear in any log query.
    let asked: std::collections::HashSet<String> =
        queries.iter().flat_map(|q| q.2.clone()).collect();
    assert_eq!(asked.len(), 2);
    assert!(!report.has_residue());
    assert_eq!(report.deposit_count(), 2);
}

#[tokio::test]
async fn logs_are_filtered_by_faucet_minimum_and_token() {
    let mut chain = Chain::new();
    chain
        .balances
        .insert((token(), addr(1)), U256::from(1_250u64));
    chain
        .logs
        .push(log(token(), &addr(0xEE).to_string(), addr(1), 1_000, 10, 0));
    chain
        .logs
        .push(log(token(), TEST_FAUCET, addr(1), 150, 11, 1));
    chain
        .logs
        .push(log(token(), &addr(0xEE).to_string(), addr(1), 99, 12, 2));
    // A log of a token that is not admitted must be ignored altogether.
    chain.logs.push(log(
        addr(0xB2),
        &addr(0xEE).to_string(),
        addr(1),
        5_000,
        13,
        3,
    ));
    let server = chain.serve().await;
    let fx = fixture(&server.uri(), 1);
    let provider = http_provider_boxed(&server.uri());

    let report = scan_chain(&fx.db, &fx.config.chains[0], &provider, &fx.regs, &opts(1))
        .await
        .unwrap();

    let h = &report.holders[0];
    let verdicts: Vec<Verdict> = h.transfers.iter().map(|t| t.verdict).collect();
    assert_eq!(
        verdicts,
        vec![Verdict::Deposit, Verdict::Faucet, Verdict::BelowMinimum]
    );
    assert_eq!(report.deposit_count(), 1);
    // 1000 + 150 + 99 explain 1249 of 1250: the rest is reported as residue.
    assert_eq!(h.residue(), U256::from(1u64));
}

#[tokio::test]
async fn a_refused_range_is_split_and_nothing_is_lost_or_duplicated() {
    let mut chain = Chain::new();
    chain.max_span = Some(10);
    chain
        .balances
        .insert((token(), addr(1)), U256::from(3_000u64));
    for (i, block) in [3u64, 17, 18, 38].iter().enumerate() {
        chain.logs.push(log(
            token(),
            &addr(0xEE).to_string(),
            addr(1),
            1_000,
            *block,
            i as u64,
        ));
    }
    // Only three of the four are inbound within 1..=40 for the balance below.
    chain
        .balances
        .insert((token(), addr(1)), U256::from(4_000u64));
    let server = chain.serve().await;
    let fx = fixture(&server.uri(), 1);
    let provider = http_provider_boxed(&server.uri());
    let mut o = opts(1);
    o.block = Some(40);
    o.log_range = 40;

    let report = scan_chain(&fx.db, &fx.config.chains[0], &provider, &fx.regs, &o)
        .await
        .unwrap();

    assert_eq!(report.block, 40);
    assert_eq!(report.holders[0].transfers.len(), 4);
    assert!(report.counters.log_refusals >= 3, "{:?}", report.counters);
    let (_, queries) = seen_of(&chain);
    assert!(
        queries.iter().any(|q| q.0 == 1 && q.1 == 40),
        "tried the whole range first"
    );
    assert!(
        queries.iter().any(|q| q.1 - q.0 < 10),
        "then ranges the provider accepts"
    );
    assert!(!report.has_residue());
}

#[tokio::test]
async fn dry_run_writes_nothing_to_the_store_but_writes_the_csv() {
    let mut chain = Chain::new();
    chain
        .balances
        .insert((token(), addr(1)), U256::from(1_000u64));
    chain
        .logs
        .push(log(token(), &addr(0xEE).to_string(), addr(1), 1_000, 10, 0));
    let server = chain.serve().await;
    let fx = fixture(&server.uri(), 2);
    fx.db.set_last_processed_block(TEST_CHAIN, 5).unwrap();

    let o = run_opts(
        &fx,
        ScanOptions {
            retries: 1,
            ..ScanOptions::default()
        },
    );
    let reports = run(&fx.config, &fx.db, &o).await.unwrap();

    assert_eq!(reports[0].deposit_count(), 1);
    assert!(reports[0].applied.is_none());
    assert!(fx
        .db
        .get_detected_erc20_deposits(TEST_CHAIN)
        .unwrap()
        .is_empty());
    assert_eq!(fx.db.get_last_processed_block(TEST_CHAIN).unwrap(), 5);
    let id = format!("{TEST_CHAIN}:{}:0", hash32(1_000));
    assert!(fx
        .db
        .get_webhook_delivery(&id, "deposit_detected")
        .unwrap()
        .is_none());

    let csv = std::fs::read_to_string(&o.csv).unwrap();
    let lines: Vec<&str> = csv.lines().collect();
    assert!(lines[0].starts_with("chain,block,address,registration_id,token,balance_at_block"));
    assert_eq!(lines.len(), 2);
    assert!(lines[1].contains(&addr(1).to_string()));
    assert!(lines[1].contains("reg-1"));
    assert!(lines[1].contains(&hash32(1_000)));
    assert!(lines[1].ends_with(",deposit"));
}

#[tokio::test]
async fn apply_stores_each_deposit_once_with_sender_and_outbox_row() {
    let mut chain = Chain::new();
    chain
        .balances
        .insert((token(), addr(1)), U256::from(1_300u64));
    chain
        .balances
        .insert((token(), addr(2)), U256::from(400u64));
    let sender = addr(0xEE).to_string();
    chain
        .logs
        .push(log(token(), &sender, addr(1), 1_000, 10, 4));
    chain.logs.push(log(token(), &sender, addr(1), 300, 20, 7));
    chain.logs.push(log(token(), &sender, addr(2), 400, 30, 2));
    let server = chain.serve().await;
    let fx = fixture(&server.uri(), 3);
    fx.db.set_last_processed_block(TEST_CHAIN, 5).unwrap();

    let o = run_opts(
        &fx,
        ScanOptions {
            apply: true,
            retries: 1,
            ..ScanOptions::default()
        },
    );
    let first = run(&fx.config, &fx.db, &o).await.unwrap();
    let a = first[0].applied.clone().unwrap();
    assert_eq!((a.new_deposits, a.already_known), (3, 0));
    assert_eq!(a.cursor_set_to, None);

    let stored = fx.db.get_detected_erc20_deposits(TEST_CHAIN).unwrap();
    assert_eq!(stored.len(), 3);
    for d in &stored {
        assert_eq!(d.from_address.as_deref(), Some(sender.as_str()));
        assert_eq!(d.token_address, token().to_string());
    }
    // Identity carries the block-level log index.
    let id = format!("{TEST_CHAIN}:{}:7", hash32(2_000 + 7));
    let row = fx
        .db
        .get_webhook_delivery(&id, "deposit_detected")
        .unwrap()
        .expect("deposit_detected outbox row");
    let payload: Value = serde_json::from_str(&row.payload).unwrap();
    assert_eq!(payload["from_address"], sender.as_str());
    assert_eq!(payload["amount"], "300");
    assert_eq!(payload["registration_id"], "reg-1");
    assert_eq!(payload["token_symbol"], "USDT");

    // Running it again stores nothing new.
    let second = run(&fx.config, &fx.db, &o).await.unwrap();
    let b = second[0].applied.clone().unwrap();
    assert_eq!((b.new_deposits, b.already_known), (0, 3));
    assert_eq!(
        fx.db.get_detected_erc20_deposits(TEST_CHAIN).unwrap().len(),
        3
    );
    // The cursor was not touched.
    assert_eq!(fx.db.get_last_processed_block(TEST_CHAIN).unwrap(), 5);
}

#[tokio::test]
async fn set_cursor_moves_it_to_the_block_after_applying() {
    let mut chain = Chain::new();
    chain
        .balances
        .insert((token(), addr(1)), U256::from(1_000u64));
    chain
        .logs
        .push(log(token(), &addr(0xEE).to_string(), addr(1), 1_000, 10, 0));
    let server = chain.serve().await;
    let fx = fixture(&server.uri(), 1);
    fx.db.set_last_processed_block(TEST_CHAIN, 5).unwrap();

    let o = run_opts(
        &fx,
        ScanOptions {
            apply: true,
            set_cursor: true,
            block: Some(900),
            chains: vec![TEST_CHAIN.to_string()],
            retries: 1,
            ..ScanOptions::default()
        },
    );
    let reports = run(&fx.config, &fx.db, &o).await.unwrap();
    assert_eq!(reports[0].applied.clone().unwrap().cursor_set_to, Some(900));
    assert_eq!(fx.db.get_last_processed_block(TEST_CHAIN).unwrap(), 900);
}

#[tokio::test]
async fn flags_that_would_skip_part_of_the_chain_are_refused() {
    let file = NamedTempFile::new().unwrap();
    let config = test_config(file.path().to_str().unwrap(), "http://127.0.0.1:1");
    let base = ScanOptions::default();
    let err = |o: ScanOptions| validate(&config, &o).unwrap_err().to_string();

    assert!(err(ScanOptions {
        set_cursor: true,
        block: Some(5),
        ..base.clone()
    })
    .contains("--apply"));
    assert!(err(ScanOptions {
        set_cursor: true,
        apply: true,
        ..base.clone()
    })
    .contains("--block"));
    assert!(err(ScanOptions {
        include_native: true,
        ..base.clone()
    })
    .contains("reserved"));
    assert!(err(ScanOptions {
        chains: vec!["nope".into()],
        ..base.clone()
    })
    .contains("unknown network"));
    assert!(validate(&config, &base).is_ok());
}

#[tokio::test]
async fn a_residue_is_reported_loudly_and_in_the_csv() {
    let mut chain = Chain::new();
    // 1000 arrived in the gap, but the address holds 1700: 700 is unexplained.
    chain
        .balances
        .insert((token(), addr(1)), U256::from(1_700u64));
    chain
        .logs
        .push(log(token(), &addr(0xEE).to_string(), addr(1), 1_000, 10, 0));
    // Fully explained holder, for contrast.
    chain
        .balances
        .insert((token(), addr(2)), U256::from(250u64));
    chain
        .logs
        .push(log(token(), &addr(0xEE).to_string(), addr(2), 250, 11, 1));
    let server = chain.serve().await;
    let fx = fixture(&server.uri(), 2);
    let provider = http_provider_boxed(&server.uri());

    let report = scan_chain(&fx.db, &fx.config.chains[0], &provider, &fx.regs, &opts(1))
        .await
        .unwrap();

    assert!(report.has_residue());
    assert_eq!(report.holders[0].residue(), U256::from(700u64));
    assert_eq!(report.holders[1].residue(), U256::ZERO);
    let summary = render_summary(std::slice::from_ref(&report), false);
    assert!(summary.contains("UNEXPLAINED BALANCE"), "{summary}");
    assert!(summary.contains("DRY RUN"));
    assert!(summary.contains("1 deposits found") || summary.contains("2 deposits found"));
    let csv = render_csv(&[report]);
    assert!(csv
        .lines()
        .any(|l| l.contains(&addr(1).to_string()) && l.contains(",700,")));

    // A clean scan prints no banner.
    let mut clean = Chain::new();
    clean
        .balances
        .insert((token(), addr(2)), U256::from(250u64));
    clean
        .logs
        .push(log(token(), &addr(0xEE).to_string(), addr(2), 250, 11, 1));
    let server2 = clean.serve().await;
    let provider2 = http_provider_boxed(&server2.uri());
    let ok = scan_chain(&fx.db, &fx.config.chains[0], &provider2, &fx.regs, &opts(1))
        .await
        .unwrap();
    assert!(!render_summary(&[ok], false).contains("UNEXPLAINED"));
}

#[tokio::test]
async fn an_empty_cursor_without_from_is_an_error() {
    let chain = Chain::new();
    let server = chain.serve().await;
    let fx = fixture(&server.uri(), 1);
    let provider = http_provider_boxed(&server.uri());
    let err = scan_chain(
        &fx.db,
        &fx.config.chains[0],
        &provider,
        &fx.regs,
        &ScanOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("--from"));
}

#[tokio::test]
async fn registrations_are_listed_in_derivation_order() {
    let fx = fixture("http://127.0.0.1:1", 3);
    let regs = fx.db.list_registrations().unwrap();
    assert_eq!(regs.len(), 3);
    assert_eq!(regs[0].0, "reg-1");
    assert_eq!(regs[2].1, 3);
    assert_eq!(regs[1].2, addr(2).to_string());
}
