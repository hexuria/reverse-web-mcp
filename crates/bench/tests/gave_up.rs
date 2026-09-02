//! A baseline that neither finishes nor asks is scored as an error, never as a commit.

use std::sync::{Arc, Mutex};

use app::domain::World as AppWorld;
use app::{router, AppState};
use async_trait::async_trait;
use bench::arms::ArmContext;
use bench::loops::run_mcp_loop;
use rwmcp::planner::Sampler;
use bench::tasks::Task;
use rwmcp::events::EventBus;
use rwmcp::ledger::{Ledger, Sample, SampleKind};
use rwmcp::Status;
use serde_json::{json, Value};
use tokio::sync::broadcast;

struct Says(&'static str);

#[async_trait]
impl Sampler for Says {
    async fn sample(&self, ledger: &mut Ledger, kind: SampleKind, body: Value) -> anyhow::Result<Value> {
        let first = body["messages"][0]["content"].as_str().unwrap();
        assert!(first.starts_with("World facts"), "the baseline gets the same facts as the planner: {first}");
        ledger.record_sample(Sample { seq: 0, kind, started_us: 0, ended_us: 1, tokens_in: 1, tokens_out: 1, model: "stub".into(), effort: "low".into() });
        Ok(json!({"stop_reason": "end_turn", "content": [{"type": "text", "text": self.0}]}))
    }
}

async fn ctx() -> ArmContext {
    let (tx, _) = broadcast::channel(1024);
    let state = Arc::new(AppState { world: Mutex::new(AppWorld::seeded(1)), events: tx });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    let base = format!("http://{addr}");
    let world = Arc::new(rwmcp::world_from(&base).await.unwrap());
    ArmContext {
        base: base.clone(),
        world,
        bus: EventBus::connect(&base).await.unwrap(),
        surfaces: vec!["api".into()],
        run_id: "r1".into(),
        browser: None,
        shots_dir: None,
        max_turns: 40,
    }
}

fn task() -> Task {
    toml::from_str("id = \"T1\"\ntitle = \"t\"\nseed = 1\ngoal = \"Create an invoice for Acme and send it.\"\n").unwrap()
}

#[tokio::test]
async fn giving_up_is_an_error() {
    let r = run_mcp_loop(&task(), &ctx().await, &Says("I am not able to help with that."), "  customers (1): Acme", true).await.unwrap();
    assert_eq!(r.status, Status::Error);
    assert!(r.error.as_deref().unwrap_or("").starts_with("gave up"));
    assert_eq!(r.samples, 1);
}

#[tokio::test]
async fn done_is_a_commit_and_a_question_is_a_yield() {
    let r = run_mcp_loop(&task(), &ctx().await, &Says("done"), "", false).await.unwrap();
    assert_eq!(r.status, Status::Committed);
    let r = run_mcp_loop(&task(), &ctx().await, &Says("Which Acme do you mean?"), "", false).await.unwrap();
    assert_eq!(r.status, Status::NeedThink);
    assert_eq!(r.forks_taken, 1);
}
