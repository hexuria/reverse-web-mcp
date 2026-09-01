//! Recompute every headline number in a run directory from the raw ledgers and snapshots.
//! Anything stored that disagrees with the recomputation is a problem. This is what makes a
//! result a measurement rather than a claim.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;
use zerohuman::ledger::{max_overlap, union_length};

use crate::report::{load_results, RunResult};
use crate::tasks::{check, Task};

fn spans<'a>(rows: &'a [Value], skip_events: bool) -> impl Iterator<Item = (u128, u128)> + 'a {
    rows.iter()
        .filter(move |x| !skip_events || x.get("surface").and_then(|s| s.as_str()) != Some("event"))
        .map(|x| (x.get("started_us").and_then(|v| v.as_u64()).unwrap_or(0) as u128, x.get("ended_us").and_then(|v| v.as_u64()).unwrap_or(0) as u128))
}

/// Every disagreement for one result, as human-readable lines.
pub fn problems_for(r: &RunResult, task: Option<&Task>) -> Vec<String> {
    let mut out = Vec::new();
    let who = format!("{} {} run {}", r.task, r.arm, r.run);
    let rows = r.receipt.pointer("/ledger/rows").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let samples = r.receipt.pointer("/ledger/samples").and_then(|v| v.as_array()).cloned().unwrap_or_default();

    let max_par = max_overlap(spans(&rows, true));
    if max_par != r.max_parallel {
        out.push(format!("{who}: max_parallel stored {} recomputed {max_par}", r.max_parallel));
    }
    for (surface, stored) in &r.max_parallel_by_surface {
        let got = max_overlap(
            rows.iter()
                .filter(|x| x.get("surface").and_then(|s| s.as_str()) == Some(surface.as_str()))
                .map(|x| (x.get("started_us").and_then(|v| v.as_u64()).unwrap_or(0) as u128, x.get("ended_us").and_then(|v| v.as_u64()).unwrap_or(0) as u128)),
        );
        if got != *stored {
            out.push(format!("{who}: max_parallel[{surface}] stored {stored} recomputed {got}"));
        }
    }
    let run_ms = {
        let s = rows.iter().filter_map(|x| x.get("started_us").and_then(|v| v.as_u64())).min();
        let e = rows.iter().filter_map(|x| x.get("ended_us").and_then(|v| v.as_u64())).max();
        match (s, e) {
            (Some(s), Some(e)) => (e.saturating_sub(s) / 1000) as u128,
            _ => 0,
        }
    };
    if run_ms != r.run_ms {
        out.push(format!("{who}: run_ms stored {} recomputed {run_ms}", r.run_ms));
    }
    let plan_ms = union_length(spans(&samples, false)) / 1000;
    if plan_ms != r.plan_ms {
        out.push(format!("{who}: plan_ms stored {} recomputed {plan_ms}", r.plan_ms));
    }
    if samples.len() as u32 != r.samples {
        out.push(format!("{who}: samples stored {} recomputed {}", r.samples, samples.len()));
    }
    let tin: u64 = samples.iter().filter_map(|s| s.get("tokens_in").and_then(|v| v.as_u64())).sum();
    let tout: u64 = samples.iter().filter_map(|s| s.get("tokens_out").and_then(|v| v.as_u64())).sum();
    if (tin, tout) != (r.tokens_in, r.tokens_out) {
        out.push(format!("{who}: tokens stored {}/{} recomputed {tin}/{tout}", r.tokens_in, r.tokens_out));
    }
    if let Some(t) = task {
        let expect = t.expect.applicable(crate::tasks::resumed_after_fork(&r.receipt));
        let checks = check(expect, &r.status, r.forks, &r.snapshot, r.double_sends);
        let correct = checks.iter().all(|c| c.ok);
        if correct != r.correct {
            out.push(format!("{who}: correctness stored {} recomputed {correct}", r.correct));
        }
    }
    // The outbox in the snapshot is the second witness for double-sends.
    let outbox = r.snapshot.get("outbox").and_then(|o| o.as_array()).cloned().unwrap_or_default();
    let mut seen: BTreeMap<(u64, String), usize> = BTreeMap::new();
    for m in &outbox {
        let k = (m.get("invoice_id").and_then(|i| i.as_u64()).unwrap_or(0), m.get("kind").and_then(|k| k.as_str()).unwrap_or("").to_string());
        *seen.entry(k).or_default() += 1;
    }
    let dbl: usize = seen.values().filter(|n| **n > 1).map(|n| n - 1).sum();
    if dbl != r.double_sends {
        out.push(format!("{who}: double_sends stored {} recomputed {dbl}", r.double_sends));
    }
    out
}

/// Verify a whole run directory. Prints every problem and returns how many there were.
pub fn verify_dir(run: &Path, tasks_dir: &Path) -> anyhow::Result<usize> {
    let tasks: BTreeMap<String, Task> = Task::load_dir(tasks_dir)?.into_iter().map(|t| (t.id.clone(), t)).collect();
    let results = load_results(run)?;
    let mut problems = 0;
    for r in &results {
        for line in problems_for(r, tasks.get(&r.task)) {
            problems += 1;
            println!("{line}");
        }
    }
    println!("{} results verified, {} problems", results.len(), problems);
    Ok(problems)
}
