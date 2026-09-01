//! The event bus: one SSE connection to the target app, shared by every wait node.
//! A wait is an edge that has not fired. It costs no tokens and no polling.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::watch;

#[derive(Debug, Clone, PartialEq, Error)]
pub enum BusError {
    #[error("timed out waiting for {kind}{}", id.map(|i| format!(" id={i}")).unwrap_or_default())]
    Timeout { kind: String, id: Option<u64> },
    #[error("event stream closed")]
    Closed,
    #[error("no event bus in this run")]
    Missing,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppEvent {
    pub seq: u64,
    pub kind: String,
    pub entity: String,
    pub id: u64,
    #[serde(default)]
    pub data: Value,
}

pub struct EventBus {
    seen: Mutex<Vec<AppEvent>>,
    tx: watch::Sender<u64>,
    connected: watch::Sender<bool>,
}

impl EventBus {
    /// Connects and returns once the stream is open, so no event after this call is missed.
    pub async fn connect(base: &str) -> Result<Arc<EventBus>, BusError> {
        let (tx, _) = watch::channel(0u64);
        let (ctx, mut crx) = watch::channel(false);
        let bus = Arc::new(EventBus { seen: Mutex::new(Vec::new()), tx, connected: ctx });
        let url = format!("{}/events", base.trim_end_matches('/'));
        let b2 = bus.clone();
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            let resp = match client.get(&url).header("accept", "text/event-stream").send().await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("event bus: {e}");
                    let _ = b2.connected.send(true);
                    return;
                }
            };
            let _ = b2.connected.send(true);
            let mut stream = resp.bytes_stream();
            let mut buf = String::new();
            let mut data = String::new();
            while let Some(chunk) = stream.next().await {
                let Ok(chunk) = chunk else { break };
                buf.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(nl) = buf.find('\n') {
                    let line = buf[..nl].trim_end_matches('\r').to_string();
                    buf.drain(..=nl);
                    if let Some(d) = line.strip_prefix("data:") {
                        data.push_str(d.trim_start());
                    } else if line.is_empty() && !data.is_empty() {
                        if let Ok(ev) = serde_json::from_str::<AppEvent>(&data) {
                            let seq = ev.seq;
                            b2.seen.lock().unwrap().push(ev);
                            let _ = b2.tx.send(seq);
                        }
                        data.clear();
                    }
                }
            }
        });
        let _ = tokio::time::timeout(Duration::from_secs(5), crx.changed()).await;
        Ok(bus)
    }

    pub fn events(&self) -> Vec<AppEvent> {
        self.seen.lock().unwrap().clone()
    }

    /// Resolve when an event of `kind` (optionally for one entity id) has been seen since connect.
    pub async fn wait_for(&self, kind: &str, id: Option<u64>, timeout: Duration) -> Result<AppEvent, BusError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut rx = self.tx.subscribe();
        loop {
            rx.borrow_and_update();
            if let Some(ev) = self.seen.lock().unwrap().iter().find(|e| e.kind == kind && id.is_none_or(|i| e.id == i)) {
                return Ok(ev.clone());
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(BusError::Timeout { kind: kind.to_string(), id });
            }
            match tokio::time::timeout(remaining, rx.changed()).await {
                Ok(Ok(())) => continue,
                Ok(Err(_)) => return Err(BusError::Closed),
                Err(_) => return Err(BusError::Timeout { kind: kind.to_string(), id }),
            }
        }
    }
}
