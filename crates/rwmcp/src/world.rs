//! The world model, derived from the target app's OpenAPI document. Never authored by hand.
//!
//! Each operation carries an `x-reverse-webmcp` block: postcondition, requirements, read/write
//! footprint, which surfaces expose it and at what cost. Events and UI-only actions come
//! from `x-reverse-webmcp-events` and `x-reverse-webmcp-ui` at the document root.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use thiserror::Error;

use crate::pred::{ParseError, Pred, Val};

#[derive(Debug, Error)]
pub enum WorldError {
    #[error("openapi document has no paths")]
    NoPaths,
    #[error("{method} {path}: no operationId")]
    MissingOperationId { method: String, path: String },
    #[error("{op}: {field}: {source}")]
    Predicate {
        op: String,
        field: &'static str,
        #[source]
        source: ParseError,
    },
    #[error("fetching openapi: {0}")]
    Fetch(String),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ParamIn {
    Path,
    Query,
    Body,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    pub location: ParamIn,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fork {
    pub when: String,
    pub ask: String,
    /// Declared by the intent: resolve without waking the planner. `lowest_id` today.
    #[serde(default)]
    pub default: Option<String>,
}

/// How to confirm an event's postcondition by reading the world, for when the event is lost.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Check {
    pub op: String,
    pub arg: String,
    pub field: String,
    pub value: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiSpec {
    pub route: String,
    pub control: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OpKind {
    /// An HTTP operation from `paths`.
    Http,
    /// A UI-only action from `x-reverse-webmcp-ui`.
    Ui,
    /// Something the outside world does; satisfied by waiting on an event.
    Event,
}

#[derive(Clone, Debug)]
pub struct Op {
    pub name: String,
    pub kind: OpKind,
    pub method: String,
    pub path: String,
    pub params: Vec<Param>,
    pub post: Option<Pred>,
    pub requires: Vec<Pred>,
    pub produces: Option<String>,
    pub reads: Vec<String>,
    pub writes: Vec<String>,
    pub external: bool,
    pub surfaces: BTreeMap<String, u32>,
    pub defaults: BTreeMap<String, Value>,
    pub fork: Option<Fork>,
    pub ui: Option<UiSpec>,
    pub check: Option<Check>,
    /// Operations this one must precede when both touch the same entity. Two writes to one
    /// thing have no order in a footprint; only the world model knows, so it says so here.
    pub before: Vec<String>,
}

impl Op {
    pub fn is_write(&self) -> bool {
        !self.writes.is_empty()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entity {
    pub name: String,
    pub id: String,
    pub fields: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct World {
    pub entities: Vec<Entity>,
    pub ops: Vec<Op>,
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|s| s.as_str().map(|s| s.to_string())).collect()).unwrap_or_default()
}

fn preds(v: Option<&Value>, op: &str, field: &'static str) -> Result<Vec<Pred>, WorldError> {
    strings(v).iter().map(|s| Pred::parse(s).map_err(|source| WorldError::Predicate { op: op.to_string(), field, source })).collect()
}

fn post(v: Option<&Value>, op: &str) -> Result<Option<Pred>, WorldError> {
    v.and_then(|p| p.as_str()).map(|s| Pred::parse(s).map_err(|source| WorldError::Predicate { op: op.to_string(), field: "post", source })).transpose()
}

fn surfaces(v: Option<&Value>) -> BTreeMap<String, u32> {
    v.and_then(|x| x.as_object()).map(|o| o.iter().filter_map(|(k, c)| c.as_u64().map(|c| (k.clone(), c as u32))).collect()).unwrap_or_default()
}

fn fork(v: Option<&Value>) -> Option<Fork> {
    let o = v?.as_object()?;
    Some(Fork { when: o.get("when")?.as_str()?.to_string(), ask: o.get("ask").and_then(|a| a.as_str()).unwrap_or("").to_string(), default: None })
}

impl World {
    pub fn from_openapi(doc: &Value) -> Result<World, WorldError> {
        let mut entities = Vec::new();
        if let Some(es) = doc.get("x-reverse-webmcp-entities").and_then(|e| e.as_object()) {
            for (name, spec) in es {
                entities.push(Entity {
                    name: name.clone(),
                    id: spec.get("id").and_then(|i| i.as_str()).unwrap_or("id").to_string(),
                    fields: strings(spec.get("fields")),
                });
            }
        }

        let mut ops = Vec::new();
        let paths = doc.get("paths").and_then(|p| p.as_object()).ok_or(WorldError::NoPaths)?;
        for (path, methods) in paths {
            let Some(methods) = methods.as_object() else { continue };
            for (method, spec) in methods {
                let Some(zh) = spec.get("x-reverse-webmcp") else { continue };
                let name = spec
                    .get("operationId")
                    .and_then(|s| s.as_str())
                    .ok_or_else(|| WorldError::MissingOperationId { method: method.clone(), path: path.clone() })?
                    .to_string();
                let mut params = Vec::new();
                if let Some(ps) = spec.get("parameters").and_then(|p| p.as_array()) {
                    for p in ps {
                        let pname = p.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                        let location = match p.get("in").and_then(|i| i.as_str()) {
                            Some("path") => ParamIn::Path,
                            _ => ParamIn::Query,
                        };
                        params.push(Param { name: pname, location });
                    }
                }
                if let Some(props) = spec.pointer("/requestBody/content/application~1json/schema/properties").and_then(|p| p.as_object()) {
                    for k in props.keys() {
                        params.push(Param { name: k.clone(), location: ParamIn::Body });
                    }
                }
                let post = post(zh.get("post"), &name)?;
                ops.push(Op {
                    name: name.clone(),
                    kind: OpKind::Http,
                    method: method.to_uppercase(),
                    path: path.clone(),
                    params,
                    post,
                    requires: preds(zh.get("requires"), &name, "requires")?,
                    produces: zh.get("produces").and_then(|p| p.as_str()).map(|s| s.to_string()),
                    reads: strings(zh.get("reads")),
                    writes: strings(zh.get("writes")),
                    external: zh.get("external").and_then(|e| e.as_bool()).unwrap_or(false),
                    surfaces: surfaces(zh.get("surfaces")),
                    defaults: zh
                        .get("defaults")
                        .and_then(|d| d.as_object())
                        .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                        .unwrap_or_default(),
                    fork: fork(zh.get("fork")),
                    ui: None,
                    check: None,
                    before: strings(zh.get("before")),
                });
            }
        }

        if let Some(ui) = doc.get("x-reverse-webmcp-ui").and_then(|u| u.as_object()) {
            for (name, zh) in ui {
                let post = post(zh.get("post"), name)?;
                let route = zh.get("route").and_then(|r| r.as_str()).unwrap_or("/").to_string();
                let mut params = Vec::new();
                for seg in route.split('/') {
                    if let Some(p) = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                        params.push(Param { name: p.to_string(), location: ParamIn::Path });
                    }
                }
                ops.push(Op {
                    name: name.clone(),
                    kind: OpKind::Ui,
                    method: "UI".into(),
                    path: route.clone(),
                    params,
                    post,
                    requires: preds(zh.get("requires"), name, "requires")?,
                    produces: None,
                    reads: strings(zh.get("reads")),
                    writes: strings(zh.get("writes")),
                    external: false,
                    surfaces: surfaces(zh.get("surfaces")),
                    defaults: BTreeMap::new(),
                    fork: None,
                    ui: Some(UiSpec { route, control: zh.get("control").cloned().unwrap_or(Value::Null) }),
                    check: None,
                    before: strings(zh.get("before")),
                });
            }
        }

        if let Some(evs) = doc.get("x-reverse-webmcp-events").and_then(|e| e.as_object()) {
            for (name, zh) in evs {
                let post = post(zh.get("post"), name)?;
                ops.push(Op {
                    name: name.clone(),
                    kind: OpKind::Event,
                    method: "EVENT".into(),
                    path: String::new(),
                    params: vec![Param { name: "id".into(), location: ParamIn::Query }],
                    post,
                    requires: vec![],
                    produces: None,
                    reads: vec![],
                    writes: vec![],
                    external: false,
                    surfaces: BTreeMap::new(),
                    defaults: BTreeMap::new(),
                    fork: None,
                    ui: None,
                    before: vec![],
                    check: zh.get("check").and_then(|c| serde_json::from_value(c.clone()).ok()),
                });
            }
        }

        Ok(World { entities, ops })
    }

    pub fn op(&self, name: &str) -> Option<&Op> {
        self.ops.iter().find(|o| o.name == name)
    }

    /// A compact description for a planner prompt: what exists and what can be made true.
    pub fn summary(&self) -> String {
        let mut out = String::new();
        out.push_str("entities:\n");
        for e in &self.entities {
            out.push_str(&format!("  {}({}): {}\n", e.name, e.id, e.fields.join(", ")));
        }
        out.push_str("can make true:\n");
        for o in &self.ops {
            if let Some(p) = &o.post {
                let via = match o.kind {
                    OpKind::Http => "api",
                    OpKind::Ui => "ui only",
                    OpKind::Event => "event from outside",
                };
                out.push_str(&format!("  {p}    [{via}: {}]\n", o.name));
            }
        }
        out
    }
}

/// Something wrong with the world model itself, found by reading it. No app needs to be running.
///
/// This is the check `annotate-world-model` describes and could not run: an agent writes the
/// `x-reverse-webmcp` blocks, and until now nothing told it whether they were coherent. A wrong
/// block does not fail loudly — it compiles a plan that is quietly incorrect.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Finding {
    /// The operation this is about, or `(document)` for the model as a whole.
    pub op: String,
    /// A stable name to branch on.
    pub code: &'static str,
    pub message: String,
    /// `true` when a plan compiled against this model can be *wrong*, not merely poorer.
    pub fatal: bool,
}

/// Fields every entity has implicitly: `exists` is "make me one", `resolved` is "find me the one".
const PSEUDO_FIELDS: [&str; 2] = ["exists", "resolved"];

fn vars_of(v: &Val, out: &mut Vec<String>) {
    match v {
        Val::Var(n, _) => out.push(n.clone()),
        Val::List(xs) | Val::Each(xs) => xs.iter().for_each(|x| vars_of(x, out)),
        Val::All(x) => vars_of(x, out),
        Val::Entity(p) => {
            p.args.iter().for_each(|(_, a)| vars_of(a, out));
            vars_of(&p.value, out);
        }
        _ => {}
    }
}

fn pred_vars(p: &Pred) -> Vec<String> {
    let mut out = Vec::new();
    p.args.iter().for_each(|(_, a)| vars_of(a, &mut out));
    vars_of(&p.value, &mut out);
    out
}

impl World {
    /// A stable hash of everything a plan depends on: which operations exist, what they promise,
    /// what they touch and where they can be reached. Two apps with the same fingerprint compile
    /// the same wants the same way.
    ///
    /// A recipe records this so re-running it against a drifted app is caught rather than
    /// discovered halfway through, and the intent cache keys on it so re-annotating an app does
    /// not silently reuse an intent planned against the old model.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        for e in &self.entities {
            let mut fields = e.fields.clone();
            fields.sort();
            h.update(format!("entity {} {} {}\n", e.name, e.id, fields.join(",")).as_bytes());
        }
        let mut ops: Vec<&Op> = self.ops.iter().collect();
        ops.sort_by(|a, b| a.name.cmp(&b.name));
        for o in ops {
            let p = |v: &Option<Pred>| v.as_ref().map(|x| x.to_string()).unwrap_or_default();
            let list = |v: &[String]| {
                let mut v = v.to_vec();
                v.sort();
                v.join(",")
            };
            let preds = |v: &[Pred]| {
                let mut v: Vec<String> = v.iter().map(|x| x.to_string()).collect();
                v.sort();
                v.join(",")
            };
            let surfaces = o.surfaces.iter().map(|(k, c)| format!("{k}={c}")).collect::<Vec<_>>().join(",");
            let line = format!(
                "op {} {:?} {} {} post={} requires={} produces={} reads={} writes={} external={} before={} surfaces={}\n",
                o.name,
                o.kind,
                o.method,
                o.path,
                p(&o.post),
                preds(&o.requires),
                o.produces.clone().unwrap_or_default(),
                list(&o.reads),
                list(&o.writes),
                o.external,
                list(&o.before),
                surfaces
            );
            h.update(line.as_bytes());
        }
        hex::encode(&h.finalize()[..16])
    }

    /// Read the model and report what cannot be right. Fatal findings mean a plan built on this
    /// model can be wrong; the rest are things that will merely work less well than they could.
    pub fn validate(&self) -> Vec<Finding> {
        let mut f: Vec<Finding> = Vec::new();
        let entities: BTreeMap<&str, &Entity> = self.entities.iter().map(|e| (e.name.as_str(), e)).collect();
        let op_names: std::collections::BTreeSet<&str> = self.ops.iter().map(|o| o.name.as_str()).collect();

        let mut add = |op: &str, code: &'static str, message: String, fatal: bool| {
            f.push(Finding { op: op.to_string(), code, message, fatal });
        };

        if self.entities.is_empty() {
            add("(document)", "no_entities", "x-reverse-webmcp-entities is missing or empty, so no want can name anything".into(), true);
        }
        if !self.ops.iter().any(|o| o.post.is_some()) {
            add("(document)", "nothing_plannable", "no operation declares a `post`, so no goal can ever be compiled".into(), true);
        }

        for o in &self.ops {
            // What a `$var` in this operation can be filled from.
            let mut fillable: std::collections::BTreeSet<&str> = o.params.iter().map(|p| p.name.as_str()).collect();
            fillable.extend(o.defaults.keys().map(|k| k.as_str()));

            for (p, field) in o.post.iter().map(|p| (p, "post")).chain(o.requires.iter().map(|p| (p, "requires"))) {
                match entities.get(p.entity.as_str()) {
                    None => add(&o.name, "unknown_entity", format!("{field} names entity '{}', which is not in x-reverse-webmcp-entities", p.entity), true),
                    Some(e) => {
                        if !p.field.is_empty() && !PSEUDO_FIELDS.contains(&p.field.as_str()) && !e.fields.contains(&p.field) {
                            add(&o.name, "unknown_field", format!("{field} asks about '{}.{}', which the entity does not have", p.entity, p.field), true);
                        }
                    }
                }
                for v in pred_vars(p) {
                    if !fillable.contains(v.as_str()) {
                        let m = format!("{field} uses ${v}, but the operation has no parameter or default by that name, so nothing can fill it");
                        add(&o.name, "unfillable_variable", m, true);
                    }
                }
            }

            if let Some(prod) = &o.produces {
                if !entities.contains_key(prod.as_str()) {
                    add(&o.name, "unknown_entity", format!("produces '{prod}', which is not in x-reverse-webmcp-entities"), true);
                }
            }

            for (spec, which) in o.reads.iter().map(|s| (s, "reads")).chain(o.writes.iter().map(|s| (s, "writes"))) {
                let Some((entity, sel)) = spec.split_once(':') else {
                    add(&o.name, "malformed_footprint", format!("{which} entry '{spec}' is not `entity:selector`"), true);
                    continue;
                };
                if !entities.contains_key(entity) {
                    add(&o.name, "unknown_entity", format!("{which} names entity '{entity}', which is not in x-reverse-webmcp-entities"), true);
                }
                if let Some(var) = sel.strip_prefix('$') {
                    let var = var.trim_end_matches("[]");
                    if !fillable.contains(var) {
                        // This is the silent one: an unbindable selector widens to `entity:*`,
                        // which conflicts with every other step touching that entity, so the
                        // plan serialises and nobody is told why.
                        let m = format!("{which} '{spec}' names ${var}, which is not a parameter; it widens to '{entity}:*' and serialises the plan");
                        add(&o.name, "unbound_footprint_param", m, true);
                    }
                }
            }

            for b in &o.before {
                if !op_names.contains(b.as_str()) {
                    add(&o.name, "before_unknown_op", format!("before names '{b}', which is not an operation in this document"), true);
                }
            }

            match o.kind {
                OpKind::Http | OpKind::Ui if o.surfaces.is_empty() => {
                    add(&o.name, "no_surfaces", "no surfaces, so nothing can ever call it".into(), true);
                }
                OpKind::Http if o.post.is_none() => {
                    add(&o.name, "no_postcondition", "no post, so no want can ask for it; it can still serve a `requires` or a check".into(), false);
                }
                OpKind::Event if o.check.is_none() => {
                    add(&o.name, "event_without_check", "no check, so a lost event can only be waited for, never confirmed by reading".into(), false);
                }
                _ => {}
            }
        }

        f.extend(self.unordered_writers());
        f
    }

    /// Two operations that write the same thing have no order in a footprint — a footprint says
    /// *what* is touched, never *when*. Only the world model knows, and it says so with `before`.
    /// Creators are exempt: `entity:new` is always ordered first by the data dependency on what
    /// it produces.
    fn unordered_writers(&self) -> Vec<Finding> {
        let writers: Vec<&Op> = self.ops.iter().filter(|o| o.writes.iter().any(|w| !w.ends_with(":new"))).collect();
        let mut out = Vec::new();
        for (i, a) in writers.iter().enumerate() {
            for b in writers.iter().skip(i + 1) {
                let shared: Vec<&String> = a.writes.iter().filter(|w| !w.ends_with(":new") && b.writes.contains(w)).collect();
                if shared.is_empty() || self.ordered(&a.name, &b.name) || self.ordered(&b.name, &a.name) {
                    continue;
                }
                let m =
                    format!("writes {} as well, and neither declares `before` the other, so their order is whatever the want order happens to be", shared[0]);
                out.push(Finding { op: format!("{} / {}", a.name, b.name), code: "unordered_writers", message: m, fatal: false });
            }
        }
        out
    }

    /// Does `from` come before `to`, directly or through a chain of `before`?
    fn ordered(&self, from: &str, to: &str) -> bool {
        let mut seen = std::collections::BTreeSet::new();
        let mut stack = vec![from.to_string()];
        while let Some(n) = stack.pop() {
            if !seen.insert(n.clone()) {
                continue;
            }
            let Some(op) = self.op(&n) else { continue };
            if op.before.iter().any(|b| b == to) {
                return true;
            }
            stack.extend(op.before.iter().cloned());
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_from_the_target_app_spec() {
        let doc: Value = serde_json::from_str(include_str!("../../app/static/openapi.json")).unwrap();
        let w = World::from_openapi(&doc).unwrap();
        assert!(w.op("createInvoice").is_some());
        assert_eq!(w.op("createInvoice").unwrap().params.iter().filter(|p| p.location == ParamIn::Body).count(), 2);
        assert_eq!(w.op("sendInvoice").unwrap().params[0].location, ParamIn::Path);
        assert_eq!(w.op("approveInvoice").unwrap().kind, OpKind::Ui);
        assert_eq!(w.op("payment.received").unwrap().kind, OpKind::Event);
        assert!(w.summary().contains("invoice(id=$id).status='sent'"));
    }
}
