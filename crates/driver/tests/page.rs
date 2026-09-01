//! The driver against the in-process target app: the page's WebMCP shim is reachable and the
//! Approve button can be clicked by its accessible name. Needs a Chrome or Chromium on this machine.

use std::sync::{Arc, Mutex};

use app::domain::World as AppWorld;
use app::{router, AppState};
use driver::BrowserPool;
use serde_json::{json, Value};
use tokio::sync::broadcast;

async fn serve() -> String {
    let (tx, _) = broadcast::channel(1024);
    let state = Arc::new(AppState { world: Mutex::new(AppWorld::seeded(1)), events: tx });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    format!("http://{addr}")
}

fn chrome() -> Option<String> {
    std::env::var("CHIFFON_CHROME").ok().or_else(|| {
        ["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome", "/usr/bin/chromium", "/usr/bin/chromium-browser", "/usr/bin/google-chrome"]
            .iter()
            .find(|p| std::path::Path::new(p).exists())
            .map(|p| p.to_string())
    })
}

#[tokio::test]
async fn webmcp_tools_and_the_approve_button() {
    let Some(chrome) = chrome() else {
        eprintln!("no chrome found; skipping");
        return;
    };
    let base = serve().await;
    let c = reqwest::Client::new();
    let inv: Value = c.post(format!("{base}/api/invoices")).json(&json!({"customer_id": 1, "amount_cents": 500})).send().await.unwrap().json().await.unwrap();
    let id = inv["id"].as_u64().unwrap();

    let pool = BrowserPool::launch(2, true, Some(&chrome)).await.unwrap();
    let page = pool.lease().await.unwrap();
    page.goto(&base).await.unwrap();
    let tools = page.eval("window.__webmcp.list().length").await.unwrap();
    assert_eq!(tools, json!(6));

    // Give the page a moment to render the invoice row, then click its Approve button.
    let mut clicked = false;
    for _ in 0..20 {
        if page.click_by_name("button", &format!("Approve invoice {id}")).await.is_ok() {
            clicked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(clicked, "the Approve button was found by its accessible name");
    let mut approved = false;
    for _ in 0..20 {
        let s: Value = c.get(format!("{base}/oracle/state")).send().await.unwrap().json().await.unwrap();
        if s["invoices"][0]["approved"] == true {
            approved = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(approved, "the click reached the UI-only approve endpoint");

    // Two leases at once, the third waits: the pool is the screen budget.
    let second = pool.lease().await.unwrap();
    let third = tokio::time::timeout(std::time::Duration::from_millis(200), pool.lease()).await;
    assert!(third.is_err(), "no third page until one is returned");
    drop(second);
    drop(page);
    pool.close().await.unwrap();
}
