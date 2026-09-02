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
            let resp_retry_after: Option<u64> = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
            let text = resp.text().await?;
            if status == 429 || status >= 500 {
                if attempt < 6 {
                    // Honour retry-after when the route says, capped so a long cooldown fails fast.
                    let after = resp_retry_after.map(|s| s * 1000).unwrap_or(500 * (1 << attempt)).min(30_000);
                    tokio::time::sleep(Duration::from_millis(after)).await;
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
ask one question instead of guessing. When the task is fully done, reply with the single word: done.\n\n\
Worked example. Task: invoice Acme and Globex, send both, then one report over both. A good run: listCustomers for each \
name to get ids; createInvoice for each id (amount 10000, keys inv-acme and inv-globex); sendInvoice for each invoice id \
(keys send-acme and send-globex); createReport over both invoice ids (key report-1); reply done. \
Independent steps may be issued together in one turn.";

fn mcp_tools_as_anthropic(tools: &Value) -> Vec<Value> {
    tools
        .as_array()
        .map(|a| a.iter().map(|t| json!({"name": t["name"], "description": t["description"], "input_schema": t["inputSchema"]})).collect())
        .unwrap_or_default()
}

/// Arms B and B2. The model drives the target app's MCP door until it says done.
/// `facts` is the same read of the world the planner gets, so the baseline is coached identically.
/// Where a loop's tool calls go. MCP over HTTP and WebMCP in a page are the same loop with a
/// different backend, so the loop is written once.
#[async_trait::async_trait]
pub trait ToolBackend: Send + Sync {
    fn surface(&self) -> &str;
    /// Tool definitions in the Messages API shape.
    async fn list(&self) -> anyhow::Result<Vec<Value>>;
    async fn call(&self, name: &str, args: Value) -> Result<Value, EffectError>;
}

/// The target app's MCP door.
pub struct McpBackend(pub McpEffector);

#[async_trait::async_trait]
impl ToolBackend for McpBackend {
    fn surface(&self) -> &str {
        "mcp"
    }
    async fn list(&self) -> anyhow::Result<Vec<Value>> {
        let listed: Value = reqwest::Client::new().post(&self.0.url).json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).send().await?.json().await?;
        Ok(mcp_tools_as_anthropic(&listed["result"]["tools"]))
    }
    async fn call(&self, name: &str, args: Value) -> Result<Value, EffectError> {
        self.0.call(name, args).await
    }
}

/// The page's WebMCP tools, called from inside a headless browser. Each call leases a page from
/// the pool, so concurrency is bounded by pages the way MCP is bounded by connections.
pub struct WebMcpBackend {
    pub base: String,
    pub pool: Arc<driver::BrowserPool>,
}

impl WebMcpBackend {
    async fn on_app(&self, page: &driver::Lease) -> anyhow::Result<()> {
        let here = page.eval("location.origin").await.unwrap_or(Value::Null);
        if here.as_str() != Some(self.base.trim_end_matches('/')) {
            page.goto(&self.base).await?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl ToolBackend for WebMcpBackend {
    fn surface(&self) -> &str {
        "webmcp"
    }
    async fn list(&self) -> anyhow::Result<Vec<Value>> {
        let page = self.pool.lease().await?;
        self.on_app(&page).await?;
        let tools = page.eval("window.__webmcp.list()").await?;
        Ok(mcp_tools_as_anthropic(&tools))
    }
    async fn call(&self, name: &str, args: Value) -> Result<Value, EffectError> {
        let page = self.pool.lease().await.map_err(|e| EffectError::Retryable(e.to_string()))?;
        self.on_app(&page).await.map_err(|e| EffectError::Retryable(e.to_string()))?;
        let js = format!("window.__webmcp.call({}, {})", serde_json::to_string(name).unwrap(), args);
        page.eval(&js).await.map_err(|e| EffectError::Fatal(e.to_string()))
    }
}

/// Arms B, B2 and C: the model drives a tool backend until it says done.
pub async fn run_tool_loop(
    task: &Task,
    ctx: &ArmContext,
    model: &dyn crate::planner::Sampler,
    facts: &str,
    parallel: bool,
    backend: Arc<dyn ToolBackend>,
    arm: &str,
) -> anyhow::Result<Receipt> {
    let tools = backend.list().await?;
    let surface = backend.surface().to_string();
    let max_turns = ctx.max_turns.max(1);

    let mut ledger = Ledger::new();
    let rec = Recorder::new(ctx.world.clone());
    let opening = format!("World facts (read just now):\n{facts}\n\nTask: {}", task.goal);
    let mut messages = vec![json!({"role": "user", "content": opening})];
    let mut status = Status::Error;
    let mut error: Option<String> = None;
    let mut yield_reason: Option<String> = None;

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
            if t.ends_with('?') {
                status = Status::NeedThink;
                yield_reason = Some(t.to_string());
                ledger.forks.push(json!({"ask": t}));
            } else if says_done(t) {
                status = Status::Committed;
            } else {
                // Neither done nor a question: the model gave up. That is a failure, not a commit.
                status = Status::Error;
                error = Some(format!("gave up: {}", t.chars().take(200).collect::<String>()));
            }
            break;
        }

        // Execute every tool call this turn (concurrently when the model emitted several).
        let futs = calls.iter().map(|c| {
            let backend = backend.clone();
            let rec = rec.clone();
            let surface = surface.clone();
            let name = c["name"].as_str().unwrap_or("").to_string();
            let input = c["input"].clone();
            let id = c["id"].as_str().unwrap_or("").to_string();
            async move {
                let key = input.get("idempotency_key").and_then(|k| k.as_str()).map(|s| s.to_string());
                let recording = rec.start(&rec.next_node_id("t"), &name, &surface, key, 1);
                let res = backend.call(&name, input.clone()).await;
                recording.finish(&res);
                let content = match res {
                    Ok(v) => v.to_string(),
                    Err(EffectError::Retryable(m)) | Err(EffectError::Fatal(m)) => format!("error: {m}"),
                    Err(EffectError::Throttled(ms, m)) => format!("error: {m} (retry after {ms} ms)"),
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
    let plan = Plan { plan_id: format!("{}-{arm}-{}", task.id, ctx.run_id), goal: task.goal.clone(), nodes: vec![], edges: vec![], gates: vec![] };
    Ok(ledger.receipt(&plan, status, yield_reason, None, error))
}

/// Arms B and B2 over the target app's MCP door.
pub async fn run_mcp_loop(task: &Task, ctx: &ArmContext, model: &dyn crate::planner::Sampler, facts: &str, parallel: bool) -> anyhow::Result<Receipt> {
    let backend = Arc::new(McpBackend(McpEffector::new(&format!("{}/mcp", ctx.base.trim_end_matches('/')), "mcp")));
    run_tool_loop(task, ctx, model, facts, parallel, backend, if parallel { "B2" } else { "B" }).await
}

/// Arm C over the page's WebMCP tools in a headless browser. Parallel tool calls allowed, like B2.
pub async fn run_webmcp_loop(task: &Task, ctx: &ArmContext, model: &dyn crate::planner::Sampler, facts: &str) -> anyhow::Result<Receipt> {
    let pool = ctx.browser.clone().ok_or_else(|| anyhow::anyhow!("arm C needs a browser pool"))?;
    let backend = Arc::new(WebMcpBackend { base: ctx.base.clone(), pool });
    run_tool_loop(task, ctx, model, facts, true, backend, "C").await
}

/// "done" means finished; "not done", "nothing done" and the like do not. A question is checked
/// before this, so a loop that asks without a question mark still reads as giving up.
pub fn says_done(text: &str) -> bool {
    let t = text.trim().to_lowercase();
    if t == "done" {
        return true;
    }
    const NEGATIONS: [&str; 6] = ["not done", "n't done", "nothing", "unable", "cannot", "can not"];
    if NEGATIONS.iter().any(|n| t.contains(n)) {
        return false;
    }
    t.split(|c: char| !c.is_alphanumeric()).any(|w| w == "done")
}

const CUA_SYSTEM: &str = "You are operating a web application by looking at screenshots and acting with a mouse and keyboard. \
You cannot call the application directly; you only see pixels. The screen is 1280 by 800; coordinates are pixels from the top-left. \
Use the computer tool once per turn. Click a form field before typing into it. After each action you get a fresh screenshot. \
Customers are listed on the page; invoices default to 10000 cents. Do only what the task asks. If the task is ambiguous in a way \
that changes what you would do, use the done action with a question instead of guessing. When the task is fully done, use done.";

fn computer_tool() -> Value {
    json!({
        "name": "computer",
        "description": "Act on the screen. Exactly one action per call.",
        "input_schema": {
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["click", "type", "key", "wait", "done"]},
                "x": {"type": "integer"}, "y": {"type": "integer"},
                "text": {"type": "string", "description": "For type: the text. For done: 'done' or a question."},
                "key": {"type": "string", "description": "For key: Enter, Tab, Escape, Backspace"}
            },
            "required": ["action"]
        }
    })
}

/// Arm A: the CUA click loop. Screenshot → model → one action, on a leased headless page.
/// Nothing here touches the host screen; the screen is the page's viewport.
pub async fn run_cua_loop(task: &Task, ctx: &ArmContext, model: &dyn crate::planner::Sampler, facts: &str) -> anyhow::Result<Receipt> {
    use base64::Engine;
    let pool = ctx.browser.clone().ok_or_else(|| anyhow::anyhow!("arm A needs a browser pool"))?;
    let page = pool.lease().await?;
    page.goto(&ctx.base).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut ledger = Ledger::new();
    let rec = Recorder::new(ctx.world.clone());
    let mut status = Status::Error;
    let mut error: Option<String> = None;
    let mut yield_reason: Option<String> = None;
    let max_turns = ctx.max_turns.max(1);
    let opening = format!("World facts (read just now):\n{facts}\n\nTask: {}\n\nHere is the screen.", task.goal);
    let mut messages: Vec<Value> = Vec::new();

    let shots = ctx.shots_dir.as_ref().map(|d| d.join(format!("{}-A-{}", task.id, ctx.run_id)));
    if let Some(d) = &shots {
        std::fs::create_dir_all(d)?;
    }
    for turn in 0..max_turns {
        let png = page.screenshot_png().await?;
        if let Some(d) = &shots {
            let _ = std::fs::write(d.join(format!("turn-{turn:02}.png")), &png);
        }
        let image =
            json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": base64::engine::general_purpose::STANDARD.encode(&png)}});
        let text = if turn == 0 { opening.clone() } else { "Here is the screen after your action.".to_string() };
        messages.push(json!({"role": "user", "content": [image, {"type": "text", "text": text}]}));
        let body = json!({
            "max_tokens": 1024,
            "system": [{"type": "text", "text": CUA_SYSTEM, "cache_control": {"type": "ephemeral"}}],
            "tools": [computer_tool()],
            "tool_choice": {"type": "auto", "disable_parallel_tool_use": true},
            "messages": messages,
        });
        let resp = match model.sample(&mut ledger, SampleKind::Turn, body).await {
            Ok(r) => r,
            Err(e) => {
                error = Some(e.to_string());
                break;
            }
        };
        let content = resp.get("content").cloned().unwrap_or(json!([]));
        messages.push(json!({"role": "assistant", "content": content}));
        let call = content.as_array().and_then(|a| a.iter().find(|b| b["type"] == "tool_use")).cloned();
        let Some(call) = call else {
            let t: String =
                content.as_array().map(|a| a.iter().filter_map(|b| b.get("text").and_then(|t| t.as_str())).collect::<Vec<_>>().join(" ")).unwrap_or_default();
            if t.trim().to_lowercase().contains("done") {
                status = Status::Committed;
            } else {
                error = Some(format!("gave up: {}", t.chars().take(200).collect::<String>()));
            }
            break;
        };
        let id = call["id"].as_str().unwrap_or("").to_string();
        let input = &call["input"];
        let action = input["action"].as_str().unwrap_or("").to_string();
        let recording = rec.start(&rec.next_node_id("px"), &format!("cua.{action}"), "pixels", None, 1);
        let outcome: Result<Value, String> = match action.as_str() {
            "click" => page
                .click_at(input["x"].as_f64().unwrap_or(0.0), input["y"].as_f64().unwrap_or(0.0))
                .await
                .map(|_| json!({"ok": true}))
                .map_err(|e| e.to_string()),
            "type" => page.type_text(input["text"].as_str().unwrap_or("")).await.map(|_| json!({"ok": true})).map_err(|e| e.to_string()),
            "key" => page.press(input["key"].as_str().unwrap_or("Enter")).await.map(|_| json!({"ok": true})).map_err(|e| e.to_string()),
            "wait" => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                Ok(json!({"ok": true}))
            }
            "done" => {
                let t = input["text"].as_str().unwrap_or("done").trim().to_string();
                if t.ends_with('?') {
                    status = Status::NeedThink;
                    yield_reason = Some(t.clone());
                    ledger.forks.push(json!({"ask": t}));
                } else {
                    status = Status::Committed;
                }
                Ok(json!({"ok": true}))
            }
            other => Err(format!("unknown action {other}")),
        };
        recording.finish(&outcome);
        if action == "done" {
            break;
        }
        // Let the page settle before the next screenshot.
        tokio::time::sleep(Duration::from_millis(250)).await;
        let result_text = match &outcome {
            Ok(_) => "ok".to_string(),
            Err(e) => format!("error: {e}"),
        };
        messages.push(json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": result_text}]}));
    }
    if status == Status::Error && error.is_none() {
        error = Some(format!("gave up after {max_turns} turns"));
    }
    rec.drain_into(&mut ledger);
    ledger.ended_ms = zerohuman::ledger::now_ms();
    let plan = Plan { plan_id: format!("{}-A-{}", task.id, ctx.run_id), goal: task.goal.clone(), nodes: vec![], edges: vec![], gates: vec![] };
    Ok(ledger.receipt(&plan, status, yield_reason, None, error))
}
