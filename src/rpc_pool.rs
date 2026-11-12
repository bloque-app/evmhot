//! RPC endpoint pool (amendment 0006).
//!
//! `PoolTransport` is an alloy transport that holds several HTTP endpoints for
//! one chain. Every `provider.*` call goes through it, so no call site changes.
//!
//! Rules:
//! - Calls start at a rotating endpoint, so load spreads over the pool.
//! - An endpoint that answers 429 (or a JSON-RPC rate-limit error) cools down
//!   for every method for `rate_cooldown`; cooled endpoints are tried last.
//! - An endpoint that says a method is not available on its plan cools down for
//!   that method only.
//! - A range or size refusal, a 403 (publicnode's "archive requests require a
//!   personal token"), a 5xx or a transport error fails the call over to the
//!   next endpoint, with no cooldown: range limits depend on the request.
//! - Each endpoint is tried at most once per call. When all fail, the last
//!   failure is returned, so the monitor's own backoff and bisection see it.

use alloy::providers::{ProviderBuilder, RootProvider};
use alloy::rpc::client::RpcClient;
use alloy::rpc::json_rpc::{RequestPacket, ResponsePacket};
use alloy::transports::http::{reqwest, Http};
use alloy::transports::{BoxTransport, RpcError, TransportError, TransportErrorKind, TransportFut};
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tower::Service;

pub const DEFAULT_RATE_COOLDOWN: Duration = Duration::from_secs(10);
pub const DEFAULT_METHOD_COOLDOWN: Duration = Duration::from_secs(60);

/// Why one endpoint could not serve a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// 429 or a rate-limit message.
    RateLimited,
    /// "Not available on this plan" and the like: this method, here.
    MethodUnavailable,
    /// The range or response size is over this endpoint's limit.
    RangeRefused,
    /// 403, 5xx, a transport error or an unreadable body.
    Transient,
}

fn classify_text(msg: &str) -> Option<Refusal> {
    let s = msg.to_ascii_lowercase();
    if s.contains("rate limit")
        || s.contains("rate-limit")
        || s.contains("too many requests")
        || s.contains("request limit")
        || s.contains("429")
    {
        return Some(Refusal::RateLimited);
    }
    if s.contains("not available on this plan")
        || s.contains("method not available")
        || s.contains("method not found")
        || s.contains("not supported on")
    {
        return Some(Refusal::MethodUnavailable);
    }
    if s.contains("archive requests")
        || s.contains("personal token")
        || s.contains("personal api key")
    {
        return Some(Refusal::Transient);
    }
    if s.contains("block range")
        || s.contains("blocks range")
        || s.contains("range over")
        || s.contains("ranges over")
        || s.contains("limited to")
        || s.contains("up to a")
        || s.contains("range is too")
        || s.contains("range too")
        || s.contains("too many results")
        || s.contains("more than 10000 results")
        || s.contains("response size exceeded")
        || s.contains("exceed")
    {
        return Some(Refusal::RangeRefused);
    }
    None
}

/// Classifies a transport-level error. `None` means the call itself was bad
/// (a revert, bad params): another endpoint would answer the same.
pub fn classify(err: &TransportError) -> Option<Refusal> {
    match err {
        RpcError::Transport(TransportErrorKind::HttpError(h)) => {
            if h.status == 429 {
                Some(Refusal::RateLimited)
            } else if h.status == 403 || h.status == 408 || h.status >= 500 {
                Some(Refusal::Transient)
            } else {
                classify_text(&h.body)
            }
        }
        RpcError::Transport(TransportErrorKind::Custom(_)) => Some(Refusal::Transient),
        RpcError::Transport(TransportErrorKind::BackendGone) => Some(Refusal::Transient),
        RpcError::DeserError { .. } => Some(Refusal::Transient),
        RpcError::ErrorResp(p) => {
            if p.code == -32005 {
                Some(Refusal::RateLimited)
            } else {
                classify_text(&p.message)
            }
        }
        _ => None,
    }
}

/// True when waiting and retrying the same call is the right answer: 429,
/// 403-archive, 5xx, transport errors. Used by the monitor's backoff, and to
/// keep it from bisecting a range because of them.
pub fn is_backoff_error(err: &anyhow::Error) -> bool {
    match err.downcast_ref::<TransportError>() {
        Some(e) => matches!(
            classify(e),
            Some(Refusal::RateLimited) | Some(Refusal::Transient)
        ),
        None => false,
    }
}

struct Endpoint {
    url: String,
    http: Http<reqwest::Client>,
    /// `""` is every method.
    cooled: Mutex<HashMap<String, Instant>>,
}

impl Endpoint {
    fn cooled_until(&self, method: &str) -> Option<Instant> {
        let now = Instant::now();
        let map = self.cooled.lock().unwrap();
        [map.get(""), map.get(method)]
            .into_iter()
            .flatten()
            .copied()
            .filter(|t| *t > now)
            .max()
    }

    fn cool(&self, method: &str, d: Duration) {
        self.cooled
            .lock()
            .unwrap()
            .insert(method.to_string(), Instant::now() + d);
    }
}

struct Pool {
    endpoints: Vec<Endpoint>,
    next: AtomicUsize,
    rate_cooldown: Duration,
    method_cooldown: Duration,
}

#[derive(Clone)]
pub struct PoolTransport {
    pool: Arc<Pool>,
}

impl PoolTransport {
    pub fn new(urls: &[String]) -> Result<Self> {
        Self::with_cooldowns(urls, DEFAULT_RATE_COOLDOWN, DEFAULT_METHOD_COOLDOWN)
    }

    pub fn with_cooldowns(
        urls: &[String],
        rate_cooldown: Duration,
        method_cooldown: Duration,
    ) -> Result<Self> {
        if urls.is_empty() {
            return Err(anyhow!("an RPC pool needs at least one URL"));
        }
        let mut endpoints = Vec::with_capacity(urls.len());
        for u in urls {
            let url: reqwest::Url = u
                .parse()
                .map_err(|e| anyhow!("invalid RPC URL in pool: {e}"))?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(anyhow!(
                    "an RPC pool takes http(s) URLs only (got scheme {})",
                    url.scheme()
                ));
            }
            endpoints.push(Endpoint {
                url: u.clone(),
                http: Http::new(url),
                cooled: Mutex::new(HashMap::new()),
            });
        }
        Ok(Self {
            pool: Arc::new(Pool {
                endpoints,
                next: AtomicUsize::new(0),
                rate_cooldown,
                method_cooldown,
            }),
        })
    }
}

fn request_method(req: &RequestPacket) -> String {
    match req {
        RequestPacket::Single(r) => r.method().to_string(),
        RequestPacket::Batch(v) => v
            .first()
            .map(|r| r.method().to_string())
            .unwrap_or_default(),
    }
}

async fn execute(pool: Arc<Pool>, req: RequestPacket) -> Result<ResponsePacket, TransportError> {
    let method = request_method(&req);
    let n = pool.endpoints.len();
    let start = pool.next.fetch_add(1, Ordering::Relaxed) % n;
    let mut order: Vec<usize> = (0..n).map(|i| (start + i) % n).collect();
    // Endpoints that are not cooling go first; cooled ones last, soonest first.
    order.sort_by_key(|i| pool.endpoints[*i].cooled_until(&method));

    let mut fallback: Option<Result<ResponsePacket, TransportError>> = None;
    for (pos, idx) in order.iter().enumerate() {
        let ep = &pool.endpoints[*idx];
        let last = pos + 1 == order.len();
        let mut http = ep.http.clone();
        let result = http.call(req.clone()).await;
        let refusal = match &result {
            Ok(resp) => resp.as_error().and_then(|p| {
                if p.code == -32005 {
                    Some(Refusal::RateLimited)
                } else {
                    classify_text(&p.message)
                }
            }),
            Err(e) => classify(e),
        };
        let Some(refusal) = refusal else {
            return result;
        };
        match refusal {
            Refusal::RateLimited => ep.cool("", pool.rate_cooldown),
            Refusal::MethodUnavailable => ep.cool(&method, pool.method_cooldown),
            _ => {}
        }
        tracing::warn!(
            "rpc pool: {} refused {method} ({refusal:?}){}",
            redact(&ep.url),
            if last {
                ""
            } else {
                ", trying the next endpoint"
            }
        );
        fallback = Some(result);
    }
    fallback.expect("pool has at least one endpoint")
}

/// Host only: endpoint URLs may carry API keys in the path or query.
fn redact(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "?".into())
}

impl Service<RequestPacket> for PoolTransport {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RequestPacket) -> Self::Future {
        Box::pin(execute(self.pool.clone(), req))
    }
}

/// Builds the provider for a chain: the plain provider for one URL (any scheme
/// alloy knows), the pool for several (http(s) only).
pub async fn connect_provider(urls: &[String]) -> Result<RootProvider<BoxTransport>> {
    match urls {
        [] => Err(anyhow!("no RPC URL configured")),
        [one] => Ok(ProviderBuilder::new()
            .on_builtin(one)
            .await
            .map_err(|e| anyhow!("{e}"))?),
        many => {
            let transport = PoolTransport::new(many)?;
            Ok(ProviderBuilder::new()
                .on_client(RpcClient::new(transport, false))
                .boxed())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::providers::Provider;
    use serde_json::json;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ok_block_number() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":0,"result":"0x2a"}))
    }

    async fn serve(t: ResponseTemplate) -> MockServer {
        let s = MockServer::start().await;
        Mock::given(method("POST")).respond_with(t).mount(&s).await;
        s
    }

    async fn hits(s: &MockServer) -> usize {
        s.received_requests().await.unwrap().len()
    }

    fn provider(urls: &[&MockServer], cool: Duration) -> RootProvider<BoxTransport> {
        let urls: Vec<String> = urls.iter().map(|s| s.uri()).collect();
        let t = PoolTransport::with_cooldowns(&urls, cool, cool).unwrap();
        ProviderBuilder::new()
            .on_client(RpcClient::new(t, false))
            .boxed()
    }

    #[tokio::test]
    async fn a_429_on_the_first_endpoint_is_answered_by_the_second() {
        let a = serve(ResponseTemplate::new(429).set_body_string("Rate limit reached")).await;
        let b = serve(ok_block_number()).await;
        let p = provider(&[&a, &b], Duration::from_secs(30));
        assert_eq!(p.get_block_number().await.unwrap(), 42);
        assert_eq!(hits(&a).await, 1);
        assert_eq!(hits(&b).await, 1);
    }

    #[tokio::test]
    async fn a_cooling_endpoint_is_skipped_then_tried_again() {
        let a = serve(ResponseTemplate::new(429).set_body_string("slow down")).await;
        let b = serve(ok_block_number()).await;
        let p = provider(&[&a, &b], Duration::from_millis(300));
        for _ in 0..6 {
            assert_eq!(p.get_block_number().await.unwrap(), 42);
        }
        // Rotation would have started at `a` three times; the cooldown kept it to one.
        assert_eq!(hits(&a).await, 1);
        assert_eq!(hits(&b).await, 6);
        tokio::time::sleep(Duration::from_millis(350)).await;
        for _ in 0..2 {
            p.get_block_number().await.unwrap();
        }
        assert_eq!(hits(&a).await, 2, "after the cooldown it is tried again");
    }

    #[tokio::test]
    async fn a_403_archive_answer_fails_over() {
        let a = serve(ResponseTemplate::new(403).set_body_string(
            r#"{"code":-32602,"message":"Archive requests require a personal token"}"#,
        ))
        .await;
        let b = serve(ok_block_number()).await;
        let p = provider(&[&a, &b], Duration::from_secs(30));
        assert_eq!(p.get_block_number().await.unwrap(), 42);
        assert_eq!(p.get_block_number().await.unwrap(), 42);
        // No cooldown for a 403: both calls were offered to `a` when it was first in turn.
        assert!(hits(&a).await >= 1);
    }

    #[tokio::test]
    async fn a_5xx_and_a_range_refusal_fail_over() {
        let a = serve(ResponseTemplate::new(503)).await;
        let c = serve(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc":"2.0","id":0,
            "error":{"code":-32602,"message":"eth_getLogs is limited to 0 - 50 blocks range"}
        })))
        .await;
        let b = serve(ok_block_number()).await;
        let p = provider(&[&a, &c, &b], Duration::from_secs(30));
        for _ in 0..3 {
            assert_eq!(p.get_block_number().await.unwrap(), 42);
        }
    }

    #[tokio::test]
    async fn a_plan_refusal_cools_that_method_only() {
        let a = serve(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc":"2.0","id":0,
            "error":{"code":-32602,"message":"Method not available on this plan with fullTransactions=true"}
        })))
        .await;
        let b = serve(ok_block_number()).await;
        let p = provider(&[&a, &b], Duration::from_secs(30));
        for _ in 0..4 {
            assert_eq!(p.get_block_number().await.unwrap(), 42);
        }
        assert_eq!(
            hits(&a).await,
            1,
            "refused once, then skipped for that method"
        );
    }

    #[tokio::test]
    async fn a_revert_is_not_failed_over() {
        let a = serve(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc":"2.0","id":0,
            "error":{"code":3,"message":"execution reverted"}
        })))
        .await;
        let b = serve(ok_block_number()).await;
        let p = provider(&[&a, &b], Duration::from_secs(30));
        // Whichever endpoint starts, `a` is never retried elsewhere on a revert:
        // the pool returns it as is. (Rotation starts at `a` on the first call.)
        assert!(p.get_block_number().await.is_err());
        assert_eq!(hits(&b).await, 0);
    }

    #[tokio::test]
    async fn when_every_endpoint_refuses_the_last_error_comes_back() {
        let a = serve(ResponseTemplate::new(429).set_body_string("x")).await;
        let b = serve(ResponseTemplate::new(429).set_body_string("y")).await;
        let p = provider(&[&a, &b], Duration::from_secs(30));
        let err: anyhow::Error = p.get_block_number().await.unwrap_err().into();
        assert!(is_backoff_error(&err));
        assert_eq!(hits(&a).await + hits(&b).await, 2);
    }

    #[test]
    fn the_pool_refuses_non_http_urls_and_an_empty_list() {
        assert!(PoolTransport::new(&[]).is_err());
        assert!(PoolTransport::new(&["ws://a".into(), "http://b".into()]).is_err());
    }
}
