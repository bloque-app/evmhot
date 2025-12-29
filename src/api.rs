use alloy::transports::Transport;
use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use evm_hot_wallet::{
    HotWalletService, RegisterRequest, RegisterResponse, VerifyTransferRequest,
    VerifyTransferResponse,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Deserialize)]
pub struct SetBlockNumberRequest {
    pub block_number: u64,
}

#[derive(Serialize)]
pub struct BlockNumberResponse {
    pub block_number: u64,
}
use tokio::net::TcpListener;

#[derive(Clone)]
struct AppState<T>
where
    T: Transport + Clone,
{
    service: Arc<HotWalletService<T>>,
}

pub async fn start_server<T>(service: HotWalletService<T>, port: u16)
where
    T: Transport + Clone + Send + Sync + 'static,
{
    let state = AppState {
        service: Arc::new(service),
    };

    let app = Router::new()
        .route("/health", get(health::<T>))
        .route("/register", post(register::<T>))
        .route("/verify_transfer", post(verify_transfer::<T>))
        .route("/block_number", get(get_block_number::<T>))
        .route("/block_number", post(set_block_number::<T>))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&addr).await.unwrap();
    tracing::info!("Listening on {}", addr);
    axum::serve(listener, app).await.unwrap();
}

async fn health<T>(State(state): State<AppState<T>>) -> impl IntoResponse
where
    T: Transport + Clone + Send + Sync + 'static,
{
    match state.service.health().await {
        Ok(msg) => (StatusCode::OK, msg),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "ERROR".to_string()),
    }
}

async fn register<T>(
    State(state): State<AppState<T>>,
    Json(payload): Json<RegisterRequest>,
) -> Result<Json<RegisterResponse>, ApiError>
where
    T: Transport + Clone + Send + Sync + 'static,
{
    match state.service.register(payload).await {
        Ok(response) => Ok(Json(response)),
        Err(e) => Err(ApiError::Internal(format!("Failed to register: {}", e))),
    }
}

async fn verify_transfer<T>(
    State(state): State<AppState<T>>,
    Json(payload): Json<VerifyTransferRequest>,
) -> Result<Json<VerifyTransferResponse>, ApiError>
where
    T: Transport + Clone + Send + Sync + 'static,
{
    match state.service.verify_transfer(payload).await {
        Ok(response) => Ok(Json(response)),
        Err(e) => Err(ApiError::Internal(format!(
            "Failed to verify transfer: {}",
            e
        ))),
    }
}

async fn get_block_number<T>(
    State(state): State<AppState<T>>,
) -> Result<Json<BlockNumberResponse>, ApiError>
where
    T: Transport + Clone + Send + Sync + 'static,
{
    let block_number = state
        .service
        .get_block_number()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(BlockNumberResponse { block_number }))
}

async fn set_block_number<T>(
    State(state): State<AppState<T>>,
    Json(payload): Json<SetBlockNumberRequest>,
) -> Result<Json<BlockNumberResponse>, ApiError>
where
    T: Transport + Clone + Send + Sync + 'static,
{
    state
        .service
        .set_block_number(payload.block_number)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(BlockNumberResponse {
        block_number: payload.block_number,
    }))
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
