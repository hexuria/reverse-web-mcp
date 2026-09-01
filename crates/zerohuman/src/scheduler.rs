//! L1. Runs the DAG as wide as its edges allow. Pools per surface, retries with the same
//! key, waits as edges, and a yield to the planner only at a declared fork or a broken
//! assumption. Zero model calls in here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
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
        m.insert("api".into(), 16);
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
}

impl Default for Policy {
    fn default() -> Self {
        Policy { max_attempts: 4, backoff_ms: 40, wait_timeout: Duration::from_secs(30), node_timeout: Duration::from_secs(20) }
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

fn resolve_arg(arg: &Arg, outputs: &HashMap<String, Value>) -> Result<Value, String> {
    match arg {
        Arg::Lit(v) => Ok(v.clone()),
        Arg::Ref { node, field } => {
            let out = outputs.get(node).ok_or_else(|| format!("output of {node} missing"))?;
            let obj = match out {
                Value::Array(a) => a.first().cloned().unwrap_or(Value::Null),
                other => other.clone(),
            };
            obj.get(field).cloned().ok_or_else(|| format!("output of {node} has no field {field}"))
        }
        Arg::List(xs) => Ok(Value::Array(xs.iter().map(|x| resolve_arg(x, outputs)).collect::<Result<_, _>>()?)),
    }
}

fn resolve_args(node: &Node, outputs: &HashMap<String, Value>) -> Result<Map<String, Value>, String> {
    let mut m = Map::new();
    for (k, a) in &node.args {
        m.insert(k.clone(), resolve_arg(a, outputs)?);
    }
    Ok(m)
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
    pub async fn run(&self, plan: &Plan, ledger: &mut Ledger) -> Outcome {
        let rec = self.recorder.clone();
        let outputs: Arc<Mutex<HashMap<String, Value>>> = Arc::new(Mutex::new(HashMap::new()));
        let sems: HashMap<String, Arc<Semaphore>> = self.pools.per_surface.iter().map(|(s, n)| (s.clone(), Arc::new(Semaphore::new(*n)))).collect();

        let mut indeg: HashMap<String, usize> = plan.nodes.iter().map(|n| (n.id.clone(), 0)).collect();
        let mut succ: HashMap<String, Vec<String>> = HashMap::new();
        for (a, b) in &plan.edges {
            *indeg.entry(b.clone()).or_default() += 1;
            succ.entry(a.clone()).or_default().push(b.clone());
        }
        let mut ready: Vec<String> = plan.nodes.iter().filter(|n| indeg[&n.id] == 0).map(|n| n.id.clone()).collect();
        let mut running: JoinSet<(String, Result<Value, Failure>)> = JoinSet::new();
        let mut outcome = Outcome { status: Status::Committed, yield_reason: None, evidence: None, error: None };
        let mut stopping = false;

        loop {
            if !stopping {
                for id in ready.drain(..) {
                    let node = plan.node(&id).unwrap().clone();
                    if let Some(gate) = plan.gates.iter().find(|g| g.node == id) {
                        if !gate.allowed {
                            let kind = gate_kind_name(&gate.kind).to_string();
                            running.spawn(async move { (id, Err(Failure::Gate(kind))) });
                            continue;
                        }
                    }
                    let args = match resolve_args(&node, &outputs.lock().unwrap()) {
                        Ok(a) => a,
                        Err(e) => {
                            running.spawn(async move { (id, Err(Failure::Fatal(e))) });
                            continue;
                        }
                    };
                    let effector = self.effectors.get(&node.surface).cloned();
                    let sem = sems.get(&node.surface).cloned();
                    let bus = self.bus.clone();
                    let policy = self.policy.clone();
                    let rec2 = rec.clone();
                    running.spawn(async move {
                        let r = execute(&node, args, effector, sem, bus, &policy, rec2).await;
                        (id, r)
                    });
                }
            } else {
                ready.clear();
            }

            let Some(joined) = running.join_next().await else { break };
            let (id, result) = match joined {
                Ok(x) => x,
                Err(e) => ("?".into(), Err(Failure::Fatal(format!("task panicked: {e}")))),
            };
            match result {
                Ok(v) => {
                    outputs.lock().unwrap().insert(id.clone(), v);
                    for s in succ.get(&id).cloned().unwrap_or_default() {
                        let d = indeg.get_mut(&s).unwrap();
                        *d -= 1;
                        if *d == 0 {
                            ready.push(s);
                        }
                    }
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
        if outcome.status == Status::Committed {
            let done = outputs.lock().unwrap().len();
            if done < plan.nodes.len() {
                outcome = Outcome {
                    status: Status::Error,
                    yield_reason: None,
                    evidence: None,
                    error: Some(format!("{done} of {} nodes completed", plan.nodes.len())),
                };
            }
        }
        outcome
    }
}

fn gate_kind_name(k: &crate::plan::GateKind) -> &'static str {
    match k {
        crate::plan::GateKind::External => "external",
        crate::plan::GateKind::Spend => "spend",
    }
}

async fn execute(
    node: &Node,
    args: Map<String, Value>,
    effector: Option<Arc<dyn Effector>>,
    sem: Option<Arc<Semaphore>>,
    bus: Option<Arc<EventBus>>,
    policy: &Policy,
    rec: Recorder,
) -> Result<Value, Failure> {
    if node.kind == OpKind::Event {
        let id = args.get("id").and_then(|v| v.as_u64());
        let attempt = rec.start(&node.id, &node.op, "event", None, 1);
        let res: Result<Value, String> = match &bus {
            Some(b) => b.wait_for(&node.op, id, policy.wait_timeout).await.map(|ev| serde_json::to_value(ev).unwrap()).map_err(|e| e.to_string()),
            None => Err(crate::events::BusError::Missing.to_string()),
        };
        attempt.finish(&res);
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
            Err(EffectError::Fatal(msg)) => return Err(Failure::Fatal(msg)),
        }
    }
}
