//! The world model, derived from the target app's OpenAPI document. Never authored by hand.
//!
//! Each operation carries an `x-zerohuman` block: postcondition, requirements, read/write
//! footprint, which surfaces expose it and at what cost. Events and UI-only actions come
//! from `x-zerohuman-events` and `x-zerohuman-ui` at the document root.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use thiserror::Error;

use crate::pred::{ParseError, Pred};

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
    /// A UI-only action from `x-zerohuman-ui`.
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
        if let Some(es) = doc.get("x-zerohuman-entities").and_then(|e| e.as_object()) {
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
                let Some(zh) = spec.get("x-zerohuman") else { continue };
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

        if let Some(ui) = doc.get("x-zerohuman-ui").and_then(|u| u.as_object()) {
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

        if let Some(evs) = doc.get("x-zerohuman-events").and_then(|e| e.as_object()) {
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
