//! Scoring and summarising are pure functions over stored data. Pin them.

use bench::report::{load_results, summarize, RunResult};
use bench::tasks::{check, Expect};
use serde_json::{json, Value};

fn snapshot(invoices: Vec<Value>, reports: usize) -> Value {
    json!({"invoices": invoices, "reports": (0..reports).map(|i| json!({"id": i})).collect::<Vec<_>>(), "outbox": []})
}

#[test]
fn check_reads_the_snapshot_not_the_arm() {
    let expect = Expect {
        status: "committed".into(),
        invoices: Some(2),
        sent: Some(1),
        paid: Some(1),
        receipts: Some(0),
        reports: Some(1),
        forks: Some(0),
        double_sends: Some(0),
        after_resume: None,
    };
    let snap = snapshot(vec![json!({"status": "paid", "receipt_sent": false}), json!({"status": "draft", "receipt_sent": false})], 1);
    let checks = check(&expect, "committed", 0, &snap, 0);
    let failed: Vec<&str> = checks.iter().filter(|c| !c.ok).map(|c| c.name.as_str()).collect();
    assert!(failed.is_empty(), "{failed:?}");
    // A paid invoice counts as sent; a draft does not.
    let checks = check(&expect, "committed", 0, &snap, 1);
    assert_eq!(checks.iter().filter(|c| !c.ok).map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["double_sends"]);
    let checks = check(&expect, "error", 0, &snap, 0);
    assert_eq!(checks.iter().filter(|c| !c.ok).map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["status"]);
}

fn result(task: &str, arm: &str, run: u32, wall_ms: u128, max_parallel: usize, correct: bool) -> RunResult {
    RunResult {
        task: task.into(),
        task_title: String::new(),
        arm: arm.into(),
        run,
        status: "committed".into(),
        planner: "none".into(),
        samples: 1,
        tokens_in: 10,
        tokens_out: 5,
        wall_ms,
        plan_ms: 0,
        run_ms: wall_ms,
        busy_ms: Some(wall_ms),
        max_parallel,
        max_parallel_by_surface: Default::default(),
        nodes: 0,
        depth: 0,
        correct,
        checks: vec![],
        double_sends: 0,
        forks: 0,
        yield_reason: None,
        error: None,
        snapshot: json!({}),
        receipt: json!({}),
        intent: Value::Null,
        model: String::new(),
        effort: String::new(),
        base_url: String::new(),
        latency_ms: 25,
        surfaces: "api".into(),
    }
}

#[test]
fn summarize_uses_medians_per_task_and_arm() {
    let rs =
        vec![result("T2", "D", 1, 100, 10, true), result("T2", "D", 2, 300, 10, true), result("T2", "D", 3, 200, 8, false), result("T2", "E", 1, 50, 10, true)];
    let cells = summarize(&rs);
    assert_eq!(cells.len(), 2);
    let d = cells.iter().find(|c| c.arm == "D").unwrap();
    assert_eq!(d.runs, 3);
    assert_eq!(d.correct, 2);
    assert_eq!(d.wall_ms_median, 200.0);
    assert_eq!(d.max_parallel_median, 10.0);
    assert_eq!((d.wall_ms_min, d.wall_ms_max), (100, 300));
    assert_eq!(d.tokens_median, 15.0);
    assert_eq!((d.wall_ms_p25, d.wall_ms_p75), (150.0, 250.0));
}

#[test]
fn a_corrupt_result_file_is_an_error_not_a_smaller_sample() {
    let dir = std::env::temp_dir().join(format!("chiffon-scoring-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("T1-D-1.json"), serde_json::to_string(&result("T1", "D", 1, 1, 1, true)).unwrap()).unwrap();
    assert_eq!(load_results(&dir).unwrap().len(), 1);
    std::fs::write(dir.join("T1-D-2.json"), "{not json").unwrap();
    let err = load_results(&dir).unwrap_err().to_string();
    assert!(err.contains("T1-D-2.json"), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}
