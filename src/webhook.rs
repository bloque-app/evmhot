use crate::config::Config;
use crate::db::Db;
use crate::traits::Service;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;
use tokio::time::sleep;
use tracing::{error, info, warn};

/// Outbox key of the copy of `deposit_swept` for a target's `notify_url`.
const NOTIFY_EVENT: &str = "deposit_swept:notify";
/// Notify deliveries keep trying for at least this long (two hours): the swap
/// service being down for an hour must not lose a sweep it was never told about.
const NOTIFY_RETRY_WINDOW_SECS: u64 = 2 * 60 * 60;
/// Minimum seconds between two attempts of one notify delivery.
const NOTIFY_MIN_SPACING_SECS: u64 = 60;

pub struct WebhookDeliverer {
    db: Db,
    jwt_token: Option<String>,
    /// Bearer tokens for sweep-target `notify_url`s, by URL. A delivery to one of
    /// these URLs carries its own token, never the registration webhooks' one.
    notify_tokens: std::collections::HashMap<String, String>,
    max_retries: u32,
    /// Attempts allowed to a notify delivery, spaced `notify_spacing_secs` apart.
    notify_max_retries: u32,
    notify_spacing_secs: u64,
    retry_delay_ms: u64,
    poll_interval_secs: u64,
    batch_size: u32,
    lease_seconds: u64,
    client: reqwest::Client,
    notify: Arc<Notify>,
    /// `WEBHOOK_DELIVERY_PAUSED`: every delivery stays `pending` in the outbox and no
    /// HTTP request is made, so the restart can sweep while nothing downstream fires.
    /// Unset it and restart to send everything held, in order.
    paused: bool,
}

/// True when `name` is set to `1`, `true` or `yes` (any case).
fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Seconds between notify attempts: the minimum, or the worker's poll interval if
/// that is longer (a row cannot be tried more often than the worker runs).
fn notify_spacing(poll_interval_secs: u64) -> u64 {
    NOTIFY_MIN_SPACING_SECS.max(poll_interval_secs)
}

/// Attempts that cover `NOTIFY_RETRY_WINDOW_SECS` at that spacing.
fn notify_attempts(poll_interval_secs: u64) -> u32 {
    NOTIFY_RETRY_WINDOW_SECS.div_ceil(notify_spacing(poll_interval_secs)) as u32
}

impl WebhookDeliverer {
    fn max_retries_for(&self, event: &str) -> u32 {
        if event == NOTIFY_EVENT {
            self.notify_max_retries
        } else {
            self.max_retries
        }
    }

    pub fn new(db: Db, config: &Config) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        let mut notify_tokens = std::collections::HashMap::new();
        for chain in &config.chains {
            for target in chain.token_targets.values() {
                if let (Some(url), Some(env)) = (&target.notify_url, &target.notify_token_env) {
                    // Config load already refused an empty variable.
                    let token = std::env::var(env)
                        .map_err(|_| anyhow!("{env} is not set (notify_url {url})"))?;
                    notify_tokens.insert(url.clone(), token.trim().to_string());
                }
            }
        }
        Ok(Self {
            db,
            jwt_token: config.webhook_jwt_token.clone(),
            notify_tokens,
            max_retries: config.webhook_max_retries,
            notify_max_retries: notify_attempts(config.webhook_retry_poll_interval_secs),
            notify_spacing_secs: notify_spacing(config.webhook_retry_poll_interval_secs),
            retry_delay_ms: config.webhook_retry_delay_ms,
            poll_interval_secs: config.webhook_retry_poll_interval_secs,
            batch_size: config.webhook_retry_batch_size,
            lease_seconds: config.webhook_lease_seconds,
            client,
            notify: Arc::new(Notify::new()),
            paused: env_flag("WEBHOOK_DELIVERY_PAUSED"),
        })
    }

    #[cfg(test)]
    pub fn new_for_test(
        db: Db,
        jwt_token: Option<String>,
        max_retries: u32,
        retry_delay_ms: u64,
        poll_interval_secs: u64,
        batch_size: u32,
        lease_seconds: u64,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            db,
            jwt_token,
            notify_tokens: std::collections::HashMap::new(),
            max_retries,
            notify_max_retries: notify_attempts(poll_interval_secs),
            notify_spacing_secs: notify_spacing(poll_interval_secs),
            retry_delay_ms,
            poll_interval_secs,
            batch_size,
            lease_seconds,
            client,
            notify: Arc::new(Notify::new()),
            paused: false,
        })
    }

    /// Test helper: bearer tokens for `notify_url`s.
    #[cfg(test)]
    pub fn with_notify_tokens(mut self, tokens: &[(&str, &str)]) -> Self {
        self.notify_tokens = tokens
            .iter()
            .map(|(u, t)| (u.to_string(), t.to_string()))
            .collect();
        self
    }

    /// Test helper: hold every delivery, as `WEBHOOK_DELIVERY_PAUSED` does.
    #[cfg(test)]
    pub fn with_delivery_paused(mut self, paused: bool) -> Self {
        self.paused = paused;
        self
    }

    pub fn delivery_paused(&self) -> bool {
        self.paused
    }

    pub fn notify_worker(&self) {
        self.notify.notify_one();
    }

    pub fn poll_interval_secs(&self) -> u64 {
        self.poll_interval_secs
    }

    pub fn notify_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.notify)
    }

    /// Hot path: persist pending delivery and wake the worker. No HTTP.
    /// Detection and sweeps now write their outbox rows in the state change's
    /// own transaction (REQ-WH-7); this is for webhooks with no state change.
    #[allow(dead_code)]
    pub async fn enqueue(
        &self,
        webhook_url: &str,
        registration_id: &str,
        payload: Value,
    ) -> Result<()> {
        let id = payload
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("webhook payload missing id"))?;
        let event = payload
            .get("event")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("webhook payload missing event"))?;

        let payload_str = payload.to_string();
        let should_notify = {
            let id = id.to_string();
            let event = event.to_string();
            let registration_id = registration_id.to_string();
            let webhook_url = webhook_url.to_string();
            self.db
                .blocking(move |db| {
                    db.upsert_webhook_delivery(
                        &id,
                        &event,
                        &registration_id,
                        &webhook_url,
                        &payload_str,
                    )
                })
                .await?
        };

        if should_notify {
            self.notify_worker();
        }
        Ok(())
    }

    /// Worker path: claim lease, perform one POST, record outcome.
    pub async fn attempt_stored(&self, id: &str, event: &str) -> Result<()> {
        if self.paused {
            // Held: the row stays `pending` with no attempt counted.
            return Ok(());
        }
        let record = {
            let id = id.to_string();
            let event = event.to_string();
            self.db
                .blocking(move |db| db.get_webhook_delivery(&id, &event))
                .await?
        };
        let Some(record) = record else {
            return Ok(());
        };

        if record.status == "delivered" {
            return Ok(());
        }

        if record.status != "pending" {
            return Ok(());
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let lease_until = now + self.lease_seconds as i64;
        let claimed = {
            let id = id.to_string();
            let event = event.to_string();
            let max_retries = self.max_retries_for(&event);
            self.db
                .blocking(move |db| {
                    db.claim_webhook_delivery(&id, &event, lease_until, max_retries)
                })
                .await?
        };
        if !claimed {
            return Ok(());
        }

        let record = {
            let id = id.to_string();
            let event = event.to_string();
            self.db
                .blocking(move |db| db.get_webhook_delivery(&id, &event))
                .await?
                .ok_or_else(|| anyhow!("webhook delivery disappeared after claim"))?
        };

        let payload: Value = serde_json::from_str(&record.payload)?;
        match self.try_post(&record.webhook_url, &payload).await {
            Ok(status) => {
                let id_owned = id.to_string();
                let event_owned = event.to_string();
                self.db
                    .blocking(move |db| {
                        db.record_webhook_attempt(
                            &id_owned,
                            &event_owned,
                            Some(status),
                            None,
                            "delivered",
                        )
                    })
                    .await?;
                info!(
                    "Webhook delivered: id={id}, event={event}, status={status}, registration_id={}",
                    record.registration_id
                );
            }
            Err(e) => {
                let (http_status, err_msg) = delivery_error_parts(&e);
                let next_status = if record.attempt_count + 1 >= self.max_retries_for(event) as u64
                {
                    "failed"
                } else {
                    "pending"
                };
                let attempts = {
                    let id_owned = id.to_string();
                    let event_owned = event.to_string();
                    let err_msg = err_msg.clone();
                    let next_status_owned = next_status.to_string();
                    let retry_after = if event == NOTIFY_EVENT {
                        self.notify_spacing_secs
                    } else {
                        0
                    };
                    self.db
                        .blocking(move |db| {
                            db.record_webhook_attempt_after(
                                &id_owned,
                                &event_owned,
                                http_status,
                                Some(&err_msg),
                                &next_status_owned,
                                retry_after,
                            )
                        })
                        .await?
                };
                let final_status = next_status;

                if final_status == "failed" {
                    warn!(
                        "Webhook delivery failed permanently: id={id}, event={event}, attempts={attempts}, error={err_msg}"
                    );
                    if event == NOTIFY_EVENT {
                        // That sweep is never registered at the swap service
                        // otherwise: say so where the alerts are read. The id is
                        // `<network>:<tx hash>:<log index>`.
                        let network = id.split(':').next().unwrap_or("-");
                        error!(
                            "EVMHOT_ALERT kind=notify_failed network={network} deposit={id} tx=- (event {event}, {attempts} attempts, last error: {err_msg}; retry with POST /admin/retry_webhooks)"
                        );
                    }
                } else {
                    warn!(
                        "Webhook delivery attempt failed: id={id}, event={event}, attempts={attempts}, error={err_msg}"
                    );
                }
            }
        }

        Ok(())
    }

    pub async fn process_pending_batch(&self) -> Result<usize> {
        let max_retries = self.max_retries;
        let notify_max_retries = self.notify_max_retries;
        let batch_size = self.batch_size;
        let keys = self
            .db
            .blocking(move |db| {
                db.get_pending_webhook_delivery_keys_with_notify(
                    max_retries,
                    notify_max_retries,
                    batch_size,
                )
            })
            .await?;

        if keys.is_empty() {
            return Ok(0);
        }

        let mut processed = 0usize;
        for (id, event) in keys {
            if let Err(e) = self.attempt_stored(&id, &event).await {
                warn!("Webhook attempt_stored failed for {id}/{event}: {e:?}");
            }
            processed += 1;

            if self.retry_delay_ms > 0 {
                sleep(Duration::from_millis(self.retry_delay_ms)).await;
            }
        }

        info!("Webhook worker processed {processed} pending delivery(ies)");
        Ok(processed)
    }

    async fn try_post(
        &self,
        url: &str,
        payload: &Value,
    ) -> std::result::Result<u16, DeliveryError> {
        let mut request = self.client.post(url).json(payload);
        // A notify URL gets its own token; everything else the webhook token.
        let token = self.notify_tokens.get(url).or(self.jwt_token.as_ref());
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }

        let response = request
            .send()
            .await
            .map_err(|e| DeliveryError::Network(e.to_string()))?;

        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            Ok(status)
        } else {
            Err(DeliveryError::HttpStatus(status))
        }
    }
}

#[derive(Debug)]
enum DeliveryError {
    Network(String),
    HttpStatus(u16),
}

impl std::fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeliveryError::Network(msg) => write!(f, "network error: {msg}"),
            DeliveryError::HttpStatus(code) => write!(f, "HTTP status {code}"),
        }
    }
}

fn delivery_error_parts(err: &DeliveryError) -> (Option<u16>, String) {
    match err {
        DeliveryError::Network(msg) => (None, msg.clone()),
        DeliveryError::HttpStatus(code) => (Some(*code), format!("HTTP status {code}")),
    }
}

pub struct WebhookRetryService {
    deliverer: Arc<WebhookDeliverer>,
}

impl WebhookRetryService {
    pub fn new(deliverer: Arc<WebhookDeliverer>) -> Self {
        Self { deliverer }
    }
}

#[async_trait]
impl Service for WebhookRetryService {
    async fn run(&self) {
        let notify = self.deliverer.notify_handle();
        loop {
            tokio::select! {
                _ = notify.notified() => {}
                _ = sleep(Duration::from_secs(self.deliverer.poll_interval_secs())) => {}
            }

            if self.deliverer.delivery_paused() {
                continue;
            }
            if let Err(e) = self.deliverer.process_pending_batch().await {
                warn!("Webhook retry worker error: {e:?}");
            }
        }
    }
}
