use crate::db::Db;
use crate::faucet::Faucet;
use crate::monitor::Monitor;
use crate::sweeper::Sweeper;
use crate::test_support::{
    chain_treasury, http_provider_boxed, test_chain_config_named, test_config,
    test_config_multichain, TEST_CHAIN,
};
use crate::traits::Service;
use crate::wallet::Wallet;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::time::sleep;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const BLOCK_NUMBER_HEX: &str = "0xA";
const BLOCK_HASH: &str = "0x000000000000000000000000000000000000000000000000000000000000000a";
const PARENT_HASH: &str = "0x0000000000000000000000000000000000000000000000000000000000000009";
const ROOT_HASH: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";
const SHARED_TX_HASH: &str = "0x0000000000000000000000000000000000000000000000000000000000000001";
const BASE_SWEEP_TX: &str = "0x00000000000000000000000000000000000000000000000000000000000000b1";
const POLYGON_SWEEP_TX: &str = "0x00000000000000000000000000000000000000000000000000000000000000b2";

#[tokio::test]
async fn test_e2e_deposit_sweep_flow() {
    let _ = tracing_subscriber::fmt::try_init();

    let rpc_server = MockServer::start().await;
    let webhook_server = MockServer::start().await;

    let db_file = NamedTempFile::new().unwrap();
    let db_path = db_file.path().to_str().unwrap();

    let config = test_config(db_path, rpc_server.uri());
    let chain_cfg = test_chain_config_named(TEST_CHAIN, rpc_server.uri());

    let wallet = Wallet::new(config.mnemonic.clone());
    let db = Db::new(&config.database_url).unwrap();

    let addr1 = wallet.derive_address(0).unwrap();
    let addr1_str = addr1.to_string();
    let webhook_url = webhook_server.uri();
    db.register_account("user_1", 0, &addr1_str, &webhook_url)
        .unwrap();

    let addr2 = wallet.derive_address(1).unwrap();
    let addr2_str = addr2.to_string();
    db.register_account("user_2", 1, &addr2_str, &webhook_url)
        .unwrap();

    let provider = http_provider_boxed(&rpc_server.uri());
    let treasury = chain_treasury(&config);

    mount_deposit_sweep_rpc_mocks(
        &rpc_server,
        &addr1_str,
        treasury.as_str(),
        SHARED_TX_HASH,
        "0x0000000000000000000000000000000000000000000000000000000000000002",
        "0x89",
    )
    .await;

    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2)
        .mount(&webhook_server)
        .await;

    spawn_chain_workers(
        chain_cfg,
        config.webhook_jwt_token.clone(),
        config.faucet_mnemonic.clone(),
        db.clone(),
        wallet.clone(),
        provider,
    );

    wait_until_detected(&db, TEST_CHAIN).await;
    wait_until_swept(&db, TEST_CHAIN).await;

    sleep(Duration::from_millis(500)).await;
}

#[tokio::test]
async fn test_e2e_multichain_same_address_both_swept() {
    let _ = tracing_subscriber::fmt::try_init();

    let base_rpc = MockServer::start().await;
    let polygon_rpc = MockServer::start().await;
    let webhook_server = MockServer::start().await;

    let db_file = NamedTempFile::new().unwrap();
    let db_path = db_file.path().to_str().unwrap();

    let config = test_config_multichain(db_path, base_rpc.uri(), polygon_rpc.uri());
    let base_cfg = test_chain_config_named("base", base_rpc.uri());
    let polygon_cfg = test_chain_config_named("polygon", polygon_rpc.uri());

    let wallet = Wallet::new(config.mnemonic.clone());
    let db = Db::new(&config.database_url).unwrap();

    let addr = wallet.derive_address(0).unwrap();
    let addr_str = addr.to_string();
    db.register_account("multichain_user", 0, &addr_str, &webhook_server.uri())
        .unwrap();

    mount_deposit_sweep_rpc_mocks(
        &base_rpc,
        &addr_str,
        &base_cfg.treasury_address,
        SHARED_TX_HASH,
        BASE_SWEEP_TX,
        "0x2105",
    )
    .await;
    mount_deposit_sweep_rpc_mocks(
        &polygon_rpc,
        &addr_str,
        &polygon_cfg.treasury_address,
        SHARED_TX_HASH,
        POLYGON_SWEEP_TX,
        "0x89",
    )
    .await;

    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(4)
        .mount(&webhook_server)
        .await;

    spawn_chain_workers(
        base_cfg.clone(),
        config.webhook_jwt_token.clone(),
        config.faucet_mnemonic.clone(),
        db.clone(),
        wallet.clone(),
        http_provider_boxed(&base_rpc.uri()),
    );
    spawn_chain_workers(
        polygon_cfg,
        config.webhook_jwt_token,
        config.faucet_mnemonic,
        db.clone(),
        wallet,
        http_provider_boxed(&polygon_rpc.uri()),
    );

    wait_until_detected(&db, "base").await;
    wait_until_detected(&db, "polygon").await;
    wait_until_swept(&db, "base").await;
    wait_until_swept(&db, "polygon").await;
}

#[tokio::test]
async fn test_e2e_one_chain_down_other_sweeps() {
    let _ = tracing_subscriber::fmt::try_init();

    let base_rpc = MockServer::start().await;
    let dead_rpc = MockServer::start().await;
    let webhook_server = MockServer::start().await;

    let db_file = NamedTempFile::new().unwrap();
    let db_path = db_file.path().to_str().unwrap();

    let config = test_config_multichain(db_path, base_rpc.uri(), dead_rpc.uri());
    let base_cfg = test_chain_config_named("base", base_rpc.uri());
    let dead_cfg = test_chain_config_named("polygon", dead_rpc.uri());

    let wallet = Wallet::new(config.mnemonic.clone());
    let db = Db::new(&config.database_url).unwrap();

    let addr = wallet.derive_address(0).unwrap();
    let addr_str = addr.to_string();
    db.register_account("isolated_user", 0, &addr_str, &webhook_server.uri())
        .unwrap();

    mount_deposit_sweep_rpc_mocks(
        &base_rpc,
        &addr_str,
        &base_cfg.treasury_address,
        SHARED_TX_HASH,
        BASE_SWEEP_TX,
        "0x2105",
    )
    .await;

    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).set_body_string("RPC unavailable"))
        .mount(&dead_rpc)
        .await;

    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2)
        .mount(&webhook_server)
        .await;

    spawn_chain_workers(
        base_cfg.clone(),
        config.webhook_jwt_token.clone(),
        config.faucet_mnemonic.clone(),
        db.clone(),
        wallet.clone(),
        http_provider_boxed(&base_rpc.uri()),
    );
    spawn_chain_workers(
        dead_cfg,
        config.webhook_jwt_token,
        config.faucet_mnemonic,
        db.clone(),
        wallet,
        http_provider_boxed(&dead_rpc.uri()),
    );

    wait_until_detected(&db, "base").await;
    wait_until_swept(&db, "base").await;

    for _ in 0..5 {
        let polygon_deposits = db.get_detected_deposits("polygon").unwrap();
        assert!(
            polygon_deposits.is_empty(),
            "dead chain must not record deposits"
        );
        sleep(Duration::from_millis(200)).await;
    }
}

fn spawn_chain_workers(
    chain_cfg: crate::config::ChainConfig,
    webhook_jwt_token: Option<String>,
    faucet_mnemonic: String,
    db: Db,
    wallet: Wallet,
    provider: alloy::providers::RootProvider<alloy::transports::BoxTransport>,
) {
    let monitor = Monitor::new(
        chain_cfg.clone(),
        webhook_jwt_token.clone(),
        db.clone(),
        provider.clone(),
    );
    let faucet = Arc::new(
        Faucet::new(
            faucet_mnemonic,
            provider.clone(),
            &chain_cfg.existential_deposit,
        )
        .unwrap(),
    );
    let sweeper = Sweeper::new(chain_cfg, webhook_jwt_token, db, wallet, provider, faucet);

    tokio::spawn(async move {
        monitor.run().await;
    });
    tokio::spawn(async move {
        sweeper.run().await;
    });
}

async fn wait_until_detected(db: &Db, chain: &str) {
    let mut detected = false;
    for _ in 0..20 {
        let deposits = db.get_detected_deposits(chain).unwrap();
        if !deposits.is_empty() {
            detected = true;
            break;
        }
        sleep(Duration::from_millis(500)).await;
    }
    assert!(detected, "Deposit should be detected on {chain}");
}

async fn wait_until_swept(db: &Db, chain: &str) {
    let mut swept = false;
    for _ in 0..20 {
        let deposits = db.get_detected_deposits(chain).unwrap();
        if deposits.is_empty() {
            swept = true;
            break;
        }
        sleep(Duration::from_millis(500)).await;
    }
    assert!(swept, "Deposit should be swept on {chain}");
}

async fn mount_deposit_sweep_rpc_mocks(
    rpc_server: &MockServer,
    deposit_addr: &str,
    treasury: &str,
    tx_hash: &str,
    sweep_tx_hash: &str,
    chain_id_hex: &str,
) {
    Mock::given(method("POST"))
        .and(body_json_contains("eth_blockNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": BLOCK_NUMBER_HEX
        })))
        .mount(rpc_server)
        .await;

    let block_response = block_with_deposit(deposit_addr, tx_hash);

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getBlockByNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(block_response))
        .mount(rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getBalance"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": "0x0DE0B6B3A7640000"
        })))
        .mount(rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_gasPrice"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": "0x3B9ACA00"
        })))
        .mount(rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_feeHistory"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "baseFeePerGas": ["0x3B9ACA00", "0x3B9ACA00"],
                "gasUsedRatio": [0.5],
                "oldestBlock": "0x9",
                "reward": [["0x3B9ACA00"]]
            }
        })))
        .mount(rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_maxPriorityFeePerGas"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": "0x3B9ACA00"
        })))
        .mount(rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getTransactionCount"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": "0x00"
        })))
        .mount(rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_sendRawTransaction"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": sweep_tx_hash
        })))
        .mount(rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getTransactionReceipt"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "transactionHash": sweep_tx_hash,
                "transactionIndex": "0x1",
                "blockHash": BLOCK_HASH,
                "blockNumber": "0xB",
                "from": deposit_addr,
                "to": treasury,
                "cumulativeGasUsed": "0x5208",
                "gasUsed": "0x5208",
                "contractAddress": null,
                "logs": [],
                "status": "0x1",
                "logsBloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
                "type": "0x0",
                "effectiveGasPrice": "0x3B9ACA00"
            }
        })))
        .mount(rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_chainId"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": chain_id_hex
        })))
        .mount(rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getLogs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": []
        })))
        .mount(rpc_server)
        .await;
}

fn block_with_deposit(deposit_addr: &str, tx_hash: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "number": BLOCK_NUMBER_HEX,
            "hash": BLOCK_HASH,
            "parentHash": PARENT_HASH,
            "nonce": "0x0000000000000000",
            "sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
            "logsBloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
            "transactionsRoot": ROOT_HASH,
            "stateRoot": ROOT_HASH,
            "receiptsRoot": ROOT_HASH,
            "miner": "0x0000000000000000000000000000000000000000",
            "difficulty": "0x0",
            "totalDifficulty": "0x0",
            "extraData": "0x",
            "size": "0x0",
            "gasLimit": "0x0",
            "gasUsed": "0x0",
            "timestamp": "0x0",
            "transactions": [
                {
                    "hash": tx_hash,
                    "nonce": "0x0",
                    "blockHash": BLOCK_HASH,
                    "blockNumber": BLOCK_NUMBER_HEX,
                    "transactionIndex": "0x0",
                    "from": "0x0000000000000000000000000000000000000000",
                    "to": deposit_addr,
                    "value": "0xDE0B6B3A7640000",
                    "gas": "0x5208",
                    "gasPrice": "0x3B9ACA00",
                    "input": "0x",
                    "v": "0x1b",
                    "r": "0x1",
                    "s": "0x1",
                    "type": "0x0",
                    "chainId": "0x1"
                }
            ],
            "uncles": []
        }
    })
}

fn body_json_contains(substring: &str) -> impl wiremock::Match {
    BodyContains(substring.to_string())
}

struct BodyContains(String);
impl wiremock::Match for BodyContains {
    fn matches(&self, request: &wiremock::Request) -> bool {
        let body_str = String::from_utf8_lossy(&request.body);
        body_str.contains(&self.0)
    }
}
