//! The arms. Each takes a task and the app, and returns a Receipt built from a Ledger.
//!
//!   D  ours: intent → compiler → scheduler
//!   E  script ceiling: a hand-written parallel program, no model
//!
//! The model-driven arms (A CUA, B MCP, B' MCP-parallel, C WebMCP) live in `loops.rs`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use zerohuman::ledger::{now_ms, Ledger, Receipt, Recorder, Status};
use zerohuman::plan::Plan;
use zerohuman::{compile, CompileOptions, Scheduler, World};

use crate::tasks::{ScriptSpec, Task};

pub struct ArmContext {
    pub base: String,
    pub world: Arc<World>,
    pub bus: Arc<zerohuman::events::EventBus>,
    pub surfaces: Vec<String>,
    pub run_id: String,
    /// A browser, when a screen surface is in play. One page per screen lane.
    pub browser: Option<Arc<driver::BrowserPool>>,
    /// Where a pixel arm saves what it saw, one PNG per turn. None means do not save.
    pub shots_dir: Option<std::path::PathBuf>,
    /// Turn cap for the model-driven loops.
    pub max_turns: u32,
}

/// The effectors for this run: the API and MCP doors from zerohuman, plus the accessibility door
/// from the driver when a browser is available.
pub fn effectors_for(ctx: &ArmContext) -> HashMap<String, Arc<dyn zerohuman::effectors::Effector>> {
    let mut m = zerohuman::default_effectors(&ctx.base, ctx.world.clone(), &ctx.surfaces);
    if ctx.surfaces.iter().any(|s| s == "a11y") {
        if let Some(pool) = &ctx.browser {
            m.insert("a11y".into(), Arc::new(driver::A11yEffector::new(&ctx.base, pool.clone())));
        }
    }
    m
}

// ---------------- D: ours ----------------

/// `intent` is the planner's output when a model planned; otherwise the task file's hand-written wants.
/// `ledger` already carries the planner's sample and tokens, if any.
/// What arm D may do when the plan stops with a question.
pub struct Planner<'a> {
    pub sampler: &'a dyn crate::planner::Sampler,
    pub facts: String,
}

/// `intent` is the planner's output when a model planned; otherwise the task file's hand-written wants.
/// `ledger` already carries the planner's samples, if any. With a `planner`, one fork is answered
/// and the plan resumed; nothing already proven done by its key is re-sent.
pub async fn run_ours(
    task: &Task,
    ctx: &ArmContext,
    intent: Option<zerohuman::Intent>,
    mut ledger: Ledger,
    planner: Option<Planner<'_>>,
) -> anyhow::Result<Receipt> {
    let mut intent = intent.unwrap_or_else(|| task.intent());
    let opts = CompileOptions { plan_id: format!("{}-{}", task.id, ctx.run_id), surfaces: ctx.surfaces.clone() };
    let effectors = effectors_for(ctx);
    let sched =
        Scheduler { effectors, bus: Some(ctx.bus.clone()), pools: Default::default(), policy: Default::default(), recorder: Recorder::new(ctx.world.clone()) };

    let mut plan = match compile(&intent, &ctx.world, &opts) {
        Ok(p) => p,
        Err(e) => return Ok(compile_failed(&opts, &intent, ledger, e)),
    };
    let mut outcome = sched.run(&plan, &mut ledger).await;

    if outcome.status == Status::NeedThink {
        if let (Some(p), Some(evidence)) = (&planner, outcome.evidence.clone()) {
            let fork = crate::planner::ForkQuestion { ask: outcome.yield_reason.clone().unwrap_or_default(), evidence };
            match crate::planner::answer_fork(task, &ctx.world, &p.facts, &intent, &fork, p.sampler, &mut ledger).await {
                Ok(answered) => {
                    intent = answered;
                    plan = match compile(&intent, &ctx.world, &opts) {
                        Ok(p) => p,
                        Err(e) => return Ok(compile_failed(&opts, &intent, ledger, e)),
                    };
                    let done = ledger.completed(&plan);
                    outcome = sched.resume(&plan, &mut ledger, &done).await;
                }
                Err(e) => {
                    outcome.error = Some(format!("fork answer failed: {e}"));
                }
            }
        }
    }
    Ok(ledger.receipt(&plan, outcome.status, outcome.yield_reason, outcome.evidence, outcome.error))
}

/// Everything arm D needs to plan with a model.
pub struct PlanRequest<'a> {
    pub sampler: &'a dyn crate::planner::Sampler,
    pub facts: String,
    pub cache: Option<&'a crate::planner::IntentCache>,
}

pub struct OursOutcome {
    pub receipt: Receipt,
    /// The intent that was compiled, or the planner's last attempt when planning failed.
    pub intent: zerohuman::Intent,
    pub cache_hit: bool,
}

/// Arm D end to end: plan (or take the task file's wants), then run. A planner that fails is an
/// error for this arm. It never falls back to the hand-written intent, which would score a model
/// failure as a model success.
pub async fn run_ours_planned(task: &Task, ctx: &ArmContext, req: Option<PlanRequest<'_>>) -> anyhow::Result<OursOutcome> {
    let Some(req) = req else {
        let receipt = run_ours(task, ctx, None, Ledger::new(), None).await?;
        return Ok(OursOutcome { receipt, intent: task.intent(), cache_hit: false });
    };
    let mut ledger = Ledger::new();
    let opts = CompileOptions { plan_id: format!("{}-{}", task.id, ctx.run_id), surfaces: ctx.surfaces.clone() };
    let mut cache_hit = false;
    let planned = match req.cache {
        Some(cache) => crate::planner::plan_cached(cache, task, &ctx.world, &req.facts, req.sampler, &mut ledger, &opts).await.map(|(i, hit)| {
            cache_hit = hit;
            i
        }),
        None => crate::planner::plan_with_lint(task, &ctx.world, &req.facts, req.sampler, &mut ledger, &opts).await,
    };
    match planned {
        Ok(intent) => {
            let receipt = run_ours(task, ctx, Some(intent.clone()), ledger, Some(Planner { sampler: req.sampler, facts: req.facts })).await?;
            Ok(OursOutcome { receipt, intent, cache_hit })
        }
        Err(e) => {
            let empty = Plan { plan_id: opts.plan_id.clone(), goal: task.goal.clone(), nodes: vec![], edges: vec![], gates: vec![] };
            ledger.ended_ms = now_ms();
            let receipt = ledger.receipt(&empty, Status::Error, None, None, Some(format!("planner: {e}")));
            Ok(OursOutcome { receipt, intent: zerohuman::Intent { goal: task.goal.clone(), ..Default::default() }, cache_hit })
        }
    }
}

fn compile_failed(opts: &CompileOptions, intent: &zerohuman::Intent, mut ledger: Ledger, e: zerohuman::compiler::CompileError) -> Receipt {
    let empty = Plan { plan_id: opts.plan_id.clone(), goal: intent.goal.clone(), nodes: vec![], edges: vec![], gates: vec![] };
    ledger.ended_ms = now_ms();
    ledger.receipt(&empty, Status::Error, None, None, Some(format!("compile: {e}")))
}

// ---------------- E: script ceiling ----------------

/// The ceiling's calls land in the same ledger, through the same Recorder, as everyone else's.
#[derive(Clone)]
pub struct Script {
    base: String,
    client: reqwest::Client,
    rec: Recorder,
}

impl Script {
    fn new(base: &str, rec: Recorder) -> Self {
        Script { base: base.trim_end_matches('/').to_string(), client: reqwest::Client::new(), rec }
    }

    async fn call(&self, op: &str, method: &str, path: &str, body: Option<Value>, key: Option<String>) -> Result<Value, String> {
        let node = self.rec.next_node_id("s");
        let mut attempt = 0;
        loop {
            attempt += 1;
            let recording = self.rec.start(&node, op, "api", key.clone(), attempt);
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
            let outcome: Result<Value, String> = match &res {
                Ok((s, v)) if (200..300).contains(s) => Ok(v.clone()),
                Ok((s, v)) => Err(format!("{s} {}", v.get("error").and_then(|e| e.as_str()).unwrap_or(""))),
                Err(e) => Err(e.clone()),
            };
            recording.finish(&outcome);
            match res {
                Ok((s, v)) if (200..300).contains(&s) => return Ok(v),
                Ok((s, _)) if (s == 429 || s >= 500) && attempt < 4 => {
                    tokio::time::sleep(Duration::from_millis(40 * (1 << (attempt - 1)))).await;
                }
                Ok(_) => return outcome,
                Err(_) if attempt < 4 => tokio::time::sleep(Duration::from_millis(40)).await,
                Err(e) => return Err(e),
            }
        }
    }

    async fn customer(&self, name: &str) -> Result<Value, String> {
        let v = self.call("listCustomers", "GET", &format!("/api/customers?name={name}"), None, None).await?;
        let arr = v.as_array().cloned().unwrap_or_default();
        if arr.len() != 1 {
            return Err(format!("fork:{} customers named {name}", arr.len()));
        }
        Ok(arr[0].clone())
    }
}

/// One customer's lane of the script: lookup, create, then whatever the spec asks for.
async fn lane(s: &Script, spec: &ScriptSpec, name: &str, kp: &str) -> Result<u64, String> {
    let c = s.customer(name).await?;
    let cid = c["id"].as_u64().unwrap();
    let inv =
        s.call("createInvoice", "POST", "/api/invoices", Some(json!({"customer_id": cid, "amount_cents": 10000})), Some(format!("{kp}/{name}/create"))).await?;
    let id = inv["id"].as_u64().unwrap();
    if spec.send {
        s.call("sendInvoice", "POST", &format!("/api/invoices/{id}/send"), None, Some(format!("{kp}/{name}/send"))).await?;
    }
    if spec.wait_paid {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let inv = s.call("getInvoice", "GET", &format!("/api/invoices/{id}"), None, None).await?;
            if inv["status"] == "paid" {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!("invoice {id} never paid"));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    if spec.receipt {
        s.call("sendReceipt", "POST", &format!("/api/invoices/{id}/receipt"), None, Some(format!("{kp}/{name}/receipt"))).await?;
    }
    if spec.report_each {
        s.call("createReport", "POST", "/api/reports", Some(json!({"invoice_ids": [id]})), Some(format!("{kp}/{name}/report"))).await?;
    }
    Ok(id)
}

pub async fn run_script(task: &Task, ctx: &ArmContext) -> anyhow::Result<Receipt> {
    let rec = Recorder::new(ctx.world.clone());
    let s = Script::new(&ctx.base, rec.clone());
    let mut ledger = Ledger::new();
    let kp = format!("E-{}-{}", task.id, ctx.run_id);
    let result: Result<(), String> = match &task.script {
        None => Err(format!("{} has no [script] block; the ceiling cannot do it (a UI-only step, most likely)", task.id)),
        Some(spec) => {
            let futs = spec.customers.iter().map(|n| lane(&s, spec, n, &kp));
            match futures::future::try_join_all(futs).await {
                Ok(ids) if spec.report_all => {
                    s.call("createReport", "POST", "/api/reports", Some(json!({"invoice_ids": ids})), Some(format!("{kp}/report"))).await.map(|_| ())
                }
                Ok(_) => Ok(()),
                Err(e) => Err(e),
            }
        }
    };
    rec.drain_into(&mut ledger);
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
