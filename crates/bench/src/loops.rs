//! Model-driven arms and the planner.
//!
//!   B   MCP loop, one tool call per turn (`disable_parallel_tool_use`)
//!   B2  MCP loop, the model may emit several tool calls per turn; they run concurrently
//!   planner  one sample that turns a goal into an intent graph for arm D
//!
//! Rust has no official Anthropic SDK, so this speaks raw HTTP to `POST /v1/messages`.
//! Every call is one "sample" in the ledger; tokens come from `usage`.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use zerohuman::effectors::{EffectError, McpEffector};
use zerohuman::ledger::{now_us, Ledger, Receipt, Recorder, Sample, SampleKind, Status};
use zerohuman::plan::Plan;

use crate::arms::ArmContext;
use crate::tasks::Task;

#[derive(Clone)]
pub struct ModelClient {
    pub base_url: String,
    pub model: String,
    pub effort: String,
    pub fallbacks: bool,
    auth: Auth,
    client: reqwest::Client,
}

#[derive(Clone)]
enum Auth {
    ApiKey(String),
    Bearer(String),
}

impl ModelClient {
    /// Any server that speaks the Messages shape works: api.anthropic.com, or a local gateway
    /// such as opencodex on http://localhost:8080 routing to other providers' models.
    ///
    /// Base URL: `--base-url`, else ANTHROPIC_BASE_URL, else api.anthropic.com.
    /// Credentials: `--api-key`, else ANTHROPIC_API_KEY, else ANTHROPIC_AUTH_TOKEN, else the `ant`
    /// CLI's active profile. A gateway that ignores keys gets a placeholder.
    pub fn from_env(model: &str, effort: &str, fallbacks: bool, base_url: Option<&str>, api_key: Option<&str>) -> anyhow::Result<ModelClient> {
        let base_url =
            base_url.map(|s| s.to_string()).or_else(|| std::env::var("ANTHROPIC_BASE_URL").ok()).unwrap_or_else(|| "https://api.anthropic.com".into());
        let is_anthropic = base_url.contains("api.anthropic.com");
        let auth = if let Some(k) = api_key {
            Auth::ApiKey(k.to_string())
        } else if let Ok(k) = std::env::var("ANTHROPIC_API_KEY") {
            Auth::ApiKey(k)
        } else if let Ok(t) = std::env::var("ANTHROPIC_AUTH_TOKEN") {
            Auth::Bearer(t)
        } else if !is_anthropic {
            Auth::ApiKey("local-gateway".into())
        } else {
            let out = std::process::Command::new("ant").args(["auth", "print-credentials", "--access-token"]).output();
            match out {
                Ok(o) if o.status.success() => Auth::Bearer(String::from_utf8_lossy(&o.stdout).trim().to_string()),
                _ => anyhow::bail!("no credentials: set ANTHROPIC_API_KEY, or run `ant auth login`"),
            }
        };
        Ok(ModelClient {
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            effort: effort.to_string(),
            // Server-side refusal fallback is an Anthropic feature; a gateway just ignores it.
            fallbacks: fallbacks && is_anthropic,
            auth,
            client: reqwest::Client::builder().timeout(Duration::from_secs(600)).build()?,
        })
    }

    /// The same client at a different effort, for the planner.
    pub fn with_effort(&self, effort: &str) -> ModelClient {
        ModelClient { effort: effort.to_string(), ..self.clone() }
    }

    /// One Messages call recorded as a sample: its span and token usage land in the ledger.
    pub async fn sample(&self, ledger: &mut Ledger, kind: SampleKind, body: Value) -> anyhow::Result<Value> {
        let started = now_us();
        let resp = self.messages(body).await?;
        let (ti, to) = usage(&resp);
        ledger.record_sample(Sample {
            seq: 0,
            kind,
            started_us: started,
            ended_us: now_us(),
            tokens_in: ti,
            tokens_out: to,
            model: self.model.clone(),
            effort: self.effort.clone(),
        });
        Ok(resp)
    }

    /// One Messages call. Returns the full response JSON. Retries 429/5xx a few times.
    pub async fn messages(&self, body: Value) -> anyhow::Result<Value> {
        let mut body = body;
        body["model"] = json!(self.model);
        body["thinking"] = json!({"type": "adaptive"});
        body["output_config"] = json!({"effort": self.effort});
        if self.fallbacks {
            body["fallbacks"] = json!("default");
        }
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut req =
                self.client.post(format!("{}/v1/messages", self.base_url)).header("anthropic-version", "2023-06-01").header("content-type", "application/json");
            let mut betas: Vec<&str> = Vec::new();
            match &self.auth {
                Auth::ApiKey(k) => req = req.header("x-api-key", k),
                Auth::Bearer(t) => {
                    req = req.header("authorization", format!("Bearer {t}"));
                    betas.push("oauth-2025-04-20");
                }
            }
            if self.fallbacks {
                betas.push("server-side-fallback-2026-07-01");
            }
            if !betas.is_empty() {
                req = req.header("anthropic-beta", betas.join(","));
            }
            let resp = req.json(&body).send().await?;
            let status = resp.status().as_u16();
            let text = resp.text().await?;
            if status == 429 || status >= 500 {
                if attempt < 4 {
                    tokio::time::sleep(Duration::from_millis(500 * (1 << attempt))).await;
                    continue;
                }
                anyhow::bail!("model API {status}: {text}");
            }
            if status != 200 {
                anyhow::bail!("model API {status}: {text}");
            }
            return Ok(serde_json::from_str(&text)?);
        }
    }
}

fn usage(resp: &Value) -> (u64, u64) {
    let u = resp.get("usage").cloned().unwrap_or(json!({}));
    let read = |k: &str| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    (read("input_tokens") + read("cache_read_input_tokens") + read("cache_creation_input_tokens"), read("output_tokens"))
}

const APP_SYSTEM: &str = "You operate an invoicing app through the tools provided. Customers are identified by name. \
Invoices default to 10000 cents unless told otherwise. Use the idempotency_key argument on every write, with a fresh \
unique key per intended effect, and reuse the same key if you retry that same effect. Do only what the task asks. \
If the task is ambiguous in a way that changes what you would do (for example two customers share a name), stop and \
ask one question instead of guessing. When the task is fully done, reply with the single word: done.";

fn mcp_tools_as_anthropic(tools: &Value) -> Vec<Value> {
    tools
        .as_array()
        .map(|a| a.iter().map(|t| json!({"name": t["name"], "description": t["description"], "input_schema": t["inputSchema"]})).collect())
        .unwrap_or_default()
}

/// Arms B and B2. The model drives the target app's MCP door until it says done.
pub async fn run_mcp_loop(task: &Task, ctx: &ArmContext, model: &ModelClient, parallel: bool) -> anyhow::Result<Receipt> {
    let mcp = Arc::new(McpEffector::new(&format!("{}/mcp", ctx.base.trim_end_matches('/')), "mcp"));
    let listed: Value = reqwest::Client::new().post(&mcp.url).json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).send().await?.json().await?;
    let tools = mcp_tools_as_anthropic(&listed["result"]["tools"]);

    let mut ledger = Ledger::new();
    let rec = Recorder::new(ctx.world.clone());
    let mut messages = vec![json!({"role": "user", "content": task.goal.clone()})];
    let mut status = Status::Error;
    let mut error: Option<String> = None;
    let mut yield_reason: Option<String> = None;
    let max_turns = 60;

    for _turn in 0..max_turns {
        let mut body = json!({
            "max_tokens": 4096,
            "system": [{"type": "text", "text": APP_SYSTEM, "cache_control": {"type": "ephemeral"}}],
            "tools": tools,
            "messages": messages,
        });
        if !parallel {
            body["tool_choice"] = json!({"type": "auto", "disable_parallel_tool_use": true});
        }
        let resp = match model.sample(&mut ledger, SampleKind::Turn, body).await {
            Ok(r) => r,
            Err(e) => {
                error = Some(e.to_string());
                break;
            }
        };
        let content = resp.get("content").cloned().unwrap_or(json!([]));
        messages.push(json!({"role": "assistant", "content": content}));
        let stop = resp.get("stop_reason").and_then(|s| s.as_str()).unwrap_or("");

        let calls: Vec<Value> = content.as_array().map(|a| a.iter().filter(|b| b["type"] == "tool_use").cloned().collect()).unwrap_or_default();
        if stop == "refusal" {
            error = Some("model refused".into());
            break;
        }
        if calls.is_empty() {
            let text: String =
                content.as_array().map(|a| a.iter().filter_map(|b| b.get("text").and_then(|t| t.as_str())).collect::<Vec<_>>().join(" ")).unwrap_or_default();
            let t = text.trim();
            if t.eq_ignore_ascii_case("done") || t.to_lowercase().contains("done") {
                status = Status::Committed;
            } else if t.ends_with('?') {
                status = Status::NeedThink;
                yield_reason = Some(t.to_string());
                ledger.forks.push(json!({"ask": t}));
            } else {
                status = Status::Committed;
            }
            break;
        }

        // Execute every tool call this turn (concurrently for B2, they arrive one at a time for B).
        let futs = calls.iter().map(|c| {
            let mcp = mcp.clone();
            let rec = rec.clone();
            let name = c["name"].as_str().unwrap_or("").to_string();
            let input = c["input"].clone();
            let id = c["id"].as_str().unwrap_or("").to_string();
            async move {
                let key = input.get("idempotency_key").and_then(|k| k.as_str()).map(|s| s.to_string());
                let recording = rec.start(&rec.next_node_id("t"), &name, "mcp", key, 1);
                let res = mcp.call(&name, input.clone()).await;
                recording.finish(&res);
                let content = match res {
                    Ok(v) => v.to_string(),
                    Err(EffectError::Retryable(m)) | Err(EffectError::Fatal(m)) => format!("error: {m}"),
                };
                json!({"type": "tool_result", "tool_use_id": id, "content": content})
            }
        });
        let results: Vec<Value> = futures::future::join_all(futs).await;
        messages.push(json!({"role": "user", "content": results}));
    }
    if status == Status::Error && error.is_none() {
        error = Some(format!("gave up after {max_turns} turns"));
    }
    rec.drain_into(&mut ledger);
    ledger.ended_ms = zerohuman::ledger::now_ms();
    let plan = Plan {
        plan_id: format!("{}-{}-{}", task.id, if parallel { "B2" } else { "B" }, ctx.run_id),
        goal: task.goal.clone(),
        nodes: vec![],
        edges: vec![],
        gates: vec![],
    };
    Ok(ledger.receipt(&plan, status, yield_reason, None, error))
}
