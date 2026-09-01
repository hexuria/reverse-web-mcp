//! Arm D against seed 6 (two Acmes): the plan stops with a question, a scripted planner answers
//! with customer(id=1), the plan resumes, and exactly one invoice goes out.

use std::sync::{Arc, Mutex};

use app::domain::World as AppWorld;
use app::{router, AppState};
use async_trait::async_trait;
use bench::arms::{run_ours, ArmContext, Planner};
use bench::planner::Sampler;
use bench::tasks::Task;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use zerohuman::events::EventBus;
use zerohuman::ledger::{Ledger, Sample, SampleKind};
use zerohuman::Status;

struct Answer;

#[async_trait]
impl Sampler for Answer {
    async fn sample(&self, ledger: &mut Ledger, kind: SampleKind, body: Value) -> anyhow::Result<Value> {
        assert_eq!(kind, SampleKind::ForkAnswer);
        let prompt = body["messages"][0]["content"].as_str().unwrap().to_string();
        assert!(prompt.contains("Acme"), "the question and evidence reach the planner");
        ledger.record_sample(Sample { seq: 0, kind, started_us: 0, ended_us: 1, tokens_in: 1, tokens_out: 1, model: "stub".into(), effort: "low".into() });
        Ok(json!({"content": [{"type": "tool_use", "name": "emit_intent", "input": {"wants": [
            "invoice(customer=customer(id=1)).exists",
            "invoice(customer=customer(id=1)).status='sent'"
        ]}}]}))
    }
}

#[tokio::test]
async fn a_fork_is_answered_once_and_the_plan_resumes() {
    let (tx, _) = broadcast::channel(1024);
    let state = Arc::new(AppState { world: Mutex::new(AppWorld::seeded(6)), events: tx });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    let base = format!("http://{addr}");
    let task = Task::load(std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tasks/T6.toml"))).unwrap();
    let world = Arc::new(zerohuman::world_from(&base).await.unwrap());
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

    let receipt =
        run_ours(&task, &ctx, None, Ledger::new(), Some(Planner { sampler: &Answer, facts: "  customers (11): Acme, Acme, ...".into() })).await.unwrap();
    assert_eq!(receipt.status, Status::Committed, "{:?}", receipt.error);
    assert_eq!(receipt.forks_taken, 1);
    assert_eq!(receipt.samples, 1, "one fork-answer sample");
    let state: Value = reqwest::get(format!("{base}/oracle/state")).await.unwrap().json().await.unwrap();
    let invoices = state["invoices"].as_array().unwrap();
    assert_eq!(invoices.len(), 1);
    assert_eq!(invoices[0]["customer_id"], 1);
    assert_eq!(invoices[0]["status"], "sent");
    let effects: Value = reqwest::get(format!("{base}/oracle/effects")).await.unwrap().json().await.unwrap();
    assert_eq!(effects["double_sends"], 0);
    assert!(bench::tasks::resumed_after_fork(&serde_json::to_value(&receipt).unwrap()));
    assert_eq!(task.expect.applicable(true).status, "committed");
    assert_eq!(task.expect.applicable(false).status, "need_think");
}

struct EmptyAnswer;

#[async_trait]
impl Sampler for EmptyAnswer {
    async fn sample(&self, ledger: &mut Ledger, kind: SampleKind, _body: Value) -> anyhow::Result<Value> {
        ledger.record_sample(Sample { seq: 0, kind, started_us: 0, ended_us: 1, tokens_in: 1, tokens_out: 1, model: "stub".into(), effort: "low".into() });
        Ok(json!({"content": [{"type": "tool_use", "name": "emit_intent", "input": {"wants": []}}]}))
    }
}

#[tokio::test]
async fn an_empty_fork_answer_leaves_the_question_open() {
    let (tx, _) = broadcast::channel(1024);
    let state = Arc::new(AppState { world: Mutex::new(AppWorld::seeded(6)), events: tx });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    let base = format!("http://{addr}");
    let task = Task::load(std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tasks/T6.toml"))).unwrap();
    let world = Arc::new(zerohuman::world_from(&base).await.unwrap());
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
    let receipt = run_ours(&task, &ctx, None, Ledger::new(), Some(Planner { sampler: &EmptyAnswer, facts: String::new() })).await.unwrap();
    assert_eq!(receipt.status, Status::NeedThink);
    assert!(receipt.error.as_deref().unwrap_or("").contains("fork answer"), "{:?}", receipt.error);
    assert!(!receipt.ledger.notes.is_empty());
    let state: Value = reqwest::get(format!("{base}/oracle/state")).await.unwrap().json().await.unwrap();
    assert_eq!(state["invoices"].as_array().unwrap().len(), 0);
}
