//! `rwmcp` — run a goal against an app that describes itself.
//!
//! One command, options only. Nothing here needs Rust from you: an app publishes an OpenAPI
//! document with `x-reverse-webmcp` blocks, an agent writes those blocks and a list of wants,
//! and this reads both, compiles a plan, shows it, and runs it.
//!
//!   rwmcp --app URL --world                          what can this app be asked for
//!   rwmcp --app URL --goal "invoice Acme and send it"  plan it (one model call), print, do nothing
//!   rwmcp --app URL --wants w.txt --run --yes         run what an agent wrote, no model at all
//!   rwmcp --app URL --wants w.txt --save billing      keep it as a recipe
//!   rwmcp --app URL --recipe billing --set who=Globex --run --yes
//!
//! The last line is the point. Once a plan has worked, only the parameters change, so a recipe
//! costs no model calls ever again.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use rwmcp::events::EventBus;
use rwmcp::intent::{lint, Intent};
use rwmcp::ledger::{Ledger, Recorder};
use rwmcp::planner::{self, ModelClient};
use rwmcp::pred::{Pred, Val};
use rwmcp::scheduler::Outcome;
use rwmcp::{compile, default_effectors, CompileOptions, Plan, Receipt, Scheduler, Status, World};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Parser)]
#[command(name = "rwmcp", version, about = "Run a goal against an app that describes itself", after_help = EXAMPLES)]
struct Cli {
    // ---- what you want ----
    /// What you want, in plain language. Costs one model call.
    #[arg(long, value_name = "TEXT")]
    goal: Option<String>,
    /// A file of wants, one per line, `#` for comments. Costs nothing.
    #[arg(long, value_name = "FILE", conflicts_with = "goal")]
    wants: Option<PathBuf>,
    /// A saved recipe: a name in the recipes directory, or a path to a .json file. Costs nothing.
    #[arg(long, value_name = "NAME|FILE", conflicts_with_all = ["goal", "wants"])]
    recipe: Option<String>,
    /// Fill a `$name` placeholder in the wants. Repeatable: --set who=Acme --set amount=5000
    #[arg(long = "set", value_name = "KEY=VALUE")]
    sets: Vec<String>,
    /// Pick up a run saved with --receipt-out, skipping whatever already succeeded.
    #[arg(long, value_name = "FILE", conflicts_with_all = ["goal", "wants", "recipe"])]
    resume: Option<PathBuf>,

    // ---- what to do ----
    /// Execute the plan. Without this, the plan is printed and nothing happens.
    #[arg(long)]
    run: bool,
    /// Proceed even though steps leave the system (email, money).
    #[arg(long)]
    yes: bool,
    /// Print what the app can be asked for, and stop.
    #[arg(long)]
    world: bool,
    /// Check the world model itself and stop: entities, fields, footprints, ordering, surfaces.
    /// This is the review `annotate-world-model` describes; it needs no app running.
    #[arg(long)]
    validate: bool,
    /// Compile the wants, compile them again in the opposite order, and prove the two plans are
    /// the same. A plan that depends on the order the wants were typed in is a bug.
    #[arg(long)]
    order_check: bool,
    /// Read a plain OpenAPI document and print a skeleton x-reverse-webmcp block per operation,
    /// for an agent to fill in. The cure for the blank page.
    #[arg(long)]
    init: bool,
    /// Save these wants as a recipe under this name.
    #[arg(long, value_name = "NAME")]
    save: Option<String>,
    /// List saved recipes and stop.
    #[arg(long)]
    list: bool,
    /// Answer a fork without a model, by rewriting the ambiguous part of every want:
    /// --answer "customer(name='Acme')=customer(id=11)". Repeatable.
    #[arg(long = "answer", value_name = "OLD=NEW")]
    answers: Vec<String>,
    /// Answer a fork by asking the model which one was meant. Costs one model call, and only
    /// when a fork actually happens.
    #[arg(long)]
    answer_with_model: bool,
    /// Write the receipt and enough state to --resume this run later.
    #[arg(long, value_name = "FILE")]
    receipt_out: Option<PathBuf>,

    // ---- where ----
    /// The app's base URL, or a path to an OpenAPI file for offline checks. A recipe remembers
    /// the app it was saved against, so with --recipe this is optional and overrides it.
    #[arg(long)]
    app: Option<String>,
    /// Surfaces the plan may use: api, mcp, webmcp, a11y, pixels.
    #[arg(long, default_value = "api")]
    surfaces: String,
    /// Where recipes live.
    #[arg(long, value_name = "DIR")]
    recipes_dir: Option<PathBuf>,
    /// Messages-API base URL for the planner. Defaults to ANTHROPIC_BASE_URL, else Anthropic.
    #[arg(long)]
    base_url: Option<String>,
    /// Planner model.
    #[arg(long, default_value = "claude-opus-5")]
    model: String,
    /// Planner effort: low, medium, high, xhigh, max.
    #[arg(long, default_value = "low")]
    effort: String,
    /// API key. Defaults to ANTHROPIC_API_KEY; a local gateway usually needs none.
    #[arg(long)]
    api_key: Option<String>,
    /// Machine-readable output.
    #[arg(long)]
    json: bool,
}

/// What happened, as a number a caller can branch on. 1 is an unexpected internal error and 2 is
/// clap's own bad-usage code, so these start at 10 and never collide with either.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(i32)]
enum Exit {
    Done = 0,
    WantsRejected = 10,
    NeedsAnswer = 11,
    Refused = 12,
    Unreachable = 13,
    PlannerFailed = 14,
    RunFailed = 15,
}

/// A failure with a machine-readable code, whatever the output mode.
struct Fail {
    exit: Exit,
    code: &'static str,
    message: String,
    detail: Value,
    /// A receipt, when the run got far enough to produce one. It becomes the JSON body so a
    /// failed run prints exactly one object with the same fields a successful run prints.
    body: Option<Value>,
}

impl Fail {
    fn new(exit: Exit, code: &'static str, message: impl Into<String>) -> Fail {
        Fail { exit, code, message: message.into(), detail: Value::Null, body: None }
    }

    fn with(mut self, detail: Value) -> Fail {
        self.detail = detail;
        self
    }

    fn with_body(mut self, body: Value) -> Fail {
        self.body = Some(body);
        self
    }

    /// Print in the caller's chosen shape. JSON goes to stdout so a pipe gets one object either way.
    fn emit(&self, json: bool) {
        if json {
            let mut body = self.body.clone().filter(Value::is_object).unwrap_or_else(|| serde_json::json!({}));
            body["ok"] = serde_json::json!(false);
            body["code"] = serde_json::json!(self.code);
            body["message"] = serde_json::json!(self.message);
            if !self.detail.is_null() {
                body["detail"] = self.detail.clone();
            }
            println!("{}", serde_json::to_string_pretty(&body).unwrap_or_default());
        } else {
            eprintln!("{}", self.message);
        }
    }
}

/// An error from anywhere that has no better classification.
fn unreachable_fail(e: impl std::fmt::Display) -> Fail {
    Fail::new(Exit::Unreachable, "unreachable", format!("{e:#}"))
}

const EXAMPLES: &str = "\
Examples:
  rwmcp --app http://localhost:8000 --world
  rwmcp --app http://localhost:8000 --goal \"invoice every customer and send it\"
  rwmcp --app http://localhost:8000 --goal \"...\" --save billing --run --yes
  rwmcp --app http://localhost:8000 --recipe billing --set who=Globex --run --yes

A recipe is a plan that already worked, with the changing bits left as $placeholders.
Re-running one costs no model calls.";

/// A plan that worked, kept so it can be run again with different values. JSON, because the thing
/// that writes it is usually an agent and the thing that reads it is usually a program.
///
/// ```json
/// {
///   "name": "billing",
///   "app": "http://localhost:8000",
///   "goal": "invoice every customer and send it",
///   "params": ["who"],
///   "wants": [
///     "invoice(customer=customer(name=$who)).exists",
///     "invoice(customer=customer(name=$who)).status='sent'"
///   ]
/// }
/// ```
#[derive(Serialize, Deserialize)]
struct Recipe {
    name: String,
    /// The app this plan was made for. Running it elsewhere is usually a mistake.
    app: String,
    /// What it was originally asked to do.
    goal: String,
    /// Placeholder names this recipe expects, in the order they were found.
    #[serde(default)]
    params: Vec<String>,
    /// Wants, with `$placeholders` where the values change.
    wants: Vec<String>,
}

impl Recipe {
    /// A name looks in the recipes directory; anything with a separator or a .json suffix is a path.
    fn path_for(dir: &std::path::Path, name_or_path: &str) -> PathBuf {
        if name_or_path.ends_with(".json") || name_or_path.contains('/') {
            PathBuf::from(name_or_path)
        } else {
            dir.join(format!("{name_or_path}.json"))
        }
    }

    fn load(p: &std::path::Path) -> anyhow::Result<Recipe> {
        let text = std::fs::read_to_string(p).with_context(|| format!("reading recipe {}", p.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing recipe {}", p.display()))
    }
}

/// Enough to pick a run back up in another process. The plan id matters as much as the ledger:
/// idempotency keys are `{plan_id}/{content hash}`, so resuming under a fresh id would match
/// nothing already done and send every email a second time.
#[derive(Serialize, Deserialize)]
struct RunState {
    app: String,
    goal: String,
    plan_id: String,
    surfaces: Vec<String>,
    wants: Vec<String>,
    ledger: Ledger,
    receipt: Receipt,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).with_writer(std::io::stderr).init();
    let mut cli = Cli::parse();
    let json = cli.json;
    let code = match cli.resolve_app() {
        Ok(()) => match go(&cli).await {
            Ok(()) => Exit::Done,
            Err(f) => {
                f.emit(json);
                f.exit
            }
        },
        Err(e) => {
            let f = unreachable_fail(e);
            f.emit(json);
            f.exit
        }
    };
    std::process::exit(code as i32);
}

async fn go(cli: &Cli) -> Result<(), Fail> {
    if cli.list {
        return list_recipes(cli).map_err(unreachable_fail);
    }
    if cli.init {
        return init_blocks(cli).await;
    }
    if cli.validate {
        return validate_world(cli).await;
    }
    if cli.world {
        return show_world(cli).await;
    }
    act(cli).await
}

// ---------- pieces ----------

const DEFAULT_APP: &str = "http://127.0.0.1:47310";

impl Cli {
    fn surface_list(&self) -> Vec<String> {
        self.surfaces.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
    }

    fn opts(&self) -> CompileOptions {
        CompileOptions { plan_id: format!("cli-{}", std::process::id()), surfaces: self.surface_list() }
    }

    /// The same options under a plan id from somewhere else, so a resumed run keeps its keys.
    fn opts_under(&self, plan_id: &str) -> CompileOptions {
        CompileOptions { plan_id: plan_id.to_string(), surfaces: self.surface_list() }
    }

    fn offline(&self) -> bool {
        std::path::Path::new(self.app()).is_file()
    }

    /// Where the app is: the flag if given, else whatever the recipe remembered, else the default.
    fn resolve_app(&mut self) -> anyhow::Result<()> {
        if self.app.is_some() {
            return Ok(());
        }
        if let Some(name) = &self.recipe {
            let r = Recipe::load(&Recipe::path_for(&self.recipes_dir(), name))?;
            if !r.app.is_empty() {
                self.app = Some(r.app);
                return Ok(());
            }
        }
        self.app = Some(DEFAULT_APP.to_string());
        Ok(())
    }

    /// The raw OpenAPI document, from a file or from the running app.
    async fn openapi(&self) -> anyhow::Result<Value> {
        if self.offline() {
            return serde_json::from_str(&std::fs::read_to_string(self.app())?).with_context(|| format!("reading {}", self.app()));
        }
        let url = format!("{}/openapi.json", self.app().trim_end_matches('/'));
        reqwest::get(&url).await.with_context(|| format!("fetching {url}"))?.json().await.with_context(|| format!("parsing {url}"))
    }

    async fn world(&self) -> anyhow::Result<World> {
        if self.offline() {
            let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(self.app())?).with_context(|| format!("reading {}", self.app()))?;
            return Ok(World::from_openapi(&doc)?);
        }
        rwmcp::world_from(self.app()).await.with_context(|| {
            format!("reading {}/openapi.json — is the app running, and does it publish x-reverse-webmcp blocks?", self.app().trim_end_matches('/'))
        })
    }

    /// The resolved app, once `resolve_app` has run.
    fn app(&self) -> &str {
        self.app.as_deref().unwrap_or(DEFAULT_APP)
    }

    fn recipes_dir(&self) -> PathBuf {
        self.recipes_dir.clone().unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".rwmcp/recipes")
        })
    }

    /// `--set k=v` pairs, parsed once.
    fn bindings(&self) -> anyhow::Result<BTreeMap<String, Val>> {
        let mut out = BTreeMap::new();
        for s in &self.sets {
            let (k, v) = s.split_once('=').with_context(|| format!("--set wants KEY=VALUE, got {s:?}"))?;
            let val = v.parse::<i64>().map(Val::Num).unwrap_or_else(|_| Val::Str(v.to_string()));
            out.insert(k.trim().to_string(), val);
        }
        Ok(out)
    }
}

/// Wants from a file: one per line, `#` comments and blank lines ignored.
fn read_wants(path: &std::path::Path) -> anyhow::Result<Vec<String>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(text.lines().map(|l| l.split('#').next().unwrap_or("").trim().to_string()).filter(|l| !l.is_empty()).collect())
}

/// Every `$placeholder` a set of wants still expects.
fn placeholders(wants: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in wants {
        let Ok(p) = Pred::parse(w) else { continue };
        collect_vars(&p, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

fn collect_vars(p: &Pred, out: &mut Vec<String>) {
    for (_, v) in &p.args {
        collect_vars_val(v, out);
    }
    collect_vars_val(&p.value, out);
}

fn collect_vars_val(v: &Val, out: &mut Vec<String>) {
    match v {
        Val::Var(n, _) => out.push(n.clone()),
        Val::List(xs) | Val::Each(xs) => xs.iter().for_each(|x| collect_vars_val(x, out)),
        Val::All(x) => collect_vars_val(x, out),
        Val::Entity(p) => collect_vars(p, out),
        _ => {}
    }
}

/// Replace `$placeholders` with the values given on the command line.
fn fill(wants: &[String], bind: &BTreeMap<String, Val>) -> Vec<String> {
    let lookup = |n: &str| bind.get(n).cloned();
    wants
        .iter()
        .map(|w| match Pred::parse(w) {
            Ok(p) => p.subst(&lookup).to_string(),
            Err(_) => w.clone(),
        })
        .collect()
}

// ---------- the three things this command does ----------

fn list_recipes(cli: &Cli) -> anyhow::Result<()> {
    let dir = cli.recipes_dir();
    let mut found = false;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut names: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
        names.sort();
        for p in names {
            let Ok(r) = Recipe::load(&p) else { continue };
            found = true;
            let params = if r.params.is_empty() { "no parameters".to_string() } else { format!("--set {}", r.params.join("=… --set ")) + "=…" };
            println!("{:<16} {:<34} {:<28} {}", r.name, r.goal.chars().take(34).collect::<String>(), r.app, params);
        }
    }
    if !found {
        println!("No recipes in {}. Save one with --save NAME.", dir.display());
    }
    Ok(())
}

/// The review `annotate-world-model` describes, made runnable. Nothing needs to be running: this
/// reads the document and says what cannot be right.
async fn validate_world(cli: &Cli) -> Result<(), Fail> {
    let world = cli.world().await.map_err(unreachable_fail)?;
    let findings = world.validate();
    let (bad, warn): (Vec<_>, Vec<_>) = findings.iter().partition(|f| f.fatal);

    // One object on stdout whichever way this goes. Printing the report here and then letting the
    // failure print its own left a parser with two objects and a syntax error.
    let body = serde_json::json!({
        "ok": bad.is_empty(),
        "operations": world.ops.len(),
        "entities": world.entities.len(),
        "errors": bad,
        "warnings": warn,
    });
    if cli.json {
        if bad.is_empty() {
            println!("{}", serde_json::to_string_pretty(&body).unwrap_or_default());
        }
    } else {
        println!("{} operations, {} entities.\n", world.ops.len(), world.entities.len());
        for f in &bad {
            println!("  ERROR  {}: {}", f.op, f.message);
        }
        for f in &warn {
            println!("  note   {}: {}", f.op, f.message);
        }
        if findings.is_empty() {
            println!("  Nothing to report. Every operation names entities and fields that exist,");
            println!("  every footprint selector binds, every `before` points somewhere, and every");
            println!("  pair of writers to one thing has a declared order.");
        }
    }
    if bad.is_empty() {
        return Ok(());
    }
    Err(Fail::new(Exit::WantsRejected, "world_invalid", format!("{} problem(s) in the world model", bad.len())).with_body(body))
}

/// Compile the wants, compile them reversed, and compare. A plan that changes when the wants are
/// listed in a different order is a plan that depends on typing order, which is the bug the
/// compiler's edge orientation once had: a report want listed first got a report over drafts.
fn order_check(intent: &Intent, world: &World, opts: &CompileOptions, json: bool) -> Result<(), Fail> {
    let forward =
        compile(intent, world, opts).map_err(|e| Fail::new(Exit::WantsRejected, "compile_failed", e.to_string()).with(serde_json::json!({ "error": e })))?;
    let mut reversed_wants = intent.wants.clone();
    reversed_wants.reverse();
    let reversed = compile(&Intent { wants: reversed_wants, ..intent.clone() }, world, opts)
        .map_err(|e| Fail::new(Exit::WantsRejected, "compile_failed_reversed", e.to_string()).with(serde_json::json!({ "error": e })))?;

    let differences = plan_diff(&forward, &reversed);
    let body = serde_json::json!({
        "ok": differences.is_empty(),
        "wants": intent.wants.len(),
        "steps": forward.nodes.len(),
        "differences": differences,
    });
    if json {
        if differences.is_empty() {
            println!("{}", serde_json::to_string_pretty(&body).unwrap_or_default());
        }
    } else if differences.is_empty() {
        println!("{} wants, {} steps, {} deep.", intent.wants.len(), forward.nodes.len(), forward.depth());
        println!("Reversing the wants gives the same plan, so nothing here depends on the order they were written in.");
    } else {
        println!("Reversing the wants changes the plan:\n");
        for d in &differences {
            println!("  {d}");
        }
    }
    if differences.is_empty() {
        return Ok(());
    }
    Err(Fail::new(Exit::WantsRejected, "order_dependent", "the plan depends on the order the wants were written in").with_body(body))
}

/// What changed between two compilations of the same wants. Keys are content-addressed, so the
/// same work has the same key whatever order it was found in; node ids are not, so compare on
/// keys and on the ordering between them.
fn plan_diff(a: &Plan, b: &Plan) -> Vec<String> {
    let mut out = Vec::new();
    if a.nodes.len() != b.nodes.len() {
        out.push(format!("{} steps one way, {} the other", a.nodes.len(), b.nodes.len()));
    }
    if a.depth() != b.depth() {
        out.push(format!("{} deep one way, {} the other", a.depth(), b.depth()));
    }
    let sig = |p: &Plan| -> std::collections::BTreeSet<String> {
        let by_id: BTreeMap<&str, &str> = p.nodes.iter().map(|n| (n.id.as_str(), n.op.as_str())).collect();
        p.edges.iter().map(|(from, to)| format!("{} -> {}", by_id.get(from.as_str()).unwrap_or(&"?"), by_id.get(to.as_str()).unwrap_or(&"?"))).collect()
    };
    let (ea, eb) = (sig(a), sig(b));
    for e in ea.difference(&eb) {
        out.push(format!("only in the declared order: {e}"));
    }
    for e in eb.difference(&ea) {
        out.push(format!("only in the reversed order: {e}"));
    }
    out
}

/// Read a plain OpenAPI document and print the block each operation still needs. An agent fills
/// these in; the skill says how, and --validate says whether it worked.
async fn init_blocks(cli: &Cli) -> Result<(), Fail> {
    let doc = cli.openapi().await.map_err(unreachable_fail)?;
    let paths = doc
        .get("paths")
        .and_then(|p| p.as_object())
        .ok_or_else(|| Fail::new(Exit::WantsRejected, "no_paths", "that OpenAPI document has no `paths`, so there is nothing to annotate"))?;

    let mut ops = serde_json::Map::new();
    let mut already = 0usize;
    for (path, methods) in paths {
        let Some(methods) = methods.as_object() else { continue };
        for (method, spec) in methods {
            if spec.get("x-reverse-webmcp").is_some() {
                already += 1;
                continue;
            }
            let name = spec.get("operationId").and_then(|s| s.as_str()).unwrap_or("").to_string();
            let mut params: Vec<String> = spec
                .get("parameters")
                .and_then(|p| p.as_array())
                .map(|a| a.iter().filter_map(|p| p.get("name").and_then(|n| n.as_str()).map(str::to_string)).collect())
                .unwrap_or_default();
            if let Some(props) = spec.pointer("/requestBody/content/application~1json/schema/properties").and_then(|p| p.as_object()) {
                params.extend(props.keys().cloned());
            }
            // A GET reads and a POST writes, more often than not. It is a starting point, not a guess
            // to trust: the skill says how to check it, and --validate says whether it holds up.
            let reading = method.eq_ignore_ascii_case("get");
            let reads: Vec<&str> = if reading { vec!["entity:*"] } else { vec![] };
            let writes: Vec<&str> = if reading { vec![] } else { vec!["entity:new"] };
            ops.insert(
                if name.is_empty() { format!("{} {path}", method.to_uppercase()) } else { name },
                serde_json::json!({
                    "post": "entity(arg=$param).field=value   # what is true afterwards; drop the whole block for a read",
                    "requires": [],
                    "produces": null,
                    "reads": reads,
                    "writes": writes,
                    "external": false,
                    "before": [],
                    "surfaces": {"api": 1},
                    "_parameters_you_can_use": params,
                }),
            );
        }
    }

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({"ok": true, "annotated": already, "todo": ops})).unwrap_or_default());
        return Ok(());
    }
    println!("{already} operation(s) already annotated, {} still to do.\n", ops.len());
    println!("x-reverse-webmcp-entities goes at the document root:\n");
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({"x-reverse-webmcp-entities": {"entity": {"id": "id", "fields": ["a_field"]}}})).unwrap_or_default()
    );
    println!("\nAnd one x-reverse-webmcp block per operation:\n");
    println!("{}", serde_json::to_string_pretty(&Value::Object(ops)).unwrap_or_default());
    println!("\nFill them in, then run --validate.");
    Ok(())
}

async fn show_world(cli: &Cli) -> Result<(), Fail> {
    let world = cli.world().await.map_err(unreachable_fail)?;
    if cli.json {
        let ops: Vec<serde_json::Value> = world
            .ops
            .iter()
            .map(|o| {
                serde_json::json!({
                    "op": o.name, "kind": format!("{:?}", o.kind).to_lowercase(),
                    "makes_true": o.post.as_ref().map(|p| p.to_string()),
                    "requires": o.requires.iter().map(|r| r.to_string()).collect::<Vec<_>>(),
                    "reads": o.reads, "writes": o.writes, "surfaces": o.surfaces,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({"entities": world.entities, "operations": ops})).unwrap_or_default());
        return Ok(());
    }
    print!("{}", world.summary());
    let missing: Vec<&str> = world.ops.iter().filter(|o| o.post.is_none()).map(|o| o.name.as_str()).collect();
    if !missing.is_empty() {
        println!("\nOperations with no postcondition, so nothing can be planned through them:");
        for m in &missing {
            println!("  {m}");
        }
        println!("\nGive each a `post` in the app's x-reverse-webmcp block to make it plannable.");
    }
    Ok(())
}

async fn act(cli: &Cli) -> Result<(), Fail> {
    let world = cli.world().await.map_err(unreachable_fail)?;
    check_surfaces(cli, &world)?;
    let bind = cli.bindings().map_err(|e| Fail::new(Exit::WantsRejected, "bad_set", format!("{e:#}")))?;

    // A resumed run brings its own ledger and plan id; everything else starts empty.
    let prior: Option<RunState> = match &cli.resume {
        Some(p) => Some(load_state(p).map_err(unreachable_fail)?),
        None => None,
    };
    let opts = match &prior {
        Some(st) => cli.opts_under(&st.plan_id),
        None => cli.opts(),
    };
    let mut ledger = prior.as_ref().map(|st| st.ledger.clone()).unwrap_or_default();

    // Where the wants come from: a saved run, a model once, a file, or a recipe that already worked.
    if let Some(st) = &prior {
        let intent = Intent { goal: st.goal.clone(), wants: st.wants.clone(), ..Default::default() };
        let plan = compile(&intent, &world, &opts)
            .map_err(|e| Fail::new(Exit::WantsRejected, "compile_failed", e.to_string()).with(serde_json::json!({ "error": e })))?;
        if !cli.json {
            let done = ledger.completed(&plan).len();
            println!("{}\n\nResuming: {done} of {} steps already succeeded.", plan.render(), plan.nodes.len());
        }
        return execute(cli, &intent, &plan, ledger, &world, &opts).await;
    }

    let (goal, raw): (String, Vec<String>) = match (&cli.goal, &cli.wants, &cli.recipe) {
        (Some(goal), _, _) => {
            let facts = if cli.offline() { String::new() } else { planner::world_facts(&world, cli.app()).await.unwrap_or_default() };
            let sampler = ModelClient::from_env(&cli.model, &cli.effort, false, cli.base_url.as_deref(), cli.api_key.as_deref())
                .map_err(|e| Fail::new(Exit::PlannerFailed, "no_model", format!("{e:#}")))?;
            let mut ctx = planner::Ctx::new(&world, &opts).facts(&facts);
            if !cli.offline() {
                ctx = ctx.at(cli.app());
            }
            let i = planner::plan_with_lint(&planner::Ask::new(goal), &ctx, &sampler, &mut ledger)
                .await
                .map_err(|e| Fail::new(Exit::PlannerFailed, "planner_failed", format!("{e:#}")))?;
            (goal.clone(), i.wants)
        }
        (_, Some(path), _) => (format!("wants from {}", path.display()), read_wants(path).map_err(unreachable_fail)?),
        (_, _, Some(name)) => {
            let r = Recipe::load(&Recipe::path_for(&cli.recipes_dir(), name)).map_err(unreachable_fail)?;
            (r.goal, r.wants)
        }
        (None, None, None) => {
            return Err(Fail::new(
                Exit::WantsRejected,
                "no_source",
                "say what you want: --goal \"...\", --wants FILE, --recipe NAME, or --resume FILE (--world to look around first)",
            ))
        }
    };

    // A recipe keeps its placeholders; --set fills them in for this run.
    if let Some(name) = &cli.save {
        let dir = cli.recipes_dir();
        std::fs::create_dir_all(&dir).map_err(unreachable_fail)?;
        let recipe = Recipe { name: name.clone(), app: cli.app().to_string(), goal: goal.clone(), params: placeholders(&raw), wants: raw.clone() };
        let path = Recipe::path_for(&dir, name);
        std::fs::write(&path, serde_json::to_string_pretty(&recipe).map_err(unreachable_fail)?).map_err(unreachable_fail)?;
        println!(
            "Saved {}{}.",
            path.display(),
            if recipe.params.is_empty() { String::new() } else { format!(", expecting --set {}=…", recipe.params.join("=… --set ")) }
        );
    }

    let wants = fill(&raw, &bind);
    let missing = placeholders(&wants);
    if !missing.is_empty() {
        return Err(Fail::new(
            Exit::WantsRejected,
            "missing_parameters",
            format!("these placeholders still need values: {}", missing.iter().map(|m| format!("--set {m}=…")).collect::<Vec<_>>().join(" ")),
        )
        .with(serde_json::json!({ "params": missing })));
    }

    let intent = Intent { goal: goal.clone(), wants, ..Default::default() };
    let intent = match cli.offline() {
        false => planner::expand_selectors(&intent, &world, cli.app()).await.unwrap_or(intent),
        true => intent,
    };

    let errs = lint(&intent, &world, &opts);
    if !errs.is_empty() {
        let mut prose = String::from("These wants do not hold up:\n");
        for e in &errs {
            prose.push_str(&format!("\n  {e}\n"));
            if let (Some(w), Some(at)) = (e.want(), e.at()) {
                prose.push_str(&format!("    {w}\n    {}^\n", " ".repeat(at.min(w.len()))));
            }
        }
        return Err(Fail::new(Exit::WantsRejected, "wants_rejected", prose.trim_end())
            .with(serde_json::json!({"errors": errs, "codes": errs.iter().map(|e| e.code()).collect::<Vec<_>>()})));
    }

    if cli.order_check {
        return order_check(&intent, &world, &opts, cli.json);
    }

    let plan =
        compile(&intent, &world, &opts).map_err(|e| Fail::new(Exit::WantsRejected, "compile_failed", e.to_string()).with(serde_json::json!({ "error": e })))?;
    if cli.json && !cli.run {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({"ok": true, "intent": intent, "plan": plan})).unwrap_or_default());
        return Ok(());
    }
    if !cli.json {
        println!("{}", plan.render());
        describe(&plan, &ledger);
    }
    if !cli.run {
        if !cli.json {
            println!("\nNothing was done. Add --run to carry it out.");
        }
        return Ok(());
    }
    execute(cli, &intent, &plan, ledger, &world, &opts).await
}

/// Resolve a fork, if the caller offered a way to. `None` means they did not, so the fork stands
/// and the run stops with a question — which is the right outcome when nobody said what they meant.
async fn answer_fork(cli: &Cli, intent: &Intent, world: &World, outcome: &Outcome, ledger: &mut Ledger, opts: &CompileOptions) -> Result<Option<Intent>, Fail> {
    if !cli.answers.is_empty() {
        let mut wants = intent.wants.clone();
        for a in &cli.answers {
            let (old, new) = a.split_once("=>").ok_or_else(|| {
                Fail::new(Exit::WantsRejected, "bad_answer", format!("--answer takes OLD=>NEW, as in \"customer(name='Acme')=>customer(id=11)\"; got {a:?}"))
            })?;
            let (old, new) = (old.trim(), new.trim());
            if !wants.iter().any(|w| w.contains(old)) {
                return Err(Fail::new(Exit::WantsRejected, "bad_answer", format!("no want contains {old:?}, so this answer would change nothing"))
                    .with(serde_json::json!({ "wants": wants })));
            }
            wants = wants.iter().map(|w| w.replace(old, new)).collect();
        }
        return Ok(Some(Intent { wants, ..intent.clone() }));
    }
    if cli.answer_with_model {
        let sampler = ModelClient::from_env(&cli.model, &cli.effort, false, cli.base_url.as_deref(), cli.api_key.as_deref())
            .map_err(|e| Fail::new(Exit::PlannerFailed, "no_model", format!("{e:#}")))?;
        let facts = planner::world_facts(world, cli.app()).await.unwrap_or_default();
        let fork = planner::ForkQuestion { ask: outcome.yield_reason.clone().unwrap_or_default(), evidence: outcome.evidence.clone().unwrap_or(Value::Null) };
        let ctx = planner::Ctx::new(world, opts).facts(&facts).at(cli.app());
        let answered = planner::answer_fork(&planner::Ask::new(&intent.goal), &ctx, intent, &fork, &sampler, ledger)
            .await
            .map_err(|e| Fail::new(Exit::PlannerFailed, "fork_answer_failed", format!("{e:#}")))?;
        return Ok(Some(answered));
    }
    Ok(None)
}

fn load_state(p: &std::path::Path) -> anyhow::Result<RunState> {
    let text = std::fs::read_to_string(p).with_context(|| format!("reading run state {}", p.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing run state {}", p.display()))
}

/// A surface this app never mentions is a typo, not a capability. Left alone it becomes an
/// unavailable effector and surfaces much later as "no surface can do this", pointing at the
/// operation rather than at the flag.
fn check_surfaces(cli: &Cli, world: &World) -> Result<(), Fail> {
    let known: std::collections::BTreeSet<&str> = world.ops.iter().flat_map(|o| o.surfaces.keys().map(|s| s.as_str())).collect();
    let asked = cli.surface_list();
    let unknown: Vec<&String> = asked.iter().filter(|s| !known.contains(s.as_str())).collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let mut names: Vec<&str> = known.into_iter().collect();
    names.sort_unstable();
    let m = format!(
        "--surfaces names {}, which this app does not offer. It offers: {}",
        unknown.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(", "),
        names.join(", ")
    );
    Err(Fail::new(Exit::WantsRejected, "unknown_surface", m).with(serde_json::json!({ "unknown": unknown, "available": names })))
}

/// The part a person reads before saying yes.
fn describe(plan: &Plan, ledger: &Ledger) {
    let external = plan.nodes.iter().filter(|n| n.external).count();
    let mut kinds: Vec<&str> = plan.nodes.iter().filter(|n| n.external).map(|n| n.op.as_str()).collect();
    kinds.sort_unstable();
    kinds.dedup();
    let waits = plan.nodes.iter().filter(|n| n.kind == rwmcp::world::OpKind::Event).count();
    let screens = plan.nodes.iter().filter(|n| n.surface == "a11y" || n.surface == "pixels").count();
    println!("{} step{}, {} deep.", plan.nodes.len(), plural(plan.nodes.len()), plan.depth());
    if external > 0 {
        println!("{external} step{} leave the system (email, money): {}", plural(external), kinds.join(", "));
    }
    if waits > 0 {
        println!("{waits} step{} wait for something outside to happen.", plural(waits));
    }
    if screens > 0 {
        println!("{screens} step{} need a screen, so they run one at a time.", plural(screens));
    }
    let n = ledger.sample_count();
    println!("Planning cost {n} model call{}.", plural(n as usize));
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

async fn execute(cli: &Cli, intent: &Intent, plan: &Plan, mut ledger: Ledger, world: &World, opts: &CompileOptions) -> Result<(), Fail> {
    if cli.offline() {
        return Err(Fail::new(Exit::Unreachable, "offline", "--app is a file, so there is nothing to run against; give me the app's URL"));
    }
    let external = plan.nodes.iter().filter(|n| n.external).count();
    if external > 0 && !cli.yes {
        return Err(Fail::new(
            Exit::Refused,
            "needs_confirmation",
            format!("{external} step(s) leave the system. Re-run with --yes once the plan above looks right."),
        )
        .with(serde_json::json!({ "external_steps": external })));
    }
    let shared = Arc::new(world.clone());
    let sched = Scheduler {
        effectors: default_effectors(cli.app(), shared.clone(), &cli.surface_list()),
        bus: Some(EventBus::connect(cli.app()).await.map_err(unreachable_fail)?),
        pools: Default::default(),
        policy: Default::default(),
        recorder: Recorder::new(shared.clone()),
    };
    if !cli.json {
        println!("\nRunning.");
    }

    let mut intent = intent.clone();
    let mut plan = plan.clone();
    let mut outcome = sched.run(&plan, &mut ledger).await;

    // A fork is a question, not a failure. Answering it recompiles the same wants with the
    // ambiguity resolved; every other want keeps its text, so it keeps its key and is not redone.
    if outcome.status == Status::NeedThink {
        if let Some(answered) = answer_fork(cli, &intent, world, &outcome, &mut ledger, opts).await? {
            intent = answered;
            plan = compile(&intent, world, opts)
                .map_err(|e| Fail::new(Exit::WantsRejected, "compile_failed", e.to_string()).with(serde_json::json!({ "error": e })))?;
            let done = ledger.completed(&plan);
            if !cli.json {
                println!("Answered. {} of {} steps already done; carrying on.", done.len(), plan.nodes.len());
            }
            outcome = sched.resume(&plan, &mut ledger, &done).await;
        }
    }

    let plan = &plan;
    let receipt = ledger.receipt(plan, outcome.status, outcome.yield_reason, outcome.evidence, outcome.error);

    if let Some(path) = &cli.receipt_out {
        let state = RunState {
            app: cli.app().to_string(),
            goal: intent.goal.clone(),
            plan_id: plan.plan_id.clone(),
            surfaces: cli.surface_list(),
            wants: intent.wants.clone(),
            ledger: ledger.clone(),
            receipt: receipt.clone(),
        };
        std::fs::write(path, serde_json::to_string_pretty(&state).map_err(unreachable_fail)?).map_err(unreachable_fail)?;
        if !cli.json {
            println!("Saved {}. Pick it back up with --resume {}.", path.display(), path.display());
        }
    }

    let as_json = || {
        let mut body = serde_json::to_value(&receipt).unwrap_or_default();
        body["ok"] = serde_json::json!(receipt.status == Status::Committed);
        body
    };
    if cli.json {
        if receipt.status == Status::Committed {
            println!("{}", serde_json::to_string_pretty(&as_json()).unwrap_or_default());
        }
    } else {
        println!();
        for e in &receipt.effects {
            println!("  {} {} {}", if e.ok { "ok  " } else { "FAIL" }, e.op, e.observed.get("id").map(|i| format!("#{i}")).unwrap_or_default());
        }
        println!(
            "\n{:?} · {} model call{} · {} ms planning · {} ms running · {} at once",
            receipt.status,
            receipt.samples,
            plural(receipt.samples as usize),
            receipt.plan_ms,
            receipt.busy_ms,
            receipt.max_parallel
        );
        if let Some(q) = &receipt.yield_reason {
            println!("\nIt stopped to ask: {q}\nAnswer with --answer \"OLD=>NEW\", or --answer-with-model to let the model choose.");
        }
        if let Some(e) = &receipt.error {
            println!("\nIt stopped: {e}");
        }
    }
    match receipt.status {
        Status::Committed => Ok(()),
        Status::NeedThink => {
            Err(Fail::new(Exit::NeedsAnswer, "needs_answer", receipt.yield_reason.clone().unwrap_or_else(|| "the plan stopped to ask a question".into()))
                .with_body(as_json()))
        }
        Status::Error => Err(Fail::new(Exit::RunFailed, "run_failed", receipt.error.clone().unwrap_or_else(|| "a step failed".into())).with_body(as_json())),
    }
}
