use crate::config::{parse_allowed_token_addresses_env, Config, MinDepositSettings};
use crate::db::Db;
use crate::faucet::Faucet;
use crate::monitor::Monitor;
use crate::sweeper::Sweeper;
use crate::test_support::{
    self, http_provider_boxed, test_chain_config, test_config, test_webhook_deliverer, TEST_CHAIN,
};
use crate::traits::Service;
use crate::wallet::Wallet;
use crate::webhook::{WebhookDeliverer, WebhookRetryService};
use crate::{
    HotWalletService, RegisterRequest, RetrySweepRequest, RetryWebhookRequest,
    VerifyTransferRequest, VerifyTransferResponse,
};
use serde_json::json;
use std::sync::Arc;
use tempfile::NamedTempFile;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn test_wallet_derivation() {
    let mnemonic = "test test test test test test test test test test test junk";
    let wallet = Wallet::new(mnemonic.to_string());

    let addr1 = wallet.derive_address(0).unwrap();
    let addr2 = wallet.derive_address(1).unwrap();

    assert_ne!(addr1, addr2);
    // Known address for index 0 of this mnemonic
    assert_eq!(
        addr1.to_string().to_lowercase(),
        "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
    );
}

#[test]
fn test_db_operations() {
    let tmp_file = NamedTempFile::new().unwrap();
    let db_path = tmp_file.path().to_str().unwrap();
    let db = Db::new(db_path).unwrap();

    // Test Account Registration
    let id = "user_1";
    let index = 0;
    let address = "0x123";

    db.register_account(id, index, address, "https://webhook.example.com")
        .unwrap();

    let fetched_addr = db.get_account_by_id(id).unwrap().unwrap().1;
    assert_eq!(fetched_addr, address);

    let fetched_id = db.get_account_by_address(address).unwrap().unwrap();
    assert_eq!(fetched_id, id);

    // Test Index Increment
    let next_idx = db.get_next_derivation_index().unwrap();
    assert_eq!(next_idx, 1);

    // Test Deposits
    let tx_hash = "0xabc";
    let amount = "100";
    db.record_deposit(TEST_CHAIN, tx_hash, id, amount).unwrap();

    let deposits = db.get_detected_deposits(TEST_CHAIN).unwrap();
    assert_eq!(deposits.len(), 1);
    assert_eq!(deposits[0].0, tx_hash);
    assert_eq!(deposits[0].2, amount);

    // Test Sweep Mark
    db.mark_deposit_swept(TEST_CHAIN, tx_hash).unwrap();
    let deposits_after = db.get_detected_deposits(TEST_CHAIN).unwrap();
    assert_eq!(deposits_after.len(), 0);
}

// ========== Monitor Unit Tests ==========

#[tokio::test]
async fn test_monitor_creation_with_http_provider() {
    // Test that Monitor can be created with an HTTP provider
    let db_file = NamedTempFile::new().unwrap();
    let db = Db::new(db_file.path().to_str().unwrap()).unwrap();

    let _config = test_config(db_file.path().to_str().unwrap(), "http://localhost:8545");

    // Create provider and monitor (no actual connection needed for this test)
    let provider = http_provider_boxed("http://localhost:8545");
    let deliverer = test_webhook_deliverer(db.clone());
    let _monitor = Monitor::new(
        test_chain_config("http://localhost:8545"),
        deliverer,
        db.clone(),
        provider,
    );
}

#[test]
fn test_monitor_db_operations() {
    // Test Monitor's interaction with DB for deposit tracking
    let db_file = NamedTempFile::new().unwrap();
    let db = Db::new(db_file.path().to_str().unwrap()).unwrap();

    let wallet =
        Wallet::new("test test test test test test test test test test test junk".to_string());
    let user_address = wallet.derive_address(0).unwrap().to_string();

    // Register account
    db.register_account("test_user", 0, &user_address, "https://webhook.example.com")
        .unwrap();

    // Verify no deposits initially
    let deposits_before = db.get_detected_deposits(TEST_CHAIN).unwrap();
    assert_eq!(deposits_before.len(), 0);

    // Simulate Monitor recording a deposit
    db.record_deposit(TEST_CHAIN, "0xtxhash", "test_user", "1000000000000000000")
        .unwrap();

    let deposits_after = db.get_detected_deposits(TEST_CHAIN).unwrap();
    assert_eq!(deposits_after.len(), 1);
    assert_eq!(deposits_after[0].0, "0xtxhash");
    assert_eq!(deposits_after[0].1, "test_user");
    assert_eq!(deposits_after[0].2, "1000000000000000000");

    // Test block tracking
    db.set_last_processed_block(TEST_CHAIN, 100).unwrap();
    assert_eq!(db.get_last_processed_block(TEST_CHAIN).unwrap(), 100);
}

#[test]
fn test_monitor_address_lookup() {
    // Test that only registered addresses are trackable
    let db_file = NamedTempFile::new().unwrap();
    let db = Db::new(db_file.path().to_str().unwrap()).unwrap();

    let wallet =
        Wallet::new("test test test test test test test test test test test junk".to_string());
    let addr1 = wallet.derive_address(0).unwrap().to_string();
    let addr2 = wallet.derive_address(1).unwrap().to_string();

    // Register only addr1
    db.register_account("user1", 0, &addr1, "https://webhook.example.com")
        .unwrap();

    // Check addr1 is registered
    let account = db.get_account_by_address(&addr1).unwrap();
    assert!(account.is_some());
    assert_eq!(account.unwrap(), "user1");

    // Check addr2 is not registered
    let account2 = db.get_account_by_address(&addr2).unwrap();
    assert!(account2.is_none());
}

// ========== Sweeper Unit Tests ==========

#[tokio::test]
async fn test_sweeper_creation() {
    // Test that Sweeper can be created with proper dependencies
    let db_file = NamedTempFile::new().unwrap();
    let db = Db::new(db_file.path().to_str().unwrap()).unwrap();

    let config = test_config(db_file.path().to_str().unwrap(), "http://localhost:8545");

    let wallet = Wallet::new(config.mnemonic.clone());
    let provider = http_provider_boxed("http://localhost:8545");
    let faucet = Arc::new(
        Faucet::new(
            config.faucet_mnemonic.clone(),
            provider.clone(),
            &test_support::chain_existential(&config),
        )
        .unwrap(),
    );

    let deliverer = test_webhook_deliverer(db.clone());
    Sweeper::new(
        config.chains[0].clone(),
        deliverer,
        db,
        wallet,
        provider.clone(),
        faucet,
    );
}

#[test]
fn test_sweeper_deposit_workflow() {
    // Test the full deposit workflow through the DB
    let db_file = NamedTempFile::new().unwrap();
    let db = Db::new(db_file.path().to_str().unwrap()).unwrap();

    let wallet =
        Wallet::new("test test test test test test test test test test test junk".to_string());
    let user_address = wallet.derive_address(0).unwrap().to_string();

    // Register account and create a deposit
    db.register_account("test_user", 0, &user_address, "https://webhook.example.com")
        .unwrap();
    db.record_deposit(TEST_CHAIN, "0xtx123", "test_user", "1000000000000000000")
        .unwrap();

    // Verify deposit exists
    let deposits_before = db.get_detected_deposits(TEST_CHAIN).unwrap();
    assert_eq!(deposits_before.len(), 1);
    assert_eq!(deposits_before[0].0, "0xtx123");
    assert_eq!(deposits_before[0].1, "test_user");

    // Simulate sweep completion
    db.mark_deposit_swept(TEST_CHAIN, "0xtx123").unwrap();
    let deposits_after = db.get_detected_deposits(TEST_CHAIN).unwrap();
    assert_eq!(deposits_after.len(), 0);

    // Verify the account details are correct for deriving keys
    let account_details = db.get_account_by_id("test_user").unwrap().unwrap();
    assert_eq!(account_details.0, 0); // derivation index
    assert_eq!(account_details.1, user_address);
}

#[test]
fn test_sweeper_wallet_integration() {
    // Test that Sweeper can work with the wallet to derive multiple accounts
    let wallet =
        Wallet::new("test test test test test test test test test test test junk".to_string());

    let addr0 = wallet.derive_address(0).unwrap();
    let addr1 = wallet.derive_address(1).unwrap();
    let addr2 = wallet.derive_address(2).unwrap();

    // All addresses should be unique
    assert_ne!(addr0, addr1);
    assert_ne!(addr1, addr2);
    assert_ne!(addr0, addr2);

    // Test that we can get signers for each
    let signer0 = wallet.get_signer(0).unwrap();
    let signer1 = wallet.get_signer(1).unwrap();

    // Signers should produce different addresses
    assert_eq!(signer0.address(), addr0);
    assert_eq!(signer1.address(), addr1);
}

#[test]
fn test_sweeper_multiple_deposits() {
    // Test handling multiple deposits for different users
    let db_file = NamedTempFile::new().unwrap();
    let db = Db::new(db_file.path().to_str().unwrap()).unwrap();

    let wallet =
        Wallet::new("test test test test test test test test test test test junk".to_string());

    // Create multiple users
    let addr0 = wallet.derive_address(0).unwrap().to_string();
    let addr1 = wallet.derive_address(1).unwrap().to_string();
    let addr2 = wallet.derive_address(2).unwrap().to_string();

    db.register_account("user_0", 0, &addr0, "https://webhook.example.com")
        .unwrap();
    db.register_account("user_1", 1, &addr1, "https://webhook.example.com")
        .unwrap();
    db.register_account("user_2", 2, &addr2, "https://webhook.example.com")
        .unwrap();

    // Record deposits for each
    db.record_deposit(TEST_CHAIN, "0xtx1", "user_0", "1000000000000000000")
        .unwrap();
    db.record_deposit(TEST_CHAIN, "0xtx2", "user_1", "2000000000000000000")
        .unwrap();
    db.record_deposit(TEST_CHAIN, "0xtx3", "user_2", "3000000000000000000")
        .unwrap();

    // Verify all deposits are tracked
    let deposits = db.get_detected_deposits(TEST_CHAIN).unwrap();
    assert_eq!(deposits.len(), 3);

    // Process one deposit at a time
    db.mark_deposit_swept(TEST_CHAIN, "0xtx1").unwrap();
    let deposits_after_1 = db.get_detected_deposits(TEST_CHAIN).unwrap();
    assert_eq!(deposits_after_1.len(), 2);

    db.mark_deposit_swept(TEST_CHAIN, "0xtx2").unwrap();
    let deposits_after_2 = db.get_detected_deposits(TEST_CHAIN).unwrap();
    assert_eq!(deposits_after_2.len(), 1);

    db.mark_deposit_swept(TEST_CHAIN, "0xtx3").unwrap();
    let deposits_after_3 = db.get_detected_deposits(TEST_CHAIN).unwrap();
    assert_eq!(deposits_after_3.len(), 0);
}

// ========== Verify Transfer Tests ==========

// Helper matcher for body contains
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

/// Case-insensitive body substring matcher. Used to check for a token address in an
/// `eth_getLogs` request without depending on whether alloy serializes `Address` in
/// checksummed or lowercase hex form.
fn body_json_contains_ci(substring: &str) -> impl wiremock::Match {
    BodyContainsCi(substring.to_lowercase())
}

struct BodyContainsCi(String);
impl wiremock::Match for BodyContainsCi {
    fn matches(&self, request: &wiremock::Request) -> bool {
        let body_str = String::from_utf8_lossy(&request.body).to_lowercase();
        body_str.contains(&self.0)
    }
}

/// Matches a JSON field with an exact hex-encoded numeric value, e.g.
/// `field_hex("fromBlock", 1)` matches `"fromBlock":"0x1"` but not `"fromBlock":"0x15"`.
/// Plain substring matching on the hex digits alone would conflate those two.
fn field_hex(field: &str, value: u64) -> String {
    format!("\"{field}\":\"0x{value:x}\"")
}

fn empty_block_rpc_response() -> serde_json::Value {
    let block_hash = "0x000000000000000000000000000000000000000000000000000000000000000a";
    let parent_hash = "0x0000000000000000000000000000000000000000000000000000000000000009";
    let root_hash = "0x0000000000000000000000000000000000000000000000000000000000000000";
    json!({
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
            "transactions": [],
            "uncles": []
        }
    })
}

#[tokio::test]
async fn test_verify_native_transfer_success() {
    let _ = tracing_subscriber::fmt::try_init();

    // Setup Mock RPC
    let rpc_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();

    let config = test_config(db_file.path().to_str().unwrap(), rpc_server.uri());

    let to_address = "0x742d35Cc6634C0532925a3b844Bc454e4438f44e";
    let tx_hash = "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";
    let amount = "1000000000000000000"; // 1 ETH

    // Mock eth_getTransactionByHash
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getTransactionByHash"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "hash": tx_hash,
                "nonce": "0x0",
                "blockHash": "0x000000000000000000000000000000000000000000000000000000000000000a",
                "blockNumber": "0xA",
                "transactionIndex": "0x0",
                "from": "0x0000000000000000000000000000000000000001",
                "to": to_address,
                "value": "0xDE0B6B3A7640000", // 1 ETH
                "gas": "0x5208",
                "gasPrice": "0x3B9ACA00",
                "input": "0x",
                "v": "0x1b",
                "r": "0x1",
                "s": "0x1",
                "type": "0x0",
                "chainId": "0x1"
            }
        })))
        .mount(&rpc_server)
        .await;

    // Mock eth_getTransactionReceipt
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getTransactionReceipt"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "transactionHash": tx_hash,
                "transactionIndex": "0x0",
                "blockHash": "0x000000000000000000000000000000000000000000000000000000000000000a",
                "blockNumber": "0xA",
                "from": "0x0000000000000000000000000000000000000001",
                "to": to_address,
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

    // Create the service
    let service = HotWalletService::new(config).await.unwrap();

    // Test verification
    let request = VerifyTransferRequest {
        chain: TEST_CHAIN.to_string(),
        tx_hash: tx_hash.to_string(),
        to_address: to_address.to_string(),
        amount: amount.to_string(),
        token_type: "native".to_string(),
        token_address: None,
        token_symbol: None,
    };

    let response = service.verify_transfer(request).await.unwrap();

    match response {
        VerifyTransferResponse::Success {
            actual_to,
            actual_amount,
            token_type,
            token_symbol,
            block_number,
        } => {
            assert_eq!(actual_to.to_lowercase(), to_address.to_lowercase());
            assert_eq!(actual_amount, amount);
            assert_eq!(token_type, "native");
            assert!(token_symbol.is_none());
            assert_eq!(block_number, Some(10));
        }
        VerifyTransferResponse::Error { message, .. } => {
            panic!("Expected success, got error: {}", message);
        }
    }
}

#[tokio::test]
async fn test_verify_native_transfer_amount_mismatch() {
    let _ = tracing_subscriber::fmt::try_init();

    let rpc_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();

    let config = test_config(db_file.path().to_str().unwrap(), rpc_server.uri());

    let to_address = "0x742d35Cc6634C0532925a3b844Bc454e4438f44e";
    let tx_hash = "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";

    // Mock eth_getTransactionByHash - returns 1 ETH
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getTransactionByHash"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "hash": tx_hash,
                "nonce": "0x0",
                "blockHash": "0x000000000000000000000000000000000000000000000000000000000000000a",
                "blockNumber": "0xA",
                "transactionIndex": "0x0",
                "from": "0x0000000000000000000000000000000000000001",
                "to": to_address,
                "value": "0xDE0B6B3A7640000", // 1 ETH
                "gas": "0x5208",
                "gasPrice": "0x3B9ACA00",
                "input": "0x",
                "v": "0x1b",
                "r": "0x1",
                "s": "0x1",
                "type": "0x0",
                "chainId": "0x1"
            }
        })))
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getTransactionReceipt"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "transactionHash": tx_hash,
                "transactionIndex": "0x0",
                "blockHash": "0x000000000000000000000000000000000000000000000000000000000000000a",
                "blockNumber": "0xA",
                "from": "0x0000000000000000000000000000000000000001",
                "to": to_address,
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

    let service = HotWalletService::new(config).await.unwrap();

    // Request expects 2 ETH but tx only has 1 ETH
    let request = VerifyTransferRequest {
        chain: TEST_CHAIN.to_string(),
        tx_hash: tx_hash.to_string(),
        to_address: to_address.to_string(),
        amount: "2000000000000000000".to_string(), // 2 ETH - more than actual
        token_type: "native".to_string(),
        token_address: None,
        token_symbol: None,
    };

    let response = service.verify_transfer(request).await.unwrap();

    match response {
        VerifyTransferResponse::Success { .. } => {
            panic!("Expected error due to amount mismatch");
        }
        VerifyTransferResponse::Error {
            message,
            token_type,
            ..
        } => {
            assert!(message.contains("amount_matches=false"));
            assert_eq!(token_type, Some("native".to_string()));
        }
    }
}

#[tokio::test]
async fn test_verify_erc20_transfer_success() {
    let _ = tracing_subscriber::fmt::try_init();

    let rpc_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();

    let config = test_config(db_file.path().to_str().unwrap(), rpc_server.uri());

    let to_address = "0x742d35Cc6634C0532925a3b844Bc454e4438f44e";
    let token_address = "0xdAC17F958D2ee523a2206206994597C13D831ec7"; // USDT
    let tx_hash = "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";
    let amount = "1000000"; // 1 USDT (6 decimals)

    // Transfer event signature keccak256("Transfer(address,address,uint256)")
    let transfer_topic = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

    // Pad addresses to 32 bytes for topics
    let from_topic = "0x0000000000000000000000000000000000000000000000000000000000000001";
    let to_topic = format!(
        "0x000000000000000000000000{}",
        &to_address[2..].to_lowercase()
    );

    // Amount as 32-byte hex (1000000 = 0xF4240)
    let amount_data = "0x00000000000000000000000000000000000000000000000000000000000f4240";

    // Mock eth_getTransactionReceipt with ERC20 Transfer log
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getTransactionReceipt"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "transactionHash": tx_hash,
                "transactionIndex": "0x0",
                "blockHash": "0x000000000000000000000000000000000000000000000000000000000000000a",
                "blockNumber": "0xA",
                "from": "0x0000000000000000000000000000000000000001",
                "to": token_address,
                "cumulativeGasUsed": "0x10000",
                "gasUsed": "0x10000",
                "contractAddress": null,
                "logs": [
                    {
                        "address": token_address,
                        "topics": [
                            transfer_topic,
                            from_topic,
                            to_topic
                        ],
                        "data": amount_data,
                        "blockNumber": "0xA",
                        "transactionHash": tx_hash,
                        "transactionIndex": "0x0",
                        "blockHash": "0x000000000000000000000000000000000000000000000000000000000000000a",
                        "logIndex": "0x0",
                        "removed": false
                    }
                ],
                "status": "0x1",
                "logsBloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
                "type": "0x0",
                "effectiveGasPrice": "0x3B9ACA00"
            }
        })))
        .mount(&rpc_server)
        .await;

    // Mock symbol() call
    Mock::given(method("POST"))
        .and(body_json_contains("eth_call"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            // ABI encoded string "USDT" - offset (32) + length (4) + "USDT" padded
            "result": "0x000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000045553445400000000000000000000000000000000000000000000000000000000"
        })))
        .mount(&rpc_server)
        .await;

    let service = HotWalletService::new(config).await.unwrap();

    let request = VerifyTransferRequest {
        chain: TEST_CHAIN.to_string(),
        tx_hash: tx_hash.to_string(),
        to_address: to_address.to_string(),
        amount: amount.to_string(),
        token_type: "erc20".to_string(),
        token_address: Some(token_address.to_string()),
        token_symbol: Some("USDT".to_string()),
    };

    let response = service.verify_transfer(request).await.unwrap();

    match response {
        VerifyTransferResponse::Success {
            actual_to,
            actual_amount,
            token_type,
            token_symbol,
            block_number,
        } => {
            assert_eq!(actual_to.to_lowercase(), to_address.to_lowercase());
            assert_eq!(actual_amount, amount);
            assert_eq!(token_type, "erc20");
            assert_eq!(token_symbol, Some("USDT".to_string()));
            assert_eq!(block_number, Some(10));
        }
        VerifyTransferResponse::Error { message, .. } => {
            panic!("Expected success, got error: {}", message);
        }
    }
}

#[tokio::test]
async fn test_verify_erc20_transfer_symbol_mismatch() {
    let _ = tracing_subscriber::fmt::try_init();

    let rpc_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();

    let config = test_config(db_file.path().to_str().unwrap(), rpc_server.uri());

    let to_address = "0x742d35Cc6634C0532925a3b844Bc454e4438f44e";
    let token_address = "0xdAC17F958D2ee523a2206206994597C13D831ec7";
    let tx_hash = "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";

    // Mock eth_getTransactionReceipt
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getTransactionReceipt"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "transactionHash": tx_hash,
                "transactionIndex": "0x0",
                "blockHash": "0x000000000000000000000000000000000000000000000000000000000000000a",
                "blockNumber": "0xA",
                "from": "0x0000000000000000000000000000000000000001",
                "to": token_address,
                "cumulativeGasUsed": "0x10000",
                "gasUsed": "0x10000",
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

    // Mock symbol() call - returns "USDT" but we'll request "USDC"
    Mock::given(method("POST"))
        .and(body_json_contains("eth_call"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": "0x000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000045553445400000000000000000000000000000000000000000000000000000000"
        })))
        .mount(&rpc_server)
        .await;

    let service = HotWalletService::new(config).await.unwrap();

    // Request expects USDC but contract returns USDT
    let request = VerifyTransferRequest {
        chain: TEST_CHAIN.to_string(),
        tx_hash: tx_hash.to_string(),
        to_address: to_address.to_string(),
        amount: "1000000".to_string(),
        token_type: "erc20".to_string(),
        token_address: Some(token_address.to_string()),
        token_symbol: Some("USDC".to_string()), // Wrong symbol
    };

    let response = service.verify_transfer(request).await.unwrap();

    match response {
        VerifyTransferResponse::Success { .. } => {
            panic!("Expected error due to symbol mismatch");
        }
        VerifyTransferResponse::Error {
            message,
            token_type,
            ..
        } => {
            assert!(message.contains("Token symbol mismatch"));
            assert!(message.contains("USDC"));
            assert!(message.contains("USDT"));
            assert_eq!(token_type, Some("erc20".to_string()));
        }
    }
}

#[tokio::test]
async fn test_verify_transfer_reverted_transaction() {
    let _ = tracing_subscriber::fmt::try_init();

    let rpc_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();

    let config = test_config(db_file.path().to_str().unwrap(), rpc_server.uri());

    let to_address = "0x742d35Cc6634C0532925a3b844Bc454e4438f44e";
    let tx_hash = "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";

    // Mock eth_getTransactionByHash
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getTransactionByHash"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "hash": tx_hash,
                "nonce": "0x0",
                "blockHash": "0x000000000000000000000000000000000000000000000000000000000000000a",
                "blockNumber": "0xA",
                "transactionIndex": "0x0",
                "from": "0x0000000000000000000000000000000000000001",
                "to": to_address,
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
        })))
        .mount(&rpc_server)
        .await;

    // Mock eth_getTransactionReceipt with status = 0 (reverted)
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getTransactionReceipt"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "transactionHash": tx_hash,
                "transactionIndex": "0x0",
                "blockHash": "0x000000000000000000000000000000000000000000000000000000000000000a",
                "blockNumber": "0xA",
                "from": "0x0000000000000000000000000000000000000001",
                "to": to_address,
                "cumulativeGasUsed": "0x5208",
                "gasUsed": "0x5208",
                "contractAddress": null,
                "logs": [],
                "status": "0x0", // REVERTED
                "logsBloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
                "type": "0x0",
                "effectiveGasPrice": "0x3B9ACA00"
            }
        })))
        .mount(&rpc_server)
        .await;

    let service = HotWalletService::new(config).await.unwrap();

    let request = VerifyTransferRequest {
        chain: TEST_CHAIN.to_string(),
        tx_hash: tx_hash.to_string(),
        to_address: to_address.to_string(),
        amount: "1000000000000000000".to_string(),
        token_type: "native".to_string(),
        token_address: None,
        token_symbol: None,
    };

    let response = service.verify_transfer(request).await.unwrap();

    match response {
        VerifyTransferResponse::Success { .. } => {
            panic!("Expected error for reverted transaction");
        }
        VerifyTransferResponse::Error { message, .. } => {
            assert!(message.contains("reverted"));
        }
    }
}

// ========== Sweep Failure Tracking Tests ==========

#[test]
fn test_erc20_deposit_failure_tracking() {
    let db_file = NamedTempFile::new().unwrap();
    let db = Db::new(db_file.path().to_str().unwrap()).unwrap();

    db.record_erc20_deposit(
        TEST_CHAIN, "0xabc", 1, "user_1", "1000000", "0xtoken", "USDC",
    )
    .unwrap();

    assert_eq!(db.get_detected_erc20_deposits(TEST_CHAIN).unwrap().len(), 1);

    for i in 1..=5 {
        let count = db
            .increment_sweep_failure_count(TEST_CHAIN, "0xabc:1")
            .unwrap();
        assert_eq!(count, i);
    }

    db.mark_erc20_deposit_failed(TEST_CHAIN, "0xabc:1").unwrap();

    assert_eq!(db.get_detected_erc20_deposits(TEST_CHAIN).unwrap().len(), 0);
}

#[test]
fn test_erc20_bulk_mark_failed_for_account_token() {
    let db_file = NamedTempFile::new().unwrap();
    let db = Db::new(db_file.path().to_str().unwrap()).unwrap();

    db.record_erc20_deposit(
        TEST_CHAIN,
        "0xaaa",
        1,
        "user_1",
        "1000000",
        "0xtoken_a",
        "USDC",
    )
    .unwrap();
    db.record_erc20_deposit(
        TEST_CHAIN,
        "0xbbb",
        2,
        "user_1",
        "2000000",
        "0xtoken_a",
        "USDC",
    )
    .unwrap();
    db.record_erc20_deposit(
        TEST_CHAIN,
        "0xccc",
        3,
        "user_1",
        "3000000",
        "0xtoken_b",
        "USDT",
    )
    .unwrap();

    assert_eq!(db.get_detected_erc20_deposits(TEST_CHAIN).unwrap().len(), 3);

    let failed = db
        .mark_erc20_deposits_failed_for_account_token(TEST_CHAIN, "user_1", "0xtoken_a")
        .unwrap();
    assert_eq!(failed.len(), 2);

    let remaining = db.get_detected_erc20_deposits(TEST_CHAIN).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].token_symbol, "USDT");
}

// ========== Dust Attack / Min Deposit Tests ==========

#[test]
fn test_parse_min_deposits_env_valid_and_empty() {
    use crate::config::parse_min_deposits_env;
    use alloy::primitives::U256;

    let empty = parse_min_deposits_env(String::new()).unwrap();
    assert!(empty.is_empty());

    let parsed = parse_min_deposits_env(
        "0xAbC=10000,0xc2132d05d31c914a87c6611c10748aeb04b58e8f=20000".to_string(),
    )
    .unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed.get("0xabc").copied(), Some(U256::from(10000u64)));
    assert_eq!(
        parsed
            .get("0xc2132d05d31c914a87c6611c10748aeb04b58e8f")
            .copied(),
        Some(U256::from(20000u64))
    );
}

#[test]
fn test_parse_min_deposits_env_skips_malformed_segments() {
    use crate::config::parse_min_deposits_env;
    use alloy::primitives::U256;

    let parsed = parse_min_deposits_env("badsegment,0xabc=10000,also-bad".to_string()).unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed.get("0xabc").copied(), Some(U256::from(10000u64)));
}

#[test]
fn test_min_deposit_for_token_matches_checksummed_address() {
    use alloy::primitives::U256;
    use std::collections::HashMap;

    let mut per_token = HashMap::new();
    per_token.insert(
        "0xc2132d05d31c914a87c6611c10748aeb04b58e8f".to_string(),
        U256::from(10000u64),
    );
    let settings = MinDepositSettings {
        per_token,
        default: U256::from(500u64),
        native: U256::ZERO,
    };

    assert_eq!(
        settings.for_token("0xc2132D05D31c914a87C6611C10748AEb04B58e8F"),
        U256::from(10000u64)
    );
    assert_eq!(
        settings.for_token("0x0000000000000000000000000000000000000001"),
        U256::from(500u64)
    );
}

#[test]
fn test_erc20_bulk_mark_swept_returns_key_and_amount() {
    let db_file = NamedTempFile::new().unwrap();
    let db = Db::new(db_file.path().to_str().unwrap()).unwrap();

    db.record_erc20_deposit(
        TEST_CHAIN,
        "0xaaa",
        1,
        "user_1",
        "3000000000",
        "0xtoken_a",
        "USDT",
    )
    .unwrap();
    db.record_erc20_deposit(TEST_CHAIN, "0xbbb", 2, "user_1", "30", "0xtoken_a", "USDT")
        .unwrap();

    let swept = db
        .mark_erc20_deposits_swept_for_account_token(TEST_CHAIN, "user_1", "0xtoken_a")
        .unwrap();

    assert_eq!(swept.len(), 2);
    assert!(swept.contains(&("0xaaa:1".to_string(), "3000000000".to_string())));
    assert!(swept.contains(&("0xbbb:2".to_string(), "30".to_string())));
    assert_eq!(db.get_detected_erc20_deposits(TEST_CHAIN).unwrap().len(), 0);
}

#[tokio::test]
async fn test_monitor_skips_erc20_below_min_deposit() {
    use alloy::primitives::U256;
    use std::collections::HashMap;
    use std::time::Duration;
    use tokio::time::sleep;

    let _ = tracing_subscriber::fmt::try_init();

    let rpc_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();
    let wallet =
        Wallet::new("test test test test test test test test test test test junk".to_string());
    let addr = wallet.derive_address(0).unwrap().to_string();

    let token_address = "0xc2132D05D31c914a87C6611C10748AEb04B58e8F";
    let mut per_token = HashMap::new();
    per_token.insert(
        "0xc2132d05d31c914a87c6611c10748aeb04b58e8f".to_string(),
        U256::from(10000u64),
    );

    let config = test_config(db_file.path().to_str().unwrap(), rpc_server.uri());

    let db = Db::new(&config.database_url).unwrap();
    db.register_account("user_1", 0, &addr, "http://localhost/webhook")
        .unwrap();

    let transfer_topic = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
    let from_topic = "0x0000000000000000000000000000000000000000000000000000000000000001";
    let to_topic = format!("0x000000000000000000000000{}", &addr[2..].to_lowercase());
    // 30 raw units, below threshold of 10000
    let amount_data = "0x000000000000000000000000000000000000000000000000000000000000001e";

    Mock::given(method("POST"))
        .and(body_json_contains("eth_blockNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0xA"
        })))
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getBlockByNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "number": "0xA",
                "hash": "0x000000000000000000000000000000000000000000000000000000000000000a",
                "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000009",
                "transactions": []
            }
        })))
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getLogs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [{
                "address": token_address,
                "topics": [transfer_topic, from_topic, to_topic],
                "data": amount_data,
                "blockNumber": "0xA",
                "transactionHash": "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
                "transactionIndex": "0x0",
                "blockHash": "0x000000000000000000000000000000000000000000000000000000000000000a",
                "logIndex": "0x0",
                "removed": false
            }]
        })))
        .mount(&rpc_server)
        .await;

    let provider = http_provider_boxed(&rpc_server.uri());
    let deliverer = test_webhook_deliverer(db.clone());
    let monitor = Monitor::new(
        config.chains[0].clone(),
        deliverer,
        db.clone(),
        provider.clone(),
    );
    let handle = tokio::spawn(async move {
        monitor.run().await;
    });

    sleep(Duration::from_millis(1500)).await;
    handle.abort();

    assert_eq!(db.get_detected_erc20_deposits(TEST_CHAIN).unwrap().len(), 0);
}

#[tokio::test]
async fn test_erc20_sweep_emits_per_deposit_webhooks() {
    use std::time::Duration;
    use tokio::time::sleep;

    let _ = tracing_subscriber::fmt::try_init();

    let rpc_server = MockServer::start().await;
    let webhook_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();

    let wallet =
        Wallet::new("test test test test test test test test test test test junk".to_string());
    let addr = wallet.derive_address(0).unwrap().to_string();
    let token_address = "0xc2132D05D31c914a87C6611C10748AEb04B58e8F";

    let config = test_config(db_file.path().to_str().unwrap(), rpc_server.uri());

    let db = Db::new(&config.database_url).unwrap();
    db.register_account("user_1", 0, &addr, &webhook_server.uri())
        .unwrap();
    db.store_token_metadata(TEST_CHAIN, token_address, "USDT", 6, "Tether USD")
        .unwrap();
    db.record_erc20_deposit(
        TEST_CHAIN,
        "0xaaa",
        1,
        "user_1",
        "3000000000",
        token_address,
        "USDT",
    )
    .unwrap();
    db.record_erc20_deposit(
        TEST_CHAIN,
        "0xbbb",
        2,
        "user_1",
        "30",
        token_address,
        "USDT",
    )
    .unwrap();

    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&webhook_server)
        .await;

    // balanceOf -> 3000000030 (aggregate on-chain balance)
    Mock::given(method("POST"))
        .and(body_json_contains("eth_call"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1,
            "result": "0x00000000000000000000000000000000000000000000000000000000b2d05e06"
        })))
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_estimateGas"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x186a0"
        })))
        .mount(&rpc_server)
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
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getBalance"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x0DE0B6B3A7640000"
        })))
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getTransactionCount"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x00"
        })))
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_chainId"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x89"
        })))
        .mount(&rpc_server)
        .await;

    let sweep_tx_hash = "0x00000000000000000000000000000000000000000000000000000000000000f1";
    Mock::given(method("POST"))
        .and(body_json_contains("eth_sendRawTransaction"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": sweep_tx_hash
        })))
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getTransactionReceipt"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {
                "transactionHash": sweep_tx_hash,
                "transactionIndex": "0x1",
                "blockHash": "0x000000000000000000000000000000000000000000000000000000000000000b",
                "blockNumber": "0xB",
                "from": addr,
                "to": token_address,
                "cumulativeGasUsed": "0x186a0",
                "gasUsed": "0x186a0",
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

    let provider = http_provider_boxed(&rpc_server.uri());
    let faucet = Arc::new(
        Faucet::new(
            config.faucet_mnemonic.clone(),
            provider.clone(),
            &test_support::chain_existential(&config),
        )
        .unwrap(),
    );
    let deliverer = test_webhook_deliverer(db.clone());
    let webhook_worker = WebhookRetryService::new(Arc::clone(&deliverer));
    tokio::spawn(async move {
        webhook_worker.run().await;
    });
    let sweeper = Sweeper::new(
        config.chains[0].clone(),
        deliverer,
        db.clone(),
        wallet,
        provider.clone(),
        faucet,
    );
    let handle = tokio::spawn(async move {
        sweeper.run().await;
    });

    let mut swept = false;
    for _ in 0..20 {
        if db
            .get_detected_erc20_deposits(TEST_CHAIN)
            .unwrap()
            .is_empty()
        {
            swept = true;
            break;
        }
        sleep(Duration::from_millis(300)).await;
    }
    handle.abort();
    assert!(swept, "ERC20 deposits should be marked swept");

    sleep(Duration::from_millis(300)).await;

    for id in [
        format!("{TEST_CHAIN}:0xaaa:1"),
        format!("{TEST_CHAIN}:0xbbb:2"),
    ] {
        let row = db
            .get_webhook_delivery(&id, "deposit_swept")
            .unwrap()
            .unwrap_or_else(|| panic!("missing webhook delivery row for {id}"));
        assert_eq!(
            row.status, "delivered",
            "webhook for {id} should be delivered"
        );
    }

    let requests = webhook_server.received_requests().await.unwrap();
    let swept_events: Vec<_> = requests
        .iter()
        .filter(|req| {
            let body = String::from_utf8_lossy(&req.body);
            body.contains("deposit_swept")
        })
        .collect();

    assert_eq!(
        swept_events.len(),
        2,
        "expected one deposit_swept per deposit"
    );

    let mut amounts = Vec::new();
    let mut original_hashes = Vec::new();
    let mut sweep_hashes = Vec::new();
    for req in swept_events {
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        amounts.push(body["amount"].as_str().unwrap().to_string());
        original_hashes.push(body["original_tx_hash"].as_str().unwrap().to_string());
        sweep_hashes.push(body["sweep_tx_hash"].as_str().unwrap().to_string());
    }

    amounts.sort();
    assert_eq!(amounts, vec!["30".to_string(), "3000000000".to_string()]);
    assert!(original_hashes.contains(&"0xaaa".to_string()));
    assert!(original_hashes.contains(&"0xbbb".to_string()));
    assert!(sweep_hashes.iter().all(|h| h == sweep_tx_hash));
    assert!(
        !amounts.contains(&"3000000030".to_string()),
        "webhook must not use aggregate on-chain balance"
    );
}

#[tokio::test]
async fn test_erc20_sweep_webhook_best_effort_on_failure() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;
    use std::time::Duration;
    use tokio::time::sleep;

    let _ = tracing_subscriber::fmt::try_init();

    let rpc_server = MockServer::start().await;
    let webhook_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();

    let wallet =
        Wallet::new("test test test test test test test test test test test junk".to_string());
    let addr = wallet.derive_address(0).unwrap().to_string();
    let token_address = "0xc2132D05D31c914a87C6611C10748AEb04B58e8F";

    let config = test_config(db_file.path().to_str().unwrap(), rpc_server.uri());

    let db = Db::new(&config.database_url).unwrap();
    db.register_account("user_1", 0, &addr, &webhook_server.uri())
        .unwrap();
    db.store_token_metadata(TEST_CHAIN, token_address, "USDT", 6, "Tether USD")
        .unwrap();
    db.record_erc20_deposit(
        TEST_CHAIN,
        "0xaaa",
        1,
        "user_1",
        "3000000000",
        token_address,
        "USDT",
    )
    .unwrap();
    db.record_erc20_deposit(
        TEST_CHAIN,
        "0xbbb",
        2,
        "user_1",
        "30",
        token_address,
        "USDT",
    )
    .unwrap();

    let webhook_attempts = StdArc::new(AtomicUsize::new(0));
    let attempts_for_mock = webhook_attempts.clone();
    Mock::given(method("POST"))
        .respond_with(move |req: &wiremock::Request| {
            let count = attempts_for_mock.fetch_add(1, Ordering::SeqCst);
            let body = String::from_utf8_lossy(&req.body);
            if body.contains("deposit_swept") && count == 0 {
                ResponseTemplate::new(503)
            } else {
                ResponseTemplate::new(200)
            }
        })
        .mount(&webhook_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_call"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1,
            "result": "0x00000000000000000000000000000000000000000000000000000000b2d05e06"
        })))
        .mount(&rpc_server)
        .await;
    Mock::given(method("POST"))
        .and(body_json_contains("eth_estimateGas"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x186a0"
        })))
        .mount(&rpc_server)
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
        .mount(&rpc_server)
        .await;
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getBalance"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x0DE0B6B3A7640000"
        })))
        .mount(&rpc_server)
        .await;
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getTransactionCount"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x00"
        })))
        .mount(&rpc_server)
        .await;
    Mock::given(method("POST"))
        .and(body_json_contains("eth_chainId"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x89"
        })))
        .mount(&rpc_server)
        .await;

    let sweep_tx_hash = "0x00000000000000000000000000000000000000000000000000000000000000f2";
    Mock::given(method("POST"))
        .and(body_json_contains("eth_sendRawTransaction"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": sweep_tx_hash
        })))
        .mount(&rpc_server)
        .await;
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getTransactionReceipt"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {
                "transactionHash": sweep_tx_hash,
                "transactionIndex": "0x1",
                "blockHash": "0x000000000000000000000000000000000000000000000000000000000000000b",
                "blockNumber": "0xB",
                "from": addr,
                "to": token_address,
                "cumulativeGasUsed": "0x186a0",
                "gasUsed": "0x186a0",
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

    let provider = http_provider_boxed(&rpc_server.uri());
    let faucet = Arc::new(
        Faucet::new(
            config.faucet_mnemonic.clone(),
            provider.clone(),
            &test_support::chain_existential(&config),
        )
        .unwrap(),
    );
    let deliverer = test_webhook_deliverer(db.clone());
    let webhook_worker = WebhookRetryService::new(Arc::clone(&deliverer));
    tokio::spawn(async move {
        webhook_worker.run().await;
    });
    let sweeper = Sweeper::new(
        config.chains[0].clone(),
        deliverer,
        db.clone(),
        wallet,
        provider.clone(),
        faucet,
    );
    let handle = tokio::spawn(async move {
        sweeper.run().await;
    });

    for _ in 0..20 {
        if db
            .get_detected_erc20_deposits(TEST_CHAIN)
            .unwrap()
            .is_empty()
        {
            break;
        }
        sleep(Duration::from_millis(300)).await;
    }
    handle.abort();

    let row_a = db
        .get_webhook_delivery(&format!("{TEST_CHAIN}:0xaaa:1"), "deposit_swept")
        .unwrap();
    let row_b = db
        .get_webhook_delivery(&format!("{TEST_CHAIN}:0xbbb:2"), "deposit_swept")
        .unwrap();
    assert!(
        row_a.is_some(),
        "first deposit should enqueue webhook delivery"
    );
    assert!(
        row_b.is_some(),
        "second deposit should enqueue webhook delivery"
    );

    let mut both_delivered = false;
    for _ in 0..40 {
        let a = db
            .get_webhook_delivery(&format!("{TEST_CHAIN}:0xaaa:1"), "deposit_swept")
            .unwrap();
        let b = db
            .get_webhook_delivery(&format!("{TEST_CHAIN}:0xbbb:2"), "deposit_swept")
            .unwrap();
        if a.as_ref().map(|r| r.status.as_str()) == Some("delivered")
            && b.as_ref().map(|r| r.status.as_str()) == Some("delivered")
        {
            both_delivered = true;
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    assert!(
        both_delivered,
        "worker should eventually deliver both webhooks"
    );
    assert!(
        webhook_attempts.load(Ordering::SeqCst) >= 3,
        "first delivery may retry after 503 before both succeed"
    );
}

// ========== Webhook Delivery Tests ==========

#[tokio::test]
async fn test_webhook_attempt_stored_retries_until_success() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;

    let webhook_server = MockServer::start().await;
    let attempts = StdArc::new(AtomicUsize::new(0));
    let attempts_for_mock = attempts.clone();
    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            let n = attempts_for_mock.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= 2 {
                ResponseTemplate::new(503)
            } else {
                ResponseTemplate::new(200)
            }
        })
        .mount(&webhook_server)
        .await;

    let db_file = NamedTempFile::new().unwrap();
    let db = Db::new(db_file.path().to_str().unwrap()).unwrap();
    let deliverer = test_webhook_deliverer(db.clone());

    let payload = json!({
        "id": "polygon:0xabc",
        "event": "deposit_detected"
    });
    deliverer
        .enqueue(&webhook_server.uri(), "user1", payload)
        .await
        .unwrap();

    for _ in 0..5 {
        deliverer
            .attempt_stored("polygon:0xabc", "deposit_detected")
            .await
            .unwrap();
        let row = db
            .get_webhook_delivery("polygon:0xabc", "deposit_detected")
            .unwrap()
            .unwrap();
        if row.status == "delivered" {
            assert_eq!(row.attempt_count, 3);
            return;
        }
    }
    panic!("webhook was not delivered after retries");
}

#[tokio::test]
async fn test_webhook_attempt_stored_marks_failed_after_max_retries() {
    let webhook_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&webhook_server)
        .await;

    let db_file = NamedTempFile::new().unwrap();
    let db = Db::new(db_file.path().to_str().unwrap()).unwrap();
    let deliverer = test_webhook_deliverer(db.clone());

    deliverer
        .enqueue(
            &webhook_server.uri(),
            "user1",
            json!({"id": "base:0x1", "event": "deposit_swept"}),
        )
        .await
        .unwrap();

    for _ in 0..5 {
        deliverer
            .attempt_stored("base:0x1", "deposit_swept")
            .await
            .unwrap();
    }

    let row = db
        .get_webhook_delivery("base:0x1", "deposit_swept")
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "failed");
    assert_eq!(row.attempt_count, 3);
}

#[tokio::test]
async fn test_webhook_enqueue_skips_already_delivered() {
    let webhook_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&webhook_server)
        .await;

    let db_file = NamedTempFile::new().unwrap();
    let db = Db::new(db_file.path().to_str().unwrap()).unwrap();
    let deliverer = test_webhook_deliverer(db.clone());

    db.upsert_webhook_delivery(
        "polygon:0xabc",
        "deposit_detected",
        "user1",
        &webhook_server.uri(),
        r#"{"id":"polygon:0xabc","event":"deposit_detected"}"#,
    )
    .unwrap();
    db.record_webhook_attempt(
        "polygon:0xabc",
        "deposit_detected",
        Some(200),
        None,
        "delivered",
    )
    .unwrap();

    deliverer
        .enqueue(
            &webhook_server.uri(),
            "user1",
            json!({"id": "polygon:0xabc", "event": "deposit_detected"}),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn test_webhook_lease_prevents_duplicate_post() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;
    use std::time::Duration;

    let webhook_server = MockServer::start().await;
    let posts = StdArc::new(AtomicUsize::new(0));
    let posts_for_mock = posts.clone();
    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            posts_for_mock.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_delay(Duration::from_millis(200))
        })
        .mount(&webhook_server)
        .await;

    let db_file = NamedTempFile::new().unwrap();
    let db = Db::new(db_file.path().to_str().unwrap()).unwrap();
    let deliverer =
        Arc::new(WebhookDeliverer::new_for_test(db.clone(), None, 3, 10, 60, 50, 60).unwrap());

    deliverer
        .enqueue(
            &webhook_server.uri(),
            "user1",
            json!({"id": "polygon:0xabc", "event": "deposit_detected"}),
        )
        .await
        .unwrap();

    let d1 = Arc::clone(&deliverer);
    let d2 = Arc::clone(&deliverer);
    let t1 =
        tokio::spawn(async move { d1.attempt_stored("polygon:0xabc", "deposit_detected").await });
    let t2 =
        tokio::spawn(async move { d2.attempt_stored("polygon:0xabc", "deposit_detected").await });
    let _ = tokio::join!(t1, t2);

    assert_eq!(posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_webhook_worker_wake_on_enqueue() {
    use std::time::Duration;
    use tokio::time::sleep;

    let webhook_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&webhook_server)
        .await;

    let db_file = NamedTempFile::new().unwrap();
    let db = Db::new(db_file.path().to_str().unwrap()).unwrap();
    let deliverer =
        Arc::new(WebhookDeliverer::new_for_test(db.clone(), None, 3, 10, 60, 50, 60).unwrap());
    let worker = WebhookRetryService::new(Arc::clone(&deliverer));
    tokio::spawn(async move {
        worker.run().await;
    });

    deliverer
        .enqueue(
            &webhook_server.uri(),
            "user1",
            json!({"id": "polygon:0xabc", "event": "deposit_detected"}),
        )
        .await
        .unwrap();

    for _ in 0..40 {
        if db
            .get_webhook_delivery("polygon:0xabc", "deposit_detected")
            .unwrap()
            .is_some_and(|r| r.status == "delivered")
        {
            return;
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("worker did not deliver webhook promptly after enqueue notify");
}

#[tokio::test]
async fn test_admin_retry_webhooks_resets_and_notifies() {
    use std::time::Duration;
    use tokio::time::sleep;

    let rpc_server = MockServer::start().await;
    let webhook_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();
    let config = test_config(db_file.path().to_str().unwrap(), rpc_server.uri());

    let db = Db::new(&config.database_url).unwrap();
    db.upsert_webhook_delivery(
        "base:0xabc:120",
        "deposit_swept",
        "user1",
        &webhook_server.uri(),
        r#"{"id":"base:0xabc:120","event":"deposit_swept"}"#,
    )
    .unwrap();
    db.record_webhook_attempt(
        "base:0xabc:120",
        "deposit_swept",
        Some(503),
        Some("HTTP status 503"),
        "failed",
    )
    .unwrap();

    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&webhook_server)
        .await;

    let service = HotWalletService::new(config).await.unwrap();
    service.start_background_services().await.unwrap();

    let response = service
        .retry_webhook(RetryWebhookRequest {
            id: "base:0xabc:120".to_string(),
            event: "deposit_swept".to_string(),
        })
        .unwrap();
    assert!(response.retried);
    assert_eq!(response.status, "pending");

    for _ in 0..40 {
        if db
            .get_webhook_delivery("base:0xabc:120", "deposit_swept")
            .unwrap()
            .is_some_and(|r| r.status == "delivered")
        {
            return;
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!("admin retry did not lead to webhook delivery");
}

// ========== Token Allowlist Tests ==========

fn test_config_with_allowlist(allowlist: std::collections::HashSet<String>) -> Config {
    let mut config = test_config(":memory:", "http://localhost:8545");
    config.chains[0].allowed_token_addresses = allowlist;
    config
}

#[test]
fn test_is_token_allowed_empty_allowlist_allows_all() {
    let config = test_config_with_allowlist(Default::default());
    assert!(config.chains[0].is_token_allowed("0xdead000000000000000000000000000000000001"));
    assert!(config.chains[0].is_token_allowed("dead000000000000000000000000000000000001"));
}

#[test]
fn test_is_token_allowed_with_entries() {
    let mut allowlist = std::collections::HashSet::new();
    allowlist.insert("0xc2132d05d31c914a87c6611c10748aeb04b58e8f".to_string());
    let config = test_config_with_allowlist(allowlist);

    assert!(config.chains[0].is_token_allowed("0xC2132D05D31c914a87C6611C10748AEb04B58e8F"));
    assert!(config.chains[0].is_token_allowed("c2132d05d31c914a87c6611c10748aeb04b58e8f"));
    assert!(!config.chains[0].is_token_allowed("0xdead000000000000000000000000000000000001"));
}

#[test]
fn test_parse_allowed_token_addresses_env() {
    let set = parse_allowed_token_addresses_env(
        " 0xC2132D05D31c914a87C6611C10748AEb04B58e8F , 0xdead000000000000000000000000000000000001 , , ".to_string(),
    )
    .unwrap();

    assert_eq!(set.len(), 2);
    assert!(set.contains("0xc2132d05d31c914a87c6611c10748aeb04b58e8f"));
    assert!(set.contains("0xdead000000000000000000000000000000000001"));

    assert!(parse_allowed_token_addresses_env(String::new())
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_monitor_skips_non_allowlisted_erc20_token() {
    use std::collections::HashSet;
    use std::time::Duration;
    use tokio::time::sleep;

    let _ = tracing_subscriber::fmt::try_init();

    let rpc_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();
    let wallet =
        Wallet::new("test test test test test test test test test test test junk".to_string());
    let addr = wallet.derive_address(0).unwrap().to_string();

    let allowed_token = "0xc2132D05D31c914a87C6611C10748AEb04B58e8F";
    let spam_token = "0xdead000000000000000000000000000000000001";

    let mut allowlist = HashSet::new();
    allowlist.insert(allowed_token.to_lowercase());

    let mut config = test_config_with_allowlist(allowlist);
    config.database_url = db_file.path().to_str().unwrap().to_string();
    config.chains[0].rpc_url = rpc_server.uri();
    config.chains[0].get_logs_max_retries = 1;
    config.chains[0].get_logs_delay_ms = 1;

    let db = Db::new(&config.database_url).unwrap();
    db.register_account("user_1", 0, &addr, "http://localhost/webhook")
        .unwrap();

    let transfer_topic = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
    let from_topic = "0x0000000000000000000000000000000000000000000000000000000000000001";
    let to_topic = format!("0x000000000000000000000000{}", &addr[2..].to_lowercase());
    let amount_data = "0x00000000000000000000000000000000000000000000000000000000000f4240";

    Mock::given(method("POST"))
        .and(body_json_contains("eth_blockNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0xA"
        })))
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getBlockByNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(empty_block_rpc_response()))
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getLogs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [{
                "address": spam_token,
                "topics": [transfer_topic, from_topic, to_topic],
                "data": amount_data,
                "blockNumber": "0xA",
                "transactionHash": "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
                "transactionIndex": "0x0",
                "blockHash": "0x000000000000000000000000000000000000000000000000000000000000000a",
                "logIndex": "0x0",
                "removed": false
            }]
        })))
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_call"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x"
        })))
        .expect(0)
        .mount(&rpc_server)
        .await;

    let provider = http_provider_boxed(&rpc_server.uri());
    let deliverer = test_webhook_deliverer(db.clone());
    let monitor = Monitor::new(
        config.chains[0].clone(),
        deliverer,
        db.clone(),
        provider.clone(),
    );
    let handle = tokio::spawn(async move {
        monitor.run().await;
    });

    sleep(Duration::from_millis(1500)).await;
    handle.abort();

    assert_eq!(db.get_detected_erc20_deposits(TEST_CHAIN).unwrap().len(), 0);
}

#[tokio::test]
async fn test_monitor_records_allowlisted_erc20_token() {
    use std::collections::HashSet;
    use std::time::Duration;
    use tokio::time::sleep;

    let _ = tracing_subscriber::fmt::try_init();

    let rpc_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();
    let wallet =
        Wallet::new("test test test test test test test test test test test junk".to_string());
    let addr = wallet.derive_address(0).unwrap().to_string();

    let allowed_token = "0xc2132D05D31c914a87C6611C10748AEb04B58e8F";

    let mut allowlist = HashSet::new();
    allowlist.insert(allowed_token.to_lowercase());

    let mut config = test_config_with_allowlist(allowlist);
    config.database_url = db_file.path().to_str().unwrap().to_string();
    config.chains[0].rpc_url = rpc_server.uri();
    config.chains[0].get_logs_max_retries = 1;
    config.chains[0].get_logs_delay_ms = 1;

    let db = Db::new(&config.database_url).unwrap();
    db.register_account("user_1", 0, &addr, "http://localhost/webhook")
        .unwrap();
    db.store_token_metadata(TEST_CHAIN, allowed_token, "USDT", 6, "Tether USD")
        .unwrap();

    let transfer_topic = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
    let from_topic = "0x0000000000000000000000000000000000000000000000000000000000000001";
    let to_topic = format!("0x000000000000000000000000{}", &addr[2..].to_lowercase());
    let amount_data = "0x00000000000000000000000000000000000000000000000000000000000f4240";

    Mock::given(method("POST"))
        .and(body_json_contains("eth_blockNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0xA"
        })))
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getBlockByNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(empty_block_rpc_response()))
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getLogs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [{
                "address": allowed_token,
                "topics": [transfer_topic, from_topic, to_topic],
                "data": amount_data,
                "blockNumber": "0xA",
                "transactionHash": "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
                "transactionIndex": "0x0",
                "blockHash": "0x000000000000000000000000000000000000000000000000000000000000000a",
                "logIndex": "0x0",
                "removed": false
            }]
        })))
        .mount(&rpc_server)
        .await;

    let provider = http_provider_boxed(&rpc_server.uri());
    let deliverer = test_webhook_deliverer(db.clone());
    let monitor = Monitor::new(
        config.chains[0].clone(),
        deliverer,
        db.clone(),
        provider.clone(),
    );
    let handle = tokio::spawn(async move {
        monitor.run().await;
    });

    sleep(Duration::from_millis(1500)).await;
    handle.abort();

    assert_eq!(db.get_detected_erc20_deposits(TEST_CHAIN).unwrap().len(), 1);
}

#[tokio::test]
async fn test_sweeper_marks_non_allowlisted_deposit_failed_without_rpc() {
    use std::collections::HashSet;
    use std::time::Duration;
    use tokio::time::sleep;

    let _ = tracing_subscriber::fmt::try_init();

    let rpc_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();
    let wallet =
        Wallet::new("test test test test test test test test test test test junk".to_string());
    let addr = wallet.derive_address(0).unwrap().to_string();

    let allowed_token = "0xc2132D05D31c914a87C6611C10748AEb04B58e8F";
    let spam_token = "0xdead000000000000000000000000000000000001";

    let mut allowlist = HashSet::new();
    allowlist.insert(allowed_token.to_lowercase());

    let mut config = test_config_with_allowlist(allowlist);
    config.database_url = db_file.path().to_str().unwrap().to_string();
    config.chains[0].rpc_url = rpc_server.uri();

    let db = Db::new(&config.database_url).unwrap();
    db.register_account("user_1", 0, &addr, "http://localhost/webhook")
        .unwrap();
    db.record_erc20_deposit(
        TEST_CHAIN, "0xspam", 1, "user_1", "1000000", spam_token, "USDC",
    )
    .unwrap();

    Mock::given(method("POST"))
        .and(body_json_contains("eth_call"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x"
        })))
        .expect(0)
        .mount(&rpc_server)
        .await;

    let provider = http_provider_boxed(&rpc_server.uri());
    let faucet = Arc::new(
        Faucet::new(
            config.faucet_mnemonic.clone(),
            provider.clone(),
            &test_support::chain_existential(&config),
        )
        .unwrap(),
    );
    let deliverer = test_webhook_deliverer(db.clone());
    let webhook_worker = WebhookRetryService::new(Arc::clone(&deliverer));
    tokio::spawn(async move {
        webhook_worker.run().await;
    });
    let sweeper = Sweeper::new(
        config.chains[0].clone(),
        deliverer,
        db.clone(),
        wallet,
        provider.clone(),
        faucet,
    );
    let handle = tokio::spawn(async move {
        sweeper.run().await;
    });

    for _ in 0..10 {
        if db
            .get_detected_erc20_deposits(TEST_CHAIN)
            .unwrap()
            .is_empty()
        {
            break;
        }
        sleep(Duration::from_millis(300)).await;
    }
    handle.abort();

    assert_eq!(db.get_detected_erc20_deposits(TEST_CHAIN).unwrap().len(), 0);
}

#[tokio::test]
async fn test_erc20_faucet_failure_keeps_deposit_detected() {
    let _ = tracing_subscriber::fmt::try_init();

    let rpc_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();
    let wallet =
        Wallet::new("test test test test test test test test test test test junk".to_string());
    let addr = wallet.derive_address(0).unwrap().to_string();
    let token_address = "0xc2132D05D31c914a87C6611C10748AEb04B58e8F";

    let config = test_config(db_file.path().to_str().unwrap(), rpc_server.uri());
    let db = Db::new(&config.database_url).unwrap();
    db.register_account("user_1", 0, &addr, "http://localhost/webhook")
        .unwrap();
    db.record_erc20_deposit(
        TEST_CHAIN,
        "0xabc",
        1,
        "user_1",
        "1000000",
        token_address,
        "USDT",
    )
    .unwrap();

    Mock::given(method("POST"))
        .and(body_json_contains("eth_call"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1,
            "result": "0x00000000000000000000000000000000000000000000000000000000000f4240"
        })))
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_estimateGas"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x186a0"
        })))
        .mount(&rpc_server)
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
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getBalance"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x0"
        })))
        .mount(&rpc_server)
        .await;

    let provider = http_provider_boxed(&rpc_server.uri());
    let faucet = Arc::new(
        Faucet::new(
            config.faucet_mnemonic.clone(),
            provider.clone(),
            &test_support::chain_existential(&config),
        )
        .unwrap(),
    );
    let deliverer = test_webhook_deliverer(db.clone());
    let sweeper = Sweeper::new(
        config.chains[0].clone(),
        deliverer,
        db.clone(),
        wallet,
        provider,
        faucet,
    );

    for _ in 0..12 {
        sweeper.process_deposits_once().await.unwrap();
    }

    assert_eq!(db.get_detected_erc20_deposits(TEST_CHAIN).unwrap().len(), 1);
    assert_eq!(
        db.get_sweep_failure_count(TEST_CHAIN, "0xabc:1").unwrap(),
        0
    );
}

#[test]
fn test_retry_sweep_service_requeues_failed_erc20_deposit() {
    let tmp = NamedTempFile::new().unwrap();
    let db = Db::new(tmp.path().to_str().unwrap()).unwrap();
    db.record_erc20_deposit(TEST_CHAIN, "0xabc", 120, "user_1", "100", "0xtoken", "USDC")
        .unwrap();
    db.mark_erc20_deposit_failed(TEST_CHAIN, "0xabc:120")
        .unwrap();

    let config = test_config(tmp.path().to_str().unwrap(), "http://localhost:0");
    let rt = tokio::runtime::Runtime::new().unwrap();
    let service = rt.block_on(HotWalletService::new(config)).unwrap();

    let response = service
        .retry_sweep(RetrySweepRequest {
            chain: TEST_CHAIN.to_string(),
            tx_hash: "0xabc".to_string(),
            log_index: Some(120),
        })
        .unwrap();

    assert!(response.retried);
    assert_eq!(response.token_type, "erc20");
    assert_eq!(
        service
            .db()
            .get_detected_erc20_deposits(TEST_CHAIN)
            .unwrap()
            .len(),
        1
    );
}

// ========== Faucet Tests ==========

async fn mount_faucet_gas_mocks(server: &MockServer) {
    Mock::given(method("POST"))
        .and(body_json_contains("eth_estimateGas"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x5208"
        })))
        .mount(server)
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
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_maxPriorityFeePerGas"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x3B9ACA00"
        })))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_gasPrice"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x3B9ACA00"
        })))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_chainId"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x89"
        })))
        .mount(server)
        .await;
}

fn faucet_send_raw_tx_nonce(body: &[u8]) -> Option<u64> {
    use alloy::consensus::{transaction::Transaction, TxEnvelope};
    use alloy::eips::eip2718::Decodable2718;
    let body_str = String::from_utf8_lossy(body);
    let v: serde_json::Value = serde_json::from_str(&body_str).ok()?;
    let raw = v["params"][0].as_str()?;
    let bytes = alloy::hex::decode(raw.trim_start_matches("0x")).ok()?;
    let mut buf = bytes.as_slice();
    TxEnvelope::decode_2718(&mut buf).ok().map(|tx| tx.nonce())
}

fn faucet_receipt_response(tx_hash: &str) -> serde_json::Value {
    json!({
        "jsonrpc": "2.0", "id": 1,
        "result": {
            "transactionHash": tx_hash,
            "transactionIndex": "0x0",
            "blockHash": "0x000000000000000000000000000000000000000000000000000000000000000b",
            "blockNumber": "0xb",
            "from": "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
            "to": "0x70997970c51812dc3a010c7d01b50e0d17dc79c8",
            "cumulativeGasUsed": "0x5208",
            "gasUsed": "0x5208",
            "contractAddress": null,
            "logs": [],
            "status": "0x1",
            "logsBloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
            "type": "0x2",
            "effectiveGasPrice": "0x3B9ACA00"
        }
    })
}

#[tokio::test]
async fn test_faucet_insufficient_balance() {
    let rpc_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getBalance"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x0"
        })))
        .mount(&rpc_server)
        .await;

    let provider = http_provider_boxed(&rpc_server.uri());
    let faucet = Faucet::new(
        "test test test test test test test test test test test junk".to_string(),
        provider,
        "10000000000000000",
    )
    .unwrap();

    let wallet =
        Wallet::new("test test test test test test test test test test test junk".to_string());
    let to = wallet.derive_address(1).unwrap().to_string();

    let err = faucet.fund_new_address(&to).await.unwrap_err();
    assert!(err
        .to_string()
        .contains("Faucet has insufficient balance to fund new address"));
}

#[tokio::test]
async fn test_faucet_happy_path_single_fund() {
    let rpc_server = MockServer::start().await;
    mount_faucet_gas_mocks(&rpc_server).await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getBalance"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x0DE0B6B3A7640000"
        })))
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getTransactionCount"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x0"
        })))
        .mount(&rpc_server)
        .await;

    let tx_hash = "0x0000000000000000000000000000000000000000000000000000000000000001";
    Mock::given(method("POST"))
        .and(body_json_contains("eth_sendRawTransaction"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": tx_hash
        })))
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getTransactionReceipt"))
        .respond_with(ResponseTemplate::new(200).set_body_json(faucet_receipt_response(tx_hash)))
        .mount(&rpc_server)
        .await;

    let provider = http_provider_boxed(&rpc_server.uri());
    let faucet = Faucet::new(
        "test test test test test test test test test test test junk".to_string(),
        provider,
        "10000000000000000",
    )
    .unwrap();

    let wallet =
        Wallet::new("test test test test test test test test test test test junk".to_string());
    let to = wallet.derive_address(1).unwrap().to_string();

    let result = faucet.fund_new_address(&to).await.unwrap();
    assert_eq!(result, tx_hash);
}

#[tokio::test]
async fn test_faucet_concurrent_funds_use_distinct_nonces() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc as StdArc, Mutex as StdMutex};

    let rpc_server = MockServer::start().await;
    mount_faucet_gas_mocks(&rpc_server).await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getBalance"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x0DE0B6B3A7640000"
        })))
        .mount(&rpc_server)
        .await;

    let nonce_rpc_calls = StdArc::new(AtomicUsize::new(0));
    let nonce_calls_for_mock = nonce_rpc_calls.clone();
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getTransactionCount"))
        .respond_with(move |_: &wiremock::Request| {
            nonce_calls_for_mock.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1, "result": "0x0"
            }))
        })
        .mount(&rpc_server)
        .await;

    let captured_nonces = StdArc::new(StdMutex::new(Vec::<u64>::new()));
    let nonces_for_mock = captured_nonces.clone();
    let send_count = StdArc::new(AtomicUsize::new(0));
    let send_count_for_mock = send_count.clone();
    Mock::given(method("POST"))
        .and(body_json_contains("eth_sendRawTransaction"))
        .respond_with(move |req: &wiremock::Request| {
            if let Some(n) = faucet_send_raw_tx_nonce(&req.body) {
                nonces_for_mock.lock().unwrap().push(n);
            }
            let idx = send_count_for_mock.fetch_add(1, Ordering::SeqCst);
            let hash = format!("0x{:064x}", idx + 1);
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1, "result": hash
            }))
        })
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getTransactionReceipt"))
        .respond_with(move |req: &wiremock::Request| {
            let body_str = String::from_utf8_lossy(&req.body);
            let v: serde_json::Value =
                serde_json::from_str(&body_str).unwrap_or(json!({"params": []}));
            let tx_hash = v["params"][0]
                .as_str()
                .unwrap_or("0x0000000000000000000000000000000000000000000000000000000000000001");
            ResponseTemplate::new(200).set_body_json(faucet_receipt_response(tx_hash))
        })
        .mount(&rpc_server)
        .await;

    let provider = http_provider_boxed(&rpc_server.uri());
    let faucet = StdArc::new(
        Faucet::new(
            "test test test test test test test test test test test junk".to_string(),
            provider,
            "10000000000000000",
        )
        .unwrap(),
    );

    let wallet =
        Wallet::new("test test test test test test test test test test test junk".to_string());
    let to1 = wallet.derive_address(1).unwrap().to_string();
    let to2 = wallet.derive_address(2).unwrap().to_string();

    let faucet_a = faucet.clone();
    let faucet_b = faucet.clone();
    let (r1, r2) = tokio::join!(
        faucet_a.fund_new_address(&to1),
        faucet_b.fund_new_address(&to2),
    );

    assert!(r1.is_ok(), "first concurrent fund failed: {r1:?}");
    assert!(r2.is_ok(), "second concurrent fund failed: {r2:?}");
    assert_ne!(r1.unwrap(), r2.unwrap(), "expected distinct tx hashes");

    let mut nonces = captured_nonces.lock().unwrap().clone();
    nonces.sort_unstable();
    assert_eq!(nonces, vec![0, 1], "expected sequential nonces 0 and 1");
    assert_eq!(
        nonce_rpc_calls.load(Ordering::SeqCst),
        1,
        "expected a single eth_getTransactionCount (cached nonce reused)"
    );
}

#[tokio::test]
async fn test_faucet_send_failure_resets_nonce_cache() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;

    let rpc_server = MockServer::start().await;
    mount_faucet_gas_mocks(&rpc_server).await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getBalance"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x0DE0B6B3A7640000"
        })))
        .mount(&rpc_server)
        .await;

    let nonce_rpc_calls = StdArc::new(AtomicUsize::new(0));
    let nonce_calls_for_mock = nonce_rpc_calls.clone();
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getTransactionCount"))
        .respond_with(move |_: &wiremock::Request| {
            nonce_calls_for_mock.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1, "result": "0x0"
            }))
        })
        .mount(&rpc_server)
        .await;

    let send_attempts = StdArc::new(AtomicUsize::new(0));
    let send_attempts_for_mock = send_attempts.clone();
    Mock::given(method("POST"))
        .and(body_json_contains("eth_sendRawTransaction"))
        .respond_with(move |_: &wiremock::Request| {
            let attempt = send_attempts_for_mock.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                ResponseTemplate::new(500)
            } else {
                let tx_hash = "0x00000000000000000000000000000000000000000000000000000000000000ab";
                ResponseTemplate::new(200).set_body_json(json!({
                    "jsonrpc": "2.0", "id": 1, "result": tx_hash
                }))
            }
        })
        .mount(&rpc_server)
        .await;

    let tx_hash = "0x00000000000000000000000000000000000000000000000000000000000000ab";
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getTransactionReceipt"))
        .respond_with(ResponseTemplate::new(200).set_body_json(faucet_receipt_response(tx_hash)))
        .mount(&rpc_server)
        .await;

    let provider = http_provider_boxed(&rpc_server.uri());
    let faucet = Faucet::new(
        "test test test test test test test test test test test junk".to_string(),
        provider,
        "10000000000000000",
    )
    .unwrap();

    let wallet =
        Wallet::new("test test test test test test test test test test test junk".to_string());
    let to = wallet.derive_address(1).unwrap().to_string();

    assert!(faucet.fund_new_address(&to).await.is_err());
    assert_eq!(nonce_rpc_calls.load(Ordering::SeqCst), 1);

    let result = faucet.fund_new_address(&to).await.unwrap();
    assert_eq!(result, tx_hash);
    assert_eq!(
        nonce_rpc_calls.load(Ordering::SeqCst),
        2,
        "expected nonce cache reset to re-fetch eth_getTransactionCount"
    );
}

#[tokio::test]
async fn test_register_does_not_fund_at_registration() {
    let rpc_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();
    let config = test_config(db_file.path().to_str().unwrap(), rpc_server.uri());

    Mock::given(method("POST"))
        .and(body_json_contains("eth_sendRawTransaction"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0xdead"
        })))
        .expect(0)
        .mount(&rpc_server)
        .await;

    let service = HotWalletService::new(config).await.unwrap();
    let response = service
        .register(RegisterRequest {
            id: "lazy_user".to_string(),
            webhook_url: "http://localhost/webhook".to_string(),
        })
        .await
        .unwrap();

    assert!(response.funding_tx.is_none());
}

#[test]
fn test_same_tx_hash_isolated_per_chain() {
    let tmp = NamedTempFile::new().unwrap();
    let db = Db::new(tmp.path().to_str().unwrap()).unwrap();

    db.record_deposit("base", "0xsame", "user1", "100").unwrap();
    db.record_deposit("polygon", "0xsame", "user2", "200")
        .unwrap();

    let base = db.get_detected_deposits("base").unwrap();
    let polygon = db.get_detected_deposits("polygon").unwrap();

    assert_eq!(base[0].1, "user1");
    assert_eq!(polygon[0].1, "user2");
}

#[tokio::test]
async fn test_verify_transfer_unknown_chain() {
    let rpc_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();
    let config = test_config(db_file.path().to_str().unwrap(), rpc_server.uri());
    let service = HotWalletService::new(config).await.unwrap();

    let result = service
        .verify_transfer(VerifyTransferRequest {
            chain: "ethereum".to_string(),
            tx_hash: "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef"
                .to_string(),
            to_address: "0x742d35Cc6634C0532925a3b844Bc454e4438f44e".to_string(),
            amount: "1".to_string(),
            token_type: "native".to_string(),
            token_address: None,
            token_symbol: None,
        })
        .await;

    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Unknown chain"));
}

#[tokio::test]
async fn test_block_number_unknown_chain() {
    let rpc_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();
    let config = test_config(db_file.path().to_str().unwrap(), rpc_server.uri());
    let service = HotWalletService::new(config).await.unwrap();

    let err = service.get_block_number("unknown").unwrap_err();
    assert!(err.to_string().contains("Unknown chain"));
}

// ========== Monitor Catch-Up Acceleration Tests ==========
//
// Builds a native-transfer block RPC response by cloning the shared empty-block
// skeleton (so the many fixed-size hex fields like logsBloom stay valid) and
// overriding only the block number and transaction list.
fn native_tx_block_response(block_num: u64, to_addr: &str, tx_hash: &str) -> serde_json::Value {
    let mut resp = empty_block_rpc_response();
    let block_hex = format!("0x{block_num:x}");
    resp["result"]["number"] = json!(block_hex);
    resp["result"]["transactions"] = json!([{
        "hash": tx_hash,
        "nonce": "0x0",
        "blockHash": "0x000000000000000000000000000000000000000000000000000000000000000a",
        "blockNumber": block_hex,
        "transactionIndex": "0x0",
        "from": "0x0000000000000000000000000000000000000001",
        "to": to_addr,
        "value": "0xDE0B6B3A7640000",
        "gas": "0x5208",
        "gasPrice": "0x3B9ACA00",
        "input": "0x",
        "v": "0x1b",
        "r": "0x1",
        "s": "0x1",
        "type": "0x0",
        "chainId": "0x1"
    }]);
    resp
}

/// [REGRESSION] `get_logs_with_retry` used to treat an empty result as a failure and
/// retry up to `get_logs_max_retries` times with `get_logs_delay_ms` sleeps between
/// attempts — on a fast-moving chain with sparse matching transfers, this alone could
/// burn seconds per block. An empty result is valid; it must return immediately.
#[tokio::test]
async fn test_get_logs_empty_result_returns_without_retry() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;
    use std::time::Duration;
    use tokio::time::sleep;

    let _ = tracing_subscriber::fmt::try_init();

    let rpc_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();
    let mut config = test_config(db_file.path().to_str().unwrap(), rpc_server.uri());
    config.chains[0].get_logs_max_retries = 30;
    config.chains[0].get_logs_delay_ms = 50;
    // Large enough that only one catch_up cycle runs during the test window, so the
    // get_logs call count reflects a single scan attempt, not multiple poll loops.
    config.chains[0].poll_interval = 3600;

    let db = Db::new(&config.database_url).unwrap();

    Mock::given(method("POST"))
        .and(body_json_contains("eth_blockNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0xA"
        })))
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getBlockByNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(empty_block_rpc_response()))
        .mount(&rpc_server)
        .await;

    let get_logs_calls = StdArc::new(AtomicUsize::new(0));
    let calls_for_mock = get_logs_calls.clone();
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getLogs"))
        .respond_with(move |_: &wiremock::Request| {
            calls_for_mock.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1, "result": []
            }))
        })
        .mount(&rpc_server)
        .await;

    let provider = http_provider_boxed(&rpc_server.uri());
    let deliverer = test_webhook_deliverer(db.clone());
    let monitor = Monitor::new(
        config.chains[0].clone(),
        deliverer,
        db.clone(),
        provider.clone(),
    );
    let handle = tokio::spawn(async move {
        monitor.run().await;
    });

    // If the regression reappeared, a buggy retry loop (30 attempts x 50ms delay)
    // would still be mid-retry at 800ms, producing well over 1 call.
    sleep(Duration::from_millis(800)).await;
    handle.abort();

    assert_eq!(
        get_logs_calls.load(Ordering::SeqCst),
        1,
        "an empty get_logs result must not be retried"
    );
}

/// The `eth_getLogs` filter must be narrowed to the allowlisted token contracts,
/// instead of matching every ERC20 Transfer event on the chain.
#[tokio::test]
async fn test_erc20_filter_includes_allowlisted_token_address() {
    use std::collections::HashSet;
    use std::time::Duration;
    use tokio::time::sleep;

    let _ = tracing_subscriber::fmt::try_init();

    let rpc_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();
    let wallet =
        Wallet::new("test test test test test test test test test test test junk".to_string());
    let addr = wallet.derive_address(0).unwrap().to_string();

    let allowed_token = "0xc2132D05D31c914a87C6611C10748AEb04B58e8F";
    let mut allowlist = HashSet::new();
    allowlist.insert(allowed_token.to_lowercase());

    let mut config = test_config_with_allowlist(allowlist);
    config.database_url = db_file.path().to_str().unwrap().to_string();
    config.chains[0].rpc_url = rpc_server.uri();
    config.chains[0].get_logs_max_retries = 1;
    config.chains[0].get_logs_delay_ms = 1;
    config.chains[0].poll_interval = 3600;

    let db = Db::new(&config.database_url).unwrap();
    db.register_account("user_1", 0, &addr, "http://localhost/webhook")
        .unwrap();

    Mock::given(method("POST"))
        .and(body_json_contains("eth_blockNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0xA"
        })))
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getBlockByNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(empty_block_rpc_response()))
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getLogs"))
        .and(body_json_contains_ci(&allowed_token.to_lowercase()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": []
        })))
        .expect(1)
        .mount(&rpc_server)
        .await;

    let provider = http_provider_boxed(&rpc_server.uri());
    let deliverer = test_webhook_deliverer(db.clone());
    let monitor = Monitor::new(
        config.chains[0].clone(),
        deliverer,
        db.clone(),
        provider.clone(),
    );
    let handle = tokio::spawn(async move {
        monitor.run().await;
    });

    sleep(Duration::from_millis(300)).await;
    handle.abort();

    // Verification of the `.expect(1)` mock above happens when `rpc_server` drops:
    // if the allowlisted token address never appeared in the eth_getLogs request,
    // this mock never matched and the drop panics.
}

/// When the monitor is far enough behind head, `catch_up` must switch to the
/// batched path: one ranged `eth_getLogs` call covering the whole gap (in one
/// chunk, since it's under `catch_up_chunk_size`) instead of one call per block.
#[tokio::test]
async fn test_batch_catchup_ranged_get_logs_records_erc20_deposit() {
    use std::collections::HashSet;
    use std::time::Duration;
    use tokio::time::sleep;

    let _ = tracing_subscriber::fmt::try_init();

    let rpc_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();
    let wallet =
        Wallet::new("test test test test test test test test test test test junk".to_string());
    let addr = wallet.derive_address(0).unwrap().to_string();

    let allowed_token = "0xc2132D05D31c914a87C6611C10748AEb04B58e8F";
    let mut allowlist = HashSet::new();
    allowlist.insert(allowed_token.to_lowercase());

    let mut config = test_config_with_allowlist(allowlist);
    config.database_url = db_file.path().to_str().unwrap().to_string();
    config.chains[0].rpc_url = rpc_server.uri();
    config.chains[0].get_logs_max_retries = 1;
    config.chains[0].get_logs_delay_ms = 1;
    config.chains[0].poll_interval = 3600;

    let db = Db::new(&config.database_url).unwrap();
    db.register_account("user_1", 0, &addr, "http://localhost/webhook")
        .unwrap();
    db.store_token_metadata(TEST_CHAIN, allowed_token, "USDT", 6, "Tether USD")
        .unwrap();
    // last_processed = 1, head = 21 -> gap of 20 blocks, above the batch threshold.
    db.set_last_processed_block(TEST_CHAIN, 1).unwrap();

    let transfer_topic = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
    let from_topic = "0x0000000000000000000000000000000000000000000000000000000000000001";
    let to_topic = format!("0x000000000000000000000000{}", &addr[2..].to_lowercase());
    let amount_data = "0x00000000000000000000000000000000000000000000000000000000000f4240";
    let tx_hash = format!("0x{}", "a".repeat(64));

    Mock::given(method("POST"))
        .and(body_json_contains("eth_blockNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x15"
        })))
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getBlockByNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(empty_block_rpc_response()))
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getLogs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [{
                "address": allowed_token,
                "topics": [transfer_topic, from_topic, to_topic],
                "data": amount_data,
                "blockNumber": "0x5",
                "transactionHash": tx_hash,
                "transactionIndex": "0x0",
                "blockHash": "0x000000000000000000000000000000000000000000000000000000000000000a",
                "logIndex": "0x0",
                "removed": false
            }]
        })))
        .expect(1)
        .mount(&rpc_server)
        .await;

    let provider = http_provider_boxed(&rpc_server.uri());
    let deliverer = test_webhook_deliverer(db.clone());
    let monitor = Monitor::new(
        config.chains[0].clone(),
        deliverer,
        db.clone(),
        provider.clone(),
    );
    let handle = tokio::spawn(async move {
        monitor.run().await;
    });

    sleep(Duration::from_millis(500)).await;
    handle.abort();

    let deposits = db.get_detected_erc20_deposits(TEST_CHAIN).unwrap();
    assert_eq!(
        deposits.len(),
        1,
        "expected one ERC20 deposit from the ranged batch scan"
    );
    assert_eq!(db.get_last_processed_block(TEST_CHAIN).unwrap(), 21);

    let deposit_id = format!("{TEST_CHAIN}:{tx_hash}:0");
    assert!(
        db.get_webhook_delivery(&deposit_id, "deposit_detected")
            .unwrap()
            .is_some(),
        "expected deposit_detected webhook enqueued exactly once"
    );
}

/// Providers cap `eth_getLogs` responses (Alchemy: "Log response size exceeded" for
/// an oversized range/response, with no partial result). The batch path must bisect
/// the range on error and retry with the two halves rather than failing outright.
#[tokio::test]
async fn test_batch_catchup_bisects_on_provider_error() {
    use std::collections::HashSet;
    use std::time::Duration;
    use tokio::time::sleep;

    let _ = tracing_subscriber::fmt::try_init();

    let rpc_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();
    let wallet =
        Wallet::new("test test test test test test test test test test test junk".to_string());
    let addr = wallet.derive_address(0).unwrap().to_string();

    let allowed_token = "0xc2132D05D31c914a87C6611C10748AEb04B58e8F";
    let mut allowlist = HashSet::new();
    allowlist.insert(allowed_token.to_lowercase());

    let mut config = test_config_with_allowlist(allowlist);
    config.database_url = db_file.path().to_str().unwrap().to_string();
    config.chains[0].rpc_url = rpc_server.uri();
    config.chains[0].get_logs_max_retries = 1;
    config.chains[0].get_logs_delay_ms = 1;
    config.chains[0].poll_interval = 3600;

    let db = Db::new(&config.database_url).unwrap();
    db.register_account("user_1", 0, &addr, "http://localhost/webhook")
        .unwrap();
    db.store_token_metadata(TEST_CHAIN, allowed_token, "USDT", 6, "Tether USD")
        .unwrap();
    // last_processed = 1, head = 21 -> whole chunk is (1, 21); mid-bisect is (1,11) + (12,21).
    db.set_last_processed_block(TEST_CHAIN, 1).unwrap();

    let transfer_topic = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
    let from_topic = "0x0000000000000000000000000000000000000000000000000000000000000001";
    let to_topic = format!("0x000000000000000000000000{}", &addr[2..].to_lowercase());
    let amount_data = "0x00000000000000000000000000000000000000000000000000000000000f4240";
    let tx_hash_left = format!("0x{}", "1".repeat(64));
    let tx_hash_right = format!("0x{}", "2".repeat(64));

    fn transfer_log(
        token: &str,
        topics: [&str; 3],
        data: &str,
        block_hex: &str,
        tx_hash: &str,
    ) -> serde_json::Value {
        json!({
            "address": token,
            "topics": topics,
            "data": data,
            "blockNumber": block_hex,
            "transactionHash": tx_hash,
            "transactionIndex": "0x0",
            "blockHash": "0x000000000000000000000000000000000000000000000000000000000000000a",
            "logIndex": "0x0",
            "removed": false
        })
    }

    Mock::given(method("POST"))
        .and(body_json_contains("eth_blockNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x15"
        })))
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getBlockByNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(empty_block_rpc_response()))
        .mount(&rpc_server)
        .await;

    // Whole-range call (1..=21): simulates a provider rejecting an oversized range.
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getLogs"))
        .and(body_json_contains(&field_hex("fromBlock", 1)))
        .and(body_json_contains(&field_hex("toBlock", 21)))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&rpc_server)
        .await;

    // Left half (1..=11): succeeds.
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getLogs"))
        .and(body_json_contains(&field_hex("fromBlock", 1)))
        .and(body_json_contains(&field_hex("toBlock", 11)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [transfer_log(
                allowed_token,
                [transfer_topic, from_topic, &to_topic],
                amount_data,
                "0x5",
                &tx_hash_left,
            )]
        })))
        .expect(1)
        .mount(&rpc_server)
        .await;

    // Right half (12..=21): succeeds.
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getLogs"))
        .and(body_json_contains(&field_hex("fromBlock", 12)))
        .and(body_json_contains(&field_hex("toBlock", 21)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [transfer_log(
                allowed_token,
                [transfer_topic, from_topic, &to_topic],
                amount_data,
                "0x10",
                &tx_hash_right,
            )]
        })))
        .expect(1)
        .mount(&rpc_server)
        .await;

    let provider = http_provider_boxed(&rpc_server.uri());
    let deliverer = test_webhook_deliverer(db.clone());
    let monitor = Monitor::new(
        config.chains[0].clone(),
        deliverer,
        db.clone(),
        provider.clone(),
    );
    let handle = tokio::spawn(async move {
        monitor.run().await;
    });

    sleep(Duration::from_millis(500)).await;
    handle.abort();

    let deposits = db.get_detected_erc20_deposits(TEST_CHAIN).unwrap();
    assert_eq!(
        deposits.len(),
        2,
        "expected deposits from both bisected halves"
    );
    assert_eq!(db.get_last_processed_block(TEST_CHAIN).unwrap(), 21);
}

/// If a range has been bisected all the way down to a single block and that block
/// still errors, the error must surface (no infinite recursion) and the chunk must
/// not be checkpointed as processed.
#[tokio::test]
async fn test_batch_catchup_bisect_floor_surfaces_error() {
    use std::collections::HashSet;
    use std::time::Duration;
    use tokio::time::sleep;

    let _ = tracing_subscriber::fmt::try_init();

    let rpc_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();

    let allowed_token = "0xc2132D05D31c914a87C6611C10748AEb04B58e8F";
    let mut allowlist = HashSet::new();
    allowlist.insert(allowed_token.to_lowercase());

    let mut config = test_config_with_allowlist(allowlist);
    config.database_url = db_file.path().to_str().unwrap().to_string();
    config.chains[0].rpc_url = rpc_server.uri();
    config.chains[0].get_logs_max_retries = 1;
    config.chains[0].get_logs_delay_ms = 1;
    config.chains[0].poll_interval = 3600;
    // Small chunk so the first (and only, for this test) chunk is just (1, 2),
    // keeping the bisection tree to 3 nodes: (1,2) -> (1,1) + (2,2).
    config.chains[0].catch_up_chunk_size = 2;

    let db = Db::new(&config.database_url).unwrap();
    // last_processed = 1, head = 21 -> gap of 20, above the batch threshold, even
    // though the first chunk itself only spans 2 blocks.
    db.set_last_processed_block(TEST_CHAIN, 1).unwrap();

    Mock::given(method("POST"))
        .and(body_json_contains("eth_blockNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x15"
        })))
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getBlockByNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(empty_block_rpc_response()))
        .mount(&rpc_server)
        .await;

    // The (1,2) range errors, bisects into (1,1) and (2,2). Bisection short-circuits
    // on the first failing half (fail-fast: once part of the chunk is unrecoverable,
    // there's no point burning a call on the sibling), so only (1,2) and (1,1) are
    // ever queried; (2,2) must not be.
    for (from, to) in [(1, 2), (1, 1)] {
        Mock::given(method("POST"))
            .and(body_json_contains("eth_getLogs"))
            .and(body_json_contains(&field_hex("fromBlock", from)))
            .and(body_json_contains(&field_hex("toBlock", to)))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&rpc_server)
            .await;
    }
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getLogs"))
        .and(body_json_contains(&field_hex("fromBlock", 2)))
        .and(body_json_contains(&field_hex("toBlock", 2)))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&rpc_server)
        .await;

    let provider = http_provider_boxed(&rpc_server.uri());
    let deliverer = test_webhook_deliverer(db.clone());
    let monitor = Monitor::new(
        config.chains[0].clone(),
        deliverer,
        db.clone(),
        provider.clone(),
    );
    let handle = tokio::spawn(async move {
        monitor.run().await;
    });

    sleep(Duration::from_millis(500)).await;
    handle.abort();

    assert_eq!(
        db.get_last_processed_block(TEST_CHAIN).unwrap(),
        1,
        "a chunk that errors all the way to the bisection floor must not be checkpointed"
    );
    assert_eq!(db.get_detected_erc20_deposits(TEST_CHAIN).unwrap().len(), 0);
}

/// The batch path fetches native blocks concurrently; deposits from multiple blocks
/// within one chunk must all be recorded, not just the first or last.
#[tokio::test]
async fn test_batch_catchup_concurrent_native_fetch_records_multiple_blocks() {
    use std::collections::HashSet;
    use std::time::Duration;
    use tokio::time::sleep;

    let _ = tracing_subscriber::fmt::try_init();

    let rpc_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();
    let wallet =
        Wallet::new("test test test test test test test test test test test junk".to_string());
    let addr = wallet.derive_address(0).unwrap().to_string();

    let allowed_token = "0xc2132D05D31c914a87C6611C10748AEb04B58e8F";
    let mut allowlist = HashSet::new();
    allowlist.insert(allowed_token.to_lowercase());

    let mut config = test_config_with_allowlist(allowlist);
    config.database_url = db_file.path().to_str().unwrap().to_string();
    config.chains[0].rpc_url = rpc_server.uri();
    config.chains[0].get_logs_max_retries = 1;
    config.chains[0].get_logs_delay_ms = 1;
    config.chains[0].poll_interval = 3600;

    let db = Db::new(&config.database_url).unwrap();
    db.register_account("user_1", 0, &addr, "http://localhost/webhook")
        .unwrap();
    // last_processed = 1, head = 21 -> gap of 20, above the batch threshold.
    db.set_last_processed_block(TEST_CHAIN, 1).unwrap();

    Mock::given(method("POST"))
        .and(body_json_contains("eth_blockNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x15"
        })))
        .mount(&rpc_server)
        .await;

    // No ERC20 activity in this test; keep the batch path focused on native.
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getLogs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": []
        })))
        .mount(&rpc_server)
        .await;

    let addr_for_mock = addr.clone();
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getBlockByNumber"))
        .respond_with(move |req: &wiremock::Request| {
            let body_str = String::from_utf8_lossy(&req.body);
            let parsed: serde_json::Value =
                serde_json::from_str(&body_str).unwrap_or(json!({"params": []}));
            let block_hex = parsed["params"][0].as_str().unwrap_or("0x0");
            let block_num =
                u64::from_str_radix(block_hex.trim_start_matches("0x"), 16).unwrap_or(0);

            if block_num == 3 || block_num == 7 {
                let tx_hash = format!("0x{block_num:064x}");
                ResponseTemplate::new(200).set_body_json(native_tx_block_response(
                    block_num,
                    &addr_for_mock,
                    &tx_hash,
                ))
            } else {
                ResponseTemplate::new(200).set_body_json(empty_block_rpc_response())
            }
        })
        .mount(&rpc_server)
        .await;

    let provider = http_provider_boxed(&rpc_server.uri());
    let deliverer = test_webhook_deliverer(db.clone());
    let monitor = Monitor::new(
        config.chains[0].clone(),
        deliverer,
        db.clone(),
        provider.clone(),
    );
    let handle = tokio::spawn(async move {
        monitor.run().await;
    });

    sleep(Duration::from_millis(800)).await;
    handle.abort();

    let deposits = db.get_detected_deposits(TEST_CHAIN).unwrap();
    assert_eq!(
        deposits.len(),
        2,
        "expected native deposits from both blocks fetched concurrently within the chunk"
    );
    assert_eq!(db.get_last_processed_block(TEST_CHAIN).unwrap(), 21);
}

/// Re-running a chunk (e.g. after a restart that re-reads a stale checkpoint) must
/// not record a duplicate deposit or re-trigger the webhook for one already detected.
#[tokio::test]
async fn test_batch_catchup_replay_is_idempotent() {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;
    use std::time::Duration;
    use tokio::time::sleep;

    let _ = tracing_subscriber::fmt::try_init();

    let rpc_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();
    let wallet =
        Wallet::new("test test test test test test test test test test test junk".to_string());
    let addr = wallet.derive_address(0).unwrap().to_string();

    let allowed_token = "0xc2132D05D31c914a87C6611C10748AEb04B58e8F";
    let mut allowlist = HashSet::new();
    allowlist.insert(allowed_token.to_lowercase());

    let mut config = test_config_with_allowlist(allowlist);
    config.database_url = db_file.path().to_str().unwrap().to_string();
    config.chains[0].rpc_url = rpc_server.uri();
    config.chains[0].get_logs_max_retries = 1;
    config.chains[0].get_logs_delay_ms = 1;
    config.chains[0].poll_interval = 3600;

    let db = Db::new(&config.database_url).unwrap();
    db.register_account("user_1", 0, &addr, "http://localhost/webhook")
        .unwrap();
    db.store_token_metadata(TEST_CHAIN, allowed_token, "USDT", 6, "Tether USD")
        .unwrap();
    db.set_last_processed_block(TEST_CHAIN, 1).unwrap();

    let transfer_topic = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
    let from_topic = "0x0000000000000000000000000000000000000000000000000000000000000001";
    let to_topic = format!("0x000000000000000000000000{}", &addr[2..].to_lowercase());
    let amount_data = "0x00000000000000000000000000000000000000000000000000000000000f4240";
    let tx_hash = format!("0x{}", "c".repeat(64));

    Mock::given(method("POST"))
        .and(body_json_contains("eth_blockNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": "0x15"
        })))
        .mount(&rpc_server)
        .await;

    Mock::given(method("POST"))
        .and(body_json_contains("eth_getBlockByNumber"))
        .respond_with(ResponseTemplate::new(200).set_body_json(empty_block_rpc_response()))
        .mount(&rpc_server)
        .await;

    let get_logs_calls = StdArc::new(AtomicUsize::new(0));
    let calls_for_mock = get_logs_calls.clone();
    let log_entry = json!({
        "address": allowed_token,
        "topics": [transfer_topic, from_topic, to_topic],
        "data": amount_data,
        "blockNumber": "0x5",
        "transactionHash": tx_hash,
        "transactionIndex": "0x0",
        "blockHash": "0x000000000000000000000000000000000000000000000000000000000000000a",
        "logIndex": "0x0",
        "removed": false
    });
    Mock::given(method("POST"))
        .and(body_json_contains("eth_getLogs"))
        .respond_with(move |_: &wiremock::Request| {
            calls_for_mock.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": [log_entry.clone()]
            }))
        })
        .mount(&rpc_server)
        .await;

    let provider = http_provider_boxed(&rpc_server.uri());

    // First run: drains the backlog, records the deposit, checkpoints to 21.
    {
        let deliverer = test_webhook_deliverer(db.clone());
        let monitor = Monitor::new(
            config.chains[0].clone(),
            deliverer,
            db.clone(),
            provider.clone(),
        );
        let handle = tokio::spawn(async move {
            monitor.run().await;
        });
        sleep(Duration::from_millis(500)).await;
        handle.abort();
    }

    assert_eq!(db.get_detected_erc20_deposits(TEST_CHAIN).unwrap().len(), 1);
    assert_eq!(db.get_last_processed_block(TEST_CHAIN).unwrap(), 21);

    // Simulate replaying the same chunk (e.g. a restart reading a stale checkpoint)
    // by resetting last_processed back to the chunk start.
    db.set_last_processed_block(TEST_CHAIN, 1).unwrap();

    {
        let deliverer = test_webhook_deliverer(db.clone());
        let monitor = Monitor::new(
            config.chains[0].clone(),
            deliverer,
            db.clone(),
            provider.clone(),
        );
        let handle = tokio::spawn(async move {
            monitor.run().await;
        });
        sleep(Duration::from_millis(500)).await;
        handle.abort();
    }

    assert_eq!(
        get_logs_calls.load(Ordering::SeqCst),
        2,
        "expected the ranged get_logs call to run again on replay"
    );
    assert_eq!(
        db.get_detected_erc20_deposits(TEST_CHAIN).unwrap().len(),
        1,
        "replaying the chunk must not record a duplicate deposit"
    );

    let deposit_id = format!("{TEST_CHAIN}:{tx_hash}:0");
    let delivery = db
        .get_webhook_delivery(&deposit_id, "deposit_detected")
        .unwrap();
    assert!(
        delivery.is_some(),
        "webhook delivery row should still exist after replay"
    );
    assert_eq!(
        delivery.unwrap().attempt_count,
        0,
        "replay must not re-enqueue/re-attempt the webhook for an already-detected deposit"
    );
}
