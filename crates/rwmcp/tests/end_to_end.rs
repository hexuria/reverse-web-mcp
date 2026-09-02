//! Arm D against an in-process target app: compile the T2 and T3 want sets, run the scheduler,
//! and check width, joins and double-sends from the app's own oracle.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use app::domain::World as AppWorld;
use app::{router, AppState};
use rwmcp::events::EventBus;
use rwmcp::intent::Intent;
use rwmcp::ledger::Recorder;
use rwmcp::{compile, default_effectors, world_from, CompileOptions, Ledger, Scheduler, Status};
use serde_json::{json, Value};
use tokio::sync::broadcast;

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

async fn run(base: &str, wants: Vec<String>) -> (rwmcp::Receipt, Value) {
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
        progress: None,
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
async fn t4_waits_for_the_payment_event_instead_of_polling() {
    let base = serve(4).await;
    // The outside world: pay every invoice 300 ms after it appears.
    let watcher = EventBus::connect(&base).await.unwrap();
    let payer = {
        let base = base.clone();
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            let mut paid = std::collections::HashSet::new();
            loop {
                for ev in watcher.events() {
                    if ev.kind == "invoice.created" && paid.insert(ev.id) {
                        let _ = client.post(format!("{base}/oracle/pay")).json(&json!({"invoice_id": ev.id, "delay_ms": 300})).send().await;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
        })
    };
    let mut w = wants(&["Acme"], false);
    w.push("invoice(customer=customer(name='Acme')).receipt_sent=true".into());
    let (r, effects) = run(&base, w).await;
    payer.abort();
    assert_eq!(r.status, Status::Committed, "{:?}", r.error);
    let wait = r.ledger.rows.iter().find(|x| x.surface == "event").expect("a wait row");
    assert!(wait.ok);
    assert!(wait.ended_us - wait.started_us >= 200_000, "the wait really waited");
    let reads = r.ledger.rows.iter().filter(|x| x.op == "getInvoice").count();
    assert_eq!(reads, 0, "no polling");
    assert_eq!(effects["double_sends"], 0);
    let state: Value = reqwest::get(format!("{base}/oracle/state")).await.unwrap().json().await.unwrap();
    assert_eq!(state["invoices"][0]["receipt_sent"], true);
}

#[tokio::test]
async fn a_lost_webhook_is_caught_by_the_state_check() {
    let base = serve(4).await;
    // The payment lands 300 ms after creation but its event is lost.
    let watcher = EventBus::connect(&base).await.unwrap();
    let payer = {
        let base = base.clone();
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            let mut paid = std::collections::HashSet::new();
            loop {
                for ev in watcher.events() {
                    if ev.kind == "invoice.created" && paid.insert(ev.id) {
                        let _ = client.post(format!("{base}/oracle/pay")).json(&json!({"invoice_id": ev.id, "delay_ms": 300, "silent": true})).send().await;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
        })
    };
    let world = Arc::new(world_from(&base).await.unwrap());
    let mut w = wants(&["Acme"], false);
    w.push("invoice(customer=customer(name='Acme')).receipt_sent=true".into());
    let intent = Intent { goal: "t4 lost".into(), wants: w, ..Default::default() };
    let opts = CompileOptions { plan_id: "lost".into(), surfaces: vec!["api".into()] };
    let plan = compile(&intent, &world, &opts).unwrap();
    let bus = EventBus::connect(&base).await.unwrap();
    let policy = rwmcp::Policy { check_every: std::time::Duration::from_millis(700), ..Default::default() };
    let sched = Scheduler {
        effectors: default_effectors(&base, world.clone(), &opts.surfaces),
        bus: Some(bus),
        pools: Default::default(),
        policy,
        recorder: Recorder::new(world.clone()),
        progress: None,
    };
    let mut ledger = Ledger::new();
    let outcome = sched.run(&plan, &mut ledger).await;
    payer.abort();
    assert_eq!(outcome.status, Status::Committed, "{:?}", outcome.error);
    let wait = ledger.rows.iter().find(|x| x.surface == "event").unwrap();
    assert!(wait.ok);
    assert_eq!(wait.observed["checked"], true, "the fact was confirmed by a read, not an event");
    let glances = ledger.rows.iter().filter(|x| x.op == "getInvoice").count();
    assert!((1..=3).contains(&glances), "a glance or two, not a poll storm: {glances}");
    let state: Value = reqwest::get(format!("{base}/oracle/state")).await.unwrap().json().await.unwrap();
    assert_eq!(state["invoices"][0]["receipt_sent"], true);
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
