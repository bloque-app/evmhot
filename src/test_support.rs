#![cfg(test)]

use crate::config::{ChainConfig, Config, MinDepositSettings};
use alloy::providers::{ProviderBuilder, RootProvider};
use alloy::transports::BoxTransport;
use std::collections::HashSet;

pub const TEST_CHAIN: &str = "polygon";
pub const TEST_MNEMONIC: &str = "test test test test test test test test test test test junk";
pub const TEST_FAUCET: &str = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266";
pub const TEST_TREASURY: &str = "0x9999999999999999999999999999999999999999";
pub const TEST_EXISTENTIAL: &str = "10000000000000000";

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
        min_deposits: MinDepositSettings::default(),
        allowed_token_addresses: HashSet::new(),
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
    Config {
        database_url: db_path.into(),
        mnemonic: TEST_MNEMONIC.to_string(),
        faucet_mnemonic: TEST_MNEMONIC.to_string(),
        port: 3000,
        webhook_jwt_token: None,
        legacy_chain: TEST_CHAIN.to_string(),
        chains: vec![
            test_chain_config_named("base", base_rpc),
            test_chain_config_named("polygon", polygon_rpc),
        ],
    }
}

pub fn test_config(db_path: impl Into<String>, rpc_url: impl Into<String>) -> Config {
    Config {
        database_url: db_path.into(),
        mnemonic: TEST_MNEMONIC.to_string(),
        faucet_mnemonic: TEST_MNEMONIC.to_string(),
        port: 3000,
        webhook_jwt_token: None,
        legacy_chain: TEST_CHAIN.to_string(),
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
