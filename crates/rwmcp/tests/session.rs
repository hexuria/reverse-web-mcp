//! The façade, exercised the way an embedder would: connect, plan, run.

use std::sync::{Arc, Mutex};

use app::domain::World as AppWorld;
use app::{router, AppState};
use rwmcp::{Intent, Session, Status};
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

fn wants(v: &[&str]) -> Intent {
    Intent { goal: "t".into(), wants: v.iter().map(|s| s.to_string()).collect(), ..Default::default() }
}

/// The three lines the docs promise, with nothing else assembled by hand.
#[tokio::test]
async fn connect_plan_run() {
    let base = serve(2).await;
    let app = Session::connect(&base).await.unwrap();

    let plan = app.plan(&wants(&["invoice(customer=customer(name='Acme')).status='sent'"])).unwrap();
    assert_eq!(plan.nodes.len(), 3);

    let receipt = app.run(&plan).await.unwrap();
    assert_eq!(receipt.status, Status::Committed);
    assert_eq!(receipt.samples, 0, "no model is involved in running a plan");
}

/// Wants that do not hold up come back as wants, not as a string.
#[tokio::test]
async fn bad_wants_come_back_typed() {
    let base = serve(2).await;
    let app = Session::connect(&base).await.unwrap();
    let err = app.plan(&wants(&["widget(name='x').exists"])).unwrap_err();
    match err {
        rwmcp::PlanError::Wants(errs) => assert_eq!(errs[0].code(), "unknown_entity"),
        other => panic!("expected lint errors, got {other}"),
    }
}

/// Two sessions under one plan id share their committed work, which is what makes a crashed run
/// safe to start again.
#[tokio::test]
async fn the_same_plan_id_does_not_repeat_committed_work() {
    let base = serve(2).await;
    let intent = wants(&["invoice(customer=customer(name='Acme')).status='sent'"]);

    let first = Session::connect(&base).await.unwrap().plan_id("shared");
    let plan = first.plan(&intent).unwrap();
    assert_eq!(first.run(&plan).await.unwrap().status, Status::Committed);

    let second = Session::connect(&base).await.unwrap().plan_id("shared");
    assert_eq!(second.run(&second.plan(&intent).unwrap()).await.unwrap().status, Status::Committed);

    let state: serde_json::Value = reqwest::get(format!("{base}/oracle/state")).await.unwrap().json().await.unwrap();
    assert_eq!(state["invoices"].as_array().unwrap().len(), 1, "the second run matched the first run's keys");

    // A different id is a different run, and does the work again.
    let other = Session::connect(&base).await.unwrap().plan_id("elsewhere");
    assert_eq!(other.run(&other.plan(&intent).unwrap()).await.unwrap().status, Status::Committed);
    let state: serde_json::Value = reqwest::get(format!("{base}/oracle/state")).await.unwrap().json().await.unwrap();
    assert_eq!(state["invoices"].as_array().unwrap().len(), 2);
}

/// A world model with no app behind it plans fine and refuses to run.
#[tokio::test]
async fn an_offline_session_plans_but_will_not_run() {
    let doc: serde_json::Value = serde_json::from_str(include_str!("../../app/static/openapi.json")).unwrap();
    let app = Session::offline(rwmcp::World::from_openapi(&doc).unwrap());
    let plan = app.plan(&wants(&["invoice(customer=customer(name='Acme')).exists"])).unwrap();
    assert_eq!(plan.nodes.len(), 2);
    let err = app.run(&plan).await.unwrap_err();
    assert!(err.to_string().contains("nothing to run against"), "{err}");
}
