use crate::config::{Config, ProviderUrl};
use crate::db::Db;
use crate::monitor::Monitor;
use crate::sweeper::Sweeper;
use crate::wallet::Wallet;
use alloy::providers::ProviderBuilder;
use tempfile::NamedTempFile;

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

    db.register_account(id, index, address).unwrap();

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
        webhook_url: "http://example.com".to_string(),
        port: 3000,
        poll_interval: 1,
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
    db.register_account("test_user", 0, &user_address).unwrap();

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
    db.register_account("user1", 0, &addr1).unwrap();

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
        webhook_url: "http://example.com".to_string(),
        port: 3000,
        poll_interval: 1,
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
    db.register_account("test_user", 0, &user_address).unwrap();
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

    db.register_account("user_0", 0, &addr0).unwrap();
    db.register_account("user_1", 1, &addr1).unwrap();
    db.register_account("user_2", 2, &addr2).unwrap();

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
