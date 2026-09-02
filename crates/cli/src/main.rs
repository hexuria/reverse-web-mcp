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
use rwmcp::{compile, default_effectors, CompileOptions, Plan, Scheduler, Status, World};
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
    /// Save these wants as a recipe under this name.
    #[arg(long, value_name = "NAME")]
    save: Option<String>,
    /// List saved recipes and stop.
    #[arg(long)]
    list: bool,

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
    let bind = cli.bindings().map_err(|e| Fail::new(Exit::WantsRejected, "bad_set", format!("{e:#}")))?;
    let mut ledger = Ledger::new();

    // Where the wants come from: a model once, a file, or a recipe that already worked.
    let (goal, raw): (String, Vec<String>) = match (&cli.goal, &cli.wants, &cli.recipe) {
        (Some(goal), _, _) => {
            let facts = if cli.offline() { String::new() } else { planner::world_facts(cli.app()).await.unwrap_or_default() };
            let sampler = ModelClient::from_env(&cli.model, &cli.effort, false, cli.base_url.as_deref(), cli.api_key.as_deref())
                .map_err(|e| Fail::new(Exit::PlannerFailed, "no_model", format!("{e:#}")))?;
            let base = if cli.offline() { None } else { Some(cli.app()) };
            let i = planner::plan_with_lint(&planner::Ask::new(goal), &world, &facts, &sampler, &mut ledger, &cli.opts(), base)
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
                "say what you want: --goal \"...\", --wants FILE, or --recipe NAME (--world to look around first)",
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
        false => planner::expand_selectors(&intent, cli.app()).await.unwrap_or(intent),
        true => intent,
    };

    let errs = lint(&intent, &world, &cli.opts());
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

    let plan = compile(&intent, &world, &cli.opts())
        .map_err(|e| Fail::new(Exit::WantsRejected, "compile_failed", e.to_string()).with(serde_json::json!({ "error": e })))?;
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
    execute(cli, &plan, ledger, &world).await
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

async fn execute(cli: &Cli, plan: &Plan, mut ledger: Ledger, world: &World) -> Result<(), Fail> {
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
    let world = Arc::new(world.clone());
    let sched = Scheduler {
        effectors: default_effectors(cli.app(), world.clone(), &cli.surface_list()),
        bus: Some(EventBus::connect(cli.app()).await.map_err(unreachable_fail)?),
        pools: Default::default(),
        policy: Default::default(),
        recorder: Recorder::new(world.clone()),
    };
    if !cli.json {
        println!("\nRunning.");
    }
    let outcome = sched.run(plan, &mut ledger).await;
    let receipt = ledger.receipt(plan, outcome.status, outcome.yield_reason, outcome.evidence, outcome.error);

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
            println!("\nIt stopped to ask: {q}\nAnswer by naming what you meant, then run it again.");
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
