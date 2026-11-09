use axum::{
    extract::{Json, Path, State},
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use evmhot::{
    config::Config, db::WriteQueueError, HotWalletService, RegisterRequest, RegisterResponse,
    RetrySweepRequest, RetrySweepResponse, RetryWebhookRequest, RetryWebhookResponse,
    VerifyTransferRequest, VerifyTransferResponse,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::net::TcpListener;

/// Body of `POST /evm/block_number` (default network).
#[derive(Deserialize)]
pub struct SetBlockNumberRequest {
    pub block_number: u64,
}

/// Response of the default-network `block_number` routes.
#[derive(Serialize)]
pub struct BlockNumberResponse {
    pub block_number: u64,
}

/// Response of the per-network `block_number` routes.
#[derive(Serialize)]
pub struct ChainBlockNumberResponse {
    pub chain: String,
    pub block_number: u64,
}

/// Response of `GET /evm/health`.
#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub enabled: bool,
}

#[derive(Clone)]
struct AppState {
    service: Arc<HotWalletService>,
    /// Network the default-network `/evm/block_number` routes act on.
    default_chain: String,
}

/// Picks the default network: `requested` (env `DEFAULT_CHAIN`) when it names a
/// configured network, otherwise the first `[[chains]]` entry.
pub fn resolve_default_chain(config: &Config, requested: Option<&str>) -> String {
    if let Some(name) = requested.map(str::trim).filter(|n| !n.is_empty()) {
        if config.chain(name).is_some() {
            return name.to_string();
        }
        tracing::warn!(
            "DEFAULT_CHAIN={name} is not a configured network; falling back to the first [[chains]] entry"
        );
    }
    config
        .chains
        .first()
        .map(|c| c.name.clone())
        .expect("config always has at least one chain")
}

/// The service's HTTP surface (SPEC section 9). Lives in the binary, not the
/// library (DEC-10).
fn router(service: Arc<HotWalletService>, default_chain: String) -> Router {
    let state = AppState {
        service,
        default_chain,
    };

    Router::new()
        .route("/evm/health", get(health))
        .route("/evm/register", post(register))
        .route("/evm/verify_transfer", post(verify_transfer))
        .route(
            "/evm/block_number",
            get(get_block_number).post(set_block_number),
        )
        .route(
            "/evm/chains/:chain/block_number",
            get(get_chain_block_number).post(set_chain_block_number),
        )
        .route("/admin/retry_sweeps", post(retry_sweeps))
        .route("/admin/retry_webhooks", post(retry_webhooks))
        .with_state(state)
}

pub async fn start_server(service: HotWalletService, port: u16) {
    let default_chain = resolve_default_chain(
        service.config(),
        std::env::var("DEFAULT_CHAIN").ok().as_deref(),
    );
    tracing::info!("Default network for /evm/block_number: {default_chain}");
    let app = router(Arc::new(service), default_chain);

    let addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&addr).await.unwrap();
    tracing::info!("Listening on {}", addr);
    axum::serve(listener, app).await.unwrap();
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    match state.service.health().await {
        Ok(msg) => Json(HealthResponse {
            status: msg,
            enabled: true,
        }),
        Err(_) => Json(HealthResponse {
            status: "error".to_string(),
            enabled: true,
        }),
    }
}

async fn register(
    State(state): State<AppState>,
    Json(payload): Json<RegisterRequest>,
) -> Result<Json<RegisterResponse>, ApiError> {
    match state.service.register(payload).await {
        Ok(response) => Ok(Json(response)),
        Err(e) => Err(map_write_error(e, "Failed to register EVM account")),
    }
}

/// Maps write-queue saturation/timeout errors to 503 (retryable, the write
/// path is overloaded or down) instead of a generic 500.
fn map_write_error(e: anyhow::Error, context: &str) -> ApiError {
    if e.downcast_ref::<WriteQueueError>().is_some() {
        ApiError::ServiceUnavailable(format!("{}: {}", context, e))
    } else {
        ApiError::Internal(format!("{}: {}", context, e))
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

async fn read_block_number(state: &AppState, chain: &str) -> Result<u64, ApiError> {
    state
        .service
        .get_block_number_async(chain)
        .await
        .map_err(|e| ApiError::Internal(format!("Failed to get block number: {}", e)))
}

async fn write_block_number(
    state: &AppState,
    chain: &str,
    block_number: u64,
) -> Result<(), ApiError> {
    state
        .service
        .set_block_number(chain, block_number)
        .await
        .map_err(|e| map_write_error(e, "Failed to set block number"))
}

async fn get_block_number(
    State(state): State<AppState>,
) -> Result<Json<BlockNumberResponse>, ApiError> {
    let block_number = read_block_number(&state, &state.default_chain).await?;
    Ok(Json(BlockNumberResponse { block_number }))
}

async fn set_block_number(
    State(state): State<AppState>,
    Json(payload): Json<SetBlockNumberRequest>,
) -> Result<Json<BlockNumberResponse>, ApiError> {
    write_block_number(&state, &state.default_chain, payload.block_number).await?;
    Ok(Json(BlockNumberResponse {
        block_number: payload.block_number,
    }))
}

async fn get_chain_block_number(
    State(state): State<AppState>,
    Path(chain): Path<String>,
) -> Result<Json<ChainBlockNumberResponse>, ApiError> {
    let block_number = read_block_number(&state, &chain).await?;
    Ok(Json(ChainBlockNumberResponse {
        chain,
        block_number,
    }))
}

async fn set_chain_block_number(
    State(state): State<AppState>,
    Path(chain): Path<String>,
    Json(payload): Json<SetBlockNumberRequest>,
) -> Result<Json<ChainBlockNumberResponse>, ApiError> {
    write_block_number(&state, &chain, payload.block_number).await?;
    Ok(Json(ChainBlockNumberResponse {
        chain,
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
        .await
        .map(Json)
        .map_err(|e| map_write_error(e, "Failed to retry sweep"))
}

async fn retry_webhooks(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<RetryWebhookRequest>,
) -> Result<Json<RetryWebhookResponse>, ApiError> {
    authorize_admin(&state, &headers)?;
    state
        .service
        .retry_webhook(payload)
        .await
        .map(Json)
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("No failed webhook delivery found") {
                ApiError::NotFound(msg)
            } else {
                map_write_error(e, "Failed to retry webhook")
            }
        })
}

#[derive(Debug)]
enum ApiError {
    Internal(String),
    Unauthorized(String),
    NotFound(String),
    ServiceUnavailable(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            ApiError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg),
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            ApiError::ServiceUnavailable(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg),
        };

        (status, message).into_response()
    }
}

#[cfg(test)]
mod tests {
    //! Shape tests for the `/evm` surface (SPEC section 9.1, REQ-API-1): each
    //! route's request in, status and JSON keys out. The chain is a mocked
    //! JSON-RPC endpoint and the store is a throwaway SQLite file.
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use evmhot::config::parse_chains_toml;
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use tempfile::NamedTempFile;
    use tower::ServiceExt;
    use wiremock::matchers::{body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The well-known anvil/hardhat development mnemonic. Never a real key.
    const DEV_MNEMONIC: &str = "test test test test test test test test test test test junk";
    const TX_HASH: &str = "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";
    const TO: &str = "0x742d35Cc6634C0532925a3b844Bc454e4438f44e";

    fn chains_toml(rpc: &str) -> String {
        let chain = |name: &str, id: u64| {
            format!(
                r#"[[chains]]
name = "{name}"
chain_id = {id}
rpc_url = "{rpc}"
treasury_address = "0x9999999999999999999999999999999999999999"
faucet_address = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
allowed_token_addresses = ["0x1111111111111111111111111111111111111111"]
"#
            )
        };
        format!("{}\n{}", chain("base", 8453), chain("polygon", 137))
    }

    struct Harness {
        app: Router,
        _db: NamedTempFile,
        _rpc: MockServer,
    }

    async fn harness(default_chain: Option<&str>) -> Harness {
        let rpc = MockServer::start().await;
        let db = NamedTempFile::new().unwrap();
        let config = Config {
            database_url: db.path().to_str().unwrap().to_string(),
            mnemonic: DEV_MNEMONIC.to_string(),
            faucet_mnemonic: DEV_MNEMONIC.to_string(),
            port: 0,
            webhook_jwt_token: Some("dev-admin-token".to_string()),
            webhook_max_retries: 5,
            webhook_retry_delay_ms: 1000,
            webhook_retry_poll_interval_secs: 30,
            webhook_retry_batch_size: 50,
            webhook_lease_seconds: 60,
            legacy_chain: "polygon".to_string(),
            db_read_pool_size: 4,
            chains: parse_chains_toml(&chains_toml(&rpc.uri())).unwrap(),
        };
        let default = resolve_default_chain(&config, default_chain);
        let service = HotWalletService::new(config).await.unwrap();
        Harness {
            app: router(Arc::new(service), default),
            _db: db,
            _rpc: rpc,
        }
    }

    async fn call(
        app: &Router,
        method: Method,
        path: &str,
        body: Option<Value>,
        bearer: Option<&str>,
    ) -> (StatusCode, String) {
        let mut req = Request::builder().method(method).uri(path);
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        if let Some(t) = bearer {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let req = req
            .body(body.map_or(Body::empty(), |b| Body::from(b.to_string())))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    fn json_body(text: &str) -> Value {
        serde_json::from_str(text).unwrap_or_else(|_| panic!("not JSON: {text}"))
    }

    fn keys(v: &Value) -> Vec<&str> {
        let mut k: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        k.sort_unstable();
        k
    }

    #[tokio::test]
    async fn health_reports_status_text_and_enabled() {
        let h = harness(None).await;
        let (status, body) = call(&h.app, Method::GET, "/evm/health", None, None).await;
        assert_eq!(status, StatusCode::OK);
        let v = json_body(&body);
        assert_eq!(keys(&v), ["enabled", "status"]);
        assert_eq!(v["enabled"], json!(true));
        assert_eq!(v["status"], json!("OK (chains: base, polygon)"));
    }

    #[tokio::test]
    async fn register_returns_address_and_is_stable() {
        let h = harness(None).await;
        let req = json!({"id": "party-1", "webhook_url": "https://hook.example/cb"});
        let (status, body) = call(
            &h.app,
            Method::POST,
            "/evm/register",
            Some(req.clone()),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let v = json_body(&body);
        // funding_tx is None and omitted, as in the host's response (REQ-RG-4).
        assert_eq!(keys(&v), ["address"]);
        let address = v["address"].as_str().unwrap().to_string();
        assert!(address.starts_with("0x") && address.len() == 42);

        // REQ-RG-1: the same id gets the same address.
        let (_, body2) = call(&h.app, Method::POST, "/evm/register", Some(req), None).await;
        assert_eq!(json_body(&body2)["address"], json!(address));
    }

    #[tokio::test]
    async fn register_with_a_malformed_body_is_a_client_error() {
        let h = harness(None).await;
        let (status, _) = call(
            &h.app,
            Method::POST,
            "/evm/register",
            Some(json!({"id": "x"})),
            None,
        )
        .await;
        assert!(status.is_client_error());
    }

    #[test]
    fn write_queue_errors_map_to_503_and_others_to_500_with_plain_text() {
        use axum::response::IntoResponse;
        let busy: anyhow::Error = WriteQueueError::QueueFull.into();
        let res = map_write_error(busy, "Failed to register EVM account").into_response();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        let other = map_write_error(anyhow::anyhow!("boom"), "Failed to register EVM account")
            .into_response();
        assert_eq!(other.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    async fn mock_native_tx(h: &Harness) {
        Mock::given(method("POST"))
            .and(body_string_contains("eth_getTransactionByHash"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1,
                "result": {
                    "hash": TX_HASH, "nonce": "0x0",
                    "blockHash": "0x000000000000000000000000000000000000000000000000000000000000000a",
                    "blockNumber": "0xA", "transactionIndex": "0x0",
                    "from": "0x0000000000000000000000000000000000000001",
                    "to": TO, "value": "0xDE0B6B3A7640000",
                    "gas": "0x5208", "gasPrice": "0x3B9ACA00", "input": "0x",
                    "v": "0x1b", "r": "0x1", "s": "0x1", "type": "0x0", "chainId": "0x1"
                }
            })))
            .mount(&h._rpc)
            .await;
        Mock::given(method("POST"))
            .and(body_string_contains("eth_getTransactionReceipt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": 1,
                "result": {
                    "transactionHash": TX_HASH, "transactionIndex": "0x0",
                    "blockHash": "0x000000000000000000000000000000000000000000000000000000000000000a",
                    "blockNumber": "0xA",
                    "from": "0x0000000000000000000000000000000000000001", "to": TO,
                    "cumulativeGasUsed": "0x5208", "gasUsed": "0x5208",
                    "contractAddress": null, "logs": [], "status": "0x1",
                    "logsBloom": format!("0x{}", "0".repeat(512)),
                    "type": "0x0", "effectiveGasPrice": "0x3B9ACA00"
                }
            })))
            .mount(&h._rpc)
            .await;
    }

    #[tokio::test]
    async fn verify_transfer_success_shape() {
        let h = harness(None).await;
        mock_native_tx(&h).await;
        let req = json!({
            "chain": "polygon", "tx_hash": TX_HASH, "to_address": TO,
            "amount": "1000000000000000000", "token_type": "native"
        });
        let (status, body) = call(
            &h.app,
            Method::POST,
            "/evm/verify_transfer",
            Some(req),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let v = json_body(&body);
        assert_eq!(
            keys(&v),
            [
                "actual_amount",
                "actual_to",
                "block_number",
                "status",
                "token_type"
            ]
        );
        assert_eq!(v["status"], json!("success"));
        assert_eq!(v["actual_amount"], json!("1000000000000000000"));
        assert_eq!(v["token_type"], json!("native"));
        assert_eq!(v["block_number"], json!(10));
    }

    #[tokio::test]
    async fn verify_transfer_mismatch_is_a_200_with_status_error() {
        let h = harness(None).await;
        mock_native_tx(&h).await;
        let req = json!({
            "chain": "polygon", "tx_hash": TX_HASH, "to_address": TO,
            "amount": "2000000000000000000", "token_type": "native"
        });
        let (status, body) = call(
            &h.app,
            Method::POST,
            "/evm/verify_transfer",
            Some(req),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let v = json_body(&body);
        assert_eq!(v["status"], json!("error"));
        assert!(v["message"].is_string());
        assert_eq!(v["token_type"], json!("native"));
    }

    #[tokio::test]
    async fn verify_transfer_unknown_chain_is_500_with_plain_text() {
        let h = harness(None).await;
        let req = json!({
            "chain": "nowhere", "tx_hash": TX_HASH, "to_address": TO,
            "amount": "1", "token_type": "native"
        });
        let (status, body) = call(
            &h.app,
            Method::POST,
            "/evm/verify_transfer",
            Some(req),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.starts_with("Failed to verify transfer: "), "{body}");
        assert!(serde_json::from_str::<Value>(&body).is_err());
    }

    #[tokio::test]
    async fn block_number_acts_on_the_default_network() {
        // Default is the first [[chains]] entry ("base") when none is named.
        let h = harness(None).await;
        let (status, body) = call(
            &h.app,
            Method::POST,
            "/evm/block_number",
            Some(json!({"block_number": 1234})),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let v = json_body(&body);
        assert_eq!(keys(&v), ["block_number"]);
        assert_eq!(v["block_number"], json!(1234));

        let (status, body) = call(&h.app, Method::GET, "/evm/block_number", None, None).await;
        assert_eq!(status, StatusCode::OK);
        let v = json_body(&body);
        assert_eq!(keys(&v), ["block_number"]);
        assert_eq!(v["block_number"], json!(1234));

        // It landed on "base", not "polygon".
        let (_, body) = call(
            &h.app,
            Method::GET,
            "/evm/chains/base/block_number",
            None,
            None,
        )
        .await;
        assert_eq!(
            json_body(&body),
            json!({"chain": "base", "block_number": 1234})
        );
        let (_, body) = call(
            &h.app,
            Method::GET,
            "/evm/chains/polygon/block_number",
            None,
            None,
        )
        .await;
        assert_ne!(json_body(&body)["block_number"], json!(1234));
    }

    #[tokio::test]
    async fn default_chain_can_be_named_and_a_bad_name_falls_back() {
        let h = harness(Some("polygon")).await;
        call(
            &h.app,
            Method::POST,
            "/evm/block_number",
            Some(json!({"block_number": 77})),
            None,
        )
        .await;
        let (_, body) = call(
            &h.app,
            Method::GET,
            "/evm/chains/polygon/block_number",
            None,
            None,
        )
        .await;
        assert_eq!(json_body(&body)["block_number"], json!(77));

        let h = harness(Some("not-a-network")).await;
        call(
            &h.app,
            Method::POST,
            "/evm/block_number",
            Some(json!({"block_number": 5})),
            None,
        )
        .await;
        let (_, body) = call(
            &h.app,
            Method::GET,
            "/evm/chains/base/block_number",
            None,
            None,
        )
        .await;
        assert_eq!(json_body(&body)["block_number"], json!(5));
    }

    #[tokio::test]
    async fn per_network_block_number_routes() {
        let h = harness(None).await;
        let (status, body) = call(
            &h.app,
            Method::POST,
            "/evm/chains/polygon/block_number",
            Some(json!({"block_number": 99})),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json_body(&body),
            json!({"chain": "polygon", "block_number": 99})
        );

        let (status, body) = call(
            &h.app,
            Method::GET,
            "/evm/chains/polygon/block_number",
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json_body(&body),
            json!({"chain": "polygon", "block_number": 99})
        );

        let (status, _) = call(
            &h.app,
            Method::GET,
            "/evm/chains/nowhere/block_number",
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn unprefixed_routes_are_gone() {
        let h = harness(None).await;
        for (m, p) in [
            (Method::GET, "/health"),
            (Method::POST, "/register"),
            (Method::POST, "/verify_transfer"),
            (Method::GET, "/block_number"),
            (Method::POST, "/block_number"),
        ] {
            let (status, _) = call(&h.app, m, p, Some(json!({})), None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{p}");
        }
    }

    #[tokio::test]
    async fn admin_routes_need_the_bearer_token() {
        let h = harness(None).await;
        let req = json!({"chain": "polygon", "tx_hash": TX_HASH});
        let (status, _) = call(
            &h.app,
            Method::POST,
            "/admin/retry_sweeps",
            Some(req.clone()),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = call(
            &h.app,
            Method::POST,
            "/admin/retry_sweeps",
            Some(req.clone()),
            Some("wrong"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, body) = call(
            &h.app,
            Method::POST,
            "/admin/retry_sweeps",
            Some(req),
            Some("dev-admin-token"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json_body(&body),
            json!({"retried": false, "token_type": "native"})
        );

        let (status, _) = call(
            &h.app,
            Method::POST,
            "/admin/retry_webhooks",
            Some(json!({"id": "x", "event": "deposit_swept"})),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
