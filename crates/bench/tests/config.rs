//! The run configuration round-trips: CLI defaults → config.json → the same options.

use bench::config::RunOpts;
use bench::report::RunResult;
use clap::Parser;

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    opts: RunOpts,
}

#[test]
fn defaults_round_trip_through_json() {
    let cli = Cli::try_parse_from(["bench"]).unwrap();
    let o = cli.opts;
    assert_eq!(o.arm_list(), vec!["D", "E"]);
    assert_eq!(o.surface_list(), vec!["api"]);
    assert_eq!(o.latency_ms, 25);
    assert!(!o.needs_model());
    let json = serde_json::to_string(&o).unwrap();
    assert!(!json.contains("api_key"), "the key must never be written: {json}");
    let back: RunOpts = serde_json::from_str(&json).unwrap();
    assert_eq!(back.arms, o.arms);
    assert_eq!(back.model, o.model);
    assert_eq!(back.tasks_dir, o.tasks_dir);
}

#[test]
fn model_arms_and_model_planner_need_a_model() {
    let o = Cli::try_parse_from(["bench", "--arms", "d,b2", "--api-key", "secret"]).unwrap().opts;
    assert_eq!(o.arm_list(), vec!["D", "B2"]);
    assert!(o.needs_model());
    assert!(!serde_json::to_string(&o).unwrap().contains("secret"));
    let o = Cli::try_parse_from(["bench", "--planner", "model"]).unwrap().opts;
    assert!(o.needs_model());
}

#[test]
fn an_old_result_file_without_provenance_fields_still_loads() {
    let old = r#"{"task":"T1","task_title":"","arm":"D","run":1,"status":"committed","planner":"none","samples":0,"tokens_in":0,"tokens_out":0,
      "wall_ms":1,"max_parallel":1,"nodes":1,"depth":1,"correct":true,"checks":[],"double_sends":0,"forks":0,"yield_reason":null,"error":null,
      "snapshot":{},"receipt":{}}"#;
    let r: RunResult = serde_json::from_str(old).unwrap();
    assert_eq!(r.model, "");
    assert_eq!(r.latency_ms, 0);
}
