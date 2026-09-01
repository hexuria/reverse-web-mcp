//! `bench run` drives arms × tasks × runs against the target app and writes one JSON per run.
//! `bench report` turns a run directory into summary.json and report.html.
//! `bench verify` recomputes every number in a run directory from the raw ledgers.

use bench::{arms, loops, oracle, report, tasks};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use zerohuman::events::EventBus;
use zerohuman::ledger::max_overlap;

use arms::ArmContext;
use oracle::Oracle;
use report::RunResult;
use tasks::Task;

#[derive(Parser)]
#[command(name = "bench", about = "chiffon benchmark runner")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

// Run carries every knob until S5 folds them into one config struct.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Cmd {
    /// Run arms × tasks × runs and write results.
    Run {
        /// Target app base URL. Ignored with --spawn.
        #[arg(long, default_value = "http://127.0.0.1:47310")]
        app: String,
        /// Start the app binary next to this one on a free port, and stop it at the end.
        #[arg(long)]
        spawn: bool,
        #[arg(long, default_value = "tasks")]
        tasks_dir: PathBuf,
        /// Comma-separated task ids. Default: every task at or below --phase.
        #[arg(long)]
        tasks: Option<String>,
        /// Comma-separated arms: A, B, B2, C, D, E.
        #[arg(long, default_value = "D,E")]
        arms: String,
        #[arg(long, default_value_t = 5)]
        runs: u32,
        /// Only tasks whose phase is at or below this.
        #[arg(long, default_value_t = 3)]
        phase: u32,
        /// Surfaces arm D may compile to.
        #[arg(long, default_value = "api")]
        surfaces: String,
        /// Added to every write by the app, so the target behaves like a real network service.
        /// Merged into each task's chaos block unless the task sets its own latency.
        #[arg(long, default_value_t = 25)]
        latency_ms: u64,
        /// Where arm D's intent comes from: `handwritten` (the task file) or `model` (one planner sample).
        #[arg(long, default_value = "handwritten")]
        planner: String,
        /// Model for the planner and the model-driven arms.
        #[arg(long, default_value = "claude-opus-5")]
        model: String,
        /// Effort for every model call: low | medium | high | xhigh | max.
        #[arg(long, default_value = "medium")]
        effort: String,
        /// Disable the server-side refusal fallback.
        #[arg(long)]
        no_fallbacks: bool,
        /// Messages-API base URL. Default ANTHROPIC_BASE_URL or https://api.anthropic.com.
        /// A local gateway such as opencodex (http://localhost:8080) works as-is.
        #[arg(long)]
        base_url: Option<String>,
        /// API key. Default ANTHROPIC_API_KEY; a local gateway needs none.
        #[arg(long)]
        api_key: Option<String>,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Summarize a run directory.
    Report {
        #[arg(long)]
        run: PathBuf,
        #[arg(long, default_value = "tasks")]
        tasks_dir: PathBuf,
    },
    /// Recompute every number in a run directory from the raw ledgers and snapshots.
    Verify {
        #[arg(long)]
        run: PathBuf,
        #[arg(long, default_value = "tasks")]
        tasks_dir: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    match Cli::parse().cmd {
        Cmd::Run { app, spawn, tasks_dir, tasks, arms, runs, phase, surfaces, latency_ms, planner, model, effort, no_fallbacks, base_url, api_key, out } => {
            let opts = RunOpts {
                app,
                spawn,
                tasks_dir,
                tasks,
                arms,
                runs,
                phase,
                surfaces,
                latency_ms,
                planner,
                model,
                effort,
                fallbacks: !no_fallbacks,
                base_url,
                api_key,
                out,
            };
            run(opts).await
        }
        Cmd::Report { run, tasks_dir } => {
            let titles = titles(&tasks_dir)?;
            let results = report::load_results(&run)?;
            let cells = report::write_report(&run, &results, &titles)?;
            print!("{}", report::text_table(&cells));
            println!("wrote {}", run.join("report.html").display());
            Ok(())
        }
        Cmd::Verify { run, tasks_dir } => verify(&run, &tasks_dir),
    }
}

fn titles(tasks_dir: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    Ok(Task::load_dir(tasks_dir)?.into_iter().map(|t| (t.id, t.title)).collect())
}

async fn spawn_app(out: &Path) -> anyhow::Result<(tokio::process::Child, String)> {
    let exe = std::env::current_exe()?;
    let app = exe.parent().map(|p| p.join("app")).filter(|p| p.exists()).ok_or_else(|| anyhow::anyhow!("no app binary next to {}", exe.display()))?;
    std::fs::create_dir_all(out)?;
    let log = std::fs::File::create(out.join("app.log"))?;
    // Two benches starting at once can race for the same ephemeral port; try a few.
    let mut last_err = None;
    for _ in 0..5 {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0")?;
            l.local_addr()?.port()
        };
        let bind = format!("127.0.0.1:{port}");
        let mut child = tokio::process::Command::new(&app)
            .arg("--bind")
            .arg(&bind)
            .kill_on_drop(true)
            .stdout(std::process::Stdio::null())
            .stderr(log.try_clone()?)
            .spawn()?;
        let base = format!("http://{bind}");
        if Oracle::new(&base).wait_healthy(Duration::from_secs(10)).await.is_ok() {
            return Ok((child, base));
        }
        let _ = child.kill().await;
        last_err = Some(anyhow::anyhow!("app on {bind} did not become healthy; see {}", out.join("app.log").display()));
    }
    Err(last_err.unwrap())
}

struct RunOpts {
    app: String,
    spawn: bool,
    tasks_dir: PathBuf,
    tasks: Option<String>,
    arms: String,
    runs: u32,
    phase: u32,
    surfaces: String,
    latency_ms: u64,
    planner: String,
    model: String,
    effort: String,
    fallbacks: bool,
    base_url: Option<String>,
    api_key: Option<String>,
    out: Option<PathBuf>,
}

async fn run(o: RunOpts) -> anyhow::Result<()> {
    let RunOpts { app, spawn, tasks_dir, tasks, arms, runs, phase, surfaces, latency_ms, planner, model, effort, fallbacks, base_url, api_key, out } = o;
    let out = out.unwrap_or_else(|| PathBuf::from("results").join(chrono::Local::now().format("%Y-%m-%dT%H-%M-%S").to_string()));
    std::fs::create_dir_all(&out)?;
    let (_child, base) = if spawn {
        let (c, b) = spawn_app(&out).await?;
        (Some(c), b)
    } else {
        (None, app)
    };
    let oracle = Oracle::new(&base);
    oracle.wait_healthy(Duration::from_secs(10)).await?;

    let wanted: Option<Vec<String>> = tasks.map(|t| t.split(',').map(|s| s.trim().to_string()).collect());
    let all = Task::load_dir(&tasks_dir)?;
    let tasks: Vec<Task> = all.into_iter().filter(|t| wanted.as_ref().map_or(t.phase <= phase, |w| w.contains(&t.id))).collect();
    let arms: Vec<String> = arms.split(',').map(|s| s.trim().to_uppercase()).collect();
    let surfaces: Vec<String> = surfaces.split(',').map(|s| s.trim().to_string()).collect();

    let world = Arc::new(zerohuman::world_from(&base).await?);
    let needs_model = planner == "model" || arms.iter().any(|a| matches!(a.as_str(), "A" | "B" | "B2" | "C"));
    let model_client =
        if needs_model { Some(loops::ModelClient::from_env(&model, &effort, fallbacks, base_url.as_deref(), api_key.as_deref())?) } else { None };
    println!(
        "app {base} · {} tasks · arms {} · {runs} runs each · latency {latency_ms} ms · planner {planner}{} · out {}",
        tasks.len(),
        arms.join(","),
        if needs_model { format!(" · model {model} @ {effort} via {}", model_client.as_ref().unwrap().base_url) } else { String::new() },
        out.display()
    );

    for task in &tasks {
        for arm in &arms {
            for run in 1..=runs {
                oracle.reset(task.seed).await?;
                let mut chaos = if task.chaos.is_object() { task.chaos.clone() } else { serde_json::json!({}) };
                if chaos.get("latency_ms").is_none() {
                    chaos["latency_ms"] = serde_json::json!(latency_ms);
                }
                oracle.chaos(&chaos).await?;
                let bus = EventBus::connect(&base).await?;
                let hook = task.hooks.pay_after_create_ms.map(|ms| oracle.pay_on_create(bus.clone(), ms));
                let ctx = ArmContext { base: base.clone(), world: world.clone(), bus: bus.clone(), surfaces: surfaces.clone(), run_id: format!("r{run}") };

                let mut used_intent = serde_json::Value::Null;
                let receipt = match arm.as_str() {
                    "D" => {
                        let mut ledger = zerohuman::Ledger::new();
                        let intent = if planner == "model" {
                            // The planner gets a read of the world, not a sample: what customers exist.
                            let customers: serde_json::Value = reqwest::get(format!("{base}/api/customers")).await?.json().await?;
                            let names: Vec<String> = customers
                                .as_array()
                                .map(|a| a.iter().filter_map(|c| c.get("name").and_then(|n| n.as_str()).map(|s| s.to_string())).collect())
                                .unwrap_or_default();
                            let facts = format!("  customers ({}): {}", names.len(), names.join(", "));
                            match loops::plan_intent(task, &world, &facts, model_client.as_ref().unwrap(), &mut ledger).await {
                                Ok(i) => Some(i),
                                Err(e) => {
                                    eprintln!("planner failed: {e}");
                                    None
                                }
                            }
                        } else {
                            None
                        };
                        used_intent = serde_json::to_value(intent.clone().unwrap_or_else(|| task.intent()))?;
                        arms::run_ours(task, &ctx, intent, ledger).await?
                    }
                    "E" => arms::run_script(task, &ctx).await?,
                    "B" => loops::run_mcp_loop(task, &ctx, model_client.as_ref().unwrap(), false).await?,
                    "B2" => loops::run_mcp_loop(task, &ctx, model_client.as_ref().unwrap(), true).await?,
                    other => {
                        let plan =
                            zerohuman::Plan { plan_id: format!("{}-{other}", task.id), goal: task.goal.clone(), nodes: vec![], edges: vec![], gates: vec![] };
                        let ledger = zerohuman::Ledger::new();
                        ledger.receipt(
                            &plan,
                            zerohuman::Status::Error,
                            None,
                            None,
                            Some(format!("arm {other} is not wired in this build (needs a model client)")),
                        )
                    }
                };
                if let Some(h) = hook {
                    h.abort();
                }

                let snapshot = oracle.snapshot().await?;
                let effects = oracle.effects().await?;
                let double_sends = effects.get("double_sends").and_then(|d| d.as_u64()).unwrap_or(0) as usize;
                let status = match receipt.status {
                    zerohuman::Status::Committed => "committed",
                    zerohuman::Status::NeedThink => "need_think",
                    zerohuman::Status::Error => "error",
                };
                let checks = tasks::check(&task.expect, status, receipt.forks_taken, &snapshot, double_sends);
                let correct = checks.iter().all(|c| c.ok);
                let result = RunResult {
                    task: task.id.clone(),
                    task_title: task.title.clone(),
                    arm: arm.clone(),
                    run,
                    status: status.into(),
                    planner: if arm == "D" {
                        format!("{planner}-intent")
                    } else if arm == "E" {
                        "none".into()
                    } else {
                        format!("{model}@{effort}")
                    },
                    samples: receipt.samples,
                    tokens_in: receipt.tokens_in,
                    tokens_out: receipt.tokens_out,
                    wall_ms: receipt.wall_ms,
                    max_parallel: receipt.max_parallel,
                    nodes: receipt.nodes,
                    depth: receipt.depth,
                    correct,
                    checks: checks.clone(),
                    double_sends,
                    forks: receipt.forks_taken,
                    yield_reason: receipt.yield_reason.clone(),
                    error: receipt.error.clone(),
                    snapshot,
                    receipt: serde_json::to_value(&receipt)?,
                    intent: used_intent,
                };
                let file = out.join(format!("{}-{}-{}.json", task.id, arm, run));
                std::fs::write(&file, serde_json::to_string_pretty(&result)?)?;
                let failed: Vec<String> = checks.iter().filter(|c| !c.ok).map(|c| format!("{}: want {} got {}", c.name, c.expected, c.actual)).collect();
                println!(
                    "{:<3} {:<2} run {} · {:<10} · {:>6} ms · max_par {} · {}{}",
                    task.id,
                    arm,
                    run,
                    status,
                    receipt.wall_ms,
                    receipt.max_parallel,
                    if correct { "correct".to_string() } else { format!("WRONG [{}]", failed.join("; ")) },
                    receipt.error.as_ref().map(|e| format!(" · {e}")).unwrap_or_default()
                );
            }
        }
    }

    let titles = titles(&tasks_dir)?;
    let results = report::load_results(&out)?;
    let cells = report::write_report(&out, &results, &titles)?;
    println!();
    print!("{}", report::text_table(&cells));
    println!("wrote {}", out.join("report.html").display());
    Ok(())
}

fn verify(run: &Path, tasks_dir: &Path) -> anyhow::Result<()> {
    let tasks: BTreeMap<String, Task> = Task::load_dir(tasks_dir)?.into_iter().map(|t| (t.id.clone(), t)).collect();
    let results = report::load_results(run)?;
    let mut problems = 0;
    for r in &results {
        let rows = r.receipt.pointer("/ledger/rows").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let spans = rows
            .iter()
            .filter(|x| x.get("surface").and_then(|s| s.as_str()) != Some("event"))
            .map(|x| (x.get("started_us").and_then(|v| v.as_u64()).unwrap_or(0) as u128, x.get("ended_us").and_then(|v| v.as_u64()).unwrap_or(0) as u128));
        let recomputed = max_overlap(spans);
        if recomputed != r.max_parallel {
            problems += 1;
            println!("{} {} run {}: max_parallel stored {} recomputed {}", r.task, r.arm, r.run, r.max_parallel, recomputed);
        }
        if let Some(t) = tasks.get(&r.task) {
            let checks = tasks::check(&t.expect, &r.status, r.forks, &r.snapshot, r.double_sends);
            let correct = checks.iter().all(|c| c.ok);
            if correct != r.correct {
                problems += 1;
                println!("{} {} run {}: correctness stored {} recomputed {}", r.task, r.arm, r.run, r.correct, correct);
            }
        }
        // The outbox in the snapshot is the second witness for double-sends.
        let outbox = r.snapshot.get("outbox").and_then(|o| o.as_array()).cloned().unwrap_or_default();
        let mut seen: BTreeMap<(u64, String), usize> = BTreeMap::new();
        for m in &outbox {
            let k = (m.get("invoice_id").and_then(|i| i.as_u64()).unwrap_or(0), m.get("kind").and_then(|k| k.as_str()).unwrap_or("").to_string());
            *seen.entry(k).or_default() += 1;
        }
        let dbl: usize = seen.values().filter(|n| **n > 1).map(|n| n - 1).sum();
        if dbl != r.double_sends {
            problems += 1;
            println!("{} {} run {}: double_sends stored {} recomputed {}", r.task, r.arm, r.run, r.double_sends, dbl);
        }
    }
    println!("{} results verified, {} problems", results.len(), problems);
    if problems > 0 {
        std::process::exit(1);
    }
    Ok(())
}
