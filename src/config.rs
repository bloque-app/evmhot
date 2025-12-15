use anyhow::Result;
use dotenvy::dotenv;
use std::env;

#[derive(Clone, Debug)]
pub enum ProviderUrl {
    Http(String),
    Ws(String),
}

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub provider_url: ProviderUrl,
    pub mnemonic: String,
    pub treasury_address: String,
    pub port: u16,
    pub poll_interval: u64,
    pub faucet_mnemonic: String,
    pub existential_deposit: String,
    pub faucet_address: String,
    pub block_offset_from_head: u64,
    pub get_logs_max_retries: u32,
    pub get_logs_delay_ms: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenv().ok();

        let database_url =
            env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:wallet.db".to_string());

        let provider_url = if let Ok(ws_url) = env::var("WS_URL") {
            ProviderUrl::Ws(ws_url)
        } else if let Ok(rpc_url) = env::var("RPC_URL") {
            ProviderUrl::Http(rpc_url)
        } else {
            return Err(anyhow::anyhow!("Either RPC_URL or WS_URL must be set"));
        };

        let mnemonic = env::var("MNEMONIC").expect("MNEMONIC must be set");
        let treasury_address = env::var("TREASURY_ADDRESS").expect("TREASURY_ADDRESS must be set");
        let faucet_mnemonic = env::var("FAUCET_MNEMONIC").expect("FAUCET_MNEMONIC must be set");
        let faucet_address = env::var("FAUCET_ADDRESS").expect("FAUCET_ADDRESS must be set");
        let existential_deposit =
            env::var("EXISTENTIAL_DEPOSIT").unwrap_or_else(|_| "10000000000000000".to_string()); // Default: 0.01 ETH
        let port = env::var("PORT")
            .unwrap_or_else(|_| "3000".to_string())
            .parse()?;
        let poll_interval = env::var("POLL_INTERVAL")
            .unwrap_or_else(|_| "10".to_string())
            .parse()?;
        let block_offset_from_head = env::var("BLOCK_OFFSET_FROM_HEAD")
            .unwrap_or_else(|_| "20".to_string())
            .parse()?;
        let get_logs_max_retries = env::var("GET_LOGS_MAX_RETRIES")
            .unwrap_or_else(|_| "30".to_string())
            .parse()?;
        let get_logs_delay_ms = env::var("GET_LOGS_DELAY_MS")
            .unwrap_or_else(|_| "50".to_string())
            .parse()?;

        Ok(Self {
            database_url,
            provider_url,
            mnemonic,
            treasury_address,
            port,
            poll_interval,
            faucet_mnemonic,
            existential_deposit,
            faucet_address,
            block_offset_from_head,
            get_logs_max_retries,
            get_logs_delay_ms,
        })
    }
}
