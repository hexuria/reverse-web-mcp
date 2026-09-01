//! The app is the measurement instrument. These tests pin the behaviours every arm relies on.

use std::sync::{Arc, Mutex};

use app::domain::World;
use app::{router, AppState};
use serde_json::{json, Value};
use tokio::sync::broadcast;

async fn serve(seed: u64) -> (String, reqwest::Client) {
    let (tx, _) = broadcast::channel(1024);
    let state = Arc::new(AppState { world: Mutex::new(World::seeded(seed)), events: tx });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    (format!("http://{addr}"), reqwest::Client::new())
}

async fn create_invoice(c: &reqwest::Client, base: &str, key: Option<&str>) -> (u16, Value) {
    let mut req = c.post(format!("{base}/api/invoices")).json(&json!({"customer_id": 1, "amount_cents": 500}));
    if let Some(k) = key {
        req = req.header("idempotency-key", k);
    }
    let r = req.send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

async fn send(c: &reqwest::Client, base: &str, id: u64, key: Option<&str>) -> (u16, Value) {
    let mut req = c.post(format!("{base}/api/invoices/{id}/send"));
    if let Some(k) = key {
        req = req.header("idempotency-key", k);
    }
    let r = req.send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

async fn effects(c: &reqwest::Client, base: &str) -> Value {
    c.get(format!("{base}/oracle/effects")).send().await.unwrap().json().await.unwrap()
}

#[tokio::test]
async fn a_keyed_write_replays_instead_of_repeating() {
    let (base, c) = serve(1).await;
    let (s1, first) = create_invoice(&c, &base, Some("k1")).await;
    let (s2, second) = create_invoice(&c, &base, Some("k1")).await;
    assert_eq!((s1, s2), (200, 200));
    assert_eq!(first, second, "the replay must return the identical body");
    let eff = effects(&c, &base).await;
    let rows = eff["effects"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["replayed"], true);
    assert_eq!(eff["double_sends"], 0);
}

#[tokio::test]
async fn two_unkeyed_sends_are_one_double_send() {
    let (base, c) = serve(1).await;
    let (_, inv) = create_invoice(&c, &base, None).await;
    let id = inv["id"].as_u64().unwrap();
    assert_eq!(send(&c, &base, id, None).await.0, 200);
    assert_eq!(send(&c, &base, id, None).await.0, 200);
    assert_eq!(effects(&c, &base).await["double_sends"], 1);
}

#[tokio::test]
async fn approve_is_refused_without_the_page_header() {
    let (base, c) = serve(1).await;
    let r = c.post(format!("{base}/ui/approve/1")).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 403);
    let (_, inv) = create_invoice(&c, &base, None).await;
    let id = inv["id"].as_u64().unwrap();
    let r = c.post(format!("{base}/ui/approve/{id}")).header("x-ui", "1").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["approved"], true);
}

#[tokio::test]
async fn send_needs_approval_when_chaos_says_so() {
    let (base, c) = serve(1).await;
    c.post(format!("{base}/oracle/chaos")).json(&json!({"require_approval": true})).send().await.unwrap();
    let (_, inv) = create_invoice(&c, &base, None).await;
    let id = inv["id"].as_u64().unwrap();
    let (status, body) = send(&c, &base, id, None).await;
    assert_eq!(status, 409, "{body}");
    c.post(format!("{base}/ui/approve/{id}")).header("x-ui", "1").send().await.unwrap();
    assert_eq!(send(&c, &base, id, None).await.0, 200);
}

#[tokio::test]
async fn the_mcp_door_reports_errors_with_their_status() {
    let (base, c) = serve(1).await;
    let (_, inv) = create_invoice(&c, &base, None).await;
    let id = inv["id"].as_u64().unwrap();
    let req = json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"sendReceipt","arguments":{"id":id}}});
    let v: Value = c.post(format!("{base}/mcp")).json(&req).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["result"]["isError"], true);
    assert_eq!(v["result"]["_status"], 409);
    let req = json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"sendInvoice","arguments":{"id":id,"idempotency_key":"s1"}}});
    let v: Value = c.post(format!("{base}/mcp")).json(&req).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["result"]["isError"], false);
    assert_eq!(v["result"]["structuredContent"]["status"], "sent");
}

#[tokio::test]
async fn reset_reseeds_and_seed_six_has_two_acmes() {
    let (base, c) = serve(1).await;
    create_invoice(&c, &base, None).await;
    c.post(format!("{base}/oracle/reset?seed=6")).send().await.unwrap();
    let state: Value = c.get(format!("{base}/oracle/state")).send().await.unwrap().json().await.unwrap();
    assert_eq!(state["invoices"].as_array().unwrap().len(), 0);
    let acmes = state["customers"].as_array().unwrap().iter().filter(|x| x["name"] == "Acme").count();
    assert_eq!(acmes, 2);
}

#[tokio::test]
async fn a_rate_limited_write_says_when_to_come_back() {
    let (base, c) = serve(1).await;
    c.post(format!("{base}/oracle/chaos")).json(&json!({"rate_limit_per_sec": 1})).send().await.unwrap();
    assert_eq!(create_invoice(&c, &base, None).await.0, 200);
    let r = c.post(format!("{base}/api/invoices")).json(&json!({"customer_id": 1, "amount_cents": 1})).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 429);
    let after = r.headers().get("retry-after-ms").unwrap().to_str().unwrap().parse::<u64>().unwrap();
    assert!((1..=1000).contains(&after), "{after}");
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["retry_after_ms"].as_u64().unwrap(), after);
}
