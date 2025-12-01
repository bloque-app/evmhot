use crate::{config::Config, db::Db, faucet::Faucet, wallet::Wallet};
use alloy::transports::Transport;
use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{error, info};

#[derive(Clone)]
struct AppState<T>
where
    T: Transport + Clone,
{
    db: Db,
    wallet: Wallet,
    faucet: Arc<Faucet<alloy::providers::RootProvider<T>>>,
}

#[derive(Deserialize)]
struct RegisterRequest {
    id: String,
    webhook_url: String,
}

#[derive(Serialize)]
struct RegisterResponse {
    address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    funding_tx: Option<String>,
}

pub async fn start_server<T>(
    config: Config,
    db: Db,
    wallet: Wallet,
    faucet: Faucet<alloy::providers::RootProvider<T>>,
) where
    T: Transport + Clone + Send + Sync + 'static,
{
    let state = AppState {
        db,
        wallet,
        faucet: Arc::new(faucet),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/register", post(register::<T>))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", config.port);
    let listener = TcpListener::bind(&addr).await.unwrap();
    tracing::info!("Listening on {}", addr);
    axum::serve(listener, app).await.unwrap();
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

async fn register<T>(
    State(state): State<AppState<T>>,
    Json(payload): Json<RegisterRequest>,
) -> Result<Json<RegisterResponse>, ApiError>
where
    T: Transport + Clone + Send + Sync + 'static,
{
    // Check if account already exists
    if let Ok(Some((_index, existing_address, _webhook))) = state.db.get_account_by_id(&payload.id)
    {
        info!(
            "Account {} already exists with address {}",
            payload.id, existing_address
        );
        return Ok(Json(RegisterResponse {
            address: existing_address,
            funding_tx: None,
        }));
    }

    // Derive deterministic index from account_id using hash
    let index = derive_index_from_account_id(&payload.id);

    // Derive address from the deterministic index
    let address = state
        .wallet
        .derive_address(index)
        .map_err(|e| ApiError::Internal(format!("Failed to derive address: {}", e)))?;
    let address_str = address.to_string();

    // Save to DB with webhook URL
    state
        .db
        .register_account(&payload.id, index, &address_str, &payload.webhook_url)
        .map_err(|e| ApiError::Internal(format!("Failed to register account: {}", e)))?;

    info!(
        "Registered account {} with address {} (index: {})",
        payload.id, address_str, index
    );

    // Fire-and-forget: Fund the new address with existential deposit in the background
    let faucet = Arc::clone(&state.faucet);
    let db = state.db.clone();
    let account_id = payload.id.clone();
    let address_for_funding = address_str.clone();
    
    tokio::spawn(async move {
        info!(
            "Background task: Starting faucet funding for address {}",
            address_for_funding
        );
        
        match faucet.fund_new_address(&address_for_funding).await {
            Ok(tx_hash) => {
                info!(
                    "Successfully funded address {} with tx: {}",
                    address_for_funding, tx_hash
                );
                
                // Send webhook notification for successful funding
                if let Err(e) = send_faucet_funding_webhook(
                    &db,
                    &account_id,
                    &address_for_funding,
                    &tx_hash,
                    true,
                    None,
                )
                .await
                {
                    error!(
                        "Failed to send faucet funding webhook for {}: {:?}",
                        account_id, e
                    );
                }
            }
            Err(e) => {
                error!(
                    "Failed to fund address {}: {:?}",
                    address_for_funding, e
                );
                
                // Send webhook notification for failed funding
                if let Err(webhook_err) = send_faucet_funding_webhook(
                    &db,
                    &account_id,
                    &address_for_funding,
                    "",
                    false,
                    Some(&e.to_string()),
                )
                .await
                {
                    error!(
                        "Failed to send faucet funding error webhook for {}: {:?}",
                        account_id, webhook_err
                    );
                }
            }
        }
    });

    Ok(Json(RegisterResponse {
        address: address_str,
        funding_tx: None, // No longer waiting for funding - it's fire-and-forget
    }))
}

/// Derive a deterministic derivation index from an account ID
/// This ensures the same account_id always generates the same address
fn derive_index_from_account_id(account_id: &str) -> u32 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    account_id.hash(&mut hasher);
    let hash = hasher.finish();

    // Use the lower 31 bits to ensure we stay within u32::MAX and avoid negative values
    // BIP32 derivation paths use 31 bits for normal (non-hardened) derivation
    (hash & 0x7FFFFFFF) as u32
}

/// Send webhook notification for faucet funding event
async fn send_faucet_funding_webhook(
    db: &Db,
    account_id: &str,
    address: &str,
    tx_hash: &str,
    success: bool,
    error_message: Option<&str>,
) -> anyhow::Result<()> {
    // Get the webhook URL for this account
    let webhook_url = match db.get_webhook_url(account_id)? {
        Some(url) => url,
        None => {
            error!("No webhook URL found for account: {}", account_id);
            return Ok(());
        }
    };

    let client = reqwest::Client::new();

    let mut payload = serde_json::json!({
        "event": "faucet_funding",
        "account_id": account_id,
        "address": address,
        "success": success,
    });

    // Add tx_hash if funding was successful
    if success && !tx_hash.is_empty() {
        payload["tx_hash"] = serde_json::json!(tx_hash);
    }

    // Add error message if funding failed
    if let Some(error) = error_message {
        payload["error"] = serde_json::json!(error);
    }

    let res = client.post(&webhook_url).json(&payload).send().await;

    match res {
        Ok(r) => info!(
            "Faucet funding webhook sent to {}: status={}",
            webhook_url,
            r.status()
        ),
        Err(e) => error!(
            "Failed to send faucet funding webhook to {}: {:?}",
            webhook_url, e
        ),
    }

    Ok(())
}

// Error handling for the API
#[derive(Debug)]
enum ApiError {
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };

        (status, message).into_response()
    }
}
