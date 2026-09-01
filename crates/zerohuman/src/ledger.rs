//! Append-only record of every attempt. The receipt is a view of this, never a claim.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::plan::Plan;

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

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Ledger {
    pub rows: Vec<Row>,
    pub samples: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub forks: Vec<Value>,
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
    pub max_parallel: usize,
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

    /// Sweep-line over rows that executed against a surface (waits excluded): the largest
    /// number of effects in flight at one instant. This is the decisive benchmark column.
    pub fn max_parallel(&self) -> usize {
        max_overlap(self.rows.iter().filter(|r| r.surface != "event").map(|r| (r.started_us, r.ended_us)))
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
            samples: self.samples,
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            wall_ms: ended.saturating_sub(self.started_ms),
            max_parallel: self.max_parallel(),
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
    fn overlap_counts_in_flight() {
        assert_eq!(max_overlap([(0, 10), (5, 15), (12, 20)].into_iter()), 2);
        assert_eq!(max_overlap([(0, 10), (10, 20)].into_iter()), 1);
        assert_eq!(max_overlap([(0, 10), (0, 10), (0, 10)].into_iter()), 3);
        assert_eq!(max_overlap(std::iter::empty()), 0);
    }
}
