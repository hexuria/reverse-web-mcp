//! Reads every result file in a run directory and writes summary.json plus report.html.
//! Every number here is recomputed from the stored ledgers and snapshots, never copied.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tasks::Check;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunResult {
    pub task: String,
    pub task_title: String,
    pub arm: String,
    pub run: u32,
    pub status: String,
    pub planner: String,
    pub samples: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub wall_ms: u128,
    #[serde(default)]
    pub plan_ms: u128,
    #[serde(default)]
    pub run_ms: u128,
    /// Time at least one call was in flight. None in results written before this existed.
    #[serde(default)]
    pub busy_ms: Option<u128>,
    pub max_parallel: usize,
    #[serde(default)]
    pub max_parallel_by_surface: BTreeMap<String, usize>,
    pub nodes: usize,
    pub depth: usize,
    pub correct: bool,
    pub checks: Vec<Check>,
    pub double_sends: usize,
    pub forks: usize,
    pub yield_reason: Option<String>,
    pub error: Option<String>,
    pub snapshot: Value,
    pub receipt: Value,
    /// The intent arm D compiled, whether hand-written or a planner sample.
    #[serde(default)]
    pub intent: Value,
    /// How this run was produced. Empty model/effort means no model was involved.
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub effort: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub latency_ms: u64,
    #[serde(default)]
    pub surfaces: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Cell {
    pub task: String,
    pub arm: String,
    pub runs: usize,
    pub correct: usize,
    pub samples_median: f64,
    pub tokens_median: f64,
    pub wall_ms_median: f64,
    pub wall_ms_min: u128,
    pub wall_ms_max: u128,
    pub plan_ms_median: f64,
    pub run_ms_median: f64,
    pub busy_ms_median: f64,
    pub samples_p25: f64,
    pub samples_p75: f64,
    pub wall_ms_p25: f64,
    pub wall_ms_p75: f64,
    pub run_ms_p25: f64,
    pub run_ms_p75: f64,
    pub max_parallel_median: f64,
    pub max_parallel_max: usize,
    pub double_sends_total: usize,
    pub forks_total: usize,
    pub errors: Vec<String>,
}

/// Linear-interpolated quantile; q in [0,1].
fn quantile(mut xs: Vec<f64>, q: f64) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pos = q * (xs.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    xs[lo] + (xs[hi] - xs[lo]) * (pos - lo as f64)
}

fn median(mut xs: Vec<f64>) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = xs.len();
    if n % 2 == 1 {
        xs[n / 2]
    } else {
        (xs[n / 2 - 1] + xs[n / 2]) / 2.0
    }
}

/// Every result file in the directory. A file that cannot be read or parsed is an error, never
/// a silently smaller sample.
pub fn load_results(dir: &Path) -> anyhow::Result<Vec<RunResult>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if p.extension().is_some_and(|x| x == "json") && name != "summary.json" && name != "config.json" {
            let text = std::fs::read_to_string(&p)?;
            let r: RunResult = serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?;
            out.push(r);
        }
    }
    out.sort_by_key(|r| (r.task.clone(), r.arm.clone(), r.run));
    Ok(out)
}

pub fn summarize(results: &[RunResult]) -> Vec<Cell> {
    let mut groups: BTreeMap<(String, String), Vec<&RunResult>> = BTreeMap::new();
    for r in results {
        groups.entry((r.task.clone(), r.arm.clone())).or_default().push(r);
    }
    groups
        .into_iter()
        .map(|((task, arm), rs)| Cell {
            task,
            arm,
            runs: rs.len(),
            correct: rs.iter().filter(|r| r.correct).count(),
            samples_median: median(rs.iter().map(|r| r.samples as f64).collect()),
            tokens_median: median(rs.iter().map(|r| (r.tokens_in + r.tokens_out) as f64).collect()),
            wall_ms_median: median(rs.iter().map(|r| r.wall_ms as f64).collect()),
            wall_ms_min: rs.iter().map(|r| r.wall_ms).min().unwrap_or(0),
            wall_ms_max: rs.iter().map(|r| r.wall_ms).max().unwrap_or(0),
            plan_ms_median: median(rs.iter().map(|r| r.plan_ms as f64).collect()),
            run_ms_median: median(rs.iter().map(|r| r.run_ms as f64).collect()),
            busy_ms_median: median(rs.iter().map(|r| r.busy_ms.unwrap_or(r.run_ms) as f64).collect()),
            samples_p25: quantile(rs.iter().map(|r| r.samples as f64).collect(), 0.25),
            samples_p75: quantile(rs.iter().map(|r| r.samples as f64).collect(), 0.75),
            wall_ms_p25: quantile(rs.iter().map(|r| r.wall_ms as f64).collect(), 0.25),
            wall_ms_p75: quantile(rs.iter().map(|r| r.wall_ms as f64).collect(), 0.75),
            run_ms_p25: quantile(rs.iter().map(|r| r.run_ms as f64).collect(), 0.25),
            run_ms_p75: quantile(rs.iter().map(|r| r.run_ms as f64).collect(), 0.75),
            max_parallel_median: median(rs.iter().map(|r| r.max_parallel as f64).collect()),
            max_parallel_max: rs.iter().map(|r| r.max_parallel).max().unwrap_or(0),
            double_sends_total: rs.iter().map(|r| r.double_sends).sum(),
            forks_total: rs.iter().map(|r| r.forks).sum(),
            errors: rs.iter().filter_map(|r| r.error.clone()).collect(),
        })
        .collect()
}

/// Samples against task depth: the structural claim. One row per task, one column per arm.
pub fn depth_table(cells: &[Cell], depths: &BTreeMap<String, u32>) -> String {
    let mut arms: Vec<String> = cells.iter().map(|c| c.arm.clone()).collect();
    arms.sort();
    arms.dedup();
    let mut tasks: Vec<String> = cells.iter().map(|c| c.task.clone()).collect();
    tasks.sort();
    tasks.dedup();
    tasks.sort_by_key(|t| depths.get(t).copied().unwrap_or(0));
    let mut s = format!("{:<4} {:>5}", "task", "depth");
    for a in &arms {
        s.push_str(&format!(" {:>10}", format!("{a} samples")));
    }
    s.push('\n');
    for t in &tasks {
        s.push_str(&format!("{:<4} {:>5}", t, depths.get(t).copied().unwrap_or(0)));
        for a in &arms {
            match cells.iter().find(|c| &c.task == t && &c.arm == a) {
                Some(c) => s.push_str(&format!(" {:>10}", c.samples_median)),
                None => s.push_str(&format!(" {:>10}", "-")),
            }
        }
        s.push('\n');
    }
    s
}

pub fn text_table(cells: &[Cell]) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "{:<4} {:<3} {:>5} {:>8} {:>9} {:>8} {:>8} {:>10} {:>7} {:>6}\n",
        "task", "arm", "runs", "correct", "samples", "plan_ms", "run_ms", "max_par", "double", "forks"
    ));
    for c in cells {
        s.push_str(&format!(
            "{:<4} {:<3} {:>5} {:>8} {:>9} {:>8} {:>8} {:>10} {:>7} {:>6}\n",
            c.task,
            c.arm,
            c.runs,
            format!("{}/{}", c.correct, c.runs),
            c.samples_median,
            c.plan_ms_median,
            c.run_ms_median,
            c.max_parallel_median,
            c.double_sends_total,
            c.forks_total
        ));
    }
    s
}

pub fn write_report(dir: &Path, results: &[RunResult], titles: &BTreeMap<String, String>, depths: &BTreeMap<String, u32>) -> anyhow::Result<Vec<Cell>> {
    let cells = summarize(results);
    std::fs::write(dir.join("summary.json"), serde_json::to_string_pretty(&cells)?)?;

    let arms = crate::arms::arm_names();
    let mut tasks: Vec<String> = cells.iter().map(|c| c.task.clone()).collect();
    tasks.dedup();

    let mut html = String::new();
    html.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>chiffon bench</title><style>");
    html.push_str("body{font:15px/1.5 system-ui,sans-serif;margin:0;padding:32px;background:#f3f5f2;color:#1a2321;max-width:1100px}h1{font-size:26px;margin:0 0 4px}h2{font-size:18px;margin:32px 0 8px}p{color:#4b5955;max-width:70ch}table{border-collapse:collapse;width:100%;background:#fff;font-variant-numeric:tabular-nums;font-size:14px}th,td{padding:7px 10px;border-bottom:1px solid #d5dcd7;text-align:left}th{font-size:11px;text-transform:uppercase;letter-spacing:.08em;color:#4b5955;background:#e9eeea}td.ok{color:#2f7d4f;font-weight:600}td.bad{color:#b23a2e;font-weight:600}td.hot{background:#dceef0;font-weight:600}code{font-family:ui-monospace,monospace;background:#eef2ef;padding:1px 5px;border-radius:4px}details{margin-top:10px}pre{font-size:12px;background:#eef2ef;padding:10px;overflow-x:auto;border-radius:6px}");
    html.push_str("</style></head><body>");
    html.push_str(&format!("<h1>chiffon bench</h1><p>run directory <code>{}</code>. Every figure is recomputed from the stored ledgers and oracle snapshots. The decisive column is <b>max parallel</b>: the largest number of effects in flight at one instant.</p>", dir.display()));

    for t in &tasks {
        let title = titles.get(t).cloned().unwrap_or_default();
        html.push_str(&format!("<h2>{t} · {title}</h2>"));
        html.push_str("<table><thead><tr><th>arm</th><th>what</th><th>runs</th><th>correct</th><th>model samples</th><th>tokens</th><th>plan ms</th><th>run ms</th><th>wall ms (median)</th><th>wall p25–p75</th><th>max parallel</th><th>double-sends</th><th>forks</th></tr></thead><tbody>");
        for c in cells.iter().filter(|c| &c.task == t) {
            let ok = if c.correct == c.runs && c.runs > 0 { "ok" } else { "bad" };
            let hot = if c.max_parallel_max > 1 { "hot" } else { "" };
            html.push_str(&format!(
                "<tr><td><b>{}</b></td><td>{}</td><td>{}</td><td class=\"{ok}\">{}/{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}–{}</td><td class=\"{hot}\">{}</td><td class=\"{}\">{}</td><td>{}</td></tr>",
                c.arm,
                arms.get(c.arm.as_str()).unwrap_or(&""),
                c.runs,
                c.correct,
                c.runs,
                c.samples_median,
                c.tokens_median,
                c.plan_ms_median,
                c.run_ms_median,
                c.wall_ms_median,
                c.wall_ms_p25,
                c.wall_ms_p75,
                c.max_parallel_median,
                if c.double_sends_total > 0 { "bad" } else { "" },
                c.double_sends_total,
                c.forks_total
            ));
        }
        html.push_str("</tbody></table>");
        let errs: Vec<&String> = cells.iter().filter(|c| &c.task == t).flat_map(|c| c.errors.iter()).collect();
        if !errs.is_empty() {
            html.push_str("<details><summary>errors</summary><pre>");
            for e in errs {
                html.push_str(&html_escape(e));
                html.push('\n');
            }
            html.push_str("</pre></details>");
        }
        if let Some(r) = results.iter().find(|r| &r.task == t && r.arm == "D") {
            if let Some(plan) = r.receipt.get("plan").and_then(|p| p.as_str()) {
                html.push_str(&format!("<details><summary>plan (arm D, run {})</summary><pre>{}</pre></details>", r.run, html_escape(plan)));
            }
        }
    }
    html.push_str("<h2>samples against depth</h2><p>A loop pays a thought per dependency level. A plan pays one thought regardless. This is the column that separates the arms.</p><pre>");
    html.push_str(&html_escape(&depth_table(&cells, depths)));
    html.push_str("</pre></body></html>");
    std::fs::write(dir.join("report.html"), html)?;
    Ok(cells)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
