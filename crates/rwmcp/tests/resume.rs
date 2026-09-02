//! A plan that failed part-way resumes without re-sending anything the ledger already holds.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use app::domain::World as AppWorld;
use app::{router, AppState};
use async_trait::async_trait;
use rwmcp::effectors::{ApiEffector, EffectError, Effector};
use rwmcp::events::EventBus;
use rwmcp::intent::Intent;
use rwmcp::ledger::Recorder;
use rwmcp::plan::Node;
use rwmcp::{compile, world_from, CompileOptions, Ledger, Scheduler, Status};
use serde_json::{json, Map, Value};
use tokio::sync::broadcast;

async fn serve() -> String {
    let (tx, _) = broadcast::channel(1024);
    let state = Arc::new(AppState { world: Mutex::new(AppWorld::seeded(3)), events: tx });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    format!("http://{addr}")
}

/// The API door, except one operation fails fatally while the switch is on.
struct Breaker {
    inner: ApiEffector,
    op: &'static str,
    broken: Arc<AtomicBool>,
}

#[async_trait]
impl Effector for Breaker {
    fn surface(&self) -> &str {
        "api"
    }
    async fn execute(&self, node: &Node, args: &Map<String, Value>) -> Result<Value, EffectError> {
        if node.op == self.op && self.broken.load(Ordering::SeqCst) {
            return Err(EffectError::Fatal("simulated outage".into()));
        }
        self.inner.execute(node, args).await
    }
}

fn wants() -> Vec<String> {
    let names = ["Acme", "Globex", "Initech"];
    let mut w = Vec::new();
    for n in names {
        w.push(format!("invoice(customer=customer(name='{n}')).exists"));
        w.push(format!("invoice(customer=customer(name='{n}')).status='sent'"));
    }
    let refs: Vec<String> = names.iter().map(|n| format!("invoice(customer=customer(name='{n}'))")).collect();
    w.push(format!("report(invoices=[{}]).exists", refs.join(",")));
    w
}

#[tokio::test]
async fn the_report_fails_then_the_plan_resumes_without_a_double_send() {
    let base = serve().await;
    let world = Arc::new(world_from(&base).await.unwrap());
    let plan = compile(
        &Intent { goal: "t3".into(), wants: wants(), ..Default::default() },
        &world,
        &CompileOptions { plan_id: "resume".into(), surfaces: vec!["api".into()] },
    )
    .unwrap();
    let broken = Arc::new(AtomicBool::new(true));
    let mut effectors: HashMap<String, Arc<dyn Effector>> = HashMap::new();
    effectors.insert("api".into(), Arc::new(Breaker { inner: ApiEffector::new(&base, world.clone()), op: "createReport", broken: broken.clone() }));
    let bus = EventBus::connect(&base).await.unwrap();
    let sched =
        Scheduler { effectors, bus: Some(bus), pools: Default::default(), policy: Default::default(), recorder: Recorder::new(world.clone()), progress: None };

    let mut ledger = Ledger::new();
    let first = sched.run(&plan, &mut ledger).await;
    assert_eq!(first.status, Status::Error, "{:?}", first.error);
    let sends_before = ledger.rows.iter().filter(|r| r.op == "sendInvoice" && r.ok).count();
    assert_eq!(sends_before, 3);
    let rows_before = ledger.rows.len();
    let keys_before: Vec<Option<String>> = plan.nodes.iter().map(|n| n.key.clone()).collect();

    // The outage ends. Resume the same plan against the same ledger.
    broken.store(false, Ordering::SeqCst);
    let done = ledger.completed(&plan);
    assert_eq!(done.len(), 6, "three creates and three sends are proven done by their keys");
    let second = sched.resume(&plan, &mut ledger, &done).await;
    assert_eq!(second.status, Status::Committed, "{:?}", second.error);

    let new_rows: Vec<_> = ledger.rows[rows_before..].to_vec();
    let new_writes: Vec<&str> = new_rows.iter().filter(|r| r.write).map(|r| r.op.as_str()).collect();
    assert_eq!(new_writes, vec!["createReport"], "exactly one new write");
    assert!(new_rows.iter().all(|r| r.op != "sendInvoice" && r.op != "createInvoice"), "nothing was re-sent");
    assert_eq!(plan.nodes.iter().map(|n| n.key.clone()).collect::<Vec<_>>(), keys_before, "unchanged lanes keep their keys");

    let effects: Value = reqwest::get(format!("{base}/oracle/effects")).await.unwrap().json().await.unwrap();
    assert_eq!(effects["double_sends"], 0);
    let state: Value = reqwest::get(format!("{base}/oracle/state")).await.unwrap().json().await.unwrap();
    assert_eq!(state["reports"].as_array().unwrap().len(), 1);
    assert_eq!(state["outbox"].as_array().unwrap().len(), 3);
    let _ = json!(null);
}
