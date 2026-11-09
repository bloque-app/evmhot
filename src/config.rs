use alloy::primitives::{Address, U256};
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

/// What a sweep target does with a token (REQ-SW-6).
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    /// Token transfer to an address.
    Transfer,
    /// No sweep: deposits are detected and notified, and stay where they are
    /// until the target's kind changes (amendment 0003).
    Hold,
}

/// A `[chains.token_targets."0x..."]` table, as written.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenTargetRaw {
    pub kind: TargetKind,
    #[serde(default)]
    pub address: Option<String>,
    /// Minimum per sweep, in the token's minor units (REQ-SW-7).
    #[serde(default)]
    pub min: Option<String>,
    /// Maximum per sweep, in the token's minor units (REQ-SW-7).
    #[serde(default)]
    pub max: Option<String>,
    /// This address admits only this token (REQ-SW-8).
    #[serde(default)]
    pub admits_only: bool,
    /// Each sweep needs one refund address: never mix senders (REQ-SW-12).
    #[serde(default)]
    pub per_sender: bool,
}

/// A validated sweep target for one token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SweepTarget {
    pub kind: TargetKind,
    /// Lowercased `0x` address. `None` only for `Hold`.
    pub address: Option<String>,
    pub min: Option<U256>,
    pub max: Option<U256>,
    pub admits_only: bool,
    pub per_sender: bool,
}

impl SweepTarget {
    fn plain(address: String) -> Self {
        Self {
            kind: TargetKind::Transfer,
            address: Some(address),
            min: None,
            max: None,
            admits_only: false,
            per_sender: false,
        }
    }
}

/// Splits `amount` into sweeps that respect a target's bounds (REQ-SW-7).
///
/// - Below `min`: no sweep (the amount waits).
/// - Above `max`: parts of `max`, then the remainder.
/// - A remainder under `min` is topped up from the previous part when both can
///   stay within bounds; otherwise it stays in the deposit address, because no
///   sweep may fall outside the target's bounds.
pub fn plan_sweep_parts(amount: U256, min: Option<U256>, max: Option<U256>) -> Vec<U256> {
    let min = min.unwrap_or(U256::ZERO);
    if amount.is_zero() || amount < min {
        return Vec::new();
    }
    let Some(max) = max.filter(|m| !m.is_zero()) else {
        return vec![amount];
    };
    if amount <= max {
        return vec![amount];
    }
    let full = amount / max;
    let rem = amount % max;
    let mut parts: Vec<U256> = Vec::new();
    let mut i = U256::ZERO;
    while i < full {
        parts.push(max);
        i += U256::from(1u8);
    }
    if !rem.is_zero() {
        if rem >= min {
            parts.push(rem);
        } else if max + rem >= min + min {
            // Both parts stay within [min, max]: rem < min gives prev < max.
            let last = parts.len() - 1;
            parts[last] = max + rem - min;
            parts.push(min);
        }
    }
    parts
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
    /// Map of token contract address -> sweep destination. Unlisted tokens
    /// (and native ETH) use `treasury_address`.
    #[serde(default)]
    pub token_treasuries: HashMap<String, String>,
    /// Map of token contract address -> sweep target table (kind, address,
    /// bounds, admission, per-sender). A token may appear here or in
    /// `token_treasuries`, not both.
    #[serde(default)]
    pub token_targets: HashMap<String, TokenTargetRaw>,
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
    /// Lowercased token contract → lowercased sweep destination. Empty means
    /// every ERC20 uses `treasury_address`.
    pub token_treasuries: HashMap<String, String>,
    /// Lowercased token contract -> table-form sweep target.
    pub token_targets: HashMap<String, SweepTarget>,
}

impl ChainConfig {
    /// Returns true when the allowlist is empty (tests only) or contains the normalized address.
    pub fn is_token_allowed(&self, token_address: &str) -> bool {
        self.allowed_token_addresses.is_empty()
            || self
                .allowed_token_addresses
                .contains(&normalize_token_address(token_address))
    }

    /// Sweep destination for an ERC20 deposit. Native ETH always uses
    /// `treasury_address` and is not looked up here.
    pub fn treasury_for_token(&self, token_address: &str) -> &str {
        self.token_treasuries
            .get(&normalize_token_address(token_address))
            .map(String::as_str)
            .unwrap_or(self.treasury_address.as_str())
    }

    /// The sweep target for a token: its `token_targets` table, else its
    /// `token_treasuries` address, else the network default (REQ-SW-2, REQ-SW-6).
    pub fn target_for_token(&self, token_address: &str) -> SweepTarget {
        let token = normalize_token_address(token_address);
        if let Some(target) = self.token_targets.get(&token) {
            return target.clone();
        }
        SweepTarget::plain(normalize_token_address(self.treasury_for_token(&token)))
    }

    /// Stable lines describing native + per-token sweep destinations, for boot logs.
    pub fn sweep_route_log_lines(&self) -> Vec<String> {
        let mut lines = Vec::with_capacity(1 + self.allowed_token_addresses.len());
        lines.push(format!("native -> {}", self.treasury_address));
        let mut tokens: Vec<&String> = self.allowed_token_addresses.iter().collect();
        tokens.sort();
        for token in tokens {
            if let Some(target) = self.token_targets.get(token) {
                let mut line = match (&target.kind, &target.address) {
                    (TargetKind::Hold, _) => format!("{token} -> hold (token_targets)"),
                    (TargetKind::Transfer, address) => format!(
                        "{token} -> {} (token_targets)",
                        address.as_deref().unwrap_or("")
                    ),
                };
                if let Some(min) = target.min {
                    line.push_str(&format!(" min={min}"));
                }
                if let Some(max) = target.max {
                    line.push_str(&format!(" max={max}"));
                }
                if target.admits_only {
                    line.push_str(" admits_only");
                }
                if target.per_sender {
                    line.push_str(" per_sender");
                }
                lines.push(line);
                continue;
            }
            let dest = self.treasury_for_token(token);
            let via = if self.token_treasuries.contains_key(token) {
                "token_treasuries"
            } else {
                "default"
            };
            lines.push(format!("{token} -> {dest} ({via})"));
        }
        lines
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

        let mut token_treasuries = HashMap::new();
        for (token, dest) in &raw.token_treasuries {
            let token_norm = normalize_token_address(token);
            if !allowed_token_addresses.contains(&token_norm) {
                bail!(
                    "Chain '{}': token_treasuries key '{}' is not in allowed_token_addresses",
                    raw.name,
                    token
                );
            }
            let dest_trimmed = dest.trim();
            Address::from_str(dest_trimmed).with_context(|| {
                format!(
                    "Chain '{}': invalid token_treasuries[{}] destination: {dest}",
                    raw.name, token
                )
            })?;
            token_treasuries.insert(token_norm, normalize_token_address(dest_trimmed));
        }

        let mut token_targets = HashMap::new();
        for (token, table) in &raw.token_targets {
            let token_norm = normalize_token_address(token);
            let field = format!("Chain '{}': token_targets[{}]", raw.name, token);
            if !allowed_token_addresses.contains(&token_norm) {
                bail!("{field} is not in allowed_token_addresses");
            }
            if token_treasuries.contains_key(&token_norm) {
                bail!("{field} is also listed in token_treasuries; use one or the other");
            }
            let min = table
                .min
                .as_deref()
                .map(|v| parse_u256_str(v, &format!("{field}.min")))
                .transpose()?;
            let max = table
                .max
                .as_deref()
                .map(|v| parse_u256_str(v, &format!("{field}.max")))
                .transpose()?;
            if max == Some(U256::ZERO) {
                bail!("{field}.max must be greater than zero");
            }
            if let (Some(min), Some(max)) = (min, max) {
                if min > max {
                    bail!("{field}: min ({min}) is greater than max ({max})");
                }
            }
            let address = match table.kind {
                TargetKind::Transfer => {
                    let Some(address) = table.address.as_deref() else {
                        bail!("{field}: kind \"transfer\" requires an address");
                    };
                    let address = address.trim();
                    Address::from_str(address)
                        .with_context(|| format!("{field}: invalid address: {address}"))?;
                    Some(normalize_token_address(address))
                }
                TargetKind::Hold => {
                    if table.address.is_some()
                        || table.min.is_some()
                        || table.max.is_some()
                        || table.admits_only
                        || table.per_sender
                    {
                        bail!(
                            "{field}: kind \"hold\" takes no address, bounds or flags; \
                             set a transfer target to release held deposits"
                        );
                    }
                    None
                }
            };
            token_targets.insert(
                token_norm,
                SweepTarget {
                    kind: table.kind,
                    address,
                    min,
                    max,
                    admits_only: table.admits_only,
                    per_sender: table.per_sender,
                },
            );
        }
        check_admits_only(
            &raw.name,
            &raw.treasury_address,
            &allowed_token_addresses,
            &token_treasuries,
            &token_targets,
        )?;

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
            token_treasuries,
            token_targets,
        });
    }

    Ok(chains)
}

/// REQ-SW-8: a target that admits only one token must not receive anything
/// else. The native coin and every token without a target of its own go to the
/// network default target, so that address cannot be an `admits_only` address.
fn check_admits_only(
    chain: &str,
    default_target: &str,
    allowed_tokens: &HashSet<String>,
    token_treasuries: &HashMap<String, String>,
    token_targets: &HashMap<String, SweepTarget>,
) -> Result<()> {
    let default_norm = normalize_token_address(default_target);
    // Where every token goes (None = held, goes nowhere).
    let route = |token: &str| -> Option<String> {
        if let Some(target) = token_targets.get(token) {
            return target.address.clone();
        }
        Some(
            token_treasuries
                .get(token)
                .cloned()
                .unwrap_or_else(|| default_norm.clone()),
        )
    };
    let mut guarded: Vec<(&String, &SweepTarget)> = token_targets
        .iter()
        .filter(|(_, t)| t.admits_only)
        .collect();
    guarded.sort_by(|a, b| a.0.cmp(b.0));
    for (token, target) in guarded {
        let Some(address) = target.address.as_deref() else {
            continue;
        };
        if default_norm == address {
            bail!(
                "Chain '{chain}': token_targets[{token}] admits only that token, but address \
                 {address} is also the network default target, which the native coin and every \
                 token without a target of its own use"
            );
        }
        let mut others: Vec<&String> = allowed_tokens.iter().filter(|t| *t != token).collect();
        others.sort();
        for other in others {
            if route(other).as_deref() == Some(address) {
                bail!(
                    "Chain '{chain}': token_targets[{token}] admits only that token, but token \
                     {other} is also routed to {address}"
                );
            }
        }
    }
    Ok(())
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

    #[test]
    fn test_parse_token_treasuries() {
        let toml = r#"
[[chains]]
name = "base"
chain_id = 8453
rpc_url = "http://localhost"
treasury_address = "0x1111111111111111111111111111111111111111"
faucet_address = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
allowed_token_addresses = [
  "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
  "0xc2132D05D31c914a87C6611C10748AEb04B58e8F",
]
[chains.token_treasuries]
"0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913" = "0xAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAa"
"#;
        let chains = parse_chains_toml(toml).unwrap();
        let chain = &chains[0];
        let usdc = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";
        let usdt = "0xc2132d05d31c914a87c6611c10748aeb04b58e8f";
        assert_eq!(
            chain.treasury_for_token(usdc),
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(chain.treasury_for_token(usdt), chain.treasury_address);
        assert_eq!(
            chain.sweep_route_log_lines(),
            vec![
                "native -> 0x1111111111111111111111111111111111111111".to_string(),
                format!("{usdc} -> 0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa (token_treasuries)"),
                format!("{usdt} -> {} (default)", chain.treasury_address),
            ]
        );
    }

    #[test]
    fn test_reject_token_treasuries_unknown_token() {
        let toml = r#"
[[chains]]
name = "base"
chain_id = 8453
rpc_url = "http://localhost"
treasury_address = "0x1111111111111111111111111111111111111111"
faucet_address = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
allowed_token_addresses = ["0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"]
[chains.token_treasuries]
"0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"#;
        let err = parse_chains_toml(toml).unwrap_err();
        assert!(err.to_string().contains("not in allowed_token_addresses"));
    }

    #[test]
    fn test_reject_token_treasuries_invalid_destination() {
        let toml = r#"
[[chains]]
name = "base"
chain_id = 8453
rpc_url = "http://localhost"
treasury_address = "0x1111111111111111111111111111111111111111"
faucet_address = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
allowed_token_addresses = ["0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"]
[chains.token_treasuries]
"0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913" = "not-an-address"
"#;
        let err = parse_chains_toml(toml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("invalid token_treasuries") || msg.contains("not-an-address"),
            "unexpected error: {msg}"
        );
    }
}
