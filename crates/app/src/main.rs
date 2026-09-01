//! The target app. Five doors into one `World`:
//!   REST + OpenAPI   /api/*         /openapi.json
//!   MCP (HTTP)       /mcp           JSON-RPC 2.0: initialize, tools/list, tools/call
//!   WebMCP           /  (the UI registers the same ops as page tools)
//!   accessibility    /  (the UI has real roles and names)
//!   pixels           /  (the same UI)
//! plus the oracle:   /oracle/*      /events (SSE)

mod domain;
mod mcp;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use futures::stream::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use domain::{Chaos, DomainError, Event, World};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:47310")]
    bind: SocketAddr,
    #[arg(long, default_value_t = 1)]
    seed: u64,
}

pub struct AppState {
    pub world: Mutex<World>,
    pub events: broadcast::Sender<Event>,
}

pub type Shared = Arc<AppState>;

pub struct ApiError(pub u16, pub String);

impl From<DomainError> for ApiError {
    fn from(e: DomainError) -> Self {
        ApiError(e.status(), e.message())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.0).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(json!({"error": self.1, "status": self.0}))).into_response()
    }
}

fn key_of(headers: &HeaderMap) -> Option<String> {
    headers.get("idempotency-key").and_then(|v| v.to_str().ok()).map(|s| s.to_string())
}

fn door_of(headers: &HeaderMap) -> String {
    headers.get("x-door").and_then(|v| v.to_str().ok()).unwrap_or("api").to_string()
}

/// Every write goes through here: run under the lock, publish events, then sleep the chaos latency
/// outside the lock so latency never serializes other lanes.
pub async fn write<F>(state: &Shared, f: F) -> Result<Value, ApiError>
where
    F: FnOnce(&mut World) -> domain::DomainResult<(Value, u64, Vec<Event>)>,
{
    let (v, lat, evs) = {
        let mut w = state.world.lock().unwrap();
        f(&mut w)?
    };
    for ev in evs {
        let _ = state.events.send(ev);
    }
    if lat > 0 {
        tokio::time::sleep(Duration::from_millis(lat)).await;
    }
    Ok(v)
}

// ---------- REST ----------

#[derive(Deserialize)]
struct NameQuery {
    name: Option<String>,
}

async fn list_customers(State(s): State<Shared>, Query(q): Query<NameQuery>) -> Json<Value> {
    let w = s.world.lock().unwrap();
    Json(json!(w.find_customers(q.name.as_deref())))
}

#[derive(Deserialize)]
struct NewCustomer {
    name: String,
    email: String,
}

async fn create_customer(State(s): State<Shared>, headers: HeaderMap, Json(b): Json<NewCustomer>) -> Result<Json<Value>, ApiError> {
    let key = key_of(&headers);
    let door = door_of(&headers);
    Ok(Json(write(&s, |w| w.create_customer(&door, key.as_deref(), &b.name, &b.email)).await?))
}

#[derive(Deserialize)]
struct InvoiceQuery {
    customer_id: Option<u64>,
}

async fn list_invoices(State(s): State<Shared>, Query(q): Query<InvoiceQuery>) -> Json<Value> {
    let w = s.world.lock().unwrap();
    Json(json!(w.list_invoices(q.customer_id)))
}

async fn get_invoice(State(s): State<Shared>, Path(id): Path<u64>) -> Result<Json<Value>, ApiError> {
    let w = s.world.lock().unwrap();
    Ok(Json(json!(w.invoice(id)?)))
}

#[derive(Deserialize)]
struct NewInvoice {
    customer_id: u64,
    amount_cents: i64,
}

async fn create_invoice(State(s): State<Shared>, headers: HeaderMap, Json(b): Json<NewInvoice>) -> Result<Json<Value>, ApiError> {
    let key = key_of(&headers);
    let door = door_of(&headers);
    Ok(Json(write(&s, |w| w.create_invoice(&door, key.as_deref(), b.customer_id, b.amount_cents)).await?))
}

async fn send_invoice(State(s): State<Shared>, headers: HeaderMap, Path(id): Path<u64>) -> Result<Json<Value>, ApiError> {
    let key = key_of(&headers);
    let door = door_of(&headers);
    Ok(Json(write(&s, |w| w.send_invoice(&door, key.as_deref(), id)).await?))
}

async fn send_receipt(State(s): State<Shared>, headers: HeaderMap, Path(id): Path<u64>) -> Result<Json<Value>, ApiError> {
    let key = key_of(&headers);
    let door = door_of(&headers);
    Ok(Json(write(&s, |w| w.send_receipt(&door, key.as_deref(), id)).await?))
}

#[derive(Deserialize)]
struct NewReport {
    invoice_ids: Vec<u64>,
}

async fn create_report(State(s): State<Shared>, headers: HeaderMap, Json(b): Json<NewReport>) -> Result<Json<Value>, ApiError> {
    let key = key_of(&headers);
    let door = door_of(&headers);
    Ok(Json(write(&s, |w| w.create_report(&door, key.as_deref(), &b.invoice_ids)).await?))
}

async fn get_report(State(s): State<Shared>, Path(id): Path<u64>) -> Result<Json<Value>, ApiError> {
    let w = s.world.lock().unwrap();
    let r = w.reports.iter().find(|r| r.id == id).cloned().ok_or(DomainError::NotFound("report", id))?;
    Ok(Json(json!(r)))
}

// ---------- UI-only ----------

/// Approve has no API. The UI sends `X-UI: 1`; anything else is refused so no arm can cheat.
async fn ui_approve(State(s): State<Shared>, headers: HeaderMap, Path(id): Path<u64>) -> Result<Json<Value>, ApiError> {
    if headers.get("x-ui").is_none() {
        return Err(ApiError(403, "approve is UI-only; use the page".into()));
    }
    let (v, evs) = {
        let mut w = s.world.lock().unwrap();
        w.approve_invoice(id)?
    };
    for ev in evs {
        let _ = s.events.send(ev);
    }
    Ok(Json(v))
}

// ---------- oracle ----------

#[derive(Deserialize)]
struct ResetQuery {
    seed: Option<u64>,
}

async fn oracle_reset(State(s): State<Shared>, Query(q): Query<ResetQuery>) -> Json<Value> {
    let seed = q.seed.unwrap_or(1);
    let mut w = s.world.lock().unwrap();
    *w = World::seeded(seed);
    Json(json!({"ok": true, "seed": seed, "customers": w.customers.len()}))
}

async fn oracle_state(State(s): State<Shared>) -> Json<Value> {
    let w = s.world.lock().unwrap();
    Json(json!(w.snapshot()))
}

async fn oracle_effects(State(s): State<Shared>) -> Json<Value> {
    let w = s.world.lock().unwrap();
    Json(json!({"effects": w.effects, "double_sends": w.double_sends()}))
}

async fn oracle_chaos(State(s): State<Shared>, Json(c): Json<Chaos>) -> Json<Value> {
    let mut w = s.world.lock().unwrap();
    w.chaos = c;
    Json(json!(w.chaos))
}

#[derive(Deserialize)]
struct Pay {
    invoice_id: u64,
    #[serde(default)]
    delay_ms: u64,
}

/// The outside world paying an invoice, optionally later. Used by the wait-for-event task.
async fn oracle_pay(State(s): State<Shared>, Json(p): Json<Pay>) -> Result<Json<Value>, ApiError> {
    if p.delay_ms == 0 {
        let (v, evs) = {
            let mut w = s.world.lock().unwrap();
            w.receive_payment(p.invoice_id)?
        };
        for ev in evs {
            let _ = s.events.send(ev);
        }
        return Ok(Json(v));
    }
    let s2 = s.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(p.delay_ms)).await;
        let res = {
            let mut w = s2.world.lock().unwrap();
            w.receive_payment(p.invoice_id)
        };
        if let Ok((_, evs)) = res {
            for ev in evs {
                let _ = s2.events.send(ev);
            }
        }
    });
    Ok(Json(json!({"scheduled": true, "invoice_id": p.invoice_id, "delay_ms": p.delay_ms})))
}

async fn events(State(s): State<Shared>) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = s.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|item| match item {
        Ok(ev) => Some(Ok(SseEvent::default()
            .event(ev.kind.clone())
            .id(ev.seq.to_string())
            .data(serde_json::to_string(&ev).unwrap_or_default()))),
        Err(_) => None,
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(10)))
}

// ---------- static ----------

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn app_js() -> Response {
    ([(header::CONTENT_TYPE, "application/javascript")], include_str!("../static/app.js")).into_response()
}

async fn openapi() -> Response {
    ([(header::CONTENT_TYPE, "application/json")], include_str!("../static/openapi.json")).into_response()
}

async fn health() -> Json<Value> {
    Json(json!({"ok": true}))
}

pub fn router(state: Shared) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/ui/invoices/{id}", get(index))
        .route("/static/app.js", get(app_js))
        .route("/openapi.json", get(openapi))
        .route("/health", get(health))
        .route("/api/customers", get(list_customers).post(create_customer))
        .route("/api/invoices", get(list_invoices).post(create_invoice))
        .route("/api/invoices/{id}", get(get_invoice))
        .route("/api/invoices/{id}/send", post(send_invoice))
        .route("/api/invoices/{id}/receipt", post(send_receipt))
        .route("/api/reports", post(create_report))
        .route("/api/reports/{id}", get(get_report))
        .route("/ui/approve/{id}", post(ui_approve))
        .route("/oracle/reset", post(oracle_reset))
        .route("/oracle/state", get(oracle_state))
        .route("/oracle/effects", get(oracle_effects))
        .route("/oracle/chaos", post(oracle_chaos))
        .route("/oracle/pay", post(oracle_pay))
        .route("/events", get(events))
        .route("/mcp", post(mcp::handle))
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(state)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let args = Args::parse();
    let (tx, _) = broadcast::channel(4096);
    let state: Shared = Arc::new(AppState { world: Mutex::new(World::seeded(args.seed)), events: tx });
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    eprintln!("target app on http://{}  (seed {})", args.bind, args.seed);
    axum::serve(listener, router(state)).await?;
    Ok(())
}
