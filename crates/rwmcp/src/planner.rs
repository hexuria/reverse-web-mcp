//! The planner: one model call turns a ask.goal into an intent graph. Never actions.
//!
//! This is the only part of the engine that talks to a model, and it is kept behind the
//! `Sampler` trait so the lint loop and the fork answer are testable without a network.
//!
//! Rust has no official Anthropic SDK, so `ModelClient` speaks raw HTTP to `POST /v1/messages`.
//! Any server with that shape works, including a local gateway in front of another provider.

use std::time::Duration;

use crate::intent::{lint, Constraints, Intent, IntentFork, LintError};
use crate::ledger::{now_us, Ledger, Sample, SampleKind};
use crate::world::World;
use crate::CompileOptions;
use async_trait::async_trait;
use serde_json::{json, Value};

#[derive(Clone)]
pub struct ModelClient {
    pub base_url: String,
    pub model: String,
    pub effort: String,
    pub fallbacks: bool,
    auth: Auth,
    client: reqwest::Client,
}

#[derive(Clone)]
enum Auth {
    ApiKey(String),
    Bearer(String),
}

impl ModelClient {
    /// Any server that speaks the Messages shape works: api.anthropic.com, or a local gateway
    /// such as opencodex on http://localhost:8080 routing to other providers' models.
    ///
    /// Base URL: `--base-url`, else ANTHROPIC_BASE_URL, else api.anthropic.com.
    /// Credentials: `--api-key`, else ANTHROPIC_API_KEY, else ANTHROPIC_AUTH_TOKEN, else the `ant`
    /// CLI's active profile. A gateway that ignores keys gets a placeholder.
    pub fn from_env(model: &str, effort: &str, fallbacks: bool, base_url: Option<&str>, api_key: Option<&str>) -> anyhow::Result<ModelClient> {
        let base_url =
            base_url.map(|s| s.to_string()).or_else(|| std::env::var("ANTHROPIC_BASE_URL").ok()).unwrap_or_else(|| "https://api.anthropic.com".into());
        let is_anthropic = base_url.contains("api.anthropic.com");
        let auth = if let Some(k) = api_key {
            Auth::ApiKey(k.to_string())
        } else if let Ok(k) = std::env::var("ANTHROPIC_API_KEY") {
            Auth::ApiKey(k)
        } else if let Ok(t) = std::env::var("ANTHROPIC_AUTH_TOKEN") {
            Auth::Bearer(t)
        } else if !is_anthropic {
            Auth::ApiKey("local-gateway".into())
        } else {
            let out = std::process::Command::new("ant").args(["auth", "print-credentials", "--access-token"]).output();
            match out {
                Ok(o) if o.status.success() => Auth::Bearer(String::from_utf8_lossy(&o.stdout).trim().to_string()),
                _ => anyhow::bail!("no credentials: set ANTHROPIC_API_KEY, or run `ant auth login`"),
            }
        };
        Ok(ModelClient {
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            effort: effort.to_string(),
            // Server-side refusal fallback is an Anthropic feature; a gateway just ignores it.
            fallbacks: fallbacks && is_anthropic,
            auth,
            client: reqwest::Client::builder().timeout(Duration::from_secs(600)).build()?,
        })
    }

    /// The same client at a different effort, for the planner.
    pub fn with_effort(&self, effort: &str) -> ModelClient {
        ModelClient { effort: effort.to_string(), ..self.clone() }
    }

    /// One Messages call recorded as a sample: its span and token usage land in the ledger.
    pub async fn sample(&self, ledger: &mut Ledger, kind: SampleKind, body: Value) -> anyhow::Result<Value> {
        let started = now_us();
        let resp = self.messages(body).await?;
        let (ti, to) = usage(&resp);
        ledger.record_sample(Sample {
            seq: 0,
            kind,
            started_us: started,
            ended_us: now_us(),
            tokens_in: ti,
            tokens_out: to,
            model: self.model.clone(),
            effort: self.effort.clone(),
        });
        Ok(resp)
    }

    /// One Messages call. Returns the full response JSON. Retries 429/5xx a few times.
    pub async fn messages(&self, body: Value) -> anyhow::Result<Value> {
        let mut body = body;
        body["model"] = json!(self.model);
        body["thinking"] = json!({"type": "adaptive"});
        body["output_config"] = json!({"effort": self.effort});
        if self.fallbacks {
            body["fallbacks"] = json!("default");
        }
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut req =
                self.client.post(format!("{}/v1/messages", self.base_url)).header("anthropic-version", "2023-06-01").header("content-type", "application/json");
            let mut betas: Vec<&str> = Vec::new();
            match &self.auth {
                Auth::ApiKey(k) => req = req.header("x-api-key", k),
                Auth::Bearer(t) => {
                    req = req.header("authorization", format!("Bearer {t}"));
                    betas.push("oauth-2025-04-20");
                }
            }
            if self.fallbacks {
                betas.push("server-side-fallback-2026-07-01");
            }
            if !betas.is_empty() {
                req = req.header("anthropic-beta", betas.join(","));
            }
            let resp = req.json(&body).send().await?;
            let status = resp.status().as_u16();
            let resp_retry_after: Option<u64> = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
            let text = resp.text().await?;
            if status == 429 || status >= 500 {
                if attempt < 6 {
                    // Honour retry-after when the route says, capped so a long cooldown fails fast.
                    let after = resp_retry_after.map(|s| s * 1000).unwrap_or(500 * (1 << attempt)).min(30_000);
                    tokio::time::sleep(Duration::from_millis(after)).await;
                    continue;
                }
                anyhow::bail!("model API {status}: {text}");
            }
            if status != 200 {
                anyhow::bail!("model API {status}: {text}");
            }
            return Ok(serde_json::from_str(&text)?);
        }
    }
}

/// Input and output tokens from a Messages response, cache reads included.
pub fn usage(resp: &Value) -> (u64, u64) {
    let u = resp.get("usage").cloned().unwrap_or(json!({}));
    let read = |k: &str| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    (read("input_tokens") + read("cache_read_input_tokens") + read("cache_creation_input_tokens"), read("output_tokens"))
}

/// What the planner is being asked for: the sentence, and the limits on how it may be met.
#[derive(Clone, Debug, Default)]
pub struct Ask {
    pub goal: String,
    pub constraints: Constraints,
}

impl Ask {
    pub fn new(goal: impl Into<String>) -> Ask {
        Ask { goal: goal.into(), constraints: Constraints::default() }
    }

    pub fn with(goal: impl Into<String>, constraints: &Constraints) -> Ask {
        Ask { goal: goal.into(), constraints: constraints.clone() }
    }
}

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
        "description": "Emit the intent graph for the ask.goal.",
        "input_schema": {
            "type": "object",
            "properties": {
                "wants": {"type": "array", "items": {"type": "string"}, "description": "Predicates in the shared language."},
                "forks": {"type": "array", "items": {"type": "object", "properties": {"when": {"type": "string"}, "ask": {"type": "string"}, "default": {"type": "string", "enum": ["lowest_id"]}}, "required": ["when", "ask"]}}
            },
            "required": ["wants"],
            "additionalProperties": false
        },
        "strict": true
    })
}

fn intent_from(resp: &Value, ask: &Ask) -> anyhow::Result<Intent> {
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
        .map(|a| {
            a.iter()
                .map(|f| IntentFork {
                    when: f["when"].as_str().unwrap_or("").into(),
                    ask: f["ask"].as_str().unwrap_or("").into(),
                    default: f["default"].as_str().map(|s| s.to_string()),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Intent { goal: ask.goal.clone(), wants, constraints: ask.constraints.clone(), forks })
}

/// Plan, lint, and if the intent is wrong hand the errors back exactly once.
/// The situation a planner call happens in, as opposed to the question being asked. Passing it as
/// one value keeps the two entry points honest: a fork answer must be linted against the same
/// world and the same surfaces as the plan it is fixing, and taking them separately made it easy
/// to forget — `answer_fork` used to lint against `CompileOptions::default()`.
pub struct Ctx<'a> {
    pub world: &'a World,
    /// What the app says is true right now, in the planner's own words.
    pub facts: &'a str,
    pub opts: &'a CompileOptions,
    /// The app's base URL, when there is a live app to expand selectors against.
    pub base: Option<&'a str>,
}

impl<'a> Ctx<'a> {
    /// The offline case: a world model with no app behind it.
    pub fn new(world: &'a World, opts: &'a CompileOptions) -> Ctx<'a> {
        Ctx { world, facts: "", opts, base: None }
    }

    pub fn facts(mut self, facts: &'a str) -> Ctx<'a> {
        self.facts = facts;
        self
    }

    pub fn at(mut self, base: &'a str) -> Ctx<'a> {
        self.base = Some(base);
        self
    }
}

/// Plan, expand selectors by reading the app, lint, and hand the errors back up to twice. Every
/// attempt is a counted sample; the expansion is a read.
pub async fn plan_with_lint(ask: &Ask, ctx: &Ctx<'_>, sampler: &dyn Sampler, ledger: &mut Ledger) -> anyhow::Result<Intent> {
    let (world, facts) = (ctx.world, ctx.facts);
    let mut intent = plan_intent(ask, world, facts, sampler, ledger).await?;
    for _ in 0..2 {
        if let Some(b) = ctx.base {
            intent = expand_selectors(&intent, b).await?;
        }
        let errs = lint(&intent, world, ctx.opts);
        if errs.is_empty() {
            return Ok(intent);
        }
        ledger.notes.push(json!({"lint": errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(), "wants": intent.wants}));
        intent = replan(ask, world, facts, &intent, &errs, sampler, ledger).await?;
    }
    if let Some(b) = ctx.base {
        intent = expand_selectors(&intent, b).await?;
    }
    let errs = lint(&intent, world, ctx.opts);
    if errs.is_empty() {
        return Ok(intent);
    }
    ledger.notes.push(json!({"lint": errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(), "wants": intent.wants}));
    anyhow::bail!("intent still wrong after two re-asks: {}", errs.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("; "))
}

async fn replan(
    ask: &Ask,
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
        ask.goal,
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
    intent_or_empty(&resp, ask, ledger)
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
- For many rows, never list names: write each(customer(name_prefix='...')) for a prefix, or each(customer()) \
  for every customer. The engine expands either from the world before compiling. each([...]) with explicit \
  names is for a handful.\n\
- each(...) fans a want out, one per element. all(X) collects into one: \
  report(invoices=[all(invoice(customer=each(customer())))]) is ONE report over every invoice, while \
  report(invoices=[invoice(customer=each(customer()))]) is ONE REPORT PER invoice. Choose deliberately.\n\
- If a name in the ask.goal matches more than one entity in the facts, still refer to it by name and declare \
  a fork: {when: 'result.count != 1', ask: '...', default: 'lowest_id'}. With a default the engine resolves it \
  itself; without one it stops and asks you. Do not ask now and do not leave wants out.\n\n\
Example goal: Invoice Acme and Globex, send both, then one report over both.\n\
Example wants:\n\
  invoice(customer=customer(name='Acme')).exists\n\
  invoice(customer=customer(name='Acme')).status='sent'\n\
  invoice(customer=customer(name='Globex')).exists\n\
  invoice(customer=customer(name='Globex')).status='sent'\n\
  report(invoices=[invoice(customer=customer(name='Acme')),invoice(customer=customer(name='Globex'))]).exists\n\
Example goal: Invoice every customer and send each.\n\
Example wants:\n\
  invoice(customer=each(customer())).exists\n\
  invoice(customer=each(customer())).status='sent'\n\
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

/// One sample: ask.goal + world summary + facts → Intent. The only model call on the happy path.
pub async fn plan_intent(ask: &Ask, world: &World, facts: &str, sampler: &dyn Sampler, ledger: &mut Ledger) -> anyhow::Result<Intent> {
    let user = format!(
        "World model:\n{}\nWorld facts (read just now):\n{}\nGoal: {}\n\nEmit the intent graph now with emit_intent.",
        world.summary(),
        facts,
        ask.goal
    );
    let body = json!({
        "max_tokens": 4096,
        "system": [{"type": "text", "text": PLANNER_SYSTEM, "cache_control": {"type": "ephemeral"}}],
        "tools": [intent_tool()],
        "messages": [{"role": "user", "content": user}],
    });
    let resp = sampler.sample(ledger, SampleKind::Plan, body).await?;
    intent_or_empty(&resp, ask, ledger)
}

/// An unparseable reply is an empty intent, which lint rejects and the loop re-asks. The raw
/// reply is kept as a note.
fn intent_or_empty(resp: &Value, ask: &Ask, ledger: &mut Ledger) -> anyhow::Result<Intent> {
    match intent_from(resp, ask) {
        Ok(i) => Ok(i),
        Err(e) => {
            ledger.notes.push(json!({"unparseable": e.to_string(), "content": resp.get("content").cloned().unwrap_or(Value::Null)}));
            Ok(Intent { goal: ask.goal.clone(), constraints: ask.constraints.clone(), ..Default::default() })
        }
    }
}

/// What the scheduler stopped on.
pub struct ForkQuestion {
    pub ask: String,
    pub evidence: Value,
}

/// The planner answers a fork: it rewrites only the wants about the ambiguous entity, naming
/// the chosen one by id, and leaves every other want byte-identical so their keys survive.
pub async fn answer_fork(ask: &Ask, ctx: &Ctx<'_>, prior: &Intent, fork: &ForkQuestion, sampler: &dyn Sampler, ledger: &mut Ledger) -> anyhow::Result<Intent> {
    let (world, facts, opts) = (ctx.world, ctx.facts, ctx.opts);
    let (question, evidence) = (&fork.ask, &fork.evidence);
    let user = format!(
        "World model:\n{}\nWorld facts (read just now):\n{}\nGoal: {}\n\nThe plan compiled from your intent stopped with a question:\n  {}\nEvidence:\n{}\n\nYour intent was:\n{}\n\nAnswer by emitting the intent again with emit_intent. Rules: identify the chosen entity by id, e.g. customer(id=11), \
in every want that referred to the ambiguous one; keep every other want exactly as it was, character for character; \
if the ask.goal gives no basis to choose, choose the lowest id and say nothing else.",
        world.summary(),
        facts,
        ask.goal,
        question,
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
    let answered = intent_from(&resp, ask)?;
    let errs = lint(&answered, world, opts);
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
    let answered = intent_from(&resp, ask)?;
    let errs = lint(&answered, world, opts);
    if !errs.is_empty() {
        ledger.notes.push(json!({"fork_answer_rejected": errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(), "wants": answered.wants}));
        anyhow::bail!("fork answer rejected twice: {}", errs.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("; "));
    }
    Ok(answered)
}

/// A cache of planner output keyed by what the planner saw: ask.goal, world facts, and the surfaces
/// available. A hit means a repeat ask.goal costs zero samples. The plan itself is recompiled, which
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
pub async fn plan_cached(cache: &IntentCache, ask: &Ask, ctx: &Ctx<'_>, sampler: &dyn Sampler, ledger: &mut Ledger) -> anyhow::Result<(Intent, bool)> {
    let (facts, surfaces) = (ctx.facts, &ctx.opts.surfaces);
    if let Some(i) = cache.get(&ask.goal, facts, surfaces) {
        return Ok((i, true));
    }
    let i = plan_with_lint(ask, ctx, sampler, ledger).await?;
    cache.put(&ask.goal, facts, surfaces, &i)?;
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
    use crate::pred::Pred;
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
async fn expand_val(v: &crate::pred::Val, client: &reqwest::Client, base: &str) -> anyhow::Result<Option<crate::pred::Val>> {
    use crate::pred::{Pred, Val};
    match v {
        Val::Each(items) if items.len() == 1 => {
            // each(customer(name_prefix='X')) selects by prefix; each(customer()) selects every one.
            if let Val::Entity(p) = &items[0] {
                if p.entity == "customer" {
                    let prefix = match p.arg("name_prefix") {
                        Some(Val::Str(x)) => Some(x.clone()),
                        _ => None,
                    };
                    if prefix.is_some() || p.args.is_empty() {
                        let mut req = client.get(format!("{}/api/customers", base.trim_end_matches('/')));
                        if let Some(x) = &prefix {
                            req = req.query(&[("name_prefix", x)]);
                        }
                        let rows: Value = req.send().await?.json().await?;
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
        Val::All(inner) => Ok(expand_val(inner, client, base).await?.map(|nv| Val::All(Box::new(nv)))),
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
