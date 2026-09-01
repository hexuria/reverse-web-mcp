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
