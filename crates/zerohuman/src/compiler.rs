//! Intent graph → physical plan.
//!
//! For each want, find an operation whose postcondition unifies with it, satisfy that
//! operation's requirements the same way, and record the node. Then derive edges from
//! read/write footprints, choose the cheapest available surface per node, stamp a key
//! on every write, and insert gates. Nothing here samples a model.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{json, Value};
use thiserror::Error;

use crate::intent::Intent;
use crate::plan::{Arg, Gate, GateKind, Node, Plan};
use crate::pred::{Pred, Val};
use crate::world::{Op, OpKind, World};

#[derive(Debug, Error)]
pub enum CompileError {
    #[error("cannot parse want '{0}': {1}")]
    Parse(String, #[source] crate::pred::ParseError),
    #[error("nothing in the world model can make '{0}' true")]
    Unsatisfiable(String),
    #[error("operation {0} has no available surface among {1:?}")]
    NoSurface(String, Vec<String>),
    #[error("argument '{0}' of {1} is unbound")]
    Unbound(String, String),
    #[error("the plan has a cycle: {0}")]
    Cycle(String),
    #[error("the intent compiles to no work at all")]
    Empty,
}

pub struct CompileOptions {
    pub plan_id: String,
    /// Surfaces this run may use, e.g. ["api"] or ["api","mcp","a11y"].
    pub surfaces: Vec<String>,
}

impl Default for CompileOptions {
    fn default() -> Self {
        CompileOptions { plan_id: "plan".into(), surfaces: vec!["api".into()] }
    }
}

type Bindings = HashMap<String, Val>;

struct Compiler<'a> {
    ops: Vec<Op>,
    intent: &'a Intent,
    opts: &'a CompileOptions,
    nodes: Vec<Node>,
    memo: HashMap<String, String>,
    deps: Vec<(String, String)>,
    /// Each node's content address: its operation plus its want with every reference expanded
    /// into the referenced node's own address. Stable under renumbering and reordering.
    canon: HashMap<String, String>,
}

pub fn compile(intent: &Intent, world: &World, opts: &CompileOptions) -> Result<Plan, CompileError> {
    let mut c = Compiler { ops: world.ops.clone(), intent, opts, nodes: Vec::new(), memo: HashMap::new(), deps: Vec::new(), canon: HashMap::new() };
    for w in &intent.wants {
        let pred = Pred::parse(w).map_err(|e| CompileError::Parse(w.clone(), e))?;
        for one in pred.unroll().map_err(|e| CompileError::Parse(w.clone(), e))? {
            c.satisfy(&one)?;
        }
    }
    if c.nodes.is_empty() {
        return Err(CompileError::Empty);
    }
    let mut edges = c.deps.clone();
    edges.extend(footprint_edges(&c.nodes));
    let edges = dedupe(edges);
    if let Some(path) = find_cycle(&c.nodes, &edges) {
        return Err(CompileError::Cycle(path));
    }
    let edges = reduce(edges, &c.nodes);
    let gates =
        c.nodes.iter().filter(|n| n.external).map(|n| Gate { node: n.id.clone(), kind: GateKind::External, allowed: intent.constraints.external_ok }).collect();
    Ok(Plan { plan_id: opts.plan_id.clone(), goal: intent.goal.clone(), nodes: c.nodes, edges, gates })
}

impl<'a> Compiler<'a> {
    fn next_id(&self) -> String {
        let i = self.nodes.len();
        let letter = (b'A' + (i % 26) as u8) as char;
        if i < 26 {
            letter.to_string()
        } else {
            format!("{letter}{}", i / 26)
        }
    }

    /// The want with every `$node.field` replaced by that node's content address.
    fn expand(&self, pred: &Pred) -> String {
        let bind = |_: &str| None;
        let p = pred.subst(&bind);
        let mut out = format!("{}(", p.entity);
        for (i, (k, v)) in p.args.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!("{k}={}", self.expand_val(v)));
        }
        out.push(')');
        if !p.field.is_empty() {
            out.push_str(&format!(".{}={}", p.field, self.expand_val(&p.value)));
        }
        out
    }

    fn expand_val(&self, v: &Val) -> String {
        match v {
            Val::Ref(n, f) => format!("@({}).{f}", self.canon.get(n).cloned().unwrap_or_else(|| n.clone())),
            Val::List(xs) | Val::Each(xs) => format!("[{}]", xs.iter().map(|x| self.expand_val(x)).collect::<Vec<_>>().join(",")),
            Val::All(x) => format!("all({})", self.expand_val(x)),
            Val::Entity(p) => self.expand(p),
            other => other.to_string(),
        }
    }

    /// Nested entity references become `$node.id` by satisfying the reference first.
    fn resolve_refs(&mut self, pred: &Pred) -> Result<Pred, CompileError> {
        let mut args = Vec::new();
        for (k, v) in &pred.args {
            args.push((k.clone(), self.resolve_val(v)?));
        }
        Ok(Pred { entity: pred.entity.clone(), args, field: pred.field.clone(), value: pred.value.clone() })
    }

    fn resolve_val(&mut self, v: &Val) -> Result<Val, CompileError> {
        Ok(match v {
            Val::Entity(inner) => {
                // `customer(id=11)` already identifies the entity: no lookup, no fork.
                if let Some(Val::Num(n)) = inner.arg("id") {
                    if inner.args.len() == 1 {
                        return Ok(Val::Num(*n));
                    }
                }
                let mut inner = (**inner).clone();
                if inner.field.is_empty() {
                    let resolved_exists = self
                        .ops
                        .iter()
                        .any(|o| o.produces.as_deref() == Some(&inner.entity) && o.post.as_ref().map(|p| p.field == "resolved").unwrap_or(false));
                    inner.field = if resolved_exists { "resolved".into() } else { "exists".into() };
                    inner.value = Val::Bool(true);
                }
                let node = self.satisfy(&inner)?;
                Val::Ref(node, "id".into())
            }
            Val::List(xs) => Val::List(xs.iter().map(|x| self.resolve_val(x)).collect::<Result<_, _>>()?),
            other => other.clone(),
        })
    }

    fn satisfy(&mut self, raw: &Pred) -> Result<String, CompileError> {
        let pred = self.resolve_refs(raw)?;
        let key = pred.to_string();
        if let Some(id) = self.memo.get(&key) {
            return Ok(id.clone());
        }

        // An existence want identified by a reference is already satisfied by that node.
        if pred.is_existence() {
            if let Some(Val::Ref(n, _)) = pred.arg("id") {
                self.memo.insert(key, n.clone());
                return Ok(n.clone());
            }
        }

        let mut candidates: Vec<Op> = self
            .ops
            .iter()
            .filter(|o| match &o.post {
                Some(p) => {
                    p.entity == pred.entity
                        && (p.field == pred.field || (pred.is_existence() && p.is_existence()))
                        && (pred.is_existence() && o.produces.as_deref() == Some(&pred.entity) || !pred.is_existence())
                }
                None => false,
            })
            .cloned()
            .collect();
        // "X exists" is satisfied by finding X before it is satisfied by making X. A read-only
        // resolver goes first; its fork yields to the planner if nothing (or too much) is found.
        if pred.is_existence() {
            candidates.sort_by_key(|o| (o.is_write(), o.name.clone()));
        }

        for op in candidates {
            let post = op.post.clone().unwrap();
            let mut b = Bindings::new();
            let identified_by_ref = !pred.is_existence() && post.args.len() == 1 && post.args[0].0 == "id" && pred.arg("id").is_none();
            let mut pred_for_unify = pred.clone();
            if identified_by_ref {
                // The op identifies the entity by id; the want identifies it by other args.
                let target = self.satisfy(&pred.as_exists())?;
                pred_for_unify = Pred {
                    entity: pred.entity.clone(),
                    args: vec![("id".into(), Val::Ref(target, "id".into()))],
                    field: pred.field.clone(),
                    value: pred.value.clone(),
                };
            }
            if pred.is_existence() && post.is_existence() && post.field != pred_for_unify.field {
                pred_for_unify.field = post.field.clone();
            }
            if !unify(&post, &pred_for_unify, &mut b) {
                continue;
            }
            let id = self.build(&op, b, &pred)?;
            self.memo.insert(key, id.clone());
            return Ok(id);
        }
        Err(CompileError::Unsatisfiable(key))
    }

    fn build(&mut self, op: &Op, b: Bindings, want: &Pred) -> Result<String, CompileError> {
        let bind = |n: &str| b.get(n).cloned();

        // Requirements first, so dependencies are created before this node.
        let mut deps: Vec<String> = Vec::new();
        for r in &op.requires {
            let r = r.subst(&bind);
            if r.is_existence() {
                let refs = refs_in(&r);
                if !refs.is_empty() {
                    deps.extend(refs);
                    continue;
                }
                // Identified by a literal id: the entity is known, nothing to resolve.
                if r.args.len() == 1 && matches!(r.arg("id"), Some(Val::Num(_))) {
                    continue;
                }
            }
            deps.push(self.satisfy(&r)?);
        }
        for v in b.values() {
            deps.extend(refs_in_val(v));
        }

        let id = self.next_id();
        let mut args = BTreeMap::new();
        for p in &op.params {
            match b.get(&p.name) {
                Some(v) => {
                    args.insert(p.name.clone(), val_to_arg(v).map_err(|n| CompileError::Unbound(n, op.name.clone()))?);
                }
                None => {
                    if let Some(d) = op.defaults.get(&p.name) {
                        args.insert(p.name.clone(), Arg::Lit(d.clone()));
                    }
                }
            }
        }
        if op.kind == OpKind::Event {
            if let Some(v) = b.get("id") {
                args.insert("id".into(), val_to_arg(v).map_err(|n| CompileError::Unbound(n, op.name.clone()))?);
            }
        }

        let surface = match op.kind {
            OpKind::Event => "event".to_string(),
            _ => op
                .surfaces
                .iter()
                .filter(|(s, _)| self.opts.surfaces.contains(s))
                .min_by_key(|(_, c)| **c)
                .map(|(s, _)| s.clone())
                .ok_or_else(|| CompileError::NoSurface(op.name.clone(), self.opts.surfaces.clone()))?,
        };

        let reads = op.reads.iter().flat_map(|r| resource(r, &b, &id)).collect();
        let writes = op.writes.iter().flat_map(|w| resource(w, &b, &id)).collect::<Vec<_>>();
        let canon = format!("{}|{}", op.name, self.expand(want));
        let key = if op.kind == OpKind::Http && !writes.is_empty() { Some(format!("{}/{}", self.opts.plan_id, content_hash(&canon))) } else { None };
        self.canon.insert(id.clone(), canon);
        let mut fork = op.fork.clone();
        if let Some(f) = &mut fork {
            if let Some(intent_fork) = self.intent.forks.iter().find(|x| x.when == f.when) {
                f.ask = intent_fork.ask.clone();
                f.default = intent_fork.default.clone();
            }
            f.ask = f.ask.replace("$name", b.get("name").and_then(|v| v.as_str()).unwrap_or("?"));
        }

        let post = op.post.as_ref().map(|p| p.subst(&bind).to_string()).unwrap_or_else(|| want.to_string());
        self.nodes.push(Node {
            id: id.clone(),
            op: op.name.clone(),
            kind: op.kind.clone(),
            args,
            surface,
            key,
            reads,
            writes,
            external: op.external,
            produces: op.produces.clone(),
            fork,
            ui: op.ui.clone(),
            check: op.check.clone(),
            before: op.before.clone(),
            post,
        });
        deps.sort();
        deps.dedup();
        for d in deps {
            if d != id {
                self.deps.push((d, id.clone()));
            }
        }
        Ok(id)
    }
}

/// Sixteen hex chars of SHA-256: enough to never collide inside one plan, short enough to read.
fn content_hash(canon: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(canon.as_bytes());
    hex::encode(&digest[..8])
}

/// Bind the op's postcondition variables against a concrete want.
fn unify(post: &Pred, want: &Pred, b: &mut Bindings) -> bool {
    if post.entity != want.entity || post.field != want.field {
        return false;
    }
    if !unify_val(&post.value, &want.value, b) {
        return false;
    }
    for (k, pv) in &post.args {
        match want.arg(k) {
            Some(wv) => {
                if !unify_val(pv, wv, b) {
                    return false;
                }
            }
            None => {
                if !matches!(pv, Val::Var(..)) {
                    return false;
                }
            }
        }
    }
    for (k, _) in &want.args {
        if post.arg(k).is_none() {
            return false;
        }
    }
    true
}

fn unify_val(pattern: &Val, concrete: &Val, b: &mut Bindings) -> bool {
    match pattern {
        Val::Var(n, _) => match b.get(n) {
            Some(existing) => existing == concrete,
            None => {
                b.insert(n.clone(), concrete.clone());
                true
            }
        },
        Val::List(ps) => match concrete {
            Val::List(cs) if ps.len() == cs.len() => ps.iter().zip(cs).all(|(p, c)| unify_val(p, c, b)),
            _ => false,
        },
        other => other == concrete,
    }
}

fn refs_in(p: &Pred) -> Vec<String> {
    p.args.iter().flat_map(|(_, v)| refs_in_val(v)).collect()
}

fn refs_in_val(v: &Val) -> Vec<String> {
    match v {
        Val::Ref(n, _) => vec![n.clone()],
        Val::List(xs) => xs.iter().flat_map(refs_in_val).collect(),
        _ => vec![],
    }
}

fn val_to_arg(v: &Val) -> Result<Arg, String> {
    Ok(match v {
        Val::Str(s) => Arg::Lit(Value::String(s.clone())),
        Val::Num(n) => Arg::Lit(json!(n)),
        Val::Bool(b) => Arg::Lit(json!(b)),
        Val::List(xs) => Arg::List(xs.iter().map(val_to_arg).collect::<Result<_, _>>()?),
        Val::Ref(n, f) => Arg::Ref { node: n.clone(), field: f.clone() },
        Val::Var(n, _) => return Err(n.clone()),
        Val::Entity(p) => return Err(p.to_string()),
        Val::Each(_) => return Err("each(...) survived unrolling".into()),
        Val::All(_) => return Err("all(...) survived unrolling".into()),
    })
}

/// `entity:selector` → concrete resource tokens. `@X` means "the entity node X produced".
fn resource(spec: &str, b: &Bindings, node: &str) -> Vec<String> {
    let (entity, sel) = match spec.split_once(':') {
        Some(x) => x,
        None => return vec![spec.to_string()],
    };
    match sel {
        "*" => vec![format!("{entity}:*")],
        "new" => vec![format!("{entity}:@{node}")],
        s if s.starts_with('$') => {
            let name = s.trim_start_matches('$').trim_end_matches("[]");
            match b.get(name) {
                Some(v) => val_tokens(entity, v),
                None => vec![format!("{entity}:*")],
            }
        }
        s => vec![format!("{entity}:{s}")],
    }
}

fn val_tokens(entity: &str, v: &Val) -> Vec<String> {
    match v {
        Val::Ref(n, _) => vec![format!("{entity}:@{n}")],
        Val::Num(x) => vec![format!("{entity}:{x}")],
        Val::Str(s) => vec![format!("{entity}:{s}")],
        Val::List(xs) => xs.iter().flat_map(|x| val_tokens(entity, x)).collect(),
        _ => vec![format!("{entity}:*")],
    }
}

fn conflicts(a: &str, b: &str) -> bool {
    let (ea, ia) = a.split_once(':').unwrap_or((a, "*"));
    let (eb, ib) = b.split_once(':').unwrap_or((b, "*"));
    ea == eb && (ia == "*" || ib == "*" || ia == ib)
}

fn any_conflict(xs: &[String], ys: &[String]) -> bool {
    xs.iter().any(|x| ys.iter().any(|y| conflicts(x, y)))
}

/// Two nodes touching the same data are ordered by what the data says, not by the order the
/// wants happened to be written in:
///
/// - a writer precedes a reader, so a reader always sees the final state;
/// - two writers are ordered by the world model's declared precedence, and only when it is
///   silent does the want order decide.
///
/// Disjoint footprints produce no edge, and that absence is where the parallelism comes from.
fn footprint_edges(nodes: &[Node]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for i in 0..nodes.len() {
        for j in (i + 1)..nodes.len() {
            let (a, b) = (&nodes[i], &nodes[j]);
            let a_then_b = any_conflict(&a.writes, &b.reads);
            let b_then_a = any_conflict(&b.writes, &a.reads);
            let both_write = any_conflict(&a.writes, &b.writes);
            if both_write {
                if b.before.contains(&a.op) {
                    out.push((b.id.clone(), a.id.clone()));
                } else {
                    out.push((a.id.clone(), b.id.clone()));
                }
            } else if a_then_b && !b_then_a {
                out.push((a.id.clone(), b.id.clone()));
            } else if b_then_a && !a_then_b {
                out.push((b.id.clone(), a.id.clone()));
            } else if a_then_b && b_then_a {
                // Each reads what the other writes and neither writes the same thing: the order
                // is genuinely undetermined, so take the order the wants were written in.
                out.push((a.id.clone(), b.id.clone()));
            }
        }
    }
    out
}

/// The first cycle found, as a readable path, or None.
fn find_cycle(nodes: &[Node], edges: &[(String, String)]) -> Option<String> {
    use std::collections::HashMap;
    let mut succ: HashMap<&str, Vec<&str>> = HashMap::new();
    for (a, b) in edges {
        succ.entry(a.as_str()).or_default().push(b.as_str());
    }
    let mut state: HashMap<&str, u8> = HashMap::new();
    let mut stack: Vec<&str> = Vec::new();
    fn walk<'a>(n: &'a str, succ: &HashMap<&'a str, Vec<&'a str>>, state: &mut HashMap<&'a str, u8>, stack: &mut Vec<&'a str>) -> Option<String> {
        match state.get(n) {
            Some(1) => {
                let from = stack.iter().position(|x| *x == n).unwrap_or(0);
                let mut path: Vec<&str> = stack[from..].to_vec();
                path.push(n);
                return Some(path.join(" → "));
            }
            Some(2) => return None,
            _ => {}
        }
        state.insert(n, 1);
        stack.push(n);
        for m in succ.get(n).into_iter().flatten() {
            if let Some(c) = walk(m, succ, state, stack) {
                return Some(c);
            }
        }
        stack.pop();
        state.insert(n, 2);
        None
    }
    for n in nodes {
        if let Some(c) = walk(n.id.as_str(), &succ, &mut state, &mut stack) {
            return Some(c);
        }
    }
    None
}

fn dedupe(edges: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut seen = HashSet::new();
    edges.into_iter().filter(|e| e.0 != e.1 && seen.insert(e.clone())).collect()
}

/// Transitive reduction, so the plan file shows only the edges that carry information.
fn reduce(edges: Vec<(String, String)>, nodes: &[Node]) -> Vec<(String, String)> {
    let order: HashMap<&str, usize> = nodes.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect();
    let mut kept: Vec<(String, String)> = Vec::new();
    let mut sorted = edges;
    sorted.sort_by_key(|(a, b)| (order.get(b.as_str()).copied(), order.get(a.as_str()).copied()));
    for (a, b) in sorted.iter() {
        let others: Vec<&(String, String)> = sorted.iter().filter(|e| !(e.0 == *a && e.1 == *b)).collect();
        if !reachable(a, b, &others) {
            kept.push((a.clone(), b.clone()));
        }
    }
    kept
}

fn reachable(from: &str, to: &str, edges: &[&(String, String)]) -> bool {
    let mut stack = vec![from];
    let mut seen = HashSet::new();
    while let Some(x) = stack.pop() {
        for (a, b) in edges {
            if a == x && seen.insert(b.as_str()) {
                if b == to {
                    return true;
                }
                stack.push(b);
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> World {
        let doc: Value = serde_json::from_str(include_str!("../../app/static/openapi.json")).unwrap();
        World::from_openapi(&doc).unwrap()
    }

    fn intent(wants: &[&str]) -> Intent {
        Intent { goal: "test".into(), wants: wants.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    #[test]
    fn two_customers_are_two_lanes() {
        let plan = compile(
            &intent(&[
                "invoice(customer=customer(name='Acme')).exists",
                "invoice(customer=customer(name='Acme')).status='sent'",
                "invoice(customer=customer(name='Globex')).exists",
                "invoice(customer=customer(name='Globex')).status='sent'",
            ]),
            &world(),
            &CompileOptions::default(),
        )
        .unwrap();
        println!("{}", plan.render());
        // lookup, create, send per customer
        assert_eq!(plan.nodes.len(), 6);
        // No edge crosses from the Acme lane into the Globex lane.
        let lane_a: HashSet<&str> = ["A", "B", "C"].into();
        for (x, y) in &plan.edges {
            assert_eq!(lane_a.contains(x.as_str()), lane_a.contains(y.as_str()), "edge {x}->{y} crosses lanes");
        }
        assert_eq!(plan.depth(), 3);
        assert!(plan.node("B").unwrap().key.is_some());
        assert!(plan.node("A").unwrap().key.is_none());
        assert!(plan.node("A").unwrap().fork.is_some());
    }

    /// Walk args back to the lookup node and read the customer name this lane is about.
    fn lane_name(plan: &Plan, node: &Node) -> String {
        let mut cur = node;
        loop {
            if let Some(Arg::Lit(Value::String(name))) = cur.args.get("name") {
                return name.clone();
            }
            let next = cur.args.values().flat_map(|a| a.refs()).next().expect("a ref back to the lookup");
            cur = plan.node(&next).unwrap();
        }
    }

    #[test]
    fn each_fans_out_into_independent_lanes() {
        let plan = compile(
            &intent(&[
                "invoice(customer=customer(name=each(['Acme','Globex','Initech']))).exists",
                "invoice(customer=customer(name=each(['Acme','Globex','Initech']))).status='sent'",
            ]),
            &world(),
            &CompileOptions::default(),
        )
        .unwrap();
        assert_eq!(plan.nodes.len(), 9, "{}", plan.render());
        assert_eq!(plan.depth(), 3);
        assert_eq!(plan.nodes.iter().filter(|n| n.op == "sendInvoice").count(), 3);
        // Same keys as the fully written-out form.
        let long = compile(
            &intent(&[
                "invoice(customer=customer(name='Acme')).exists",
                "invoice(customer=customer(name='Globex')).exists",
                "invoice(customer=customer(name='Initech')).exists",
                "invoice(customer=customer(name='Acme')).status='sent'",
                "invoice(customer=customer(name='Globex')).status='sent'",
                "invoice(customer=customer(name='Initech')).status='sent'",
            ]),
            &world(),
            &CompileOptions::default(),
        )
        .unwrap();
        let mut k1: Vec<_> = plan.nodes.iter().filter_map(|n| n.key.clone()).collect();
        let mut k2: Vec<_> = long.nodes.iter().filter_map(|n| n.key.clone()).collect();
        k1.sort();
        k2.sort();
        assert_eq!(k1, k2);
    }

    #[test]
    fn three_hundred_lanes_compile_fast() {
        let names: Vec<String> = (1..=300).map(|i| format!("'Customer {i:03}'")).collect();
        let list = names.join(",");
        let t0 = std::time::Instant::now();
        let plan = compile(
            &intent(&[
                &format!("invoice(customer=customer(name=each([{list}]))).exists"),
                &format!("invoice(customer=customer(name=each([{list}]))).status='sent'"),
            ]),
            &world(),
            &CompileOptions::default(),
        )
        .unwrap();
        let took = t0.elapsed();
        assert_eq!(plan.nodes.len(), 900);
        assert_eq!(plan.depth(), 3);
        // 0.12 s in release; the generous bound here only has to catch a return to quadratic.
        assert!(took < std::time::Duration::from_secs(10), "compile took {took:?}");
    }

    #[test]
    fn a_report_per_invoice_and_one_over_all() {
        let three = "each([customer(name='Acme'),customer(name='Globex'),customer(name='Initech')])";
        let plan = compile(
            &intent(&[
                &format!("invoice(customer={three}).exists"),
                &format!("report(invoices=[invoice(customer={three})]).exists"),
                &format!("report(invoices=[all(invoice(customer={three}))]).exists"),
            ]),
            &world(),
            &CompileOptions::default(),
        )
        .unwrap();
        let reports: Vec<&Node> = plan.nodes.iter().filter(|n| n.op == "createReport").collect();
        assert_eq!(reports.len(), 4, "three per-invoice and one over all\n{}", plan.render());
        let over_all = reports.iter().find(|n| plan.preds_of(&n.id).len() == 3).expect("one report joins all three");
        assert!(reports.iter().filter(|n| n.id != over_all.id).all(|n| plan.preds_of(&n.id).len() == 1));
    }

    #[test]
    fn one_report_over_every_invoice_from_one_want() {
        let plan = compile(
            &intent(&[
                "invoice(customer=each([customer(name='Acme'),customer(name='Globex'),customer(name='Initech')])).exists",
                "invoice(customer=each([customer(name='Acme'),customer(name='Globex'),customer(name='Initech')])).status='sent'",
                "report(invoices=[all(invoice(customer=each([customer(name='Acme'),customer(name='Globex'),customer(name='Initech')])))]).exists",
            ]),
            &world(),
            &CompileOptions::default(),
        )
        .unwrap();
        assert_eq!(plan.nodes.iter().filter(|n| n.op == "createReport").count(), 1, "{}", plan.render());
        assert_eq!(plan.nodes.iter().filter(|n| n.op == "sendInvoice").count(), 3);
        let report = plan.nodes.iter().find(|n| n.op == "createReport").unwrap();
        assert_eq!(plan.preds_of(&report.id).len(), 3, "the report joins all three lanes");
    }

    /// The order the wants were written in must not change the plan.
    #[test]
    fn edges_follow_the_data_not_the_want_order() {
        let two = "each([customer(name='Acme'),customer(name='Globex')])";
        let natural = intent(&[
            &format!("invoice(customer={two}).exists"),
            &format!("invoice(customer={two}).status='sent'"),
            &format!("report(invoices=[all(invoice(customer={two}))]).exists"),
        ]);
        let mut reversed = natural.clone();
        reversed.wants.reverse();
        for (name, i) in [("natural", &natural), ("reversed", &reversed)] {
            let plan = compile(i, &world(), &CompileOptions::default()).unwrap();
            let report = plan.nodes.iter().find(|n| n.op == "createReport").unwrap();
            let sends: Vec<&Node> = plan.nodes.iter().filter(|n| n.op == "sendInvoice").collect();
            assert_eq!(sends.len(), 2, "{name}\n{}", plan.render());
            for s in &sends {
                assert!(reaches(&plan, &s.id, &report.id), "{name}: the report must wait for send {}\n{}", s.id, plan.render());
                assert!(!reaches(&plan, &report.id, &s.id), "{name}: the report must not precede send {}", s.id);
            }
        }
    }

    /// Approval precedes sending because the world model says so, not because of want order.
    #[test]
    fn declared_precedence_orders_two_writers() {
        let one = "customer(name='Acme')";
        let opts = CompileOptions { plan_id: "p".into(), surfaces: vec!["api".into(), "a11y".into()] };
        for wants in [
            vec![format!("invoice(customer={one}).approved=true"), format!("invoice(customer={one}).status='sent'")],
            vec![format!("invoice(customer={one}).status='sent'"), format!("invoice(customer={one}).approved=true")],
        ] {
            let i = Intent { goal: "t".into(), wants, ..Default::default() };
            let plan = compile(&i, &world(), &opts).unwrap();
            let approve = plan.nodes.iter().find(|n| n.op == "approveInvoice").unwrap();
            let send = plan.nodes.iter().find(|n| n.op == "sendInvoice").unwrap();
            assert!(reaches(&plan, &approve.id, &send.id), "approve before send\n{}", plan.render());
            assert!(!reaches(&plan, &send.id, &approve.id));
        }
    }

    fn reaches(plan: &Plan, from: &str, to: &str) -> bool {
        let mut stack = vec![from.to_string()];
        let mut seen = std::collections::HashSet::new();
        while let Some(x) = stack.pop() {
            for (a, b) in &plan.edges {
                if *a == x && seen.insert(b.clone()) {
                    if b == to {
                        return true;
                    }
                    stack.push(b.clone());
                }
            }
        }
        false
    }

    #[test]
    fn keys_survive_node_renumbering() {
        let a = intent(&[
            "invoice(customer=customer(name='Acme')).exists",
            "invoice(customer=customer(name='Acme')).status='sent'",
            "invoice(customer=customer(name='Globex')).exists",
            "invoice(customer=customer(name='Globex')).status='sent'",
        ]);
        let mut reversed = a.clone();
        reversed.wants.reverse();
        let p1 = compile(&a, &world(), &CompileOptions::default()).unwrap();
        let p2 = compile(&reversed, &world(), &CompileOptions::default()).unwrap();
        let send = |p: &Plan, name: &str| p.nodes.iter().find(|n| n.op == "sendInvoice" && lane_name(p, n) == name).cloned().unwrap();
        let (s1, s2) = (send(&p1, "Acme"), send(&p2, "Acme"));
        assert_ne!(s1.id, s2.id, "the node was renumbered");
        assert_eq!(s1.key, s2.key, "but its key is the same");
        assert_ne!(send(&p1, "Acme").key, send(&p1, "Globex").key);
        assert!(s1.key.as_deref().unwrap().starts_with("plan/"));
        assert_eq!(s1.key.as_deref().unwrap().len(), "plan/".len() + 16);
    }

    #[test]
    fn a_literal_id_needs_no_lookup_and_no_fork() {
        let plan = compile(
            &intent(&["invoice(customer=customer(id=11)).exists", "invoice(customer=customer(id=11)).status='sent'"]),
            &world(),
            &CompileOptions::default(),
        )
        .unwrap();
        assert_eq!(plan.nodes.len(), 2, "{}", plan.render());
        assert!(plan.nodes.iter().all(|n| n.op != "listCustomers"));
        assert_eq!(plan.node("A").unwrap().args.get("customer_id"), Some(&Arg::Lit(json!(11))));
    }

    #[test]
    fn report_waits_for_sends_and_emails_start_early() {
        let plan = compile(
            &intent(&[
                "invoice(customer=customer(name='Acme')).exists",
                "invoice(customer=customer(name='Acme')).status='sent'",
                "invoice(customer=customer(name='Globex')).exists",
                "invoice(customer=customer(name='Globex')).status='sent'",
                "invoice(customer=customer(name='Initech')).exists",
                "invoice(customer=customer(name='Initech')).status='sent'",
                "report(invoices=[invoice(customer=customer(name='Acme')),invoice(customer=customer(name='Globex')),invoice(customer=customer(name='Initech'))]).exists",
            ]),
            &world(),
            &CompileOptions::default(),
        )
        .unwrap();
        println!("{}", plan.render());
        let report = plan.nodes.iter().find(|n| n.op == "createReport").unwrap();
        let sends: Vec<&Node> = plan.nodes.iter().filter(|n| n.op == "sendInvoice").collect();
        assert_eq!(sends.len(), 3);
        let preds = plan.preds_of(&report.id);
        for s in &sends {
            assert!(preds.contains(&s.id.as_str()), "report must wait for send {}", s.id);
        }
        // Each send depends only on its own create, never on another lane's.
        for s in &sends {
            assert_eq!(plan.preds_of(&s.id).len(), 1);
        }
    }

    #[test]
    fn receipt_waits_on_the_payment_event() {
        let plan = compile(
            &intent(&[
                "invoice(customer=customer(name='Acme')).exists",
                "invoice(customer=customer(name='Acme')).status='sent'",
                "invoice(customer=customer(name='Acme')).receipt_sent=true",
            ]),
            &world(),
            &CompileOptions::default(),
        )
        .unwrap();
        println!("{}", plan.render());
        let wait = plan.nodes.iter().find(|n| n.kind == OpKind::Event).expect("a wait node");
        assert_eq!(wait.op, "payment.received");
        let receipt = plan.nodes.iter().find(|n| n.op == "sendReceipt").unwrap();
        assert!(plan.preds_of(&receipt.id).contains(&wait.id.as_str()));
    }

    #[test]
    fn ui_only_approve_needs_a_screen_surface() {
        let i = intent(&["invoice(customer=customer(name='Acme')).exists", "invoice(customer=customer(name='Acme')).approved=true"]);
        let err = compile(&i, &world(), &CompileOptions::default()).unwrap_err();
        assert!(matches!(err, CompileError::NoSurface(..)), "{err}");
        let plan = compile(&i, &world(), &CompileOptions { plan_id: "p".into(), surfaces: vec!["api".into(), "a11y".into()] }).unwrap();
        let approve = plan.nodes.iter().find(|n| n.op == "approveInvoice").unwrap();
        assert_eq!(approve.surface, "a11y");
        assert_eq!(approve.kind, OpKind::Ui);
    }
}
