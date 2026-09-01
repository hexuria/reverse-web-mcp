//! MCP over HTTP: a JSON-RPC 2.0 endpoint exposing the same handlers as tools.
//! Enough of the protocol for an agent loop: initialize, tools/list, tools/call.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::{write, ApiError, Shared};

pub fn tools() -> Value {
    json!([
        {"name":"listCustomers","description":"Find customers, optionally by exact name.",
         "inputSchema":{"type":"object","properties":{"name":{"type":"string"}}}},
        {"name":"listInvoices","description":"List invoices, optionally for one customer id.",
         "inputSchema":{"type":"object","properties":{"customer_id":{"type":"integer"}}}},
        {"name":"getInvoice","description":"Read one invoice by id.",
         "inputSchema":{"type":"object","properties":{"id":{"type":"integer"}},"required":["id"]}},
        {"name":"createInvoice","description":"Create a draft invoice for a customer id.",
         "inputSchema":{"type":"object","properties":{"customer_id":{"type":"integer"},"amount_cents":{"type":"integer"},"idempotency_key":{"type":"string"}},"required":["customer_id","amount_cents"]}},
        {"name":"sendInvoice","description":"Email an invoice to its customer. Marks it sent.",
         "inputSchema":{"type":"object","properties":{"id":{"type":"integer"},"idempotency_key":{"type":"string"}},"required":["id"]}},
        {"name":"sendReceipt","description":"Email a receipt for a paid invoice. Fails until payment arrives.",
         "inputSchema":{"type":"object","properties":{"id":{"type":"integer"},"idempotency_key":{"type":"string"}},"required":["id"]}},
        {"name":"createReport","description":"Create a report over a list of invoice ids.",
         "inputSchema":{"type":"object","properties":{"invoice_ids":{"type":"array","items":{"type":"integer"}},"idempotency_key":{"type":"string"}},"required":["invoice_ids"]}}
    ])
}

fn rpc_ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":result})
}

fn rpc_err(id: Value, code: i64, msg: String) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":msg}})
}

fn u64_arg(args: &Value, k: &str) -> Result<u64, ApiError> {
    args.get(k).and_then(|v| v.as_u64()).ok_or_else(|| ApiError(400, format!("missing integer argument '{k}'")))
}

async fn call(state: &Shared, name: &str, args: &Value) -> Result<Value, ApiError> {
    let key = args.get("idempotency_key").and_then(|v| v.as_str()).map(|s| s.to_string());
    let door = "mcp";
    match name {
        "listCustomers" => {
            let w = state.world.lock().unwrap();
            Ok(json!(w.find_customers(args.get("name").and_then(|v| v.as_str()))))
        }
        "listInvoices" => {
            let w = state.world.lock().unwrap();
            Ok(json!(w.list_invoices(args.get("customer_id").and_then(|v| v.as_u64()))))
        }
        "getInvoice" => {
            let id = u64_arg(args, "id")?;
            let w = state.world.lock().unwrap();
            Ok(json!(w.invoice(id)?))
        }
        "createInvoice" => {
            let cid = u64_arg(args, "customer_id")?;
            let amt = args.get("amount_cents").and_then(|v| v.as_i64()).unwrap_or(0);
            write(state, |w| w.create_invoice(door, key.as_deref(), cid, amt)).await
        }
        "sendInvoice" => {
            let id = u64_arg(args, "id")?;
            write(state, |w| w.send_invoice(door, key.as_deref(), id)).await
        }
        "sendReceipt" => {
            let id = u64_arg(args, "id")?;
            write(state, |w| w.send_receipt(door, key.as_deref(), id)).await
        }
        "createReport" => {
            let ids: Vec<u64> = args
                .get("invoice_ids")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_u64()).collect())
                .unwrap_or_default();
            write(state, |w| w.create_report(door, key.as_deref(), &ids)).await
        }
        other => Err(ApiError(404, format!("unknown tool '{other}'"))),
    }
}

pub async fn handle(State(state): State<Shared>, Json(req): Json<Value>) -> Response {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    match method {
        "initialize" => Json(rpc_ok(
            id,
            json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"chiffon-target-app","version":"0.1.0"}}),
        ))
        .into_response(),
        "notifications/initialized" | "ping" => {
            if id.is_null() {
                StatusCode::ACCEPTED.into_response()
            } else {
                Json(rpc_ok(id, json!({}))).into_response()
            }
        }
        "tools/list" => Json(rpc_ok(id, json!({"tools": tools()}))).into_response(),
        "tools/call" => {
            let params = req.get("params").cloned().unwrap_or(json!({}));
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match call(&state, name, &args).await {
                Ok(v) => Json(rpc_ok(
                    id,
                    json!({"content":[{"type":"text","text":v.to_string()}],"structuredContent":v,"isError":false}),
                ))
                .into_response(),
                Err(ApiError(status, msg)) => Json(rpc_ok(
                    id,
                    json!({"content":[{"type":"text","text":format!("error {status}: {msg}")}],"isError":true,"_status":status}),
                ))
                .into_response(),
            }
        }
        other => Json(rpc_err(id, -32601, format!("method not found: {other}"))).into_response(),
    }
}
