//! T7 end to end: three invoices, each approved through the page's accessibility name, then sent
//! and reported. The screen pool is one; the API stays wide in the same plan.

use std::sync::{Arc, Mutex};

use app::domain::World as AppWorld;
use app::{router, AppState};
use bench::arms::{run_ours, ArmContext};
use bench::tasks::Task;
use driver::{find_chrome, BrowserPool};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use zerohuman::events::EventBus;
use zerohuman::{Ledger, Status};

#[tokio::test]
async fn approve_is_one_screen_lane_inside_a_wide_api_plan() {
    let Some(chrome) = find_chrome() else {
        eprintln!("no chrome found; skipping");
        return;
    };
    let (tx, _) = broadcast::channel(1024);
    let state = Arc::new(AppState { world: Mutex::new(AppWorld::seeded(7)), events: tx });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    let base = format!("http://{addr}");
    let c = reqwest::Client::new();
    c.post(format!("{base}/oracle/chaos")).json(&json!({"require_approval": true, "latency_ms": 25})).send().await.unwrap();

    let task = Task::load(std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tasks/T7.toml"))).unwrap();
    let world = Arc::new(zerohuman::world_from(&base).await.unwrap());
    let pool = BrowserPool::launch(1, true, Some(&chrome)).await.unwrap();
    let ctx = ArmContext {
        base: base.clone(),
        world,
        bus: EventBus::connect(&base).await.unwrap(),
        surfaces: vec!["api".into(), "a11y".into()],
        run_id: "r1".into(),
        browser: Some(pool.clone()),
        shots_dir: None,
    };
    let receipt = run_ours(&task, &ctx, None, Ledger::new(), None).await.unwrap();
    assert_eq!(receipt.status, Status::Committed, "{:?}\n{}", receipt.error, receipt.plan);
    let by = &receipt.max_parallel_by_surface;
    assert_eq!(by.get("a11y"), Some(&1), "{by:?}");
    assert!(by.get("api").copied().unwrap_or(0) >= 3, "{by:?}");
    let state: Value = c.get(format!("{base}/oracle/state")).send().await.unwrap().json().await.unwrap();
    let invoices = state["invoices"].as_array().unwrap();
    assert_eq!(invoices.len(), 3);
    assert!(invoices.iter().all(|i| i["approved"] == true && i["status"] == "sent"));
    assert_eq!(state["reports"].as_array().unwrap().len(), 1);
    let effects: Value = c.get(format!("{base}/oracle/effects")).send().await.unwrap().json().await.unwrap();
    assert_eq!(effects["double_sends"], 0);
    let approvals = effects["effects"].as_array().unwrap().iter().filter(|e| e["op"] == "approveInvoice" && e["door"] == "ui").count();
    assert_eq!(approvals, 3);
    pool.close().await.unwrap();
}
