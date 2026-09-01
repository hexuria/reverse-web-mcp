//! The arms. Each takes a task and the app, and returns a Receipt built from a Ledger.
//!
//!   D  ours: intent → compiler → scheduler
//!   E  script ceiling: a hand-written parallel program, no model
//!
//! The model-driven arms (A CUA, B MCP, B' MCP-parallel, C WebMCP) live in `loops.rs`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use zerohuman::ledger::{now_ms, now_us, Ledger, Receipt, Row, Status};
use zerohuman::plan::Plan;
use zerohuman::{compile, CompileOptions, Scheduler, World};

use crate::tasks::Task;

pub struct ArmContext {
    pub base: String,
    pub world: Arc<World>,
    pub bus: Arc<zerohuman::events::EventBus>,
    pub surfaces: Vec<String>,
    pub run_id: String,
}

// ---------------- D: ours ----------------

/// `intent` is the planner's output when a model planned; otherwise the task file's hand-written wants.
/// `ledger` already carries the planner's sample and tokens, if any.
pub async fn run_ours(task: &Task, ctx: &ArmContext, intent: Option<zerohuman::Intent>, mut ledger: Ledger) -> anyhow::Result<Receipt> {
    let intent = intent.unwrap_or_else(|| task.intent());
    let opts = CompileOptions { plan_id: format!("{}-{}", task.id, ctx.run_id), surfaces: ctx.surfaces.clone() };
    let plan = match compile(&intent, &ctx.world, &opts) {
        Ok(p) => p,
        Err(e) => {
            let empty = Plan { plan_id: opts.plan_id.clone(), goal: intent.goal.clone(), nodes: vec![], edges: vec![], gates: vec![] };
            ledger.ended_ms = now_ms();
            return Ok(ledger.receipt(&empty, Status::Error, None, None, Some(format!("compile: {e}"))));
        }
    };
    let effectors = zerohuman::default_effectors(&ctx.base, ctx.world.clone(), &ctx.surfaces);
    let sched = Scheduler { effectors, bus: Some(ctx.bus.clone()), pools: Default::default(), policy: Default::default() };
    let outcome = sched.run(&plan, &mut ledger).await;
    Ok(ledger.receipt(&plan, outcome.status, outcome.yield_reason, outcome.evidence, outcome.error))
}

// ---------------- E: script ceiling ----------------

/// A tiny recorder so the script's calls land in a ledger the same way ours do.
#[derive(Clone)]
pub struct Script {
    base: String,
    client: reqwest::Client,
    rows: Arc<Mutex<Vec<Row>>>,
    counter: Arc<Mutex<u32>>,
}

impl Script {
    fn new(base: &str) -> Self {
        Script { base: base.trim_end_matches('/').to_string(), client: reqwest::Client::new(), rows: Arc::new(Mutex::new(Vec::new())), counter: Arc::new(Mutex::new(0)) }
    }

    fn next(&self) -> String {
        let mut c = self.counter.lock().unwrap();
        *c += 1;
        format!("s{}", *c)
    }

    async fn call(&self, op: &str, method: &str, path: &str, body: Option<Value>, key: Option<String>, write: bool) -> Result<Value, String> {
        let node = self.next();
        let mut attempt = 0;
        loop {
            attempt += 1;
            let started = now_us();
            let url = format!("{}{}", self.base, path);
            let mut req = if method == "GET" { self.client.get(&url) } else { self.client.post(&url) };
            req = req.header("x-door", "api");
            if let Some(k) = &key {
                req = req.header("idempotency-key", k);
            }
            if let Some(b) = &body {
                req = req.json(b);
            } else if method == "POST" {
                req = req.json(&json!({}));
            }
            let res: Result<(u16, Value), String> = match req.send().await {
                Ok(r) => {
                    let status = r.status().as_u16();
                    let v = r.json::<Value>().await.unwrap_or(Value::Null);
                    Ok((status, v))
                }
                Err(e) => Err(e.to_string()),
            };
            let ended = now_us();
            let (ok, observed, err) = match &res {
                Ok((s, v)) if (200..300).contains(s) => (true, v.clone(), None),
                Ok((s, v)) => (false, v.clone(), Some(format!("{s} {}", v.get("error").and_then(|e| e.as_str()).unwrap_or("")))),
                Err(e) => (false, Value::Null, Some(e.clone())),
            };
            self.rows.lock().unwrap().push(Row { node: node.clone(), op: op.into(), surface: "api".into(), key: key.clone(), attempt, started_us: started, ended_us: ended, ok, write, error: err.clone(), observed: observed.clone() });
            match res {
                Ok((s, v)) if (200..300).contains(&s) => return Ok(v),
                Ok((s, _)) if (s == 429 || s >= 500) && attempt < 4 => {
                    tokio::time::sleep(Duration::from_millis(40 * (1 << (attempt - 1)))).await;
                }
                Ok(_) => return Err(err.unwrap_or_default()),
                Err(_) if attempt < 4 => tokio::time::sleep(Duration::from_millis(40)).await,
                Err(e) => return Err(e),
            }
        }
    }

    async fn customer(&self, name: &str) -> Result<Value, String> {
        let v = self.call("listCustomers", "GET", &format!("/api/customers?name={name}"), None, None, false).await?;
        let arr = v.as_array().cloned().unwrap_or_default();
        if arr.len() != 1 {
            return Err(format!("fork:{} customers named {name}", arr.len()));
        }
        Ok(arr[0].clone())
    }

    async fn invoice_and_send(&self, name: &str, key_prefix: &str) -> Result<u64, String> {
        let c = self.customer(name).await?;
        let cid = c["id"].as_u64().unwrap();
        let inv = self.call("createInvoice", "POST", "/api/invoices", Some(json!({"customer_id": cid, "amount_cents": 10000})), Some(format!("{key_prefix}/{name}/create")), true).await?;
        let id = inv["id"].as_u64().unwrap();
        self.call("sendInvoice", "POST", &format!("/api/invoices/{id}/send"), None, Some(format!("{key_prefix}/{name}/send")), true).await?;
        Ok(id)
    }
}

async fn t4(s: &Script, kp: &str) -> Result<(), String> {
    let id = s.invoice_and_send("Acme", kp).await?;
    // The ceiling is allowed to poll: it is the speed of light, not the claim.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline {
        let inv = s.call("getInvoice", "GET", &format!("/api/invoices/{id}"), None, None, false).await?;
        if inv["status"] == "paid" {
            s.call("sendReceipt", "POST", &format!("/api/invoices/{id}/receipt"), None, Some(format!("{kp}/receipt")), true).await?;
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err("never paid".into())
}

const TEN: [&str; 10] = ["Acme", "Globex", "Initech", "Umbrella", "Hooli", "Vandelay", "Stark", "Wayne", "Wonka", "Tyrell"];

pub async fn run_script(task: &Task, ctx: &ArmContext) -> anyhow::Result<Receipt> {
    let s = Script::new(&ctx.base);
    let mut ledger = Ledger::new();
    ledger.samples = 0;
    let kp = format!("E-{}-{}", task.id, ctx.run_id);
    let result: Result<(), String> = match task.id.as_str() {
        "T1" => s.invoice_and_send("Acme", &kp).await.map(|_| ()),
        "T2" | "T5" => {
            let futs = TEN.iter().map(|n| s.invoice_and_send(n, &kp));
            futures::future::try_join_all(futs).await.map(|_| ())
        }
        "T3" => {
            let futs = TEN[..3].iter().map(|n| s.invoice_and_send(n, &kp));
            match futures::future::try_join_all(futs).await {
                Ok(ids) => s.call("createReport", "POST", "/api/reports", Some(json!({"invoice_ids": ids})), Some(format!("{kp}/report")), true).await.map(|_| ()),
                Err(e) => Err(e),
            }
        }
        "T4" => t4(&s, &kp).await,
        "T6" => s.invoice_and_send("Acme", &kp).await.map(|_| ()),
        "T7" => Err("T7 needs the UI-only approve; the script has no screen".into()),
        other => Err(format!("no script for {other}")),
    };
    ledger.rows = s.rows.lock().unwrap().clone();
    ledger.rows.sort_by_key(|r| r.started_us);
    ledger.ended_ms = now_ms();
    let plan = Plan { plan_id: kp, goal: task.goal.clone(), nodes: vec![], edges: vec![], gates: vec![] };
    Ok(match result {
        Ok(()) => ledger.receipt(&plan, Status::Committed, None, None, None),
        Err(e) if e.starts_with("fork:") => {
            ledger.forks.push(json!({"reason": e}));
            ledger.receipt(&plan, Status::NeedThink, Some(e), None, None)
        }
        Err(e) => ledger.receipt(&plan, Status::Error, None, None, Some(e)),
    })
}

pub fn arm_names() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        ("A", "CUA click loop"),
        ("B", "MCP loop, one call per turn"),
        ("B2", "MCP loop, parallel tool calls"),
        ("C", "WebMCP loop"),
        ("D", "ours: planned graph"),
        ("E", "script ceiling, no model"),
    ])
}
