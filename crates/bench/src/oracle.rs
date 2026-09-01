//! The bench's view of the target app: reset, chaos, snapshot, effects, and the payment hook.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use zerohuman::events::EventBus;

#[derive(Clone)]
pub struct Oracle {
    pub base: String,
    pub client: reqwest::Client,
}

impl Oracle {
    pub fn new(base: &str) -> Self {
        Oracle { base: base.trim_end_matches('/').to_string(), client: reqwest::Client::new() }
    }

    pub async fn healthy(&self) -> bool {
        self.client.get(format!("{}/health", self.base)).send().await.map(|r| r.status().is_success()).unwrap_or(false)
    }

    pub async fn wait_healthy(&self, timeout: Duration) -> anyhow::Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if self.healthy().await {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        anyhow::bail!("target app at {} did not become healthy", self.base)
    }

    pub async fn reset(&self, seed: u64) -> anyhow::Result<()> {
        self.client.post(format!("{}/oracle/reset?seed={seed}", self.base)).send().await?.error_for_status()?;
        Ok(())
    }

    pub async fn chaos(&self, chaos: &Value) -> anyhow::Result<()> {
        let body = if chaos.is_object() { chaos.clone() } else { json!({}) };
        self.client.post(format!("{}/oracle/chaos", self.base)).json(&body).send().await?.error_for_status()?;
        Ok(())
    }

    pub async fn snapshot(&self) -> anyhow::Result<Value> {
        Ok(self.client.get(format!("{}/oracle/state", self.base)).send().await?.json().await?)
    }

    pub async fn effects(&self) -> anyhow::Result<Value> {
        Ok(self.client.get(format!("{}/oracle/effects", self.base)).send().await?.json().await?)
    }

    pub async fn pay(&self, invoice_id: u64, delay_ms: u64) -> anyhow::Result<()> {
        self.client
            .post(format!("{}/oracle/pay", self.base))
            .json(&json!({"invoice_id": invoice_id, "delay_ms": delay_ms}))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// The outside world: every invoice that appears gets paid `delay_ms` later.
    /// Runs until the returned handle is aborted.
    pub fn pay_on_create(&self, bus: Arc<EventBus>, delay_ms: u64) -> tokio::task::JoinHandle<()> {
        let oracle = self.clone();
        tokio::spawn(async move {
            let mut seen = std::collections::HashSet::new();
            loop {
                for ev in bus.events() {
                    if ev.kind == "invoice.created" && seen.insert(ev.id) {
                        let _ = oracle.pay(ev.id, delay_ms).await;
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
    }
}
