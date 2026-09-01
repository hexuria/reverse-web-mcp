//! zerohuman core: one model sample in, a parallel plan out, a receipt that proves it.
//!
//! ```text
//! goal ──▶ planner (a model, elsewhere) ──▶ Intent
//! Intent + World ──▶ compiler ──▶ Plan
//! Plan ──▶ scheduler (effectors, event bus, pools) ──▶ Ledger ──▶ Receipt
//! ```

pub mod compiler;
pub mod effectors;
pub mod events;
pub mod intent;
pub mod ledger;
pub mod plan;
pub mod pred;
pub mod scheduler;
pub mod world;

pub use compiler::{compile, CompileOptions};
pub use intent::Intent;
pub use ledger::{Ledger, Receipt, Status};
pub use plan::Plan;
pub use scheduler::{Policy, Pools, Scheduler};
pub use world::World;

use std::collections::HashMap;
use std::sync::Arc;

/// Fetch the target app's OpenAPI document and derive the world model.
pub async fn world_from(base: &str) -> Result<World, String> {
    let url = format!("{}/openapi.json", base.trim_end_matches('/'));
    let doc: serde_json::Value = reqwest::get(&url).await.map_err(|e| e.to_string())?.json().await.map_err(|e| e.to_string())?;
    World::from_openapi(&doc)
}

/// Effectors for the target app: the API door and its MCP door. Anything else the plan asks
/// for fails loudly through `Unavailable`.
pub fn default_effectors(base: &str, world: Arc<World>, surfaces: &[String]) -> HashMap<String, Arc<dyn effectors::Effector>> {
    let mut m: HashMap<String, Arc<dyn effectors::Effector>> = HashMap::new();
    for s in surfaces {
        let e: Arc<dyn effectors::Effector> = match s.as_str() {
            "api" => Arc::new(effectors::ApiEffector::new(base, world.clone())),
            "mcp" => Arc::new(effectors::McpEffector::new(&format!("{}/mcp", base.trim_end_matches('/')), "mcp")),
            other => Arc::new(effectors::Unavailable(other.to_string())),
        };
        m.insert(s.clone(), e);
    }
    m
}
