use crate::config::{ChainConfig, Config, MinDepositSettings};
use crate::db::Db;
use crate::webhook::WebhookDeliverer;
use alloy::providers::{ProviderBuilder, RootProvider};
use alloy::transports::BoxTransport;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub const TEST_CHAIN: &str = "polygon";
pub const TEST_MNEMONIC: &str = "test test test test test test test test test test test junk";
pub const TEST_FAUCET: &str = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266";
pub const TEST_TREASURY: &str = "0x9999999999999999999999999999999999999999";
pub const TEST_EXISTENTIAL: &str = "10000000000000000";

pub fn default_webhook_config_fields() -> (u32, u64, u64, u32, u64) {
    (5, 1000, 30, 50, 60)
}

pub fn test_webhook_deliverer(db: Db) -> Arc<WebhookDeliverer> {
    Arc::new(WebhookDeliverer::new_for_test(db, None, 3, 10, 60, 50, 60).unwrap())
}

#[allow(dead_code)]
pub fn test_webhook_deliverer_fast(db: Db) -> Arc<WebhookDeliverer> {
    Arc::new(WebhookDeliverer::new_for_test(db, None, 3, 10, 5, 50, 60).unwrap())
}

pub fn test_chain_config(rpc_url: impl Into<String>) -> ChainConfig {
    ChainConfig {
        name: TEST_CHAIN.to_string(),
        chain_id: 137,
        rpc_url: rpc_url.into(),
        treasury_address: TEST_TREASURY.to_string(),
        faucet_address: TEST_FAUCET.to_string(),
        existential_deposit: TEST_EXISTENTIAL.to_string(),
        block_offset_from_head: 0,
        poll_interval: 1,
        get_logs_max_retries: 30,
        get_logs_delay_ms: 50,
        catch_up_chunk_size: 500,
        block_fetch_concurrency: 10,
        min_deposits: MinDepositSettings::default(),
        allowed_token_addresses: HashSet::new(),
        token_treasuries: HashMap::new(),
        token_targets: HashMap::new(),
    }
}

pub fn test_chain_config_named(name: &str, rpc_url: impl Into<String>) -> ChainConfig {
    let mut cfg = test_chain_config(rpc_url);
    cfg.name = name.to_string();
    if name == "base" {
        cfg.chain_id = 8453;
    } else if name == "polygon" {
        cfg.chain_id = 137;
    }
    cfg
}

pub fn test_config_multichain(
    db_path: impl Into<String>,
    base_rpc: impl Into<String>,
    polygon_rpc: impl Into<String>,
) -> Config {
    let (
        webhook_max_retries,
        webhook_retry_delay_ms,
        webhook_retry_poll_interval_secs,
        webhook_retry_batch_size,
        webhook_lease_seconds,
    ) = default_webhook_config_fields();
    Config {
        database_url: db_path.into(),
        mnemonic: TEST_MNEMONIC.to_string(),
        faucet_mnemonic: TEST_MNEMONIC.to_string(),
        port: 3000,
        webhook_jwt_token: None,
        webhook_max_retries,
        webhook_retry_delay_ms,
        webhook_retry_poll_interval_secs,
        webhook_retry_batch_size,
        webhook_lease_seconds,
        legacy_chain: TEST_CHAIN.to_string(),
        db_read_pool_size: 20,
        chains: vec![
            test_chain_config_named("base", base_rpc),
            test_chain_config_named("polygon", polygon_rpc),
        ],
    }
}

pub fn test_config(db_path: impl Into<String>, rpc_url: impl Into<String>) -> Config {
    let (
        webhook_max_retries,
        webhook_retry_delay_ms,
        webhook_retry_poll_interval_secs,
        webhook_retry_batch_size,
        webhook_lease_seconds,
    ) = default_webhook_config_fields();
    Config {
        database_url: db_path.into(),
        mnemonic: TEST_MNEMONIC.to_string(),
        faucet_mnemonic: TEST_MNEMONIC.to_string(),
        port: 3000,
        webhook_jwt_token: None,
        webhook_max_retries,
        webhook_retry_delay_ms,
        webhook_retry_poll_interval_secs,
        webhook_retry_batch_size,
        webhook_lease_seconds,
        legacy_chain: TEST_CHAIN.to_string(),
        db_read_pool_size: 20,
        chains: vec![test_chain_config(rpc_url)],
    }
}

pub fn http_provider_boxed(url: &str) -> RootProvider<BoxTransport> {
    ProviderBuilder::new()
        .on_http(url.parse().expect("invalid test rpc url"))
        .boxed()
}

pub fn chain_treasury(config: &Config) -> String {
    config.chains[0].treasury_address.clone()
}

pub fn chain_existential(config: &Config) -> String {
    config.chains[0].existential_deposit.clone()
}

/// A 32-byte hash with `n` in its low bytes, for fixtures.
pub fn hash32(n: u64) -> String {
    format!("0x{n:064x}")
}

fn topic_for_address(address: &str) -> String {
    format!(
        "0x000000000000000000000000{}",
        address.trim_start_matches("0x").to_lowercase()
    )
}

/// An ERC-20 `Transfer` log as an `eth_getTransactionReceipt` / `eth_getLogs`
/// entry. `log_index` is the block-level index.
#[allow(clippy::too_many_arguments)]
pub fn transfer_log_json(
    token: &str,
    from: &str,
    to: &str,
    amount: u128,
    log_index: u64,
    tx_hash: &str,
    block_number: u64,
    tx_index: u64,
) -> serde_json::Value {
    serde_json::json!({
        "address": token,
        "topics": [
            "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
            topic_for_address(from),
            topic_for_address(to),
        ],
        "data": format!("0x{amount:064x}"),
        "blockNumber": format!("0x{block_number:x}"),
        "transactionHash": tx_hash,
        "transactionIndex": format!("0x{tx_index:x}"),
        "blockHash": hash32(block_number),
        "logIndex": format!("0x{log_index:x}"),
        "removed": false
    })
}

/// A transaction receipt (the JSON-RPC `result` object).
pub fn receipt_json(
    tx_hash: &str,
    block_number: u64,
    tx_index: u64,
    from: &str,
    to: &str,
    success: bool,
    logs: Vec<serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "transactionHash": tx_hash,
        "transactionIndex": format!("0x{tx_index:x}"),
        "blockHash": hash32(block_number),
        "blockNumber": format!("0x{block_number:x}"),
        "from": from,
        "to": to,
        "cumulativeGasUsed": "0x186a0",
        "gasUsed": "0x186a0",
        "contractAddress": null,
        "logs": logs,
        "status": if success { "0x1" } else { "0x0" },
        "logsBloom": format!("0x{}", "0".repeat(512)),
        "type": "0x0",
        "effectiveGasPrice": "0x3B9ACA00"
    })
}
