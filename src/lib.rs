// Library modules
pub mod config;
pub mod db;
pub mod redb_store;
pub mod redb_import;
pub(crate) mod faucet;
mod monitor;
mod sweeper;
pub mod traits;
mod wallet;

#[cfg(test)]
mod e2e_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

use alloy::providers::{ProviderBuilder, RootProvider};
use alloy::transports::BoxTransport;
use config::{ChainConfig, Config};
use db::Db;
use faucet::Faucet;
use monitor::Monitor;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use sweeper::Sweeper;
use traits::Service;
use wallet::Wallet;

/// Request structure for registering a new account
#[derive(Deserialize, Clone)]
pub struct RegisterRequest {
    pub id: String,
    pub webhook_url: String,
}

/// Response structure for account registration
#[derive(Serialize, Clone)]
pub struct RegisterResponse {
    pub address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub funding_tx: Option<String>,
}

/// Request structure for verifying a transfer
#[derive(Deserialize, Clone, Debug)]
pub struct VerifyTransferRequest {
    /// Chain name (e.g. "base", "polygon")
    pub chain: String,
    /// Transaction hash to verify
    pub tx_hash: String,
    /// Expected recipient address
    pub to_address: String,
    /// Expected amount (as string to handle large numbers)
    pub amount: String,
    /// Token type: "native" for ETH/native currency, or "erc20" for ERC20 tokens
    #[serde(default = "default_token_type")]
    pub token_type: String,
    /// Token contract address (required for ERC20)
    #[serde(default)]
    pub token_address: Option<String>,
    /// Token symbol (optional, for additional validation with ERC20)
    #[serde(default)]
    pub token_symbol: Option<String>,
}

fn default_token_type() -> String {
    "native".to_string()
}

/// Response structure for transfer verification
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum VerifyTransferResponse {
    Success {
        actual_to: String,
        actual_amount: String,
        token_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        token_symbol: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        block_number: Option<u64>,
    },
    Error {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        token_type: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        block_number: Option<u64>,
    },
}

/// Request to re-queue a failed deposit for sweeping.
#[derive(Deserialize, Clone, Debug)]
pub struct RetrySweepRequest {
    pub chain: String,
    pub tx_hash: String,
    /// Required for ERC20 deposits; omit for native deposits.
    #[serde(default)]
    pub log_index: Option<u64>,
}

/// Response for a sweep retry request.
#[derive(Serialize, Clone, Debug)]
pub struct RetrySweepResponse {
    pub retried: bool,
    pub token_type: String,
}

/// Per-chain runtime context (provider + faucet).
pub struct ChainContext {
    pub cfg: ChainConfig,
    pub provider: RootProvider<BoxTransport>,
    pub faucet: Arc<Faucet>,
}

/// Core Hot Wallet Service that manages background tasks and provides account registration.
pub struct HotWalletService {
    config: Config,
    db: Db,
    wallet: Wallet,
    chains: Vec<ChainContext>,
}

impl HotWalletService {
    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn chain_names(&self) -> Vec<String> {
        self.chains.iter().map(|c| c.cfg.name.clone()).collect()
    }

    pub async fn health(&self) -> anyhow::Result<String> {
        let names: Vec<_> = self.chain_names();
        Ok(format!("OK (chains: {})", names.join(", ")))
    }

    pub fn set_block_number(&self, chain: &str, block_number: u64) -> anyhow::Result<()> {
        if self.config.chain(chain).is_none() {
            return Err(anyhow::anyhow!("Unknown chain: {chain}"));
        }
        self.db.set_last_processed_block(chain, block_number)
    }

    pub fn get_block_number(&self, chain: &str) -> anyhow::Result<u64> {
        if self.config.chain(chain).is_none() {
            return Err(anyhow::anyhow!("Unknown chain: {chain}"));
        }
        self.db.get_last_processed_block(chain)
    }

    pub fn retry_sweep(&self, request: RetrySweepRequest) -> anyhow::Result<RetrySweepResponse> {
        if self.config.chain(&request.chain).is_none() {
            return Err(anyhow::anyhow!("Unknown chain: {}", request.chain));
        }

        if let Some(log_index) = request.log_index {
            let retried = self
                .db
                .retry_erc20_deposit(&request.chain, &request.tx_hash, log_index)?;
            Ok(RetrySweepResponse {
                retried,
                token_type: "erc20".to_string(),
            })
        } else {
            let retried = self
                .db
                .retry_native_deposit(&request.chain, &request.tx_hash)?;
            Ok(RetrySweepResponse {
                retried,
                token_type: "native".to_string(),
            })
        }
    }

    pub async fn new(config: Config) -> anyhow::Result<Self> {
        let db = Db::new(&config.database_url)?;
        let wallet = Wallet::new(config.mnemonic.clone());

        let mut chains = Vec::with_capacity(config.chains.len());
        for chain_cfg in &config.chains {
            let provider = ProviderBuilder::new()
                .on_builtin(&chain_cfg.rpc_url)
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "Failed to connect to chain '{}' at {}: {e}",
                        chain_cfg.name,
                        chain_cfg.rpc_url
                    )
                })?;

            let faucet = Arc::new(Faucet::new(
                config.faucet_mnemonic.clone(),
                provider.clone(),
                &chain_cfg.existential_deposit,
            )?);

            chains.push(ChainContext {
                cfg: chain_cfg.clone(),
                provider,
                faucet,
            });
        }

        Ok(Self {
            config,
            db,
            wallet,
            chains,
        })
    }

    pub async fn start_background_services(&self) -> anyhow::Result<()> {
        for ctx in &self.chains {
            let chain_name = ctx.cfg.name.clone();
            let monitor = Monitor::new(
                ctx.cfg.clone(),
                self.config.webhook_jwt_token.clone(),
                self.db.clone(),
                ctx.provider.clone(),
            );
            tokio::spawn(async move {
                tracing::info!("[{chain_name}] Starting Monitor");
                monitor.run().await;
            });

            let sweeper = Sweeper::new(
                ctx.cfg.clone(),
                self.config.webhook_jwt_token.clone(),
                self.db.clone(),
                self.wallet.clone(),
                ctx.provider.clone(),
                Arc::clone(&ctx.faucet),
            );
            let chain_name = ctx.cfg.name.clone();
            tokio::spawn(async move {
                tracing::info!("[{chain_name}] Starting Sweeper");
                sweeper.run().await;
            });
        }
        Ok(())
    }

    fn chain_context(&self, name: &str) -> anyhow::Result<&ChainContext> {
        self.chains
            .iter()
            .find(|c| c.cfg.name == name)
            .ok_or_else(|| anyhow::anyhow!("Unknown chain: {name}"))
    }

    pub async fn verify_transfer(
        &self,
        request: VerifyTransferRequest,
    ) -> anyhow::Result<VerifyTransferResponse> {
        use alloy::primitives::{Address, FixedBytes, U256};
        use std::str::FromStr;
        use tracing::info;

        info!("Verifying transfer: {:?}", request);

        let ctx = self.chain_context(&request.chain)?;

        let tx_hash: FixedBytes<32> = request
            .tx_hash
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid transaction hash format"))?;

        let expected_to = Address::from_str(&request.to_address)
            .map_err(|_| anyhow::anyhow!("Invalid to_address format"))?;
        let expected_amount = U256::from_str(&request.amount)
            .map_err(|_| anyhow::anyhow!("Invalid amount format"))?;

        let is_native = request.token_type.to_lowercase() == "native";

        if is_native {
            self.verify_native_transfer(&ctx.provider, tx_hash, expected_to, expected_amount)
                .await
        } else {
            let token_address_str = request
                .token_address
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("token_address is required for ERC20 transfers"))?;

            let token_address = Address::from_str(token_address_str)
                .map_err(|_| anyhow::anyhow!("Invalid token_address format"))?;

            self.verify_erc20_transfer(
                &ctx.provider,
                tx_hash,
                expected_to,
                expected_amount,
                token_address,
                request.token_symbol.as_deref(),
            )
            .await
        }
    }

    async fn verify_native_transfer(
        &self,
        provider: &RootProvider<BoxTransport>,
        tx_hash: alloy::primitives::FixedBytes<32>,
        expected_to: alloy::primitives::Address,
        expected_amount: alloy::primitives::U256,
    ) -> anyhow::Result<VerifyTransferResponse> {
        use alloy::providers::Provider;

        let tx = provider
            .get_transaction_by_hash(tx_hash)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Transaction not found"))?;

        let receipt = provider.get_transaction_receipt(tx_hash).await?;
        let block_number = receipt.as_ref().and_then(|r| r.block_number);

        if let Some(ref r) = receipt {
            if !r.status() {
                return Ok(VerifyTransferResponse::Error {
                    message: "Transaction failed (reverted)".to_string(),
                    token_type: Some("native".to_string()),
                    block_number,
                });
            }
        }

        let actual_to = tx.to;
        let actual_amount = tx.value;

        let to_matches = actual_to
            .map(|to| {
                to.to_string()
                    .eq_ignore_ascii_case(&expected_to.to_string())
            })
            .unwrap_or(false);
        let amount_matches = actual_amount >= expected_amount;

        if to_matches && amount_matches {
            Ok(VerifyTransferResponse::Success {
                actual_to: actual_to.map(|a| a.to_string()).unwrap_or_default(),
                actual_amount: actual_amount.to_string(),
                token_type: "native".to_string(),
                token_symbol: None,
                block_number,
            })
        } else {
            Ok(VerifyTransferResponse::Error {
                message: format!(
                    "Mismatch: to_matches={}, amount_matches={} (expected >= {})",
                    to_matches, amount_matches, expected_amount
                ),
                token_type: Some("native".to_string()),
                block_number,
            })
        }
    }

    async fn verify_erc20_transfer(
        &self,
        provider: &RootProvider<BoxTransport>,
        tx_hash: alloy::primitives::FixedBytes<32>,
        expected_to: alloy::primitives::Address,
        expected_amount: alloy::primitives::U256,
        token_address: alloy::primitives::Address,
        expected_symbol: Option<&str>,
    ) -> anyhow::Result<VerifyTransferResponse> {
        use alloy::primitives::{Address, FixedBytes, U256};
        use alloy::providers::Provider;

        let receipt = provider
            .get_transaction_receipt(tx_hash)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Transaction receipt not found"))?;

        let block_number = receipt.block_number;

        if !receipt.status() {
            return Ok(VerifyTransferResponse::Error {
                message: "Transaction failed (reverted)".to_string(),
                token_type: Some("erc20".to_string()),
                block_number,
            });
        }

        let actual_symbol = self.fetch_token_symbol(provider, token_address).await.ok();

        if let Some(expected) = expected_symbol {
            if let Some(ref actual) = actual_symbol {
                if !actual.eq_ignore_ascii_case(expected) {
                    return Ok(VerifyTransferResponse::Error {
                        message: format!(
                            "Token symbol mismatch: expected '{}', got '{}'",
                            expected, actual
                        ),
                        token_type: Some("erc20".to_string()),
                        block_number,
                    });
                }
            }
        }

        let transfer_signature: FixedBytes<32> =
            alloy::primitives::keccak256("Transfer(address,address,uint256)".as_bytes());

        for log in receipt.inner.logs() {
            if log.address() != token_address {
                continue;
            }
            if log.topics().len() < 3 || log.topics()[0] != transfer_signature {
                continue;
            }

            let to_address = Address::from_slice(&log.topics()[2].as_slice()[12..]);
            let amount = if !log.data().data.is_empty() {
                U256::from_be_slice(&log.data().data)
            } else {
                U256::ZERO
            };

            let to_matches = to_address
                .to_string()
                .eq_ignore_ascii_case(&expected_to.to_string());
            let amount_matches = amount >= expected_amount;

            if to_matches && amount_matches {
                return Ok(VerifyTransferResponse::Success {
                    actual_to: to_address.to_string(),
                    actual_amount: amount.to_string(),
                    token_type: "erc20".to_string(),
                    token_symbol: actual_symbol,
                    block_number,
                });
            }
        }

        Ok(VerifyTransferResponse::Error {
            message: format!(
                "No matching ERC20 Transfer event found to {} with amount >= {}",
                expected_to, expected_amount
            ),
            token_type: Some("erc20".to_string()),
            block_number,
        })
    }

    async fn fetch_token_symbol(
        &self,
        provider: &RootProvider<BoxTransport>,
        token_address: alloy::primitives::Address,
    ) -> anyhow::Result<String> {
        use alloy::sol;

        sol! {
            #[sol(rpc)]
            contract IERC20Symbol {
                function symbol() external view returns (string memory);
            }
        }

        let contract = IERC20Symbol::new(token_address, provider);
        let symbol = contract.symbol().call().await?._0;
        Ok(symbol)
    }

    /// Register a new account. Address derivation is chain-agnostic; no faucet funding at registration.
    pub async fn register(&self, request: RegisterRequest) -> anyhow::Result<RegisterResponse> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        use tracing::info;

        if let Ok(Some((_index, existing_address, _webhook))) =
            self.db.get_account_by_id(&request.id)
        {
            info!(
                "Account {} already exists with address {}",
                request.id, existing_address
            );
            return Ok(RegisterResponse {
                address: existing_address,
                funding_tx: None,
            });
        }

        let mut hasher = DefaultHasher::new();
        request.id.hash(&mut hasher);
        let hash = hasher.finish();
        let index = (hash & 0x7FFFFFFF) as u32;

        let address = self.wallet.derive_address(index)?;
        let address_str = address.to_string();

        self.db
            .register_account(&request.id, index, &address_str, &request.webhook_url)?;

        info!(
            "Registered account {} with address {} (index: {})",
            request.id, address_str, index
        );

        Ok(RegisterResponse {
            address: address_str,
            funding_tx: None,
        })
    }
}
