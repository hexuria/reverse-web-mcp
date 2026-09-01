//! L0. One interface, one adapter per surface. Effectors never decide anything.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use crate::plan::Node;
use crate::world::{OpKind, ParamIn, World};

#[derive(Debug, Clone)]
pub enum EffectError {
    /// Try again with the same key: 429, 5xx, transport.
    Retryable(String),
    /// The plan's assumption is wrong: 4xx other than 429.
    Fatal(String),
}

impl std::fmt::Display for EffectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EffectError::Retryable(s) => write!(f, "retryable: {s}"),
            EffectError::Fatal(s) => write!(f, "fatal: {s}"),
        }
    }
}

#[async_trait]
pub trait Effector: Send + Sync {
    fn surface(&self) -> &str;
    async fn execute(&self, node: &Node, args: &Map<String, Value>) -> Result<Value, EffectError>;
}

fn classify(status: u16, body: &str) -> EffectError {
    let msg: String = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(|s| s.to_string()))
        .unwrap_or_else(|| body.chars().take(200).collect());
    if status == 429 || status >= 500 {
        EffectError::Retryable(format!("{status} {msg}"))
    } else {
        EffectError::Fatal(format!("{status} {msg}"))
    }
}

/// The native door. Builds the request from the OpenAPI operation.
pub struct ApiEffector {
    pub base: String,
    pub client: reqwest::Client,
    pub world: Arc<World>,
}

impl ApiEffector {
    pub fn new(base: &str, world: Arc<World>) -> Self {
        ApiEffector { base: base.trim_end_matches('/').to_string(), client: reqwest::Client::new(), world }
    }
}

#[async_trait]
impl Effector for ApiEffector {
    fn surface(&self) -> &str {
        "api"
    }

    async fn execute(&self, node: &Node, args: &Map<String, Value>) -> Result<Value, EffectError> {
        let op = self.world.op(&node.op).ok_or_else(|| EffectError::Fatal(format!("unknown op {}", node.op)))?;
        if op.kind != OpKind::Http {
            return Err(EffectError::Fatal(format!("{} is not an HTTP operation", node.op)));
        }
        let mut path = op.path.clone();
        let mut query: Vec<(String, String)> = Vec::new();
        let mut body = Map::new();
        for p in &op.params {
            let Some(v) = args.get(&p.name) else { continue };
            let s = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            match p.location {
                ParamIn::Path => path = path.replace(&format!("{{{}}}", p.name), &s),
                ParamIn::Query => query.push((p.name.clone(), s)),
                ParamIn::Body => {
                    body.insert(p.name.clone(), v.clone());
                }
            }
        }
        let url = format!("{}{}", self.base, path);
        let mut req = match op.method.as_str() {
            "GET" => self.client.get(&url),
            "POST" => self.client.post(&url),
            "PUT" => self.client.put(&url),
            "DELETE" => self.client.delete(&url),
            m => return Err(EffectError::Fatal(format!("unsupported method {m}"))),
        };
        req = req.query(&query).header("x-door", "api");
        if let Some(k) = &node.key {
            req = req.header("idempotency-key", k);
        }
        if !body.is_empty() || op.method == "POST" {
            req = req.json(&Value::Object(body));
        }
        let resp = req.send().await.map_err(|e| EffectError::Retryable(format!("transport: {e}")))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| EffectError::Retryable(format!("body: {e}")))?;
        if !(200..300).contains(&status) {
            return Err(classify(status, &text));
        }
        serde_json::from_str(&text).map_err(|e| EffectError::Fatal(format!("bad json: {e}")))
    }
}

/// The MCP door: JSON-RPC `tools/call` against any MCP server over HTTP. The same adapter
/// reaches the target app's `/mcp` and a computer-use driver that speaks MCP.
pub struct McpEffector {
    pub url: String,
    pub surface_name: String,
    pub client: reqwest::Client,
}

impl McpEffector {
    pub fn new(url: &str, surface_name: &str) -> Self {
        McpEffector { url: url.to_string(), surface_name: surface_name.to_string(), client: reqwest::Client::new() }
    }

    pub async fn call(&self, name: &str, args: Value) -> Result<Value, EffectError> {
        let req = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}});
        let resp = self.client.post(&self.url).json(&req).send().await.map_err(|e| EffectError::Retryable(format!("transport: {e}")))?;
        let v: Value = resp.json().await.map_err(|e| EffectError::Retryable(format!("bad json: {e}")))?;
        if let Some(err) = v.get("error") {
            return Err(EffectError::Fatal(err.to_string()));
        }
        let result = v.get("result").cloned().unwrap_or(Value::Null);
        if result.get("isError").and_then(|b| b.as_bool()).unwrap_or(false) {
            let status = result.get("_status").and_then(|s| s.as_u64()).unwrap_or(500) as u16;
            let text = result.pointer("/content/0/text").and_then(|t| t.as_str()).unwrap_or("tool error").to_string();
            return Err(if status == 429 || status >= 500 { EffectError::Retryable(text) } else { EffectError::Fatal(text) });
        }
        if let Some(sc) = result.get("structuredContent") {
            return Ok(sc.clone());
        }
        let text = result.pointer("/content/0/text").and_then(|t| t.as_str()).unwrap_or("null");
        Ok(serde_json::from_str(text).unwrap_or(Value::String(text.to_string())))
    }
}

#[async_trait]
impl Effector for McpEffector {
    fn surface(&self) -> &str {
        &self.surface_name
    }

    async fn execute(&self, node: &Node, args: &Map<String, Value>) -> Result<Value, EffectError> {
        let mut a = args.clone();
        if let Some(k) = &node.key {
            a.insert("idempotency_key".into(), Value::String(k.clone()));
        }
        self.call(&node.op, Value::Object(a)).await
    }
}

/// A surface the plan chose but this run has not wired. Fails loudly, never silently serial.
pub struct Unavailable(pub String);

#[async_trait]
impl Effector for Unavailable {
    fn surface(&self) -> &str {
        &self.0
    }

    async fn execute(&self, node: &Node, _args: &Map<String, Value>) -> Result<Value, EffectError> {
        Err(EffectError::Fatal(format!("surface '{}' has no effector in this run (node {} {})", self.0, node.id, node.op)))
    }
}
