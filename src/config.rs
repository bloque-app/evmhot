use alloy::primitives::U256;
use anyhow::{bail, Context, Result};
use dotenvy::dotenv;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::Path;
use std::str::FromStr;

#[derive(Clone, Debug, Default)]
pub struct MinDepositSettings {
    /// Per-token minimum deposit amounts keyed by lowercased token contract address.
    pub per_token: HashMap<String, U256>,
    /// Fallback minimum for ERC20 tokens not listed in `per_token`.
    pub default: U256,
    /// Minimum native (ETH/MATIC) deposit amount.
    pub native: U256,
}

impl MinDepositSettings {
    pub fn for_token(&self, token_addr: &str) -> U256 {
        self.per_token
            .get(&token_addr.to_lowercase())
            .copied()
            .unwrap_or(self.default)
    }
}

/// Per-chain configuration loaded from `chains.toml`.
#[derive(Clone, Debug, Deserialize)]
pub struct ChainConfigRaw {
    pub name: String,
    pub chain_id: u64,
    pub rpc_url: String,
    pub treasury_address: String,
    pub faucet_address: String,
    #[serde(default = "default_existential_deposit")]
    pub existential_deposit: String,
    #[serde(default)]
    pub allowed_token_addresses: Vec<String>,
    #[serde(default)]
    pub min_deposit_default: Option<String>,
    #[serde(default)]
    pub min_deposit_native: Option<String>,
    /// Map of token address -> raw amount string
    #[serde(default)]
    pub min_deposits: HashMap<String, String>,
    #[serde(default = "default_block_offset")]
    pub block_offset_from_head: u64,
    #[serde(default = "default_poll_interval")]
    pub poll_interval: u64,
    #[serde(default = "default_get_logs_max_retries")]
    pub get_logs_max_retries: u32,
    #[serde(default = "default_get_logs_delay_ms")]
    pub get_logs_delay_ms: u64,
    /// Block span per ranged `eth_getLogs` call when the monitor is far behind head.
    /// A soft performance hint, not a correctness knob: the monitor bisects any range
    /// that a provider rejects as too large, regardless of this setting.
    #[serde(default = "default_catch_up_chunk_size")]
    pub catch_up_chunk_size: u64,
    /// Max number of blocks fetched concurrently for native-transfer scanning while
    /// draining a backlog.
    #[serde(default = "default_block_fetch_concurrency")]
    pub block_fetch_concurrency: u64,
}

fn default_existential_deposit() -> String {
    "10000000000000000".to_string()
}

fn default_block_offset() -> u64 {
    20
}

fn default_poll_interval() -> u64 {
    10
}

fn default_get_logs_max_retries() -> u32 {
    30
}

fn default_get_logs_delay_ms() -> u64 {
    50
}

fn default_catch_up_chunk_size() -> u64 {
    500
}

fn default_block_fetch_concurrency() -> u64 {
    10
}

#[derive(Clone, Debug, Deserialize)]
struct ChainsFile {
    chains: Vec<ChainConfigRaw>,
}

/// Validated per-chain runtime configuration.
#[derive(Clone, Debug)]
pub struct ChainConfig {
    pub name: String,
    pub chain_id: u64,
    pub rpc_url: String,
    pub treasury_address: String,
    pub faucet_address: String,
    pub existential_deposit: String,
    pub block_offset_from_head: u64,
    pub poll_interval: u64,
    pub get_logs_max_retries: u32,
    pub get_logs_delay_ms: u64,
    pub catch_up_chunk_size: u64,
    pub block_fetch_concurrency: u64,
    pub min_deposits: MinDepositSettings,
    /// Lowercased `0x`-prefixed ERC20 contract addresses permitted for detection and sweep.
    pub allowed_token_addresses: HashSet<String>,
}

impl ChainConfig {
    /// Returns true when the allowlist is empty (tests only) or contains the normalized address.
    pub fn is_token_allowed(&self, token_address: &str) -> bool {
        self.allowed_token_addresses.is_empty()
            || self
                .allowed_token_addresses
                .contains(&normalize_token_address(token_address))
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub mnemonic: String,
    pub faucet_mnemonic: String,
    pub port: u16,
    /// Optional JWT token for webhook authorization
    pub webhook_jwt_token: Option<String>,
    pub webhook_max_retries: u32,
    pub webhook_retry_delay_ms: u64,
    pub webhook_retry_poll_interval_secs: u64,
    pub webhook_retry_batch_size: u32,
    pub webhook_lease_seconds: u64,
    /// Default chain name for the redb→SQLite importer only (`LEGACY_CHAIN`, default: `polygon`).
    pub legacy_chain: String,
    /// Max concurrent SQLite read-pool connections (`DB_READ_POOL_SIZE`, default 20).
    /// Shared by every chain's monitor/sweeper/webhook-retry loop plus inbound
    /// registrations, so this should scale with the number of configured chains.
    pub db_read_pool_size: u32,
    pub chains: Vec<ChainConfig>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenv().ok();

        let database_url =
            env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:wallet.db".to_string());

        let mnemonic = env::var("MNEMONIC").context("MNEMONIC must be set")?;
        let faucet_mnemonic = env::var("FAUCET_MNEMONIC").context("FAUCET_MNEMONIC must be set")?;
        let port = env::var("PORT")
            .unwrap_or_else(|_| "3000".to_string())
            .parse()?;
        let webhook_jwt_token = env::var("WEBHOOK_JWT_TOKEN").ok();
        let webhook_max_retries = env_u32("WEBHOOK_MAX_RETRIES", 5);
        let webhook_retry_delay_ms = env_u64("WEBHOOK_RETRY_DELAY_MS", 1000);
        let webhook_retry_poll_interval_secs = env_u64("WEBHOOK_RETRY_POLL_INTERVAL", 30);
        let webhook_retry_batch_size = env_u32("WEBHOOK_RETRY_BATCH_SIZE", 50);
        let webhook_lease_seconds = env_u64("WEBHOOK_LEASE_SECONDS", 60);
        let legacy_chain = env::var("LEGACY_CHAIN").unwrap_or_else(|_| "polygon".to_string());
        let db_read_pool_size = env_u32("DB_READ_POOL_SIZE", 20);

        let chains_config_path =
            env::var("CHAINS_CONFIG").unwrap_or_else(|_| "chains.toml".to_string());
        let chains = load_chains_from_file(&chains_config_path)?;

        Ok(Self {
            database_url,
            mnemonic,
            faucet_mnemonic,
            port,
            webhook_jwt_token,
            webhook_max_retries,
            webhook_retry_delay_ms,
            webhook_retry_poll_interval_secs,
            webhook_retry_batch_size,
            webhook_lease_seconds,
            legacy_chain,
            db_read_pool_size,
            chains,
        })
    }

    pub fn chain(&self, name: &str) -> Option<&ChainConfig> {
        self.chains.iter().find(|c| c.name == name)
    }

    /// Address derived from `FAUCET_MNEMONIC` at index 0 (same on all EVM chains).
    pub fn derived_faucet_address(&self) -> Result<String> {
        use crate::wallet::Wallet;
        Ok(Wallet::new(self.faucet_mnemonic.clone())
            .derive_address(0)?
            .to_string())
    }
}

pub fn load_chains_from_file(path: impl AsRef<Path>) -> Result<Vec<ChainConfig>> {
    let content = fs::read_to_string(path.as_ref())
        .with_context(|| format!("Failed to read chains config: {:?}", path.as_ref()))?;
    parse_chains_toml(&content)
}

pub fn parse_chains_toml(content: &str) -> Result<Vec<ChainConfig>> {
    let file: ChainsFile =
        toml::from_str(content).context("Failed to parse chains.toml as TOML")?;

    if file.chains.is_empty() {
        bail!("chains.toml must define at least one [[chains]] entry");
    }

    let mut names = HashSet::new();
    let mut chains = Vec::with_capacity(file.chains.len());

    for raw in file.chains {
        validate_chain_name(&raw.name)?;

        if !names.insert(raw.name.clone()) {
            bail!("Duplicate chain name in chains.toml: {}", raw.name);
        }

        if raw.allowed_token_addresses.is_empty() {
            bail!(
                "Chain '{}' must define at least one allowed_token_addresses entry",
                raw.name
            );
        }

        let allowed_token_addresses: HashSet<String> = raw
            .allowed_token_addresses
            .iter()
            .map(|a| normalize_token_address(a))
            .collect();

        let min_deposit_default = parse_u256_str(
            raw.min_deposit_default.as_deref().unwrap_or("0"),
            &format!("chains.{}.min_deposit_default", raw.name),
        )?;
        let min_deposit_native = parse_u256_str(
            raw.min_deposit_native.as_deref().unwrap_or("0"),
            &format!("chains.{}.min_deposit_native", raw.name),
        )?;

        let mut per_token = HashMap::new();
        for (address, amount_str) in &raw.min_deposits {
            let amount = parse_u256_str(
                amount_str,
                &format!("chains.{}.min_deposits[{}]", raw.name, address),
            )?;
            per_token.insert(normalize_token_address(address), amount);
        }

        chains.push(ChainConfig {
            name: raw.name,
            chain_id: raw.chain_id,
            rpc_url: raw.rpc_url,
            treasury_address: raw.treasury_address,
            faucet_address: raw.faucet_address,
            existential_deposit: raw.existential_deposit,
            block_offset_from_head: raw.block_offset_from_head,
            poll_interval: raw.poll_interval,
            get_logs_max_retries: raw.get_logs_max_retries,
            get_logs_delay_ms: raw.get_logs_delay_ms,
            catch_up_chunk_size: raw.catch_up_chunk_size,
            block_fetch_concurrency: raw.block_fetch_concurrency,
            min_deposits: MinDepositSettings {
                per_token,
                default: min_deposit_default,
                native: min_deposit_native,
            },
            allowed_token_addresses,
        });
    }

    Ok(chains)
}

fn validate_chain_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Chain name must not be empty");
    }
    if name.contains(':') {
        bail!("Chain name '{}' must not contain ':'", name);
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        bail!(
            "Chain name '{}' must match [a-z0-9_-]+ (lowercase alphanumeric, underscore, hyphen)",
            name
        );
    }
    Ok(())
}

fn parse_u256_str(value: &str, field: &str) -> Result<U256> {
    U256::from_str(value.trim()).with_context(|| format!("Invalid {field} value: {value}"))
}

fn env_u64(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_u32(name: &str, default: u32) -> u32 {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Parse `MIN_DEPOSITS` as comma-separated `address=rawamount` pairs.
pub fn parse_min_deposits_env(raw: String) -> Result<HashMap<String, U256>> {
    let mut map = HashMap::new();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(map);
    }

    for segment in trimmed.split(',') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }

        let Some((address, amount_str)) = segment.split_once('=') else {
            continue;
        };

        let address = address.trim().to_lowercase();
        if address.is_empty() {
            continue;
        }

        let amount = U256::from_str(amount_str.trim())
            .with_context(|| format!("Invalid MIN_DEPOSITS amount for {address}: {amount_str}"))?;
        map.insert(address, amount);
    }

    Ok(map)
}

/// Normalize an EVM address to lowercase with a `0x` prefix.
pub fn normalize_token_address(address: &str) -> String {
    let trimmed = address.trim().to_lowercase();
    if trimmed.is_empty() {
        return trimmed;
    }
    if trimmed.starts_with("0x") {
        trimmed
    } else {
        format!("0x{trimmed}")
    }
}

/// Parse `ALLOWED_TOKEN_ADDRESSES` as a comma-separated list of contract addresses.
pub fn parse_allowed_token_addresses_env(raw: String) -> Result<HashSet<String>> {
    let mut set = HashSet::new();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(set);
    }

    for segment in trimmed.split(',') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        set.insert(normalize_token_address(segment));
    }

    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_TOML: &str = r#"
[[chains]]
name = "base"
chain_id = 8453
rpc_url = "https://base.example.com"
treasury_address = "0x1111111111111111111111111111111111111111"
faucet_address = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
allowed_token_addresses = ["0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"]

[[chains]]
name = "polygon"
chain_id = 137
rpc_url = "https://polygon.example.com"
treasury_address = "0x2222222222222222222222222222222222222222"
faucet_address = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
allowed_token_addresses = ["0xc2132d05d31c914a87c6611c10748aeb04b58e8f"]
min_deposit_native = "1000"
[chains.min_deposits]
"0xc2132d05d31c914a87c6611c10748aeb04b58e8f" = "10000"
"#;

    #[test]
    fn test_parse_valid_chains_toml() {
        let chains = parse_chains_toml(SAMPLE_TOML).unwrap();
        assert_eq!(chains.len(), 2);
        assert_eq!(chains[0].name, "base");
        assert_eq!(chains[0].chain_id, 8453);
        assert_eq!(chains[1].name, "polygon");
        assert_eq!(chains[1].min_deposits.native, U256::from(1000u64));
        assert!(chains[1].is_token_allowed("0xc2132d05d31c914a87c6611c10748aeb04b58e8f"));
    }

    #[test]
    fn test_reject_empty_chains() {
        let err = parse_chains_toml("chains = []").unwrap_err();
        assert!(err.to_string().contains("at least one"));
    }

    #[test]
    fn test_reject_duplicate_chain_names() {
        let toml = r#"
[[chains]]
name = "base"
chain_id = 1
rpc_url = "http://localhost"
treasury_address = "0x1"
faucet_address = "0x2"
allowed_token_addresses = ["0xabc"]

[[chains]]
name = "base"
chain_id = 2
rpc_url = "http://localhost"
treasury_address = "0x1"
faucet_address = "0x2"
allowed_token_addresses = ["0xabc"]
"#;
        let err = parse_chains_toml(toml).unwrap_err();
        assert!(err.to_string().contains("Duplicate"));
    }

    #[test]
    fn test_reject_chain_without_tokens() {
        let toml = r#"
[[chains]]
name = "base"
chain_id = 1
rpc_url = "http://localhost"
treasury_address = "0x1"
faucet_address = "0x2"
allowed_token_addresses = []
"#;
        let err = parse_chains_toml(toml).unwrap_err();
        assert!(err.to_string().contains("allowed_token_addresses"));
    }

    #[test]
    fn test_reject_invalid_chain_name() {
        let toml = r#"
[[chains]]
name = "Base:main"
chain_id = 1
rpc_url = "http://localhost"
treasury_address = "0x1"
faucet_address = "0x2"
allowed_token_addresses = ["0xabc"]
"#;
        let err = parse_chains_toml(toml).unwrap_err();
        assert!(err.to_string().contains(':'));
    }
}
