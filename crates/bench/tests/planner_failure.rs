//! A planner that cannot produce a usable intent is an error for arm D, never a silent fallback.

use std::sync::{Arc, Mutex};

use app::domain::World as AppWorld;
use app::{router, AppState};
use async_trait::async_trait;
use bench::arms::{run_ours_planned, ArmContext, PlanRequest};
use rwmcp::planner::Sampler;
use bench::tasks::Task;
use rwmcp::events::EventBus;
use rwmcp::ledger::{Ledger, Sample, SampleKind};
use rwmcp::Status;
use serde_json::{json, Value};
use tokio::sync::broadcast;

struct Nothing;

#[async_trait]
impl Sampler for Nothing {
    async fn sample(&self, ledger: &mut Ledger, kind: SampleKind, _body: Value) -> anyhow::Result<Value> {
        ledger.record_sample(Sample { seq: 0, kind, started_us: 0, ended_us: 1, tokens_in: 1, tokens_out: 1, model: "stub".into(), effort: "low".into() });
        Ok(json!({"content": [{"type": "tool_use", "name": "emit_intent", "input": {"wants": []}}]}))
    }
}

#[tokio::test]
async fn a_failed_planner_is_an_error_not_the_handwritten_intent() {
    let (tx, _) = broadcast::channel(1024);
    let state = Arc::new(AppState { world: Mutex::new(AppWorld::seeded(1)), events: tx });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    let base = format!("http://{addr}");
    let task = Task::load(std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tasks/T1.toml"))).unwrap();
    let world = Arc::new(rwmcp::world_from(&base).await.unwrap());
    let ctx = ArmContext {
        base: base.clone(),
        world,
        bus: EventBus::connect(&base).await.unwrap(),
        surfaces: vec!["api".into()],
        run_id: "r1".into(),
        browser: None,
        shots_dir: None,
        max_turns: 40,
    };
    let out = run_ours_planned(&task, &ctx, Some(PlanRequest { sampler: &Nothing, facts: String::new(), cache: None })).await.unwrap();
    assert_eq!(out.receipt.status, Status::Error);
    assert!(out.receipt.error.as_deref().unwrap_or("").starts_with("planner:"), "{:?}", out.receipt.error);
    assert_eq!(out.receipt.samples, 3, "plan and two re-asks");
    assert!(out.intent.wants.is_empty());
    let state: Value = reqwest::get(format!("{base}/oracle/state")).await.unwrap().json().await.unwrap();
    assert_eq!(state["invoices"].as_array().unwrap().len(), 0, "nothing ran");
    // Without a planner the task file's wants are used, and that is labelled as such by the caller.
    let out = run_ours_planned(&task, &ctx, None).await.unwrap();
    assert_eq!(out.receipt.status, Status::Committed);
}
