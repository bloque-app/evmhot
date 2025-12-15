use crate::config::{Config, ProviderUrl};
use crate::db::Db;
use crate::monitor::Monitor;
use crate::sweeper::Sweeper;
use crate::wallet::Wallet;
use crate::{HotWalletService, VerifyTransferRequest, VerifyTransferResponse};
use alloy::providers::ProviderBuilder;
use serde_json::json;
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
    db.record_deposit(tx_hash, id, amount).unwrap();

    let deposits = db.get_detected_deposits().unwrap();
    assert_eq!(deposits.len(), 1);
    assert_eq!(deposits[0].0, tx_hash);
    assert_eq!(deposits[0].2, amount);

    // Test Sweep Mark
    db.mark_deposit_swept(tx_hash).unwrap();
    let deposits_after = db.get_detected_deposits().unwrap();
    assert_eq!(deposits_after.len(), 0);
}

// ========== Monitor Unit Tests ==========

#[tokio::test]
async fn test_monitor_creation_with_http_provider() {
    // Test that Monitor can be created with an HTTP provider
    let db_file = NamedTempFile::new().unwrap();
    let db = Db::new(db_file.path().to_str().unwrap()).unwrap();

    let config = Config {
        database_url: db_file.path().to_str().unwrap().to_string(),
        provider_url: ProviderUrl::Http("http://localhost:8545".to_string()),
        mnemonic: "test test test test test test test test test test test junk".to_string(),
        treasury_address: "0x9999999999999999999999999999999999999999".to_string(),
        port: 3000,
        poll_interval: 1,
        block_offset_from_head: 0,
        faucet_mnemonic: "test test test test test test test test test test test junk".to_string(),
        existential_deposit: "10000000000000000".to_string(),
        faucet_address: "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266".to_string(),
    };

    // Create provider and monitor (no actual connection needed for this test)
    let provider = ProviderBuilder::new().on_http("http://localhost:8545".parse().unwrap());
    let _monitor = Monitor::new(config, db.clone(), provider);
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
    let deposits_before = db.get_detected_deposits().unwrap();
    assert_eq!(deposits_before.len(), 0);

    // Simulate Monitor recording a deposit
    db.record_deposit("0xtxhash", "test_user", "1000000000000000000")
        .unwrap();

    let deposits_after = db.get_detected_deposits().unwrap();
    assert_eq!(deposits_after.len(), 1);
    assert_eq!(deposits_after[0].0, "0xtxhash");
    assert_eq!(deposits_after[0].1, "test_user");
    assert_eq!(deposits_after[0].2, "1000000000000000000");

    // Test block tracking
    db.set_last_processed_block(100).unwrap();
    assert_eq!(db.get_last_processed_block().unwrap(), 100);
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

    let config = Config {
        database_url: db_file.path().to_str().unwrap().to_string(),
        provider_url: ProviderUrl::Http("http://localhost:8545".to_string()),
        mnemonic: "test test test test test test test test test test test junk".to_string(),
        treasury_address: "0x9999999999999999999999999999999999999999".to_string(),
        port: 3000,
        poll_interval: 1,
        block_offset_from_head: 0,
        faucet_mnemonic: "test test test test test test test test test test test junk".to_string(),
        existential_deposit: "10000000000000000".to_string(),
        faucet_address: "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266".to_string(),
    };

    let wallet = Wallet::new(config.mnemonic.clone());
    let provider = ProviderBuilder::new().on_http("http://localhost:8545".parse().unwrap());

    Sweeper::new(config, db, wallet, provider);
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
    db.record_deposit("0xtx123", "test_user", "1000000000000000000")
        .unwrap();

    // Verify deposit exists
    let deposits_before = db.get_detected_deposits().unwrap();
    assert_eq!(deposits_before.len(), 1);
    assert_eq!(deposits_before[0].0, "0xtx123");
    assert_eq!(deposits_before[0].1, "test_user");

    // Simulate sweep completion
    db.mark_deposit_swept("0xtx123").unwrap();
    let deposits_after = db.get_detected_deposits().unwrap();
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
    db.record_deposit("0xtx1", "user_0", "1000000000000000000")
        .unwrap();
    db.record_deposit("0xtx2", "user_1", "2000000000000000000")
        .unwrap();
    db.record_deposit("0xtx3", "user_2", "3000000000000000000")
        .unwrap();

    // Verify all deposits are tracked
    let deposits = db.get_detected_deposits().unwrap();
    assert_eq!(deposits.len(), 3);

    // Process one deposit at a time
    db.mark_deposit_swept("0xtx1").unwrap();
    let deposits_after_1 = db.get_detected_deposits().unwrap();
    assert_eq!(deposits_after_1.len(), 2);

    db.mark_deposit_swept("0xtx2").unwrap();
    let deposits_after_2 = db.get_detected_deposits().unwrap();
    assert_eq!(deposits_after_2.len(), 1);

    db.mark_deposit_swept("0xtx3").unwrap();
    let deposits_after_3 = db.get_detected_deposits().unwrap();
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

#[tokio::test]
async fn test_verify_native_transfer_success() {
    let _ = tracing_subscriber::fmt::try_init();

    // Setup Mock RPC
    let rpc_server = MockServer::start().await;
    let db_file = NamedTempFile::new().unwrap();

    let config = Config {
        database_url: db_file.path().to_str().unwrap().to_string(),
        provider_url: ProviderUrl::Http(rpc_server.uri()),
        mnemonic: "test test test test test test test test test test test junk".to_string(),
        treasury_address: "0x9999999999999999999999999999999999999999".to_string(),
        port: 3000,
        poll_interval: 1,
        block_offset_from_head: 0,
        faucet_mnemonic: "test test test test test test test test test test test junk".to_string(),
        existential_deposit: "10000000000000000".to_string(),
        faucet_address: "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266".to_string(),
    };

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
    let service = HotWalletService::new_http(config).await.unwrap();

    // Test verification
    let request = VerifyTransferRequest {
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

    let config = Config {
        database_url: db_file.path().to_str().unwrap().to_string(),
        provider_url: ProviderUrl::Http(rpc_server.uri()),
        mnemonic: "test test test test test test test test test test test junk".to_string(),
        treasury_address: "0x9999999999999999999999999999999999999999".to_string(),
        port: 3000,
        poll_interval: 1,
        block_offset_from_head: 0,
        faucet_mnemonic: "test test test test test test test test test test test junk".to_string(),
        existential_deposit: "10000000000000000".to_string(),
        faucet_address: "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266".to_string(),
    };

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

    let service = HotWalletService::new_http(config).await.unwrap();

    // Request expects 2 ETH but tx only has 1 ETH
    let request = VerifyTransferRequest {
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

    let config = Config {
        database_url: db_file.path().to_str().unwrap().to_string(),
        provider_url: ProviderUrl::Http(rpc_server.uri()),
        mnemonic: "test test test test test test test test test test test junk".to_string(),
        treasury_address: "0x9999999999999999999999999999999999999999".to_string(),
        port: 3000,
        poll_interval: 1,
        block_offset_from_head: 0,
        faucet_mnemonic: "test test test test test test test test test test test junk".to_string(),
        existential_deposit: "10000000000000000".to_string(),
        faucet_address: "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266".to_string(),
    };

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

    let service = HotWalletService::new_http(config).await.unwrap();

    let request = VerifyTransferRequest {
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

    let config = Config {
        database_url: db_file.path().to_str().unwrap().to_string(),
        provider_url: ProviderUrl::Http(rpc_server.uri()),
        mnemonic: "test test test test test test test test test test test junk".to_string(),
        treasury_address: "0x9999999999999999999999999999999999999999".to_string(),
        port: 3000,
        poll_interval: 1,
        block_offset_from_head: 0,
        faucet_mnemonic: "test test test test test test test test test test test junk".to_string(),
        existential_deposit: "10000000000000000".to_string(),
        faucet_address: "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266".to_string(),
    };

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

    let service = HotWalletService::new_http(config).await.unwrap();

    // Request expects USDC but contract returns USDT
    let request = VerifyTransferRequest {
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

    let config = Config {
        database_url: db_file.path().to_str().unwrap().to_string(),
        provider_url: ProviderUrl::Http(rpc_server.uri()),
        mnemonic: "test test test test test test test test test test test junk".to_string(),
        treasury_address: "0x9999999999999999999999999999999999999999".to_string(),
        port: 3000,
        poll_interval: 1,
        block_offset_from_head: 0,
        faucet_mnemonic: "test test test test test test test test test test test junk".to_string(),
        existential_deposit: "10000000000000000".to_string(),
        faucet_address: "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266".to_string(),
    };

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

    let service = HotWalletService::new_http(config).await.unwrap();

    let request = VerifyTransferRequest {
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
