//! The CLI is the whole product surface for someone who will never open a Rust file, so the
//! paths that need no model are tested end to end against a live app.

use std::process::Command;
use std::sync::{Arc, Mutex};

use app::domain::World as AppWorld;
use app::{router, AppState};
use serde_json::Value;
use tokio::sync::broadcast;

async fn serve(seed: u64) -> String {
    let (tx, _) = broadcast::channel(1024);
    let state = Arc::new(AppState { world: Mutex::new(AppWorld::seeded(seed)), events: tx });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    format!("http://{addr}")
}

fn rwmcp(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_rwmcp")).args(args).output().expect("run rwmcp");
    (out.status.success(), String::from_utf8_lossy(&out.stdout).to_string(), String::from_utf8_lossy(&out.stderr).to_string())
}

fn wants_file(name: &str, body: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("rwmcp-{}-{name}.wants", std::process::id()));
    std::fs::write(&p, body).unwrap();
    p
}

#[tokio::test]
async fn world_reports_what_the_app_can_be_asked_for() {
    let base = serve(2).await;
    let (ok, out, err) = rwmcp(&["world", "--app", &base]);
    assert!(ok, "{err}");
    assert!(out.contains("invoice(id=$id).status='sent'"), "{out}");
    assert!(out.contains("[ui only: approveInvoice]"), "{out}");
    assert!(out.contains("no postcondition"), "unplannable operations are named: {out}");
}

#[tokio::test]
async fn a_wants_file_plans_and_runs_with_no_model_call() {
    let base = serve(2).await;
    let w = wants_file(
        "good",
        "# an agent wrote this\ninvoice(customer=each(customer())).exists\ninvoice(customer=each(customer())).status='sent'\nreport(invoices=[all(invoice(customer=each(customer())))]).exists\n",
    );
    let w = w.to_str().unwrap();

    let (ok, out, err) = rwmcp(&["check", "--app", &base, "--wants", w]);
    assert!(ok, "{err}");
    assert!(out.contains("3 wants check out"), "{out}");

    let (ok, out, err) = rwmcp(&["plan", "--app", &base, "--wants", w]);
    assert!(ok, "{err}");
    assert!(out.contains("31 steps, 4 deep"), "{out}");
    assert!(out.contains("10 steps leave the system (email, money): sendInvoice"), "the op is named once, not ten times: {out}");
    assert!(!out.contains("model call"), "a wants file costs nothing: {out}");

    // An effectful plan refuses to run unnoticed.
    let (ok, _, err) = rwmcp(&["run", "--app", &base, "--wants", w]);
    assert!(!ok);
    assert!(err.contains("Re-run with --yes"), "{err}");

    // And nothing happened.
    let state: Value = reqwest::get(format!("{base}/oracle/state")).await.unwrap().json().await.unwrap();
    assert_eq!(state["invoices"].as_array().unwrap().len(), 0);

    let (ok, out, err) = rwmcp(&["run", "--app", &base, "--wants", w, "--yes"]);
    assert!(ok, "{err}");
    assert!(out.contains("Committed"), "{out}");
    assert!(out.contains("0 model calls"), "{out}");
    let state: Value = reqwest::get(format!("{base}/oracle/state")).await.unwrap().json().await.unwrap();
    assert_eq!(state["invoices"].as_array().unwrap().len(), 10);
    assert!(state["invoices"].as_array().unwrap().iter().all(|i| i["status"] == "sent"));
    assert_eq!(state["reports"].as_array().unwrap().len(), 1);
    let effects: Value = reqwest::get(format!("{base}/oracle/effects")).await.unwrap().json().await.unwrap();
    assert_eq!(effects["double_sends"], 0);
}

#[tokio::test]
async fn a_dry_run_stops_before_the_first_effect() {
    let base = serve(2).await;
    let w = wants_file("dry", "invoice(customer=customer(name='Acme')).status='sent'\n");
    let (ok, out, err) = rwmcp(&["run", "--app", &base, "--wants", w.to_str().unwrap(), "--dry-run"]);
    assert!(ok, "{err}");
    assert!(out.contains("stopping before the first effect"), "{out}");
    let state: Value = reqwest::get(format!("{base}/oracle/state")).await.unwrap().json().await.unwrap();
    assert_eq!(state["invoices"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn a_bad_want_is_explained_and_nothing_is_planned() {
    let base = serve(2).await;
    let w = wants_file("bad", "widget(name='x').exists\ninvoice(customer=customer(name=$who)).exists\n");
    let (ok, _, err) = rwmcp(&["check", "--app", &base, "--wants", w.to_str().unwrap()]);
    assert!(!ok);
    assert!(err.contains("unknown entity 'widget'"), "{err}");
    assert!(err.contains("uses a variable"), "{err}");
}

#[tokio::test]
async fn an_openapi_file_can_be_inspected_without_a_running_app() {
    let doc = concat!(env!("CARGO_MANIFEST_DIR"), "/../app/static/openapi.json");
    let (ok, out, err) = rwmcp(&["world", "--app", doc]);
    assert!(ok, "{err}");
    assert!(out.contains("can make true"), "{out}");
    // But running against a file is refused rather than half-attempted.
    let w = wants_file("file", "invoice(customer=customer(name='Acme')).exists\n");
    let (ok, _, err) = rwmcp(&["run", "--app", doc, "--wants", w.to_str().unwrap(), "--yes"]);
    assert!(!ok);
    assert!(err.contains("give me the app's URL"), "{err}");
}
