//! `rwmcp` — run a goal against an app that describes itself.
//!
//! Nothing here needs Rust from you. An app publishes an OpenAPI document with
//! `x-reverse-webmcp` blocks; an agent writes those blocks and a list of wants; this command
//! reads both, compiles a plan, shows it to you, and runs it.
//!
//!   rwmcp world --app URL              what the app can be asked for
//!   rwmcp check --app URL --wants FILE do these wants make sense
//!   rwmcp plan  --app URL --goal "..."  what would happen, without doing it
//!   rwmcp run   --app URL --goal "..."  do it, and print a receipt

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use rwmcp::events::EventBus;
use rwmcp::intent::{lint, Constraints, Intent};
use rwmcp::ledger::{Ledger, Recorder};
use rwmcp::planner::{self, ModelClient, Sampler};
use rwmcp::{compile, default_effectors, CompileOptions, Plan, Scheduler, Status, World};

#[derive(Parser)]
#[command(name = "rwmcp", version, about = "Run a goal against an app that describes itself")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// Where the app is and how to reach a model. Every subcommand takes these.
#[derive(clap::Args, Clone)]
struct Common {
    /// The app's base URL, or a path to an OpenAPI file for offline checks.
    #[arg(long, global = true, default_value = "http://127.0.0.1:47310")]
    app: String,
    /// Surfaces the plan may use, cheapest first: api, mcp, webmcp, a11y, pixels.
    #[arg(long, global = true, default_value = "api")]
    surfaces: String,
    /// Messages-API base URL for the planner. Defaults to ANTHROPIC_BASE_URL, else Anthropic.
    #[arg(long, global = true)]
    base_url: Option<String>,
    /// Planner model.
    #[arg(long, global = true, default_value = "claude-opus-5")]
    model: String,
    /// Planner effort: low, medium, high, xhigh, max.
    #[arg(long, global = true, default_value = "low")]
    effort: String,
    /// API key. Defaults to ANTHROPIC_API_KEY; a local gateway usually needs none.
    #[arg(long, global = true)]
    api_key: Option<String>,
    /// Print machine-readable JSON instead of prose.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show what the app says it can do: its entities and the facts it can make true.
    World {
        #[command(flatten)]
        common: Common,
    },
    /// Check a list of wants against the app, without planning or running anything.
    Check {
        /// A file of wants, one per line. `#` starts a comment.
        #[arg(long)]
        wants: PathBuf,
        #[command(flatten)]
        common: Common,
    },
    /// Compile a plan and print it. Nothing is executed.
    Plan {
        /// What you want, in plain language. Needs a model.
        #[arg(long, conflicts_with = "wants")]
        goal: Option<String>,
        /// A file of wants, one per line, instead of asking a model.
        #[arg(long)]
        wants: Option<PathBuf>,
        #[command(flatten)]
        common: Common,
    },
    /// Compile a plan and run it.
    Run {
        #[arg(long, conflicts_with = "wants")]
        goal: Option<String>,
        #[arg(long)]
        wants: Option<PathBuf>,
        /// Run without asking, even though the plan causes effects that leave the system.
        #[arg(long)]
        yes: bool,
        /// Plan and check, but stop before the first effect.
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        common: Common,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).with_writer(std::io::stderr).init();
    match Cli::parse().cmd {
        Cmd::World { common } => world_cmd(&common).await,
        Cmd::Check { wants, common } => check_cmd(&wants, &common).await,
        Cmd::Plan { goal, wants, common } => plan_cmd(goal, wants, &common).await,
        Cmd::Run { goal, wants, yes, dry_run, common } => run_cmd(goal, wants, yes, dry_run, &common).await,
    }
}

// ---------- shared ----------

impl Common {
    fn surface_list(&self) -> Vec<String> {
        self.surfaces.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
    }

    fn opts(&self) -> CompileOptions {
        CompileOptions { plan_id: format!("cli-{}", std::process::id()), surfaces: self.surface_list() }
    }

    /// The world model, from a live app or from a file on disk.
    async fn world(&self) -> anyhow::Result<World> {
        let path = std::path::Path::new(&self.app);
        if path.is_file() {
            let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?).with_context(|| format!("reading {}", self.app))?;
            return Ok(World::from_openapi(&doc)?);
        }
        rwmcp::world_from(&self.app)
            .await
            .with_context(|| format!("reading {}/openapi.json — is the app running, and does it publish x-reverse-webmcp blocks?", self.app.trim_end_matches('/')))
    }

    fn sampler(&self) -> anyhow::Result<ModelClient> {
        ModelClient::from_env(&self.model, &self.effort, false, self.base_url.as_deref(), self.api_key.as_deref())
    }
}

/// Wants from a file: one predicate per line, `#` comments and blank lines ignored.
fn read_wants(path: &std::path::Path) -> anyhow::Result<Vec<String>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(text
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// The intent, either written by an agent into a file or asked of a model once.
async fn intent_for(goal: Option<String>, wants: Option<PathBuf>, c: &Common, world: &World, ledger: &mut Ledger) -> anyhow::Result<Intent> {
    match (goal, wants) {
        (_, Some(path)) => Ok(Intent { goal: format!("wants from {}", path.display()), wants: read_wants(&path)?, ..Default::default() }),
        (Some(goal), None) => {
            let facts = planner::world_facts(&c.app).await.unwrap_or_default();
            let sampler = c.sampler()?;
            let base = if std::path::Path::new(&c.app).is_file() { None } else { Some(c.app.as_str()) };
            planner::plan_with_lint(&goal, &Constraints::default(), world, &facts, &sampler, ledger, &c.opts(), base).await
        }
        (None, None) => anyhow::bail!("give me a --goal to plan from, or a --wants file to run"),
    }
}

fn report_lint(intent: &Intent, world: &World, opts: &CompileOptions) -> anyhow::Result<()> {
    let errs = lint(intent, world, opts);
    if errs.is_empty() {
        return Ok(());
    }
    eprintln!("These wants do not hold up:\n");
    for e in &errs {
        eprintln!("  - {e}");
    }
    anyhow::bail!("{} want{} rejected", errs.len(), if errs.len() == 1 { "" } else { "s" })
}

// ---------- commands ----------

async fn world_cmd(c: &Common) -> anyhow::Result<()> {
    let world = c.world().await?;
    if c.json {
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
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({"entities": world.entities, "operations": ops}))?);
        return Ok(());
    }
    print!("{}", world.summary());
    let missing: Vec<&str> = world.ops.iter().filter(|o| o.post.is_none()).map(|o| o.name.as_str()).collect();
    if !missing.is_empty() {
        println!("\nOperations with no postcondition, so nothing can be planned through them:");
        for m in &missing {
            println!("  {m}");
        }
        println!("\nAdd a `post` to each in the app's x-reverse-webmcp block to make it plannable.");
    }
    Ok(())
}

async fn check_cmd(wants: &std::path::Path, c: &Common) -> anyhow::Result<()> {
    let world = c.world().await?;
    let intent = Intent { goal: "check".into(), wants: read_wants(wants)?, ..Default::default() };
    let base = if std::path::Path::new(&c.app).is_file() { None } else { Some(c.app.as_str()) };
    let intent = match base {
        Some(b) => planner::expand_selectors(&intent, b).await.unwrap_or(intent),
        None => intent,
    };
    report_lint(&intent, &world, &c.opts())?;
    println!("{} want{} check out.", intent.wants.len(), if intent.wants.len() == 1 { "" } else { "s" });
    Ok(())
}

async fn compile_for(goal: Option<String>, wants: Option<PathBuf>, c: &Common) -> anyhow::Result<(Plan, Intent, Ledger)> {
    let world = c.world().await?;
    let mut ledger = Ledger::new();
    let intent = intent_for(goal, wants, c, &world, &mut ledger).await?;
    let base = if std::path::Path::new(&c.app).is_file() { None } else { Some(c.app.as_str()) };
    let intent = match base {
        Some(b) => planner::expand_selectors(&intent, b).await.unwrap_or(intent),
        None => intent,
    };
    report_lint(&intent, &world, &c.opts())?;
    let plan = compile(&intent, &world, &c.opts())?;
    Ok((plan, intent, ledger))
}

async fn plan_cmd(goal: Option<String>, wants: Option<PathBuf>, c: &Common) -> anyhow::Result<()> {
    let (plan, intent, ledger) = compile_for(goal, wants, c).await?;
    if c.json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({"intent": intent, "plan": plan}))?);
        return Ok(());
    }
    println!("Wants:");
    for w in &intent.wants {
        println!("  {w}");
    }
    println!("\n{}", plan.render());
    describe(&plan, &ledger);
    Ok(())
}

/// The part a person actually reads before saying yes.
fn describe(plan: &Plan, ledger: &Ledger) {
    let external: Vec<&str> = plan.nodes.iter().filter(|n| n.external).map(|n| n.op.as_str()).collect();
    let mut external_kinds: Vec<&str> = external.clone();
    external_kinds.sort_unstable();
    external_kinds.dedup();
    let waits = plan.nodes.iter().filter(|n| n.kind == rwmcp::world::OpKind::Event).count();
    let screens = plan.nodes.iter().filter(|n| n.surface == "a11y" || n.surface == "pixels").count();
    println!("{} step{}, {} deep.", plan.nodes.len(), if plan.nodes.len() == 1 { "" } else { "s" }, plan.depth());
    if !external.is_empty() {
        println!("{} step{} leave the system (email, money): {}", external.len(), if external.len() == 1 { "" } else { "s" }, external_kinds.join(", "));
    }
    if waits > 0 {
        println!("{waits} step{} wait for something outside to happen.", if waits == 1 { "" } else { "s" });
    }
    if screens > 0 {
        println!("{screens} step{} need a screen, so they run one at a time.", if screens == 1 { "" } else { "s" });
    }
    if ledger.sample_count() > 0 {
        println!("Planning cost {} model call{}.", ledger.sample_count(), if ledger.sample_count() == 1 { "" } else { "s" });
    }
}

async fn run_cmd(goal: Option<String>, wants: Option<PathBuf>, yes: bool, dry_run: bool, c: &Common) -> anyhow::Result<()> {
    if std::path::Path::new(&c.app).is_file() {
        anyhow::bail!("--app is a file, so there is nothing to run against; give me the app's URL");
    }
    let (plan, _intent, mut ledger) = compile_for(goal, wants, c).await?;
    println!("{}", plan.render());
    describe(&plan, &ledger);

    if dry_run {
        println!("\nDry run: stopping before the first effect.");
        return Ok(());
    }
    let external = plan.nodes.iter().filter(|n| n.external).count();
    if external > 0 && !yes {
        anyhow::bail!("{external} step(s) leave the system. Re-run with --yes once the plan above looks right.");
    }

    let world = Arc::new(c.world().await?);
    let bus = EventBus::connect(&c.app).await?;
    let sched = Scheduler {
        effectors: default_effectors(&c.app, world.clone(), &c.surface_list()),
        bus: Some(bus),
        pools: Default::default(),
        policy: Default::default(),
        recorder: Recorder::new(world.clone()),
    };
    println!("\nRunning.");
    let outcome = sched.run(&plan, &mut ledger).await;
    let receipt = ledger.receipt(&plan, outcome.status, outcome.yield_reason, outcome.evidence, outcome.error);

    if c.json {
        println!("{}", serde_json::to_string_pretty(&receipt)?);
    } else {
        println!();
        for e in &receipt.effects {
            println!("  {} {} {}", if e.ok { "ok  " } else { "FAIL" }, e.op, e.observed.get("id").map(|i| format!("#{i}")).unwrap_or_default());
        }
        println!(
            "\n{:?} · {} model call{} · {} ms planning · {} ms running · {} at once",
            receipt.status,
            receipt.samples,
            if receipt.samples == 1 { "" } else { "s" },
            receipt.plan_ms,
            receipt.busy_ms,
            receipt.max_parallel
        );
        if let Some(q) = &receipt.yield_reason {
            println!("\nIt stopped to ask: {q}");
            println!("Answer by naming the entity you meant, then run again.");
        }
        if let Some(e) = &receipt.error {
            println!("\nIt stopped: {e}");
        }
    }
    match receipt.status {
        Status::Committed => Ok(()),
        _ => std::process::exit(1),
    }
}
