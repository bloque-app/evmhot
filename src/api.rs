use crate::{config::Config, db::Db, wallet::Wallet};
use axum::{
    extract::{Json, State},
    routing::post,
    Router,
};
use serde::{Deserialize, Serialize};

use tokio::net::TcpListener;

#[derive(Clone)]
struct AppState {
    db: Db,
    wallet: Wallet,
}

#[derive(Deserialize)]
struct RegisterRequest {
    id: String,
}

#[derive(Serialize)]
struct RegisterResponse {
    address: String,
}

pub async fn start_server(config: Config, db: Db, wallet: Wallet) {
    let state = AppState { db, wallet };

    let app = Router::new()
        .route("/register", post(register))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", config.port);
    let listener = TcpListener::bind(&addr).await.unwrap();
    tracing::info!("Listening on {}", addr);
    axum::serve(listener, app).await.unwrap();
}

async fn register(
    State(state): State<AppState>,
    Json(payload): Json<RegisterRequest>,
) -> Json<RegisterResponse> {
    // 1. Get next derivation index
    let index = state.db.get_next_derivation_index().unwrap_or(0);

    // 2. Derive address
    let address = state
        .wallet
        .derive_address(index)
        .expect("Failed to derive address");
    let address_str = address.to_string();

    // 3. Save to DB
    state
        .db
        .register_account(&payload.id, index, &address_str)
        .expect("Failed to register account");

    Json(RegisterResponse {
        address: address_str,
    })
}
