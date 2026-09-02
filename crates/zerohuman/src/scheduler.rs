//! L1. Runs the DAG as wide as its edges allow. Pools per surface, retries with the same
//! key, waits as edges, and a yield to the planner only at a declared fork or a broken
//! assumption. Zero model calls in here.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::effectors::{EffectError, Effector};
use crate::events::EventBus;
use crate::ledger::{now_ms, Ledger, Recorder, Status};
use crate::plan::{Arg, Node, Plan};
use crate::world::OpKind;

#[derive(Clone, Debug)]
pub struct Pools {
    pub per_surface: HashMap<String, usize>,
}

impl Default for Pools {
    fn default() -> Self {
        let mut m = HashMap::new();
        m.insert("api".into(), 64);
        m.insert("mcp".into(), 16);
        m.insert("webmcp".into(), 4);
        // A screen is one pair of hands.
        m.insert("a11y".into(), 1);
        m.insert("pixels".into(), 1);
        Pools { per_surface: m }
    }
}

#[derive(Clone, Debug)]
pub struct Policy {
    pub max_attempts: u32,
    pub backoff_ms: u64,
    pub wait_timeout: Duration,
    pub node_timeout: Duration,
    /// While a wait has not fired, glance at the world this often. A lost webhook is not a
    /// lost fact. Long enough that a normal wait never reads at all.
    pub check_every: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        // Six attempts at 40·2^n ms is ~1.3 s of backoff: enough to outlast a one-second rate-limit window.
        Policy {
            max_attempts: 6,
            backoff_ms: 40,
            wait_timeout: Duration::from_secs(30),
            node_timeout: Duration::from_secs(20),
            check_every: Duration::from_secs(3),
        }
    }
}

pub struct Scheduler {
    pub effectors: HashMap<String, Arc<dyn Effector>>,
    pub bus: Option<Arc<EventBus>>,
    pub pools: Pools,
    pub policy: Policy,
    pub recorder: Recorder,
}

#[derive(Clone, Debug)]
pub struct Outcome {
    pub status: Status,
    pub yield_reason: Option<String>,
    pub evidence: Option<Value>,
    pub error: Option<String>,
}

enum Failure {
    Fatal(String),
    Fork(Value),
    Gate(String),
}

fn resolve_arg(arg: &Arg, outputs: &HashMap<String, Value>, guarded: &HashSet<String>) -> Result<Value, String> {
    match arg {
        Arg::Lit(v) => Ok(v.clone()),
        Arg::Ref { node, field } => {
            let out = outputs.get(node).ok_or_else(|| format!("output of {node} missing"))?;
            let obj = match out {
                // A fork resolved by a declared default: the chosen element is the referent.
                Value::Object(o) if o.contains_key("__fork_default") => o["chosen"].clone(),
                // A list is only an acceptable referent when the producing node's fork guard
                // already proved it has exactly one element.
                Value::Array(a) if guarded.contains(node) && a.len() == 1 => a[0].clone(),
                Value::Array(a) => return Err(format!("output of {node} is a list of {} and {node} has no fork guard", a.len())),
                other => other.clone(),
            };
            obj.get(field).cloned().ok_or_else(|| format!("output of {node} has no field {field}"))
        }
        Arg::List(xs) => Ok(Value::Array(xs.iter().map(|x| resolve_arg(x, outputs, guarded)).collect::<Result<_, _>>()?)),
    }
}

fn resolve_args(node: &Node, outputs: &HashMap<String, Value>, guarded: &HashSet<String>) -> Result<Map<String, Value>, String> {
    let mut m = Map::new();
    for (k, a) in &node.args {
        m.insert(k.clone(), resolve_arg(a, outputs, guarded)?);
    }
    Ok(m)
}

/// The mutable state of one run: what is done, what is ready, what waits on what.
pub struct RunState {
    indeg: HashMap<String, usize>,
    succ: HashMap<String, Vec<String>>,
    outputs: HashMap<String, Value>,
    ready: Vec<String>,
    guarded: HashSet<String>,
}

impl RunState {
    /// Build from a plan, treating `done` nodes as already completed with those outputs.
    pub fn new(plan: &Plan, done: &HashMap<String, Value>) -> RunState {
        let mut indeg: HashMap<String, usize> = plan.nodes.iter().map(|n| (n.id.clone(), 0)).collect();
        let mut succ: HashMap<String, Vec<String>> = HashMap::new();
        for (a, b) in &plan.edges {
            *indeg.entry(b.clone()).or_default() += 1;
            succ.entry(a.clone()).or_default().push(b.clone());
        }
        for id in done.keys() {
            for s in succ.get(id).cloned().unwrap_or_default() {
                if let Some(d) = indeg.get_mut(&s) {
                    *d = d.saturating_sub(1);
                }
            }
        }
        let ready = plan.nodes.iter().filter(|n| !done.contains_key(&n.id) && indeg[&n.id] == 0).map(|n| n.id.clone()).collect();
        let guarded = plan.nodes.iter().filter(|n| n.fork.is_some()).map(|n| n.id.clone()).collect();
        RunState { indeg, succ, outputs: done.clone(), ready, guarded }
    }

    fn complete(&mut self, id: &str, output: Value) {
        self.outputs.insert(id.to_string(), output);
        for s in self.succ.get(id).cloned().unwrap_or_default() {
            // A successor already proven done (on resume) never re-runs.
            if self.outputs.contains_key(&s) {
                continue;
            }
            let d = self.indeg.get_mut(&s).unwrap();
            *d = d.saturating_sub(1);
            if *d == 0 {
                self.ready.push(s);
            }
        }
    }

    pub fn completed(&self) -> usize {
        self.outputs.len()
    }
}

/// Resolve a fired fork by a rule the intent declared. Only `lowest_id` exists today.
fn apply_fork_default(rule: Option<&str>, output: &Value) -> Option<Value> {
    let items = output.as_array()?;
    match rule? {
        "lowest_id" => items.iter().filter(|x| x.get("id").and_then(|i| i.as_u64()).is_some()).min_by_key(|x| x["id"].as_u64().unwrap()).cloned(),
        _ => None,
    }
}

/// The only fork conditions the world model uses today.
fn fork_fires(when: &str, output: &Value) -> Option<Value> {
    let count = output.as_array().map(|a| a.len()).unwrap_or(1);
    let fires = match when.trim() {
        "result.count != 1" => count != 1,
        "result.count == 0" => count == 0,
        _ => false,
    };
    if fires {
        Some(json!({"when": when, "count": count, "result": output}))
    } else {
        None
    }
}

impl Scheduler {
    /// Run a plan from the start.
    pub async fn run(&self, plan: &Plan, ledger: &mut Ledger) -> Outcome {
        let done = ledger.completed(plan);
        self.resume(plan, ledger, &done).await
    }

    /// Run a plan, skipping every node in `done` and using its recorded output. The ledger keeps
    /// its earlier rows; new rows are appended. With content-addressed keys this is safe to call
    /// after a fork answer or any other yield: nothing in `done` is re-sent. Rows live in the
    /// Recorder until the run ends, so this is not crash recovery.
    pub async fn resume(&self, plan: &Plan, ledger: &mut Ledger, done: &HashMap<String, Value>) -> Outcome {
        let rec = self.recorder.clone();
        let sems: HashMap<String, Arc<Semaphore>> = self.pools.per_surface.iter().map(|(s, n)| (s.clone(), Arc::new(Semaphore::new(*n)))).collect();
        let mut state = RunState::new(plan, done);
        let mut running: JoinSet<(String, Result<Value, Failure>)> = JoinSet::new();
        let mut outcome = Outcome { status: Status::Committed, yield_reason: None, evidence: None, error: None };
        let mut stopping = false;

        loop {
            if !stopping {
                for id in state.ready.drain(..).collect::<Vec<_>>() {
                    let node = plan.node(&id).unwrap().clone();
                    if let Some(gate) = plan.gates.iter().find(|g| g.node == id) {
                        if !gate.allowed {
                            let kind = gate_kind_name(&gate.kind).to_string();
                            running.spawn(async move { (id, Err(Failure::Gate(kind))) });
                            continue;
                        }
                    }
                    let args = match resolve_args(&node, &state.outputs, &state.guarded) {
                        Ok(a) => a,
                        Err(e) => {
                            running.spawn(async move { (id, Err(Failure::Fatal(e))) });
                            continue;
                        }
                    };
                    let effector = self.effectors.get(&node.surface).cloned();
                    let reader = self.effectors.get("api").cloned();
                    let sem = sems.get(&node.surface).cloned();
                    let bus = self.bus.clone();
                    let policy = self.policy.clone();
                    let rec2 = rec.clone();
                    running.spawn(async move {
                        let r = execute(&node, args, effector, reader, sem, bus, &policy, rec2).await;
                        (id, r)
                    });
                }
            } else {
                state.ready.clear();
            }

            let Some(joined) = running.join_next().await else { break };
            let (id, result) = match joined {
                Ok(x) => x,
                Err(e) => ("?".into(), Err(Failure::Fatal(format!("task panicked: {e}")))),
            };
            match result {
                Ok(v) => {
                    if let Some(rule) = v.get("__fork_default") {
                        // A resumed plan re-runs the unkeyed lookup, so record each node once.
                        if !ledger.forks.iter().any(|f| f.get("node").and_then(|n| n.as_str()) == Some(id.as_str())) {
                            ledger.forks.push(json!({"node": id, "ask": v["__fork_ask"], "evidence": v["__evidence"], "auto": rule, "chosen": v["chosen"]}));
                        }
                    }
                    state.complete(&id, v)
                }
                Err(Failure::Fork(ev)) => {
                    stopping = true;
                    let node = plan.node(&id).unwrap();
                    let ask = node.fork.as_ref().map(|f| f.ask.clone()).unwrap_or_default();
                    ledger.forks.push(json!({"node": id, "ask": ask, "evidence": ev}));
                    if outcome.status == Status::Committed {
                        outcome = Outcome { status: Status::NeedThink, yield_reason: Some(format!("fork at {id}: {ask}")), evidence: Some(ev), error: None };
                    }
                }
                Err(Failure::Gate(kind)) => {
                    stopping = true;
                    if outcome.status == Status::Committed {
                        outcome = Outcome { status: Status::NeedThink, yield_reason: Some(format!("gate at {id}: {kind}")), evidence: None, error: None };
                    }
                }
                Err(Failure::Fatal(e)) => {
                    stopping = true;
                    if outcome.status == Status::Committed {
                        outcome = Outcome { status: Status::Error, yield_reason: None, evidence: None, error: Some(format!("{id}: {e}")) };
                    }
                }
            }
        }

        rec.drain_into(ledger);
        ledger.ended_ms = now_ms();
        if outcome.status == Status::Committed && state.completed() < plan.nodes.len() {
            outcome = Outcome {
                status: Status::Error,
                yield_reason: None,
                evidence: None,
                error: Some(format!("{} of {} nodes completed", state.completed(), plan.nodes.len())),
            };
        }
        outcome
    }
}

/// Wait for the event; every `check_every` without it, read the world once and accept the fact
/// if it already holds. The wait row spans the whole wait; each glance is its own read row.
async fn wait_with_check(
    node: &Node,
    args: &Map<String, Value>,
    reader: Option<Arc<dyn Effector>>,
    bus: Option<Arc<EventBus>>,
    policy: &Policy,
    rec: &Recorder,
) -> Result<Value, String> {
    let id = args.get("id").and_then(|v| v.as_u64());
    let attempt = rec.start(&node.id, &node.op, "event", None, 1);
    let Some(bus) = bus else {
        let e = Err(crate::events::BusError::Missing.to_string());
        attempt.finish(&e);
        return e;
    };
    let deadline = tokio::time::Instant::now() + policy.wait_timeout;
    let mut glances = 0u32;
    let res: Result<Value, String> = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break Err(format!("timed out waiting for {}{}", node.op, id.map(|i| format!(" id={i}")).unwrap_or_default()));
        }
        let slice = remaining.min(policy.check_every);
        match bus.wait_for(&node.op, id, slice).await {
            Ok(ev) => break Ok(serde_json::to_value(ev).unwrap()),
            Err(crate::events::BusError::Timeout { .. }) => {}
            Err(e) => break Err(e.to_string()),
        }
        let (Some(check), Some(reader)) = (&node.check, &reader) else { continue };
        glances += 1;
        let mut read_args = Map::new();
        if let Some(v) = args.get(&check.arg) {
            read_args.insert(check.arg.clone(), v.clone());
        }
        let read_node =
            Node { id: format!("{}~{glances}", node.id), op: check.op.clone(), kind: OpKind::Http, surface: "api".into(), key: None, ..node.clone() };
        let glance = rec.start(&read_node.id, &read_node.op, "api", None, glances);
        let seen = reader.execute(&read_node, &read_args).await;
        glance.finish(&seen);
        if let Ok(v) = seen {
            if v.get(&check.field) == Some(&check.value) {
                break Ok(json!({"checked": true, "kind": node.op, "id": id, "state": v}));
            }
        }
    };
    attempt.finish(&res);
    res
}

fn gate_kind_name(k: &crate::plan::GateKind) -> &'static str {
    match k {
        crate::plan::GateKind::External => "external",
        crate::plan::GateKind::Spend => "spend",
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    node: &Node,
    args: Map<String, Value>,
    effector: Option<Arc<dyn Effector>>,
    reader: Option<Arc<dyn Effector>>,
    sem: Option<Arc<Semaphore>>,
    bus: Option<Arc<EventBus>>,
    policy: &Policy,
    rec: Recorder,
) -> Result<Value, Failure> {
    if node.kind == OpKind::Event {
        let res = wait_with_check(node, &args, reader, bus, policy, &rec).await;
        return res.map_err(Failure::Fatal);
    }

    let Some(effector) = effector else {
        return Err(Failure::Fatal(format!("no effector for surface {}", node.surface)));
    };
    let _permit = match sem {
        Some(s) => Some(s.acquire_owned().await.map_err(|e| Failure::Fatal(e.to_string()))?),
        None => None,
    };
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let recording = rec.start(&node.id, &node.op, &node.surface, node.key.clone(), attempt);
        let res = tokio::time::timeout(policy.node_timeout, effector.execute(node, &args)).await;
        let res: Result<Value, EffectError> = match res {
            Ok(r) => r,
            Err(_) => Err(EffectError::Retryable("node timeout".into())),
        };
        recording.finish(&res);
        match res {
            Ok(v) => {
                if let Some(f) = &node.fork {
                    if let Some(ev) = fork_fires(&f.when, &v) {
                        // A declared default answers the question without waking the planner.
                        if let Some(chosen) = apply_fork_default(f.default.as_deref(), &v) {
                            return Ok(
                                json!({"__fork_default": f.default, "__fork_ask": f.ask, "__evidence": ev, "result": [chosen.clone()], "chosen": chosen}),
                            );
                        }
                        return Err(Failure::Fork(ev));
                    }
                }
                return Ok(v);
            }
            Err(EffectError::Retryable(msg)) => {
                if attempt >= policy.max_attempts {
                    return Err(Failure::Fatal(format!("gave up after {attempt} attempts: {msg}")));
                }
                let backoff = policy.backoff_ms * (1u64 << (attempt - 1));
                tokio::time::sleep(Duration::from_millis(backoff)).await;
            }
            Err(EffectError::Throttled(after_ms, msg)) => {
                // Server-directed backoff does not count against the attempt budget the same
                // way: wait what it asked plus a little spread, so lanes do not all return at once.
                if attempt >= policy.max_attempts * 2 {
                    return Err(Failure::Fatal(format!("gave up after {attempt} attempts: {msg}")));
                }
                let spread = (attempt as u64 * 7) % 50;
                tokio::time::sleep(Duration::from_millis(after_ms + spread)).await;
            }
            Err(EffectError::Fatal(msg)) => return Err(Failure::Fatal(msg)),
        }
    }
}
