//! Arm C end to end: a scripted model drives the page's WebMCP tools in a headless browser.

use std::sync::{Arc, Mutex};

use app::domain::World as AppWorld;
use app::{router, AppState};
use async_trait::async_trait;
use bench::arms::ArmContext;
use bench::loops::run_webmcp_loop;
use bench::planner::Sampler;
use bench::tasks::Task;
use driver::{find_chrome, BrowserPool};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use zerohuman::events::EventBus;
use zerohuman::ledger::{Ledger, Sample, SampleKind};
use zerohuman::Status;

/// Looks up Acme, creates an invoice from the returned id, sends it, says done.
struct Script(Mutex<u32>);

fn last_tool_result(body: &Value) -> Value {
    let msgs = body["messages"].as_array().unwrap();
    let last = msgs.last().unwrap();
    let text = last["content"][0]["content"].as_str().unwrap_or("null");
    serde_json::from_str(text).unwrap_or(Value::Null)
}

#[async_trait]
impl Sampler for Script {
    async fn sample(&self, ledger: &mut Ledger, kind: SampleKind, body: Value) -> anyhow::Result<Value> {
        ledger.record_sample(Sample { seq: 0, kind, started_us: 0, ended_us: 1, tokens_in: 1, tokens_out: 1, model: "stub".into(), effort: "low".into() });
        let mut turn = self.0.lock().unwrap();
        *turn += 1;
        let tool =
            |id: &str, name: &str, input: Value| json!({"stop_reason": "tool_use", "content": [{"type": "tool_use", "id": id, "name": name, "input": input}]});
        Ok(match *turn {
            1 => tool("t1", "listCustomers", json!({"name": "Acme"})),
            2 => {
                let cid = last_tool_result(&body)[0]["id"].as_u64().expect("customer id from the page");
                tool("t2", "createInvoice", json!({"customer_id": cid, "amount_cents": 10000, "idempotency_key": "c-acme"}))
            }
            3 => {
                let id = last_tool_result(&body)["id"].as_u64().expect("invoice id from the page");
                tool("t3", "sendInvoice", json!({"id": id, "idempotency_key": "s-acme"}))
            }
            _ => json!({"stop_reason": "end_turn", "content": [{"type": "text", "text": "done"}]}),
        })
    }
}

#[tokio::test]
async fn the_webmcp_loop_runs_in_a_real_page() {
    let Some(chrome) = find_chrome() else {
        eprintln!("no chrome found; skipping");
        return;
    };
    let (tx, _) = broadcast::channel(1024);
    let state = Arc::new(AppState { world: Mutex::new(AppWorld::seeded(1)), events: tx });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    let base = format!("http://{addr}");
    let task: Task = toml::from_str("id = \"T1\"\ntitle = \"t\"\nseed = 1\ngoal = \"Create an invoice for Acme and send it.\"\n").unwrap();
    let world = Arc::new(zerohuman::world_from(&base).await.unwrap());
    let pool = BrowserPool::launch(2, true, Some(&chrome)).await.unwrap();
    let ctx = ArmContext {
        base: base.clone(),
        world,
        bus: EventBus::connect(&base).await.unwrap(),
        surfaces: vec!["api".into()],
        run_id: "r1".into(),
        browser: Some(pool.clone()),
    };

    let r = run_webmcp_loop(&task, &ctx, &Script(Mutex::new(0)), "  customers (10): Acme, ...").await.unwrap();
    assert_eq!(r.status, Status::Committed, "{:?}", r.error);
    assert_eq!(r.samples, 4);
    assert!(r.ledger.rows.iter().all(|row| row.surface == "webmcp"), "every effect went through the page");
    let state: Value = reqwest::get(format!("{base}/oracle/state")).await.unwrap().json().await.unwrap();
    assert_eq!(state["invoices"][0]["status"], "sent");
    let effects: Value = reqwest::get(format!("{base}/oracle/effects")).await.unwrap().json().await.unwrap();
    assert!(effects["effects"].as_array().unwrap().iter().all(|e| e["door"] == "webmcp"), "the app saw the webmcp door");
    pool.close().await.unwrap();
}
