//! Arm D against an in-process target app: compile the T2 and T3 want sets, run the scheduler,
//! and check width, joins and double-sends from the app's own oracle.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use app::domain::World as AppWorld;
use app::{router, AppState};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use zerohuman::events::EventBus;
use zerohuman::intent::Intent;
use zerohuman::ledger::Recorder;
use zerohuman::{compile, default_effectors, world_from, CompileOptions, Ledger, Scheduler, Status};

async fn serve(seed: u64) -> String {
    let (tx, _) = broadcast::channel(1024);
    let state = Arc::new(AppState { world: Mutex::new(AppWorld::seeded(seed)), events: tx });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    let base = format!("http://{addr}");
    // Real network latency, so effects genuinely overlap instead of finishing in microseconds.
    reqwest::Client::new().post(format!("{base}/oracle/chaos")).json(&json!({"latency_ms": 25})).send().await.unwrap();
    base
}

fn wants(names: &[&str], report: bool) -> Vec<String> {
    let mut w = Vec::new();
    for n in names {
        w.push(format!("invoice(customer=customer(name='{n}')).exists"));
        w.push(format!("invoice(customer=customer(name='{n}')).status='sent'"));
    }
    if report {
        let refs: Vec<String> = names.iter().map(|n| format!("invoice(customer=customer(name='{n}'))")).collect();
        w.push(format!("report(invoices=[{}]).exists", refs.join(",")));
    }
    w
}

async fn run(base: &str, wants: Vec<String>) -> (zerohuman::Receipt, Value) {
    let world = Arc::new(world_from(base).await.unwrap());
    let intent = Intent { goal: "test".into(), wants, ..Default::default() };
    let opts = CompileOptions { plan_id: "e2e".into(), surfaces: vec!["api".into()] };
    let plan = compile(&intent, &world, &opts).unwrap();
    let bus = EventBus::connect(base).await.unwrap();
    let sched = Scheduler {
        effectors: default_effectors(base, world.clone(), &opts.surfaces),
        bus: Some(bus),
        pools: Default::default(),
        policy: Default::default(),
        recorder: Recorder::new(world.clone()),
    };
    let mut ledger = Ledger::new();
    let outcome = sched.run(&plan, &mut ledger).await;
    let receipt = ledger.receipt(&plan, outcome.status, outcome.yield_reason, outcome.evidence, outcome.error);
    let effects: Value = reqwest::get(format!("{base}/oracle/effects")).await.unwrap().json().await.unwrap();
    (receipt, effects)
}

const TEN: [&str; 10] = ["Acme", "Globex", "Initech", "Umbrella", "Hooli", "Vandelay", "Stark", "Wayne", "Wonka", "Tyrell"];

#[tokio::test]
async fn t2_runs_ten_wide_with_one_key_per_write() {
    let base = serve(2).await;
    let (r, effects) = run(&base, wants(&TEN, false)).await;
    assert_eq!(r.status, Status::Committed, "{:?}", r.error);
    assert!(r.max_parallel >= 10, "max_parallel {}", r.max_parallel);
    assert_eq!(r.samples, 0);
    assert_eq!(effects["double_sends"], 0);
    let keyed = effects["effects"].as_array().unwrap().iter().filter(|e| e["key"].is_string()).count();
    assert_eq!(keyed, 20, "ten creates and ten sends, each keyed");
    let state: Value = reqwest::get(format!("{base}/oracle/state")).await.unwrap().json().await.unwrap();
    assert!(state["invoices"].as_array().unwrap().iter().all(|i| i["status"] == "sent"));
}

#[tokio::test]
async fn t3_joins_on_the_report_and_starts_emails_early() {
    let base = serve(3).await;
    let (r, effects) = run(&base, wants(&TEN[..3], true)).await;
    assert_eq!(r.status, Status::Committed, "{:?}", r.error);
    assert!(r.max_parallel >= 3, "max_parallel {}", r.max_parallel);
    assert_eq!(effects["double_sends"], 0);
    // The report row starts after every send row has ended.
    let rows = &r.ledger.rows;
    let report = rows.iter().find(|x| x.op == "createReport").unwrap();
    let last_send_end = rows.iter().filter(|x| x.op == "sendInvoice").map(|x| x.ended_us).max().unwrap();
    assert!(report.started_us >= last_send_end, "the join held");
    // Each send starts before the last create ends: emails do not wait for all three invoices.
    let first_send_start = rows.iter().filter(|x| x.op == "sendInvoice").map(|x| x.started_us).min().unwrap();
    let last_create_end = rows.iter().filter(|x| x.op == "createInvoice").map(|x| x.ended_us).max().unwrap();
    assert!(first_send_start <= last_create_end + 2000, "a send waited for a create it did not depend on");
    let state: Value = reqwest::get(format!("{base}/oracle/state")).await.unwrap().json().await.unwrap();
    assert_eq!(state["reports"].as_array().unwrap().len(), 1);
    let _: HashMap<String, Value> = HashMap::new();
}

#[tokio::test]
async fn t6_two_acmes_yield_once_without_writing() {
    let base = serve(6).await;
    let (r, effects) = run(&base, wants(&["Acme"], false)).await;
    assert_eq!(r.status, Status::NeedThink);
    assert_eq!(r.forks_taken, 1);
    assert!(r.yield_reason.as_deref().unwrap_or("").contains("Acme"));
    let writes = effects["effects"].as_array().unwrap().len();
    assert_eq!(writes, 0, "nothing was written before the question");
}
