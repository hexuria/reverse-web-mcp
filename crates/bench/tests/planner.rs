//! The lint loop with a stub sampler: a wrong intent costs one extra sample, then compiles.

use std::sync::Mutex;

use async_trait::async_trait;
use bench::tasks::Task;
use rwmcp::ledger::{Ledger, Sample, SampleKind};
use rwmcp::planner::{plan_with_lint, Sampler};
use rwmcp::{CompileOptions, World};
use serde_json::{json, Value};

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
    let (w, opts) = (world(), CompileOptions::default());
    let ctx = rwmcp::planner::Ctx::new(&w, &opts).facts("  customers (1): Acme");
    let intent = plan_with_lint(&rwmcp::planner::Ask::with(&task().goal, &task().constraints), &ctx, &sampler, &mut ledger).await.unwrap();
    assert_eq!(intent.wants.len(), 2);
    assert_eq!(ledger.sample_count(), 2);
    assert_eq!(ledger.samples[0].kind, SampleKind::Plan);
    assert_eq!(ledger.samples[1].kind, SampleKind::Lint);
}

#[tokio::test]
async fn a_good_first_intent_costs_one_sample() {
    let sampler = Scripted(Mutex::new(vec![vec!["invoice(customer=customer(name='Acme')).status='sent'"]]));
    let mut ledger = Ledger::new();
    plan_with_lint(
        &rwmcp::planner::Ask::with(&task().goal, &task().constraints),
        &rwmcp::planner::Ctx::new(&world(), &CompileOptions::default()),
        &sampler,
        &mut ledger,
    )
    .await
    .unwrap();
    assert_eq!(ledger.sample_count(), 1);
}

#[tokio::test]
async fn two_bad_intents_then_a_good_one_is_three_samples() {
    let sampler =
        Scripted(Mutex::new(vec![vec!["nonsense("], vec!["customer(name='Acme').exists"], vec!["invoice(customer=customer(name='Acme')).status='sent'"]]));
    let mut ledger = Ledger::new();
    let intent = plan_with_lint(
        &rwmcp::planner::Ask::with(&task().goal, &task().constraints),
        &rwmcp::planner::Ctx::new(&world(), &CompileOptions::default()),
        &sampler,
        &mut ledger,
    )
    .await
    .unwrap();
    assert_eq!(intent.wants.len(), 1);
    assert_eq!(ledger.sample_count(), 3);
    assert_eq!(ledger.notes.len(), 2, "both rejections are on record");
}

#[tokio::test]
async fn three_bad_intents_is_an_error_not_a_fourth_sample() {
    let sampler = Scripted(Mutex::new(vec![vec!["nonsense("], vec!["customer(name='Acme').exists"], vec![], vec!["should not be asked"]]));
    let mut ledger = Ledger::new();
    let err = plan_with_lint(
        &rwmcp::planner::Ask::with(&task().goal, &task().constraints),
        &rwmcp::planner::Ctx::new(&world(), &CompileOptions::default()),
        &sampler,
        &mut ledger,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("two re-asks"), "{err}");
    assert_eq!(ledger.sample_count(), 3);
}

#[test]
fn long_runs_of_numbered_names_are_summarised() {
    use rwmcp::planner::summarise_names;
    let mut names: Vec<String> = vec!["Acme".into(), "Globex".into()];
    names.extend((1..=300).map(|i| format!("Customer {i:03}")));
    let s = summarise_names(&names);
    assert_eq!(s, "Acme, Globex, and 300 named 'Customer 001' … 'Customer 300' (prefix 'Customer ')");
    assert_eq!(summarise_names(&["Acme".to_string(), "Wayne 7".to_string()]), "Acme, Wayne 7");
}

#[tokio::test]
async fn a_repeat_goal_costs_zero_samples() {
    use rwmcp::planner::{plan_cached, IntentCache};
    let dir = std::env::temp_dir().join(format!("rwmcp-cache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cache = IntentCache::new(&dir);
    let sampler = Scripted(Mutex::new(vec![vec!["invoice(customer=customer(name='Acme')).status='sent'"]]));
    let mut ledger = Ledger::new();
    let (w, opts) = (world(), CompileOptions::default());
    let cctx = rwmcp::planner::Ctx::new(&w, &opts).facts("facts");
    let (first, hit) = plan_cached(&cache, &rwmcp::planner::Ask::with(&task().goal, &task().constraints), &cctx, &sampler, &mut ledger).await.unwrap();
    assert!(!hit);
    assert_eq!(ledger.sample_count(), 1);
    let mut ledger2 = Ledger::new();
    let (second, hit) = plan_cached(&cache, &rwmcp::planner::Ask::with(&task().goal, &task().constraints), &cctx, &sampler, &mut ledger2).await.unwrap();
    assert!(hit);
    assert_eq!(ledger2.sample_count(), 0);
    assert_eq!(first.wants, second.wants);
    // Different facts, different key: a changed world never hits.
    assert!(cache.get(&task().goal, "other facts", &CompileOptions::default().surfaces, &w).is_none());
    assert!(cache.get(&task().goal, "facts", &["api".to_string(), "mcp".to_string()], &w).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_tool_call_rendered_as_text_is_still_an_intent() {
    struct Garbled;
    #[async_trait]
    impl Sampler for Garbled {
        async fn sample(&self, ledger: &mut Ledger, kind: SampleKind, _body: Value) -> anyhow::Result<Value> {
            ledger.record_sample(Sample { seq: 0, kind, started_us: 0, ended_us: 1, tokens_in: 1, tokens_out: 1, model: "stub".into(), effort: "low".into() });
            Ok(
                json!({"content": [{"type": "text", "text": "```0eemit_intentcallwants=[\"invoice(customer=customer(name='Acme')).exists\", \"invoice(customer=customer(name='Acme')).status='sent'\"]"}]}),
            )
        }
    }
    let mut ledger = Ledger::new();
    let intent = plan_with_lint(
        &rwmcp::planner::Ask::with(&task().goal, &task().constraints),
        &rwmcp::planner::Ctx::new(&world(), &CompileOptions::default()),
        &Garbled,
        &mut ledger,
    )
    .await
    .unwrap();
    assert_eq!(intent.wants.len(), 2);
    assert_eq!(ledger.sample_count(), 1);
}

#[tokio::test]
async fn an_unparseable_reply_is_re_asked_not_fatal() {
    struct Junk(Mutex<u32>);
    #[async_trait]
    impl Sampler for Junk {
        async fn sample(&self, ledger: &mut Ledger, kind: SampleKind, _body: Value) -> anyhow::Result<Value> {
            ledger.record_sample(Sample { seq: 0, kind, started_us: 0, ended_us: 1, tokens_in: 1, tokens_out: 1, model: "stub".into(), effort: "low".into() });
            let mut n = self.0.lock().unwrap();
            *n += 1;
            Ok(if *n == 1 {
                json!({"content": [{"type": "text", "text": "Sure! Let me think about that."}]})
            } else {
                json!({"content": [{"type": "tool_use", "name": "emit_intent", "input": {"wants": ["invoice(customer=customer(name='Acme')).status='sent'"]}}]})
            })
        }
    }
    let mut ledger = Ledger::new();
    let intent = plan_with_lint(
        &rwmcp::planner::Ask::with(&task().goal, &task().constraints),
        &rwmcp::planner::Ctx::new(&world(), &CompileOptions::default()),
        &Junk(Mutex::new(0)),
        &mut ledger,
    )
    .await
    .unwrap();
    assert_eq!(intent.wants.len(), 1);
    assert_eq!(ledger.sample_count(), 2);
    assert!(ledger.notes.iter().any(|n| n.get("unparseable").is_some()));
}
