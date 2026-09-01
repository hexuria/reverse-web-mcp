//! The lint loop with a stub sampler: a wrong intent costs one extra sample, then compiles.

use std::sync::Mutex;

use async_trait::async_trait;
use bench::planner::{plan_with_lint, Sampler};
use bench::tasks::Task;
use serde_json::{json, Value};
use zerohuman::ledger::{Ledger, Sample, SampleKind};
use zerohuman::{CompileOptions, World};

struct Scripted(Mutex<Vec<Vec<&'static str>>>);

#[async_trait]
impl Sampler for Scripted {
    async fn sample(&self, ledger: &mut Ledger, kind: SampleKind, _body: Value) -> anyhow::Result<Value> {
        let wants = self.0.lock().unwrap().remove(0);
        ledger.record_sample(Sample { seq: 0, kind, started_us: 0, ended_us: 1000, tokens_in: 10, tokens_out: 5, model: "stub".into(), effort: "low".into() });
        Ok(json!({"content": [{"type": "tool_use", "name": "emit_intent", "input": {"wants": wants}}]}))
    }
}

fn world() -> World {
    let doc: Value = serde_json::from_str(include_str!("../../app/static/openapi.json")).unwrap();
    World::from_openapi(&doc).unwrap()
}

fn task() -> Task {
    toml::from_str(
        r#"
id = "T1"
title = "t"
seed = 1
goal = "Create an invoice for Acme and send it."
"#,
    )
    .unwrap()
}

#[tokio::test]
async fn a_bad_first_intent_costs_exactly_one_more_sample() {
    let sampler = Scripted(Mutex::new(vec![
        vec!["customer(name='Acme').exists", "invoice(customer=customer(name=$name)).exists"],
        vec!["invoice(customer=customer(name='Acme')).exists", "invoice(customer=customer(name='Acme')).status='sent'"],
    ]));
    let mut ledger = Ledger::new();
    let intent = plan_with_lint(&task(), &world(), "  customers (1): Acme", &sampler, &mut ledger, &CompileOptions::default()).await.unwrap();
    assert_eq!(intent.wants.len(), 2);
    assert_eq!(ledger.sample_count(), 2);
    assert_eq!(ledger.samples[0].kind, SampleKind::Plan);
    assert_eq!(ledger.samples[1].kind, SampleKind::Lint);
}

#[tokio::test]
async fn a_good_first_intent_costs_one_sample() {
    let sampler = Scripted(Mutex::new(vec![vec!["invoice(customer=customer(name='Acme')).status='sent'"]]));
    let mut ledger = Ledger::new();
    plan_with_lint(&task(), &world(), "", &sampler, &mut ledger, &CompileOptions::default()).await.unwrap();
    assert_eq!(ledger.sample_count(), 1);
}

#[tokio::test]
async fn two_bad_intents_is_an_error_not_a_third_sample() {
    let sampler = Scripted(Mutex::new(vec![vec!["nonsense("], vec!["customer(name='Acme').exists"], vec!["should not be asked"]]));
    let mut ledger = Ledger::new();
    let err = plan_with_lint(&task(), &world(), "", &sampler, &mut ledger, &CompileOptions::default()).await.unwrap_err();
    assert!(err.to_string().contains("still wrong"), "{err}");
    assert_eq!(ledger.sample_count(), 2);
}
