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
use tracing::{info, warn};

pub struct WebhookDeliverer {
    db: Db,
    jwt_token: Option<String>,
    max_retries: u32,
    retry_delay_ms: u64,
    poll_interval_secs: u64,
    batch_size: u32,
    lease_seconds: u64,
    client: reqwest::Client,
    notify: Arc<Notify>,
}

impl WebhookDeliverer {
    pub fn new(db: Db, config: &Config) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            db,
            jwt_token: config.webhook_jwt_token.clone(),
            max_retries: config.webhook_max_retries,
            retry_delay_ms: config.webhook_retry_delay_ms,
            poll_interval_secs: config.webhook_retry_poll_interval_secs,
            batch_size: config.webhook_retry_batch_size,
            lease_seconds: config.webhook_lease_seconds,
            client,
            notify: Arc::new(Notify::new()),
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
            max_retries,
            retry_delay_ms,
            poll_interval_secs,
            batch_size,
            lease_seconds,
            client,
            notify: Arc::new(Notify::new()),
        })
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
        let should_notify = self.db.upsert_webhook_delivery(
            id,
            event,
            registration_id,
            webhook_url,
            &payload_str,
        )?;

        if should_notify {
            self.notify_worker();
        }
        Ok(())
    }

    /// Worker path: claim lease, perform one POST, record outcome.
    pub async fn attempt_stored(&self, id: &str, event: &str) -> Result<()> {
        let Some(record) = self.db.get_webhook_delivery(id, event)? else {
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
        if !self
            .db
            .claim_webhook_delivery(id, event, lease_until, self.max_retries)?
        {
            return Ok(());
        }

        let record = self
            .db
            .get_webhook_delivery(id, event)?
            .ok_or_else(|| anyhow!("webhook delivery disappeared after claim"))?;

        let payload: Value = serde_json::from_str(&record.payload)?;
        match self.try_post(&record.webhook_url, &payload).await {
            Ok(status) => {
                self.db
                    .record_webhook_attempt(id, event, Some(status), None, "delivered")?;
                info!(
                    "Webhook delivered: id={id}, event={event}, status={status}, registration_id={}",
                    record.registration_id
                );
            }
            Err(e) => {
                let (http_status, err_msg) = delivery_error_parts(&e);
                let next_status = if record.attempt_count + 1 >= self.max_retries as u64 {
                    "failed"
                } else {
                    "pending"
                };
                let attempts = self.db.record_webhook_attempt(
                    id,
                    event,
                    http_status,
                    Some(&err_msg),
                    next_status,
                )?;
                let final_status = next_status;

                if final_status == "failed" {
                    warn!(
                        "Webhook delivery failed permanently: id={id}, event={event}, attempts={attempts}, error={err_msg}"
                    );
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
        let keys = self
            .db
            .get_pending_webhook_delivery_keys(self.max_retries, self.batch_size)?;

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

    async fn try_post(&self, url: &str, payload: &Value) -> std::result::Result<u16, DeliveryError> {
        let mut request = self.client.post(url).json(payload);
        if let Some(ref token) = self.jwt_token {
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

            if let Err(e) = self.deliverer.process_pending_batch().await {
                warn!("Webhook retry worker error: {e:?}");
            }
        }
    }
}
