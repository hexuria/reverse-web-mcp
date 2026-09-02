//! What the planner emits: wants, constraints, and the forks where it agrees to be woken.
//! Never actions.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Constraints {
    /// May the plan cause effects that leave the system (email, money)? Default yes.
    #[serde(default = "yes")]
    pub external_ok: bool,
    #[serde(default)]
    pub spend_max_cents: Option<i64>,
}

fn yes() -> bool {
    true
}

impl Default for Constraints {
    fn default() -> Self {
        Constraints { external_ok: true, spend_max_cents: None }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IntentFork {
    pub when: String,
    pub ask: String,
    /// A resolution the planner declares up front, so the scheduler need not wake it:
    /// `lowest_id` picks the match with the smallest id.
    #[serde(default)]
    pub default: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Intent {
    pub goal: String,
    /// Predicates in the shared language, e.g. `invoice(customer=customer(name='Acme')).status='sent'`.
    pub wants: Vec<String>,
    #[serde(default)]
    pub constraints: Constraints,
    #[serde(default)]
    pub forks: Vec<IntentFork>,
}

use crate::compiler::{compile, CompileError, CompileOptions};
use crate::pred::{ParseError, Pred, Val};
use crate::world::World;

/// Why an intent cannot be trusted yet. Returned to the planner once, verbatim.
#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum LintError {
    #[error("want '{want}' does not parse: {source}")]
    Unparseable { want: String, source: ParseError },
    #[error("want '{want}' uses a variable; wants must be concrete")]
    VariableInWant { want: String },
    #[error("want '{want}' names an unknown entity '{entity}'")]
    UnknownEntity { want: String, entity: String },
    #[error("want '{want}' asks about '{entity}.{field}', which does not exist")]
    UnknownField { want: String, entity: String, field: String },
    #[error("want '{want}' asks for an entity that already exists and is found, never made; refer to it inside another predicate")]
    WantsAnEntityThatAlreadyExists { want: String },
    #[error("intent cannot be compiled: {reason}")]
    Unsatisfiable { reason: String },
    #[error("intent has no wants; every goal needs at least one fact that must become true")]
    Empty,
}

fn has_var(v: &Val) -> bool {
    match v {
        Val::Var(..) => true,
        Val::List(xs) | Val::Each(xs) => xs.iter().any(has_var),
        Val::Entity(p) => p.args.iter().any(|(_, v)| has_var(v)) || has_var(&p.value),
        _ => false,
    }
}

fn walk_entities<'a>(p: &'a Pred, out: &mut Vec<&'a Pred>) {
    out.push(p);
    for (_, v) in &p.args {
        walk_vals(v, out);
    }
}

fn walk_vals<'a>(v: &'a Val, out: &mut Vec<&'a Pred>) {
    match v {
        Val::Entity(p) => walk_entities(p, out),
        Val::List(xs) | Val::Each(xs) => xs.iter().for_each(|x| walk_vals(x, out)),
        _ => {}
    }
}

/// Check an intent against the world model before spending anything on it.
pub fn lint(intent: &Intent, world: &World, opts: &CompileOptions) -> Vec<LintError> {
    let mut errs = Vec::new();
    if intent.wants.is_empty() {
        return vec![LintError::Empty];
    }
    let resolvable: Vec<&str> =
        world.ops.iter().filter(|o| o.post.as_ref().is_some_and(|p| p.field == "resolved")).filter_map(|o| o.produces.as_deref()).collect();
    for want in &intent.wants {
        let pred = match Pred::parse(want) {
            Ok(p) => p,
            Err(source) => {
                errs.push(LintError::Unparseable { want: want.clone(), source });
                continue;
            }
        };
        if pred.args.iter().any(|(_, v)| has_var(v)) || has_var(&pred.value) {
            errs.push(LintError::VariableInWant { want: want.clone() });
        }
        let mut seen = Vec::new();
        walk_entities(&pred, &mut seen);
        for p in seen {
            match world.entities.iter().find(|e| e.name == p.entity) {
                None => errs.push(LintError::UnknownEntity { want: want.clone(), entity: p.entity.clone() }),
                Some(e) => {
                    if !p.field.is_empty() && !p.is_existence() && !e.fields.contains(&p.field) {
                        errs.push(LintError::UnknownField { want: want.clone(), entity: p.entity.clone(), field: p.field.clone() });
                    }
                }
            }
        }
        if pred.is_existence() && resolvable.contains(&pred.entity.as_str()) {
            errs.push(LintError::WantsAnEntityThatAlreadyExists { want: want.clone() });
        }
    }
    if errs.is_empty() {
        if let Err(e) = compile(intent, world, opts) {
            let reason = match e {
                CompileError::Parse(w, e) => format!("{w}: {e}"),
                other => other.to_string(),
            };
            errs.push(LintError::Unsatisfiable { reason });
        }
    }
    errs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> World {
        let doc: serde_json::Value = serde_json::from_str(include_str!("../../app/static/openapi.json")).unwrap();
        World::from_openapi(&doc).unwrap()
    }

    fn lint_one(want: &str) -> Vec<LintError> {
        let intent = Intent { goal: "t".into(), wants: vec![want.to_string()], ..Default::default() };
        lint(&intent, &world(), &CompileOptions::default())
    }

    type Expect = fn(&LintError) -> bool;

    #[test]
    fn an_empty_intent_is_rejected() {
        let intent = Intent { goal: "t".into(), ..Default::default() };
        assert_eq!(lint(&intent, &world(), &CompileOptions::default()), vec![LintError::Empty]);
    }

    #[test]
    fn the_table() {
        let cases: Vec<(&str, Expect)> = vec![
            ("invoice(customer=customer(name='Acme')).exists", |_| false),
            ("invoice(customer=customer(name='Acme')).status='sent'", |_| false),
            ("invoice(", |e| matches!(e, LintError::Unparseable { .. })),
            ("invoice(customer=customer(name=$name)).exists", |e| matches!(e, LintError::VariableInWant { .. })),
            ("widget(name='x').exists", |e| matches!(e, LintError::UnknownEntity { entity, .. } if entity == "widget")),
            ("invoice(customer=customer(name='Acme')).colour='red'", |e| matches!(e, LintError::UnknownField { field, .. } if field == "colour")),
            ("customer(name='Acme').exists", |e| matches!(e, LintError::WantsAnEntityThatAlreadyExists { .. })),
            ("invoice(customer=customer(name='Acme')).approved=true", |e| matches!(e, LintError::Unsatisfiable { .. })),
        ];
        for (want, expected) in cases {
            let errs = lint_one(want);
            if errs.is_empty() {
                assert!(!expected(&LintError::Unsatisfiable { reason: String::new() }), "{want}: expected an error, got none");
                continue;
            }
            assert!(errs.iter().any(expected), "{want}: got {errs:?}");
        }
    }
}
