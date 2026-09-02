//! verify recomputes from raw rows; a doctored headline number is a problem.

use bench::report::RunResult;
use bench::verify::problems_for;
use serde_json::{json, Value};

fn honest() -> RunResult {
    let rows = json!([
        {"node":"A","op":"createInvoice","surface":"api","key":"k","attempt":1,"started_us":100,"ended_us":3100,"ok":true,"write":true,"error":null,"observed":{}},
        {"node":"B","op":"createInvoice","surface":"api","key":"k2","attempt":1,"started_us":200,"ended_us":3200,"ok":true,"write":true,"error":null,"observed":{}}
    ]);
    let samples = json!([{"seq":1,"kind":"plan","started_us":0,"ended_us":50000,"tokens_in":10,"tokens_out":5,"model":"m","effort":"low"}]);
    let r: RunResult = serde_json::from_value(json!({
        "task":"T2","task_title":"","arm":"D","run":1,"status":"committed","planner":"model","samples":1,"tokens_in":10,"tokens_out":5,
        "wall_ms":60,"plan_ms":50,"run_ms":3,"busy_ms":3,"max_parallel":2,"nodes":2,"depth":1,"correct":true,"checks":[],"double_sends":0,"forks":0,
        "yield_reason":null,"error":null,"snapshot":{"outbox":[]},"receipt":{"ledger":{"rows":rows,"samples":samples}},"intent":null
    }))
    .unwrap();
    r
}

#[test]
fn an_honest_result_has_no_problems() {
    assert!(problems_for(&honest(), None).is_empty());
}

#[test]
fn every_doctored_headline_is_caught() {
    let mut r = honest();
    r.max_parallel = 1;
    assert!(problems_for(&r, None).iter().any(|p| p.contains("max_parallel")));
    let mut r = honest();
    r.plan_ms = 1;
    assert!(problems_for(&r, None).iter().any(|p| p.contains("plan_ms")));
    let mut r = honest();
    r.run_ms = 99;
    assert!(problems_for(&r, None).iter().any(|p| p.contains("run_ms")));
    let mut r = honest();
    r.busy_ms = Some(99);
    assert!(problems_for(&r, None).iter().any(|p| p.contains("busy_ms")));
    let mut r = honest();
    r.forks = 2;
    assert!(problems_for(&r, None).iter().any(|p| p.contains("forks")));
    let mut r = honest();
    r.samples = 3;
    assert!(problems_for(&r, None).iter().any(|p| p.contains("samples")));
    let mut r = honest();
    r.tokens_out = 999;
    assert!(problems_for(&r, None).iter().any(|p| p.contains("tokens")));
    let mut r = honest();
    r.snapshot = json!({"outbox":[{"invoice_id":1,"kind":"invoice"},{"invoice_id":1,"kind":"invoice"}]});
    assert!(problems_for(&r, None).iter().any(|p| p.contains("double_sends")));
    let _: Value = json!(null);
}
