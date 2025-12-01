use crate::{config::Config, db::Db, faucet::Faucet, wallet::Wallet};
use alloy::transports::Transport;
use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
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
        .route("/register", post(register::<T>))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", config.port);
    let listener = TcpListener::bind(&addr).await.unwrap();
    tracing::info!("Listening on {}", addr);
    axum::serve(listener, app).await.unwrap();
}

async fn register<T>(
    State(state): State<AppState<T>>,
    Json(payload): Json<RegisterRequest>,
) -> Result<Json<RegisterResponse>, ApiError>
where
    T: Transport + Clone,
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

    // Fund the new address with existential deposit
    let funding_tx = match state.faucet.fund_new_address(&address_str).await {
        Ok(tx_hash) => {
            info!(
                "Successfully funded address {} with tx: {}",
                address_str, tx_hash
            );
            Some(tx_hash)
        }
        Err(e) => {
            error!("Failed to fund address {}: {:?}", address_str, e);
            // Don't fail registration if funding fails - just log it
            None
        }
    };

    Ok(Json(RegisterResponse {
        address: address_str,
        funding_tx,
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
