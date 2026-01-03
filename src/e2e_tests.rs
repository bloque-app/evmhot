use crate::config::{Config, ProviderUrl};
use crate::db::Db;
use crate::faucet::Faucet;
use crate::monitor::Monitor;
use crate::sweeper::Sweeper;
use crate::traits::Service;
use crate::wallet::Wallet;
use alloy::providers::ProviderBuilder;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::time::sleep;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn test_e2e_deposit_sweep_flow() {
    let _ = tracing_subscriber::fmt::try_init();

    // 1. Setup Mock RPC
    let rpc_server = MockServer::start().await;
    let webhook_server = MockServer::start().await;

    // 2. Setup Config & DB
    let db_file = NamedTempFile::new().unwrap();
    let db_path = db_file.path().to_str().unwrap();

    let config = Config {
        database_url: db_path.to_string(),
        provider_url: ProviderUrl::Http(rpc_server.uri()),
        mnemonic: "test test test test test test test test test test test junk".to_string(),
        treasury_address: "0x9999999999999999999999999999999999999999".to_string(),
        port: 3001,
        poll_interval: 1,
        faucet_mnemonic: "test test test test test test test test test test test junk".to_string(),
        existential_deposit: "10000000000000000".to_string(),
        faucet_address: "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266".to_string(),
        block_offset_from_head: 0, // Use 0 for tests to avoid underflow with low block numbers
        get_logs_max_retries: 30,
        get_logs_delay_ms: 50,
    };

    let wallet = Wallet::new(config.mnemonic.clone());
    let db = Db::new(&config.database_url).unwrap();

    // 3. Register Users
    // User 1 -> Index 0
    let addr1 = wallet.derive_address(0).unwrap();
    let addr1_str = addr1.to_string();
    let webhook_url = webhook_server.uri();
    db.register_account("user_1", 0, &addr1_str, &webhook_url)
        .unwrap();

    // User 2 -> Index 1
    let addr2 = wallet.derive_address(1).unwrap();
    let addr2_str = addr2.to_string();
    db.register_account("user_2", 1, &addr2_str, &webhook_url)
        .unwrap();

    // 4. Initialize Provider
    let provider = ProviderBuilder::new().on_http(rpc_server.uri().parse().unwrap());

    // 5. Mock RPC Responses

    // eth_blockNumber (Start at 10, increment)
    Mock::given(method("POST"))
        .and(body_json_contains("eth_blockNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": "0xA" // 10
        })))
        .mount(&rpc_server)
        .await;

    // eth_getBlockByNumber (Block 10 with deposit for User 1)
    let block_hash = "0x000000000000000000000000000000000000000000000000000000000000000a";
    let parent_hash = "0x0000000000000000000000000000000000000000000000000000000000000009";
    let tx_hash = "0x0000000000000000000000000000000000000000000000000000000000000001";
    let root_hash = "0x0000000000000000000000000000000000000000000000000000000000000000";

    let block_10_response = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "number": "0xA",
            "hash": block_hash,
            "parentHash": parent_hash,
            "nonce": "0x0000000000000000",
            "sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
            "logsBloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
            "transactionsRoot": root_hash,
            "stateRoot": root_hash,
            "receiptsRoot": root_hash,
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
                    "blockHash": block_hash,
                    "blockNumber": "0xA",
                    "transactionIndex": "0x0",
                    "from": "0x0000000000000000000000000000000000000000",
                    "to": addr1_str, // Use the derived address string
                    "value": "0xDE0B6B3A7640000", // 1 ETH
                    "gas": "0x5208", // 21000
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
    });

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getBlockByNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(block_10_response))
        .mount(&rpc_server)
        .await;

    // eth_getBalance (Return 1 ETH for User 1)
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getBalance"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": "0x0DE0B6B3A7640000" // 1 ETH (padded to even length)
        })))
        .mount(&rpc_server)
        .await;

    // eth_gasPrice
    Mock::given(method("POST"))
        .and(body_json_contains("eth_gasPrice"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": "0x3B9ACA00" // 1 Gwei
        })))
        .mount(&rpc_server)
        .await;

    // eth_getTransactionCount (Nonce)
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getTransactionCount"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": "0x00"
        })))
        .mount(&rpc_server)
        .await;

    // eth_sendRawTransaction (Sweep)
    let sweep_tx_hash = "0x0000000000000000000000000000000000000000000000000000000000000002";
    Mock::given(method("POST"))
        .and(body_json_contains("eth_sendRawTransaction"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": sweep_tx_hash
        })))
        .mount(&rpc_server)
        .await;

    // eth_getTransactionReceipt (Confirm sweep)
    Mock::given(method("POST"))
            .and(body_json_contains("eth_getTransactionReceipt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "transactionHash": sweep_tx_hash,
                    "transactionIndex": "0x1",
                    "blockHash": block_hash,
                    "blockNumber": "0xB", // Next block
                    "from": addr1_str,
                    "to": config.treasury_address,
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
            .mount(&rpc_server)
            .await;

    // eth_chainId
    Mock::given(method("POST"))
        .and(body_json_contains("eth_chainId"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": "0x89" // Polygon 137
        })))
        .mount(&rpc_server)
        .await;

    // eth_getLogs (for ERC20 Transfer events - return empty array)
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getLogs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": []
        })))
        .mount(&rpc_server)
        .await;

    // Webhook Expectation (exactly 2 calls: 1 deposit_detected + 1 deposit_swept)
    // Now that we have duplicate detection, we should only get exactly 2 webhook calls
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2)
        .mount(&webhook_server)
        .await;

    // 5. Run Monitor & Sweeper
    let monitor = Monitor::new(config.clone(), db.clone(), provider.clone());
    let faucet = Arc::new(
        Faucet::new(
            config.faucet_mnemonic.clone(),
            provider.clone(),
            &config.existential_deposit,
        )
        .unwrap(),
    );
    let sweeper = Sweeper::new(config.clone(), db.clone(), wallet.clone(), provider.clone(), faucet);

    // Run monitor once (manually or spawn short lived)
    // We can't easily "run once" with the loop, but we can spawn and wait a bit.
    // For testability, it's better if Monitor/Sweeper had a `run_once` method, but we can just let them run.

    let _monitor_handle = tokio::spawn(async move {
        monitor.run().await;
    });

    let _sweeper_handle = tokio::spawn(async move {
        sweeper.run().await;
    });

    // 6. Wait and Verify
    // Wait for deposit detection
    let mut detected = false;
    for _ in 0..10 {
        let deposits = db.get_detected_deposits().unwrap();
        if !deposits.is_empty() {
            detected = true;
            break;
        }
        sleep(Duration::from_millis(500)).await;
    }
    assert!(detected, "Deposit should be detected");

    // Wait for sweep (status change in DB)
    let mut swept = false;
    for _ in 0..10 {
        // We don't have a direct "get_swept_deposits" but we can check if detected list is empty
        // assuming we only had one. Or check DB directly if we exposed a method.
        // Let's check if detected becomes empty.
        let deposits = db.get_detected_deposits().unwrap();
        if deposits.is_empty() {
            swept = true;
            break;
        }
        sleep(Duration::from_millis(500)).await;
    }
    assert!(swept, "Deposit should be swept");

    // Verify Webhook (Wiremock expectation)
    // The expectation is checked on Drop or manually.
    // Since we are in a test, we can just wait a bit for the async call to finish.
    sleep(Duration::from_millis(500)).await;
}

// Helper matcher
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
