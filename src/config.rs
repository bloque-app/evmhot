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
    pub webhook_url: String,
    pub port: u16,
    pub poll_interval: u64,
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
        let webhook_url = env::var("WEBHOOK_URL").expect("WEBHOOK_URL must be set");
        let port = env::var("PORT")
            .unwrap_or_else(|_| "3000".to_string())
            .parse()?;
        let poll_interval = env::var("POLL_INTERVAL")
            .unwrap_or_else(|_| "10".to_string())
            .parse()?;

        Ok(Self {
            database_url,
            provider_url,
            mnemonic,
            treasury_address,
            webhook_url,
            port,
            poll_interval,
        })
    }
}
