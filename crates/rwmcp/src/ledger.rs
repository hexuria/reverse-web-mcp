//! Append-only record of every attempt. The receipt is a view of this, never a claim.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::plan::Plan;
use crate::world::World;

pub fn now_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// Microseconds. Effects against a local app finish in well under a millisecond, so spans
/// must be finer than that or sequential calls look concurrent.
pub fn now_us() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros()).unwrap_or(0)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Row {
    pub node: String,
    pub op: String,
    pub surface: String,
    pub key: Option<String>,
    pub attempt: u32,
    pub started_us: u128,
    pub ended_us: u128,
    pub ok: bool,
    /// True for rows that changed the world (or tried to). Waits and reads are false.
    pub write: bool,
    pub error: Option<String>,
    pub observed: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Committed,
    NeedThink,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SampleKind {
    /// The planner turning a goal into an intent.
    Plan,
    /// The planner re-asked after lint errors.
    Lint,
    /// One turn of an agent loop.
    Turn,
    /// The planner answering a fork.
    ForkAnswer,
}

/// One model call. Samples are rows too, so cost and thinking time are recomputed, never claimed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sample {
    pub seq: u32,
    pub kind: SampleKind,
    pub started_us: u128,
    pub ended_us: u128,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub model: String,
    pub effort: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Ledger {
    pub rows: Vec<Row>,
    pub samples: Vec<Sample>,
    pub forks: Vec<Value>,
    /// Anything worth reading later that is not an effect or a sample: lint errors, rejected answers.
    #[serde(default)]
    pub notes: Vec<Value>,
    pub started_ms: u128,
    pub ended_ms: u128,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectSummary {
    pub node: String,
    pub op: String,
    pub key: Option<String>,
    pub attempts: u32,
    pub ok: bool,
    pub observed: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub plan_id: String,
    pub status: Status,
    pub samples: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub wall_ms: u128,
    /// Time the model was thinking: the union of sample spans.
    pub plan_ms: u128,
    /// First effect start to last effect end, including any gaps.
    pub run_ms: u128,
    /// Time at least one call was actually in flight.
    pub busy_ms: u128,
    pub max_parallel: usize,
    /// The same sweep per surface: the screen pool should read 1 while the API stays wide.
    #[serde(default)]
    pub max_parallel_by_surface: std::collections::BTreeMap<String, usize>,
    pub nodes: usize,
    pub depth: usize,
    pub effects: Vec<EffectSummary>,
    pub forks_taken: usize,
    pub yield_reason: Option<String>,
    pub evidence: Option<Value>,
    pub error: Option<String>,
    pub plan: String,
    pub ledger: Ledger,
}

impl Ledger {
    pub fn new() -> Self {
        Ledger { started_ms: now_ms(), ..Default::default() }
    }

    /// Sweep-line over rows that executed against a surface (waits excluded): the largest number
    /// of calls in flight at one instant. Reads count, so this is concurrency, not writes.
    pub fn max_parallel(&self) -> usize {
        max_overlap(self.rows.iter().filter(|r| r.surface != "event").map(|r| (r.started_us, r.ended_us)))
    }

    pub fn max_parallel_by_surface(&self) -> std::collections::BTreeMap<String, usize> {
        let mut out = std::collections::BTreeMap::new();
        let mut surfaces: Vec<&str> = self.rows.iter().map(|r| r.surface.as_str()).filter(|s| *s != "event").collect();
        surfaces.sort();
        surfaces.dedup();
        for s in surfaces {
            out.insert(s.to_string(), max_overlap(self.rows.iter().filter(|r| r.surface == s).map(|r| (r.started_us, r.ended_us))));
        }
        out
    }

    /// Append a sample; its `seq` is assigned here.
    pub fn record_sample(&mut self, sample: Sample) {
        let seq = self.samples.len() as u32 + 1;
        self.samples.push(Sample { seq, ..sample });
    }

    pub fn sample_count(&self) -> u32 {
        self.samples.len() as u32
    }

    pub fn tokens(&self) -> (u64, u64) {
        (self.samples.iter().map(|s| s.tokens_in).sum(), self.samples.iter().map(|s| s.tokens_out).sum())
    }

    /// Microseconds the model was thinking: the union of sample spans, so overlapping samples
    /// (a parallel fan-out of planners, one day) are not double counted.
    pub fn plan_us(&self) -> u128 {
        union_length(self.samples.iter().map(|s| (s.started_us, s.ended_us)))
    }

    /// Nodes of `plan` this ledger already proves done: a keyed node whose key has an ok row.
    /// Unkeyed nodes (reads, waits) are never "done"; they re-run on resume, which is harmless.
    pub fn completed(&self, plan: &Plan) -> std::collections::HashMap<String, Value> {
        let mut done = std::collections::HashMap::new();
        for n in &plan.nodes {
            let Some(key) = &n.key else { continue };
            if let Some(row) = self.rows.iter().rev().find(|r| r.ok && r.key.as_deref() == Some(key)) {
                done.insert(n.id.clone(), row.observed.clone());
            }
        }
        done
    }

    /// Microseconds in which at least one call was in flight. Unlike `run_us` this excludes the
    /// gaps an agent loop spends thinking between tool calls, so arms are comparable.
    pub fn busy_us(&self) -> u128 {
        union_length(self.rows.iter().filter(|r| r.surface != "event").map(|r| (r.started_us, r.ended_us)))
    }

    /// Microseconds from the first effect starting to the last effect ending, gaps included.
    pub fn run_us(&self) -> u128 {
        let start = self.rows.iter().map(|r| r.started_us).min();
        let end = self.rows.iter().map(|r| r.ended_us).max();
        match (start, end) {
            (Some(s), Some(e)) => e.saturating_sub(s),
            _ => 0,
        }
    }

    pub fn receipt(&self, plan: &Plan, status: Status, yield_reason: Option<String>, evidence: Option<Value>, error: Option<String>) -> Receipt {
        let mut effects: Vec<EffectSummary> = Vec::new();
        for n in &plan.nodes {
            let rows: Vec<&Row> = self.rows.iter().filter(|r| r.node == n.id).collect();
            if rows.is_empty() {
                continue;
            }
            let last = rows.last().unwrap();
            effects.push(EffectSummary {
                node: n.id.clone(),
                op: n.op.clone(),
                key: n.key.clone(),
                attempts: rows.len() as u32,
                ok: last.ok,
                observed: last.observed.clone(),
            });
        }
        let ended = if self.ended_ms == 0 { now_ms() } else { self.ended_ms };
        Receipt {
            plan_id: plan.plan_id.clone(),
            status,
            samples: self.sample_count(),
            tokens_in: self.tokens().0,
            tokens_out: self.tokens().1,
            wall_ms: ended.saturating_sub(self.started_ms),
            plan_ms: self.plan_us() / 1000,
            run_ms: self.run_us() / 1000,
            busy_ms: self.busy_us() / 1000,
            max_parallel: self.max_parallel(),
            max_parallel_by_surface: self.max_parallel_by_surface(),
            nodes: plan.nodes.len(),
            depth: plan.depth(),
            effects,
            forks_taken: self.forks.len(),
            yield_reason,
            evidence,
            error,
            plan: plan.render(),
            ledger: self.clone(),
        }
    }
}

/// The one writer of ledger rows. Every arm records through this, so a row means the same
/// thing whether ours, a script, or a model loop produced it. Write-ness comes from the world
/// model, never from an arm's own list.
#[derive(Clone)]
pub struct Recorder {
    world: Arc<World>,
    rows: Arc<Mutex<Vec<Row>>>,
    next: Arc<AtomicU32>,
}

/// An effect in flight. Finishing it writes exactly one row.
pub struct Attempt {
    rec: Recorder,
    node: String,
    op: String,
    surface: String,
    key: Option<String>,
    attempt: u32,
    started_us: u128,
}

impl Recorder {
    pub fn new(world: Arc<World>) -> Self {
        Recorder { world, rows: Arc::new(Mutex::new(Vec::new())), next: Arc::new(AtomicU32::new(0)) }
    }

    /// A fresh node id for arms that have no plan (scripts and model loops).
    pub fn next_node_id(&self, prefix: &str) -> String {
        format!("{prefix}{}", self.next.fetch_add(1, Ordering::Relaxed) + 1)
    }

    pub fn start(&self, node: &str, op: &str, surface: &str, key: Option<String>, attempt: u32) -> Attempt {
        Attempt { rec: self.clone(), node: node.to_string(), op: op.to_string(), surface: surface.to_string(), key, attempt, started_us: now_us() }
    }

    pub fn is_write(&self, op: &str) -> bool {
        self.world.op(op).map(|o| o.is_write()).unwrap_or(false)
    }

    pub fn rows(&self) -> Vec<Row> {
        self.rows.lock().unwrap().clone()
    }

    /// Move every row into the ledger, in start order.
    pub fn drain_into(&self, ledger: &mut Ledger) {
        ledger.rows.extend(self.rows.lock().unwrap().drain(..));
        ledger.rows.sort_by_key(|r| r.started_us);
    }
}

impl Attempt {
    pub fn finish<E: std::fmt::Display>(self, res: &Result<Value, E>) {
        let row = Row {
            node: self.node,
            write: self.rec.is_write(&self.op),
            op: self.op,
            surface: self.surface,
            key: self.key,
            attempt: self.attempt,
            started_us: self.started_us,
            ended_us: now_us(),
            ok: res.is_ok(),
            error: res.as_ref().err().map(|e| e.to_string()),
            observed: res.as_ref().ok().cloned().unwrap_or(Value::Null),
        };
        self.rec.rows.lock().unwrap().push(row);
    }
}

/// Total length covered by a set of spans, overlaps counted once.
pub fn union_length<I: Iterator<Item = (u128, u128)>>(spans: I) -> u128 {
    let mut v: Vec<(u128, u128)> = spans.map(|(s, e)| (s, e.max(s))).collect();
    v.sort();
    let mut total = 0u128;
    let mut cur: Option<(u128, u128)> = None;
    for (s, e) in v {
        match cur {
            Some((cs, ce)) if s <= ce => cur = Some((cs, ce.max(e))),
            Some((cs, ce)) => {
                total += ce - cs;
                cur = Some((s, e));
            }
            None => cur = Some((s, e)),
        }
    }
    if let Some((cs, ce)) = cur {
        total += ce - cs;
    }
    total
}

pub fn max_overlap<I: Iterator<Item = (u128, u128)>>(spans: I) -> usize {
    let mut points: Vec<(u128, i32)> = Vec::new();
    for (s, e) in spans {
        points.push((s, 1));
        points.push((e.max(s + 1), -1));
    }
    // Ends before starts at the same instant, so touching spans don't count as overlapping.
    points.sort_by_key(|(t, d)| (*t, *d));
    let mut cur = 0i32;
    let mut best = 0i32;
    for (_, d) in points {
        cur += d;
        best = best.max(cur);
    }
    best.max(0) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn union_counts_overlaps_once() {
        assert_eq!(union_length([(0, 10), (5, 15)].into_iter()), 15);
        assert_eq!(union_length([(0, 10), (20, 30)].into_iter()), 20);
        assert_eq!(union_length([(0, 10), (0, 10)].into_iter()), 10);
        assert_eq!(union_length(std::iter::empty()), 0);
    }

    #[test]
    fn overlap_counts_in_flight() {
        assert_eq!(max_overlap([(0, 10), (5, 15), (12, 20)].into_iter()), 2);
        assert_eq!(max_overlap([(0, 10), (10, 20)].into_iter()), 1);
        assert_eq!(max_overlap([(0, 10), (0, 10), (0, 10)].into_iter()), 3);
        assert_eq!(max_overlap(std::iter::empty()), 0);
    }
}
