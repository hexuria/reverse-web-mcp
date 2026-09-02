//! Three different "arms" record through the same Recorder and produce rows that mean the same thing.

use std::sync::Arc;
use std::time::Duration;

use rwmcp::ledger::{Ledger, Recorder};
use rwmcp::World;
use serde_json::{json, Value};

fn world() -> Arc<World> {
    let doc: Value = serde_json::from_str(include_str!("../../app/static/openapi.json")).unwrap();
    Arc::new(World::from_openapi(&doc).unwrap())
}

async fn two_overlapping(rec: &Recorder, surface: &str, prefix: &str) {
    let a = rec.start(&rec.next_node_id(prefix), "createInvoice", surface, Some("k1".into()), 1);
    let b = rec.start(&rec.next_node_id(prefix), "listCustomers", surface, None, 1);
    tokio::time::sleep(Duration::from_millis(3)).await;
    a.finish::<String>(&Ok(json!({"id": 1})));
    b.finish::<String>(&Err("boom".into()));
}

#[tokio::test]
async fn every_arm_records_the_same_shape() {
    let mut shapes = Vec::new();
    for (surface, prefix) in [("api", "A"), ("api", "s"), ("mcp", "t")] {
        let rec = Recorder::new(world());
        two_overlapping(&rec, surface, prefix).await;
        let mut ledger = Ledger::new();
        rec.drain_into(&mut ledger);
        assert_eq!(ledger.rows.len(), 2);
        assert_eq!(ledger.max_parallel(), 2, "{surface}: both effects were in flight together");
        let create = ledger.rows.iter().find(|r| r.op == "createInvoice").unwrap();
        let list = ledger.rows.iter().find(|r| r.op == "listCustomers").unwrap();
        assert!(create.write, "write-ness comes from the world model");
        assert!(!list.write);
        assert!(create.ok && !list.ok);
        assert_eq!(list.error.as_deref(), Some("boom"));
        assert_eq!(create.key.as_deref(), Some("k1"));
        shapes.push((create.write, list.write, create.ok, list.ok, create.attempt));
    }
    assert!(shapes.windows(2).all(|w| w[0] == w[1]), "{shapes:?}");
}

#[test]
fn an_event_wait_is_never_a_write() {
    let rec = Recorder::new(world());
    assert!(!rec.is_write("payment.received"));
    assert!(rec.is_write("sendInvoice"));
    assert!(!rec.is_write("getInvoice"));
    assert!(!rec.is_write("no-such-op"));
}
