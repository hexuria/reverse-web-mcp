//! Arm A's mechanics with a scripted model: the model only ever sees a screenshot, its action is
//! executed through CDP, and each action is a pixels row.

use std::sync::{Arc, Mutex};

use app::domain::World as AppWorld;
use app::{router, AppState};
use async_trait::async_trait;
use bench::arms::ArmContext;
use bench::loops::run_cua_loop;
use bench::planner::Sampler;
use bench::tasks::Task;
use driver::{find_chrome, BrowserPool};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use zerohuman::events::EventBus;
use zerohuman::ledger::{Ledger, Sample, SampleKind};
use zerohuman::Status;

struct Clicks(Mutex<u32>);

#[async_trait]
impl Sampler for Clicks {
    async fn sample(&self, ledger: &mut Ledger, kind: SampleKind, body: Value) -> anyhow::Result<Value> {
        ledger.record_sample(Sample { seq: 0, kind, started_us: 0, ended_us: 1, tokens_in: 1, tokens_out: 1, model: "stub".into(), effort: "low".into() });
        let last = body["messages"].as_array().unwrap().last().unwrap().clone();
        let has_image = last["content"].as_array().unwrap().iter().any(|b| b["type"] == "image" && b["source"]["media_type"] == "image/png");
        assert!(has_image, "every turn shows the model a screenshot");
        assert!(body["tools"][0]["name"] == "computer");
        let mut n = self.0.lock().unwrap();
        *n += 1;
        let act = |id: &str, input: Value| json!({"stop_reason": "tool_use", "content": [{"type": "tool_use", "id": id, "name": "computer", "input": input}]});
        Ok(match *n {
            1 => act("a1", json!({"action": "click", "x": 640, "y": 400})),
            2 => act("a2", json!({"action": "key", "key": "Tab"})),
            3 => act("a3", json!({"action": "type", "text": "hello"})),
            _ => act("a4", json!({"action": "done", "text": "done"})),
        })
    }
}

#[tokio::test]
async fn the_pixel_loop_sees_only_screenshots_and_records_pixel_rows() {
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
    let pool = BrowserPool::launch(1, true, Some(&chrome)).await.unwrap();
    let ctx = ArmContext {
        base: base.clone(),
        world,
        bus: EventBus::connect(&base).await.unwrap(),
        surfaces: vec!["api".into()],
        run_id: "r1".into(),
        browser: Some(pool.clone()),
        shots_dir: None,
    };

    let r = run_cua_loop(&task, &ctx, &Clicks(Mutex::new(0)), "  customers (10): Acme, ...").await.unwrap();
    assert_eq!(r.status, Status::Committed, "{:?}", r.error);
    assert_eq!(r.samples, 4);
    let ops: Vec<&str> = r.ledger.rows.iter().map(|x| x.op.as_str()).collect();
    assert_eq!(ops, vec!["cua.click", "cua.key", "cua.type", "cua.done"]);
    assert!(r.ledger.rows.iter().all(|x| x.surface == "pixels" && !x.write));
    assert_eq!(r.max_parallel_by_surface.get("pixels"), Some(&1));
    pool.close().await.unwrap();
}
