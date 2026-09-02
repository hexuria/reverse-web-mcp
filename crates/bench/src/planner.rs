//! The planner: one sample turns a goal into an intent graph. Never actions.
//!
//! Behind the `Sampler` trait so the lint loop and the fork answer are testable with a stub.

use async_trait::async_trait;
use serde_json::{json, Value};
use zerohuman::intent::{lint, Intent, IntentFork, LintError};
use zerohuman::ledger::{Ledger, SampleKind};
use zerohuman::{CompileOptions, World};

use crate::loops::ModelClient;
use crate::tasks::Task;

/// Anything that can answer one Messages-shaped request and record it as a sample.
#[async_trait]
pub trait Sampler: Send + Sync {
    async fn sample(&self, ledger: &mut Ledger, kind: SampleKind, body: Value) -> anyhow::Result<Value>;
}

#[async_trait]
impl Sampler for ModelClient {
    async fn sample(&self, ledger: &mut Ledger, kind: SampleKind, body: Value) -> anyhow::Result<Value> {
        ModelClient::sample(self, ledger, kind, body).await
    }
}

/// What exists right now, read from the app. A read, not a sample.
pub async fn world_facts(base: &str) -> anyhow::Result<String> {
    let customers: Value = reqwest::get(format!("{}/api/customers", base.trim_end_matches('/'))).await?.json().await?;
    let names: Vec<String> =
        customers.as_array().map(|a| a.iter().filter_map(|c| c.get("name").and_then(|n| n.as_str()).map(|s| s.to_string())).collect()).unwrap_or_default();
    Ok(format!("  customers ({}): {}", names.len(), summarise_names(&names)))
}

fn intent_tool() -> Value {
    json!({
        "name": "emit_intent",
        "description": "Emit the intent graph for the goal.",
        "input_schema": {
            "type": "object",
            "properties": {
                "wants": {"type": "array", "items": {"type": "string"}, "description": "Predicates in the shared language."},
                "forks": {"type": "array", "items": {"type": "object", "properties": {"when": {"type": "string"}, "ask": {"type": "string"}}, "required": ["when", "ask"]}}
            },
            "required": ["wants"],
            "additionalProperties": false
        },
        "strict": true
    })
}

fn intent_from(resp: &Value, task: &Task) -> anyhow::Result<Intent> {
    let content = resp.get("content").cloned().unwrap_or(json!([]));
    let call = content.as_array().and_then(|a| a.iter().find(|b| b["type"] == "tool_use" && b["name"] == "emit_intent")).cloned();
    let input = match call {
        Some(c) => c["input"].clone(),
        None => {
            let text: String =
                content.as_array().map(|a| a.iter().filter_map(|b| b.get("text").and_then(|t| t.as_str())).collect::<Vec<_>>().join("\n")).unwrap_or_default();
            match (text.find('{'), text.rfind('}')) {
                (Some(start), Some(end)) if end > start => serde_json::from_str(&text[start..=end])?,
                // Some routes render a tool call as text: `emit_intentcallwants=[...]`. Recover the list.
                _ => match (text.find("wants=["), text.rfind(']')) {
                    (Some(start), Some(end)) if end > start + 6 => json!({"wants": serde_json::from_str::<Value>(&text[start + 6..=end])?}),
                    _ => anyhow::bail!("planner emitted no intent: {text}"),
                },
            }
        }
    };
    let wants: Vec<String> = input["wants"].as_array().map(|a| a.iter().filter_map(|w| w.as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
    let forks = input["forks"]
        .as_array()
        .map(|a| a.iter().map(|f| IntentFork { when: f["when"].as_str().unwrap_or("").into(), ask: f["ask"].as_str().unwrap_or("").into() }).collect())
        .unwrap_or_default();
    Ok(Intent { goal: task.goal.clone(), wants, constraints: task.constraints.clone(), forks })
}

/// Plan, lint, and if the intent is wrong hand the errors back exactly once.
#[allow(clippy::too_many_arguments)]
/// Plan, expand selectors by reading the app, lint, and hand the errors back up to twice. Every
/// attempt is a counted sample; the expansion is a read.
pub async fn plan_with_lint(
    task: &Task,
    world: &World,
    facts: &str,
    sampler: &dyn Sampler,
    ledger: &mut Ledger,
    opts: &CompileOptions,
    base: Option<&str>,
) -> anyhow::Result<Intent> {
    let mut intent = plan_intent(task, world, facts, sampler, ledger).await?;
    for _ in 0..2 {
        if let Some(b) = base {
            intent = expand_selectors(&intent, b).await?;
        }
        let errs = lint(&intent, world, opts);
        if errs.is_empty() {
            return Ok(intent);
        }
        ledger.notes.push(json!({"lint": errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(), "wants": intent.wants}));
        intent = replan(task, world, facts, &intent, &errs, sampler, ledger).await?;
    }
    if let Some(b) = base {
        intent = expand_selectors(&intent, b).await?;
    }
    let errs = lint(&intent, world, opts);
    if errs.is_empty() {
        return Ok(intent);
    }
    ledger.notes.push(json!({"lint": errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(), "wants": intent.wants}));
    anyhow::bail!("intent still wrong after two re-asks: {}", errs.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("; "))
}

async fn replan(
    task: &Task,
    world: &World,
    facts: &str,
    prior: &Intent,
    errs: &[LintError],
    sampler: &dyn Sampler,
    ledger: &mut Ledger,
) -> anyhow::Result<Intent> {
    let listed = errs.iter().map(|e| format!("- {e}")).collect::<Vec<_>>().join("\n");
    let user = format!(
        "World model:\n{}\nWorld facts (read just now):\n{}\nGoal: {}\n\nYour previous intent was:\n{}\n\nIt was rejected:\n{}\n\nEmit a corrected intent graph with emit_intent.",
        world.summary(),
        facts,
        task.goal,
        prior.wants.iter().map(|w| format!("  {w}")).collect::<Vec<_>>().join("\n"),
        listed
    );
    let body = json!({
        "max_tokens": 4096,
        "system": [{"type": "text", "text": PLANNER_SYSTEM, "cache_control": {"type": "ephemeral"}}],
        "tools": [intent_tool()],
        "messages": [{"role": "user", "content": user}],
    });
    let resp = sampler.sample(ledger, SampleKind::Lint, body).await?;
    intent_from(&resp, task)
}

const PLANNER_SYSTEM: &str = "You are the planner for an execution engine. You never emit actions. You emit an intent graph: \
a list of wants, each a predicate that must be true when the work is done, plus forks: conditions under which \
you agree to be woken and asked. The engine compiles wants into a parallel plan and runs it without you.\n\n\
Predicate language:\n\
  entity(arg=value, ...).field=value\n\
  entity(arg=value, ...).exists\n\
  A nested entity(...) as a value refers to that entity, e.g. customer(name='Acme').\n\
  Strings use single quotes. Lists use [a,b].\n\n\
Rules:\n\
- One want per fact that must be true at the end. Never emit wants for reads or lookups; the engine derives those.\n\
- Customers already exist. Never want a customer to exist; refer to them by name inside other predicates.\n\
- Identify invoices by their customer, never by an id you invent.\n\
- Do not assume order. The compiler derives order from data dependencies.\n\
- If a fact depends on something the outside world does (a payment arriving), still want the final fact; \
  the engine waits for the event. Forks are only for genuine ambiguity, not for waiting or retrying.\n\
- Use the real names from the world facts. Never use variables like $name.\n\
- For many rows, never list names: write each(customer(name_prefix='...')) and the engine expands it \
  from the world before compiling. each([...]) with explicit names is for a handful.\n\
- If a name in the goal matches more than one entity in the facts, still refer to it by name. \
  The engine stops at that point and asks you which one; do not ask now and do not leave wants out.\n\n\
Example goal: Invoice Acme and Globex, send both, then one report over both.\n\
Example wants:\n\
  invoice(customer=customer(name='Acme')).exists\n\
  invoice(customer=customer(name='Acme')).status='sent'\n\
  invoice(customer=customer(name='Globex')).exists\n\
  invoice(customer=customer(name='Globex')).status='sent'\n\
  report(invoices=[invoice(customer=customer(name='Acme')),invoice(customer=customer(name='Globex'))]).exists\n\
Example goal: Invoice every customer whose name starts with 'Bulk ' and send each.\n\
Example wants:\n\
  invoice(customer=each(customer(name_prefix='Bulk '))).exists\n\
  invoice(customer=each(customer(name_prefix='Bulk '))).status='sent'\n\
Example goal: Invoice Acme, send it, and email a receipt once it is paid.\n\
Example wants:\n\
  invoice(customer=customer(name='Acme')).exists\n\
  invoice(customer=customer(name='Acme')).status='sent'\n\
  invoice(customer=customer(name='Acme')).receipt_sent=true\n\n\
Reply with the emit_intent tool.";

/// One sample: goal + world summary + facts → Intent. The only model call on the happy path.
pub async fn plan_intent(task: &Task, world: &World, facts: &str, sampler: &dyn Sampler, ledger: &mut Ledger) -> anyhow::Result<Intent> {
    let user = format!(
        "World model:\n{}\nWorld facts (read just now):\n{}\nGoal: {}\n\nEmit the intent graph now with emit_intent.",
        world.summary(),
        facts,
        task.goal
    );
    let body = json!({
        "max_tokens": 4096,
        "system": [{"type": "text", "text": PLANNER_SYSTEM, "cache_control": {"type": "ephemeral"}}],
        "tools": [intent_tool()],
        "messages": [{"role": "user", "content": user}],
    });
    let resp = sampler.sample(ledger, SampleKind::Plan, body).await?;
    intent_from(&resp, task)
}

/// What the scheduler stopped on.
pub struct ForkQuestion {
    pub ask: String,
    pub evidence: Value,
}

/// The planner answers a fork: it rewrites only the wants about the ambiguous entity, naming
/// the chosen one by id, and leaves every other want byte-identical so their keys survive.
pub async fn answer_fork(
    task: &Task,
    world: &World,
    facts: &str,
    prior: &Intent,
    fork: &ForkQuestion,
    sampler: &dyn Sampler,
    ledger: &mut Ledger,
) -> anyhow::Result<Intent> {
    let (ask, evidence) = (&fork.ask, &fork.evidence);
    let user = format!(
        "World model:\n{}\nWorld facts (read just now):\n{}\nGoal: {}\n\nThe plan compiled from your intent stopped with a question:\n  {}\nEvidence:\n{}\n\nYour intent was:\n{}\n\nAnswer by emitting the intent again with emit_intent. Rules: identify the chosen entity by id, e.g. customer(id=11), \
in every want that referred to the ambiguous one; keep every other want exactly as it was, character for character; \
if the goal gives no basis to choose, choose the lowest id and say nothing else.",
        world.summary(),
        facts,
        task.goal,
        ask,
        serde_json::to_string_pretty(evidence).unwrap_or_default(),
        prior.wants.iter().map(|w| format!("  {w}")).collect::<Vec<_>>().join("\n")
    );
    let body = json!({
        "max_tokens": 4096,
        "system": [{"type": "text", "text": PLANNER_SYSTEM, "cache_control": {"type": "ephemeral"}}],
        "tools": [intent_tool()],
        "messages": [{"role": "user", "content": user}],
    });
    let resp = sampler.sample(ledger, SampleKind::ForkAnswer, body.clone()).await?;
    let answered = intent_from(&resp, task)?;
    let errs = lint(&answered, world, &CompileOptions::default());
    if errs.is_empty() {
        return Ok(answered);
    }
    ledger.notes.push(json!({"fork_answer_rejected": errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(), "wants": answered.wants}));
    // One more try, with the rejection in hand.
    let mut body2 = body;
    let listed = errs.iter().map(|e| format!("- {e}")).collect::<Vec<_>>().join("\n");
    let prior = body2["messages"][0]["content"].as_str().unwrap_or("").to_string();
    body2["messages"] = json!([{"role": "user", "content": format!("{prior}\n\nYour previous answer was rejected:\n{listed}\n\nAnswer again with emit_intent, keeping every unaffected want unchanged.")}]);
    let resp = sampler.sample(ledger, SampleKind::ForkAnswer, body2).await?;
    let answered = intent_from(&resp, task)?;
    let errs = lint(&answered, world, &CompileOptions::default());
    if !errs.is_empty() {
        ledger.notes.push(json!({"fork_answer_rejected": errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(), "wants": answered.wants}));
        anyhow::bail!("fork answer rejected twice: {}", errs.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("; "));
    }
    Ok(answered)
}

/// A cache of planner output keyed by what the planner saw: goal, world facts, and the surfaces
/// available. A hit means a repeat goal costs zero samples. The plan itself is recompiled, which
/// is cheap, and content-addressed keys make the recompiled plan safe to run again.
pub struct IntentCache {
    dir: std::path::PathBuf,
}

impl IntentCache {
    pub fn new(dir: impl Into<std::path::PathBuf>) -> IntentCache {
        IntentCache { dir: dir.into() }
    }

    pub fn key(goal: &str, facts: &str, surfaces: &[String]) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(goal.trim().as_bytes());
        h.update(b"\n");
        h.update(facts.trim().as_bytes());
        h.update(b"\n");
        h.update(surfaces.join(",").as_bytes());
        hex::encode(&h.finalize()[..16])
    }

    fn path(&self, key: &str) -> std::path::PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    pub fn get(&self, goal: &str, facts: &str, surfaces: &[String]) -> Option<Intent> {
        let text = std::fs::read_to_string(self.path(&Self::key(goal, facts, surfaces))).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn put(&self, goal: &str, facts: &str, surfaces: &[String], intent: &Intent) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(self.path(&Self::key(goal, facts, surfaces)), serde_json::to_string_pretty(intent)?)?;
        Ok(())
    }
}

/// Plan through the cache: a hit is free, a miss is planned, linted and stored.
#[allow(clippy::too_many_arguments)]
pub async fn plan_cached(
    cache: &IntentCache,
    task: &Task,
    world: &World,
    facts: &str,
    sampler: &dyn Sampler,
    ledger: &mut Ledger,
    opts: &CompileOptions,
    base: Option<&str>,
) -> anyhow::Result<(Intent, bool)> {
    if let Some(i) = cache.get(&task.goal, facts, &opts.surfaces) {
        return Ok((i, true));
    }
    let i = plan_with_lint(task, world, facts, sampler, ledger, opts, base).await?;
    cache.put(&task.goal, facts, &opts.surfaces, &i)?;
    Ok((i, false))
}

/// "Acme, Globex, and 300 named 'Customer 001' … 'Customer 300' (prefix 'Customer ')".
pub fn summarise_names(names: &[String]) -> String {
    use std::collections::BTreeMap;
    let prefix_of = |n: &str| -> String {
        match n.rfind(' ') {
            Some(i) if !n[i + 1..].is_empty() && n[i + 1..].chars().all(|c| c.is_ascii_digit()) => n[..=i].to_string(),
            _ => String::new(),
        }
    };
    let mut by_prefix: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for n in names {
        by_prefix.entry(prefix_of(n)).or_default().push(n.clone());
    }
    let big: BTreeMap<String, Vec<String>> = by_prefix.into_iter().filter(|(p, v)| !p.is_empty() && v.len() >= 20).collect();
    let mut parts: Vec<String> = names.iter().filter(|n| !big.contains_key(&prefix_of(n))).cloned().collect();
    for (p, v) in &big {
        parts.push(format!("and {} named '{}' … '{}' (prefix '{}')", v.len(), v[0], v[v.len() - 1], p));
    }
    parts.join(", ")
}

/// Replace `each(customer(name_prefix='X'))` with the matching names, read from the app.
/// A read before compiling, not a sample; keys come out identical to the written-out form.
pub async fn expand_selectors(intent: &Intent, base: &str) -> anyhow::Result<Intent> {
    use zerohuman::pred::Pred;
    let client = reqwest::Client::new();
    let mut out = intent.clone();
    for w in out.wants.iter_mut() {
        let Ok(mut pred) = Pred::parse(w) else { continue };
        let mut changed = false;
        for (_, v) in pred.args.iter_mut() {
            if let Some(nv) = expand_val(v, &client, base).await? {
                *v = nv;
                changed = true;
            }
        }
        if changed {
            *w = pred.to_string();
        }
    }
    Ok(out)
}

#[async_recursion::async_recursion]
async fn expand_val(v: &zerohuman::pred::Val, client: &reqwest::Client, base: &str) -> anyhow::Result<Option<zerohuman::pred::Val>> {
    use zerohuman::pred::{Pred, Val};
    match v {
        Val::Each(items) if items.len() == 1 => {
            if let Val::Entity(p) = &items[0] {
                if p.entity == "customer" {
                    if let Some(Val::Str(prefix)) = p.arg("name_prefix") {
                        let rows: Value =
                            client.get(format!("{}/api/customers", base.trim_end_matches('/'))).query(&[("name_prefix", prefix)]).send().await?.json().await?;
                        let names: Vec<Val> = rows
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|c| c.get("name").and_then(|n| n.as_str()))
                                    .map(|n| {
                                        Val::Entity(Box::new(Pred {
                                            entity: "customer".into(),
                                            args: vec![("name".into(), Val::Str(n.to_string()))],
                                            field: String::new(),
                                            value: Val::Bool(true),
                                        }))
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        return Ok(Some(Val::Each(names)));
                    }
                }
            }
            Ok(None)
        }
        Val::Entity(p) => {
            let mut p2 = (**p).clone();
            let mut changed = false;
            for (_, a) in p2.args.iter_mut() {
                if let Some(nv) = expand_val(a, client, base).await? {
                    *a = nv;
                    changed = true;
                }
            }
            Ok(if changed { Some(Val::Entity(Box::new(p2))) } else { None })
        }
        Val::List(xs) => {
            let mut ys = Vec::new();
            let mut changed = false;
            for x in xs {
                match expand_val(x, client, base).await? {
                    Some(nv) => {
                        ys.push(nv);
                        changed = true;
                    }
                    None => ys.push(x.clone()),
                }
            }
            Ok(if changed { Some(Val::List(ys)) } else { None })
        }
        _ => Ok(None),
    }
}
