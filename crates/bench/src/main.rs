//! `bench run` drives arms × tasks × runs against the target app and writes one JSON per run.
//! `bench report` turns a run directory into summary.json and report.html.
//! `bench verify` recomputes every number in a run directory from the raw ledgers.

use bench::config::RunOpts;
use bench::{arms, loops, oracle, planner, report, tasks};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use zerohuman::events::EventBus;

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

#[derive(Subcommand)]
enum Cmd {
    /// Run arms × tasks × runs and write results.
    Run(Box<RunOpts>),
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
        Cmd::Run(opts) => run(*opts).await,
        Cmd::Report { run, tasks_dir } => {
            let titles = titles(&tasks_dir)?;
            let depths = depths(&tasks_dir)?;
            let results = report::load_results(&run)?;
            let cells = report::write_report(&run, &results, &titles, &depths)?;
            print!("{}", report::text_table(&cells));
            println!();
            print!("{}", report::depth_table(&cells, &depths));
            println!("wrote {}", run.join("report.html").display());
            Ok(())
        }
        Cmd::Verify { run, tasks_dir } => {
            let problems = bench::verify::verify_dir(&run, &tasks_dir)?;
            if problems > 0 {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

fn titles(tasks_dir: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    Ok(Task::load_dir(tasks_dir)?.into_iter().map(|t| (t.id, t.title)).collect())
}

fn depths(tasks_dir: &Path) -> anyhow::Result<BTreeMap<String, u32>> {
    Ok(Task::load_dir(tasks_dir)?.into_iter().map(|t| (t.id, t.depth)).collect())
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

async fn run(opts: RunOpts) -> anyhow::Result<()> {
    let out = opts.out_dir();
    std::fs::create_dir_all(&out)?;
    std::fs::write(out.join("config.json"), serde_json::to_string_pretty(&opts)?)?;
    let arms = opts.arm_list();
    let surfaces = opts.surface_list();
    let wanted = opts.task_filter();
    let RunOpts { app, spawn, tasks_dir, runs, phase, latency_ms, planner, model, effort, no_fallbacks, base_url, api_key, .. } = opts.clone();
    let fallbacks = !no_fallbacks;
    let (_child, base) = if spawn {
        let (c, b) = spawn_app(&out).await?;
        (Some(c), b)
    } else {
        (None, app)
    };
    let oracle = Oracle::new(&base);
    oracle.wait_healthy(Duration::from_secs(10)).await?;

    let all = Task::load_dir(&tasks_dir)?;
    let tasks: Vec<Task> = all.into_iter().filter(|t| wanted.as_ref().map_or(t.phase <= phase, |w| w.contains(&t.id))).collect();

    let world = Arc::new(zerohuman::world_from(&base).await?);
    let needs_model = opts.needs_model();
    let wants_screen = surfaces.iter().any(|s| s == "a11y" || s == "pixels");
    let wants_pages = arms.iter().any(|a| a == "C" || a == "A");
    let browser = if wants_screen || wants_pages {
        let chrome = driver::find_chrome();
        let pages = if wants_pages { zerohuman::Pools::default().per_surface.get("webmcp").copied().unwrap_or(4) } else { 1 };
        Some(driver::BrowserPool::launch(pages, true, chrome.as_deref()).await?)
    } else {
        None
    };
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
                let ctx = ArmContext {
                    base: base.clone(),
                    world: world.clone(),
                    bus: bus.clone(),
                    surfaces: surfaces.clone(),
                    run_id: format!("r{run}"),
                    browser: browser.clone(),
                    shots_dir: Some(out.join("shots")),
                    max_turns: opts.max_turns,
                };

                let mut used_intent = serde_json::Value::Null;
                let mut cache_hit = false;
                let receipt = match arm.as_str() {
                    "D" => {
                        let req = if planner == "model" {
                            let facts = planner::world_facts(&base).await?;
                            Some((model_client.as_ref().unwrap().with_effort(&opts.planner_effort), facts))
                        } else {
                            None
                        };
                        let cache = opts.plan_cache.as_ref().map(planner::IntentCache::new);
                        let outcome = arms::run_ours_planned(
                            task,
                            &ctx,
                            req.as_ref().map(|(m, facts)| arms::PlanRequest { sampler: m, facts: facts.clone(), cache: cache.as_ref() }),
                        )
                        .await?;
                        used_intent = serde_json::to_value(&outcome.intent)?;
                        cache_hit = outcome.cache_hit;
                        outcome.receipt
                    }
                    "E" => arms::run_script(task, &ctx).await?,
                    "B" | "B2" => {
                        let facts = planner::world_facts(&base).await?;
                        loops::run_mcp_loop(task, &ctx, model_client.as_ref().unwrap(), &facts, arm == "B2").await?
                    }
                    "C" => {
                        let facts = planner::world_facts(&base).await?;
                        loops::run_webmcp_loop(task, &ctx, model_client.as_ref().unwrap(), &facts).await?
                    }
                    "A" => {
                        let facts = planner::world_facts(&base).await?;
                        loops::run_cua_loop(task, &ctx, model_client.as_ref().unwrap(), &facts).await?
                    }
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
                let receipt_json = serde_json::to_value(&receipt)?;
                let expect = task.expect.applicable(tasks::resumed_after_fork(&receipt_json));
                let checks = tasks::check(expect, status, receipt.forks_taken, &snapshot, double_sends);
                let correct = checks.iter().all(|c| c.ok);
                let result = RunResult {
                    task: task.id.clone(),
                    task_title: task.title.clone(),
                    arm: arm.clone(),
                    run,
                    status: status.into(),
                    planner: if arm == "D" && cache_hit {
                        "cached-intent".into()
                    } else if arm == "D" {
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
                    plan_ms: receipt.plan_ms,
                    run_ms: receipt.run_ms,
                    busy_ms: Some(receipt.busy_ms),
                    max_parallel: receipt.max_parallel,
                    max_parallel_by_surface: receipt.max_parallel_by_surface.clone(),
                    nodes: receipt.nodes,
                    depth: receipt.depth,
                    correct,
                    checks: checks.clone(),
                    double_sends,
                    forks: receipt.forks_taken,
                    yield_reason: receipt.yield_reason.clone(),
                    error: receipt.error.clone(),
                    snapshot,
                    receipt: receipt_json,
                    intent: used_intent,
                    model: if arm == "E" || (arm == "D" && planner != "model") { String::new() } else { model.clone() },
                    effort: if arm == "E" || (arm == "D" && planner != "model") { String::new() } else { effort.clone() },
                    base_url: model_client.as_ref().map(|m| m.base_url.clone()).unwrap_or_default(),
                    latency_ms,
                    surfaces: surfaces.join(","),
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

    if let Some(b) = &browser {
        let _ = b.close().await;
    }
    let titles = titles(&tasks_dir)?;
    let depths = depths(&tasks_dir)?;
    let results = report::load_results(&out)?;
    let cells = report::write_report(&out, &results, &titles, &depths)?;
    println!();
    print!("{}", report::text_table(&cells));
    println!();
    print!("{}", report::depth_table(&cells, &depths));
    println!("wrote {}", out.join("report.html").display());
    Ok(())
}
