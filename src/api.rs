use axum::{
    extract::{Json, Query, State},
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use evm_hot_wallet::{
    HotWalletService, RegisterRequest, RegisterResponse, RetrySweepRequest, RetrySweepResponse,
    RetryWebhookRequest, RetryWebhookResponse, VerifyTransferRequest, VerifyTransferResponse,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::net::TcpListener;

#[derive(Deserialize)]
pub struct SetBlockNumberRequest {
    pub chain: String,
    pub block_number: u64,
}

#[derive(Deserialize)]
pub struct ChainQuery {
    pub chain: String,
}

#[derive(Serialize)]
pub struct BlockNumberResponse {
    pub chain: String,
    pub block_number: u64,
}

#[derive(Clone)]
struct AppState {
    service: Arc<HotWalletService>,
}

pub async fn start_server(service: HotWalletService, port: u16) {
    let state = AppState {
        service: Arc::new(service),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/register", post(register))
        .route("/verify_transfer", post(verify_transfer))
        .route("/block_number", get(get_block_number))
        .route("/block_number", post(set_block_number))
        .route("/admin/retry_sweeps", post(retry_sweeps))
        .route("/admin/retry_webhooks", post(retry_webhooks))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&addr).await.unwrap();
    tracing::info!("Listening on {}", addr);
    axum::serve(listener, app).await.unwrap();
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    match state.service.health().await {
        Ok(msg) => (StatusCode::OK, msg),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "ERROR".to_string()),
    }
}

async fn register(
    State(state): State<AppState>,
    Json(payload): Json<RegisterRequest>,
) -> Result<Json<RegisterResponse>, ApiError> {
    match state.service.register(payload).await {
        Ok(response) => Ok(Json(response)),
        Err(e) => Err(ApiError::Internal(format!("Failed to register: {}", e))),
    }
}

async fn verify_transfer(
    State(state): State<AppState>,
    Json(payload): Json<VerifyTransferRequest>,
) -> Result<Json<VerifyTransferResponse>, ApiError> {
    match state.service.verify_transfer(payload).await {
        Ok(response) => Ok(Json(response)),
        Err(e) => Err(ApiError::Internal(format!(
            "Failed to verify transfer: {}",
            e
        ))),
    }
}

async fn get_block_number(
    State(state): State<AppState>,
    Query(query): Query<ChainQuery>,
) -> Result<Json<BlockNumberResponse>, ApiError> {
    let block_number = state
        .service
        .get_block_number(&query.chain)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(BlockNumberResponse {
        chain: query.chain,
        block_number,
    }))
}

async fn set_block_number(
    State(state): State<AppState>,
    Json(payload): Json<SetBlockNumberRequest>,
) -> Result<Json<BlockNumberResponse>, ApiError> {
    state
        .service
        .set_block_number(&payload.chain, payload.block_number)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(BlockNumberResponse {
        chain: payload.chain,
        block_number: payload.block_number,
    }))
}

fn authorize_admin(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(expected) = state.service.config().webhook_jwt_token.as_ref() else {
        return Err(ApiError::Unauthorized(
            "Admin auth is not configured (set WEBHOOK_JWT_TOKEN)".to_string(),
        ));
    };
    let authorized = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {expected}"));
    if !authorized {
        return Err(ApiError::Unauthorized("Unauthorized".to_string()));
    }
    Ok(())
}

async fn retry_sweeps(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<RetrySweepRequest>,
) -> Result<Json<RetrySweepResponse>, ApiError> {
    authorize_admin(&state, &headers)?;
    state
        .service
        .retry_sweep(payload)
        .map(Json)
        .map_err(|e| ApiError::Internal(e.to_string()))
}

async fn retry_webhooks(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<RetryWebhookRequest>,
) -> Result<Json<RetryWebhookResponse>, ApiError> {
    authorize_admin(&state, &headers)?;
    state.service.retry_webhook(payload).map(Json).map_err(|e| {
        let msg = e.to_string();
        if msg.contains("No failed webhook delivery found") {
            ApiError::NotFound(msg)
        } else {
            ApiError::Internal(msg)
        }
    })
}

#[derive(Debug)]
enum ApiError {
    Internal(String),
    Unauthorized(String),
    NotFound(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            ApiError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg),
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
        };

        (status, message).into_response()
    }
}
