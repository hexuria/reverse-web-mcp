//! `each(customer(name_prefix='…'))` is expanded by a read before compiling; keys match the written-out form.

use std::sync::{Arc, Mutex};

use app::domain::World as AppWorld;
use app::{router, AppState};
use async_trait::async_trait;
use bench::planner::expand_selectors;
use bench::planner::{plan_with_lint, Sampler};
use bench::tasks::Task;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use zerohuman::intent::Intent;
use zerohuman::ledger::{Ledger, Sample, SampleKind};
use zerohuman::{compile, world_from, CompileOptions};

struct Selector;

#[async_trait]
impl Sampler for Selector {
    async fn sample(&self, ledger: &mut Ledger, kind: SampleKind, _body: Value) -> anyhow::Result<Value> {
        ledger.record_sample(Sample { seq: 0, kind, started_us: 0, ended_us: 1, tokens_in: 1, tokens_out: 68, model: "stub".into(), effort: "low".into() });
        Ok(json!({"content": [{"type": "tool_use", "name": "emit_intent", "input": {"wants": [
            "invoice(customer=each(customer(name_prefix='Customer '))).exists",
            "invoice(customer=each(customer(name_prefix='Customer '))).status='sent'"
        ]}}]}))
    }
}

#[tokio::test]
async fn a_prefix_selector_becomes_the_matching_names() {
    let (tx, _) = broadcast::channel(1024);
    let state = Arc::new(AppState { world: Mutex::new(AppWorld::seeded(11)), events: tx });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    let base = format!("http://{addr}");
    let intent = Intent {
        goal: "bulk".into(),
        wants: vec![
            "invoice(customer=each(customer(name_prefix='Customer '))).exists".into(),
            "invoice(customer=each(customer(name_prefix='Customer '))).status='sent'".into(),
        ],
        ..Default::default()
    };
    let expanded = expand_selectors(&intent, &base).await.unwrap();
    assert!(expanded.wants[0].contains("customer(name='Customer 001')"));
    assert!(expanded.wants[0].contains("customer(name='Customer 300')"));
    assert!(!expanded.wants[0].contains("name_prefix"));
    let world = Arc::new(world_from(&base).await.unwrap());
    let plan = compile(&expanded, &world, &CompileOptions::default()).unwrap();
    assert_eq!(plan.nodes.len(), 900);
    // Untouched wants pass through unchanged.
    let plain = Intent { goal: "g".into(), wants: vec!["invoice(customer=customer(name='Acme')).exists".into()], ..Default::default() };
    assert_eq!(expand_selectors(&plain, &base).await.unwrap().wants, plain.wants);
}

#[tokio::test]
async fn a_selector_from_the_planner_passes_lint_in_one_sample() {
    let (tx, _) = broadcast::channel(1024);
    let state = Arc::new(AppState { world: Mutex::new(AppWorld::seeded(11)), events: tx });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    let base = format!("http://{addr}");
    let world = world_from(&base).await.unwrap();
    let task = Task::load(std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tasks/T11.toml"))).unwrap();
    let mut ledger = Ledger::new();
    let intent = plan_with_lint(&task, &world, "", &Selector, &mut ledger, &CompileOptions::default(), Some(&base)).await.unwrap();
    assert_eq!(ledger.sample_count(), 1, "the selector is expanded before lint, so no re-ask");
    assert!(intent.wants[0].contains("customer(name='Customer 300')"));
    let plan = compile(&intent, &Arc::new(world), &CompileOptions::default()).unwrap();
    assert_eq!(plan.nodes.len(), 900);
}
