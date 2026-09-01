//! Every knob of a benchmark run, parsed from the CLI and written to `config.json` in the run
//! directory so a result directory always says how it was produced.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(clap::Args, Clone, Debug, Serialize, Deserialize)]
pub struct RunOpts {
    /// Target app base URL. Ignored with --spawn.
    #[arg(long, default_value = "http://127.0.0.1:47310")]
    pub app: String,
    /// Start the app binary next to this one on a free port, and stop it at the end.
    #[arg(long)]
    #[serde(default)]
    pub spawn: bool,
    #[arg(long, default_value = "tasks")]
    pub tasks_dir: PathBuf,
    /// Comma-separated task ids. Default: every task at or below --phase.
    #[arg(long)]
    pub tasks: Option<String>,
    /// Comma-separated arms: A, B, B2, C, D, E.
    #[arg(long, default_value = "D,E")]
    pub arms: String,
    #[arg(long, default_value_t = 5)]
    pub runs: u32,
    /// Only tasks whose phase is at or below this.
    #[arg(long, default_value_t = 3)]
    pub phase: u32,
    /// Surfaces arm D may compile to.
    #[arg(long, default_value = "api")]
    pub surfaces: String,
    /// Added to every write by the app, so the target behaves like a real network service.
    /// Merged into each task's chaos block unless the task sets its own latency.
    #[arg(long, default_value_t = 25)]
    pub latency_ms: u64,
    /// Where arm D's intent comes from: `handwritten` (the task file) or `model` (one planner sample).
    #[arg(long, default_value = "handwritten")]
    pub planner: String,
    /// Model for the planner and the model-driven arms.
    #[arg(long, default_value = "claude-opus-5")]
    pub model: String,
    /// Effort for the model-driven arms' calls: low | medium | high | xhigh | max.
    #[arg(long, default_value = "medium")]
    pub effort: String,
    /// Effort for the planner's single sample. Low is the default: the intent is short.
    #[arg(long, default_value = "low")]
    pub planner_effort: String,
    /// Disable the server-side refusal fallback.
    #[arg(long)]
    #[serde(default)]
    pub no_fallbacks: bool,
    /// Messages-API base URL. Default ANTHROPIC_BASE_URL or https://api.anthropic.com.
    /// A local gateway such as opencodex (http://localhost:8080) works as-is.
    #[arg(long)]
    pub base_url: Option<String>,
    /// API key. Default ANTHROPIC_API_KEY; a local gateway needs none. Never written to disk.
    #[arg(long)]
    #[serde(skip)]
    pub api_key: Option<String>,
    /// Directory for the planner's intent cache. A repeat goal against the same facts costs zero samples.
    #[arg(long)]
    pub plan_cache: Option<PathBuf>,
    #[arg(long)]
    pub out: Option<PathBuf>,
}

impl RunOpts {
    pub fn arm_list(&self) -> Vec<String> {
        self.arms.split(',').map(|s| s.trim().to_uppercase()).filter(|s| !s.is_empty()).collect()
    }

    pub fn surface_list(&self) -> Vec<String> {
        self.surfaces.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
    }

    pub fn task_filter(&self) -> Option<Vec<String>> {
        self.tasks.as_ref().map(|t| t.split(',').map(|s| s.trim().to_string()).collect())
    }

    pub fn needs_model(&self) -> bool {
        self.planner == "model" || self.arm_list().iter().any(|a| matches!(a.as_str(), "A" | "B" | "B2" | "C"))
    }

    /// The directory this run writes to, minted from the clock when not given.
    pub fn out_dir(&self) -> PathBuf {
        self.out.clone().unwrap_or_else(|| PathBuf::from("results").join(chrono::Local::now().format("%Y-%m-%dT%H-%M-%S").to_string()))
    }
}
