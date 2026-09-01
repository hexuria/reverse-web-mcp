//! plan_ms and run_ms are derived from sample and effect spans, never stored as claims.

use serde_json::json;
use zerohuman::ledger::{Ledger, Row, Sample, SampleKind};
use zerohuman::Plan;

fn sample(kind: SampleKind, s: u128, e: u128, tin: u64, tout: u64) -> Sample {
    Sample { seq: 0, kind, started_us: s, ended_us: e, tokens_in: tin, tokens_out: tout, model: "grok-4.6".into(), effort: "low".into() }
}

fn row(node: &str, s: u128, e: u128) -> Row {
    Row {
        node: node.into(),
        op: "createInvoice".into(),
        surface: "api".into(),
        key: None,
        attempt: 1,
        started_us: s,
        ended_us: e,
        ok: true,
        write: true,
        error: None,
        observed: json!({}),
    }
}

#[test]
fn one_slow_thought_and_ten_fast_effects() {
    let mut l = Ledger::new();
    l.record_sample(sample(SampleKind::Plan, 1_000_000, 6_000_000, 900, 120));
    for i in 0..10 {
        l.rows.push(row(&format!("n{i}"), 6_010_000, 6_013_000));
    }
    let plan = Plan { plan_id: "p".into(), goal: String::new(), nodes: vec![], edges: vec![], gates: vec![] };
    let r = l.receipt(&plan, zerohuman::Status::Committed, None, None, None);
    assert_eq!(r.plan_ms, 5000);
    assert_eq!(r.run_ms, 3);
    assert_eq!(r.max_parallel, 10);
    assert_eq!(r.samples, 1);
    assert_eq!((r.tokens_in, r.tokens_out), (900, 120));
}

#[test]
fn overlapping_samples_are_not_double_counted() {
    let mut l = Ledger::new();
    l.record_sample(sample(SampleKind::Turn, 0, 4_000_000, 1, 1));
    l.record_sample(sample(SampleKind::Turn, 2_000_000, 5_000_000, 1, 1));
    assert_eq!(l.plan_us(), 5_000_000);
    assert_eq!(l.sample_count(), 2);
    assert_eq!(l.run_us(), 0, "no effects, no run time");
}
