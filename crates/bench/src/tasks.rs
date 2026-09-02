//! Task files: goal, wants, seed, chaos, hooks, and what the oracle must show at the end.

use std::path::Path;

use rwmcp::intent::{Constraints, Intent, IntentFork};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Expect {
    #[serde(default = "committed")]
    pub status: String,
    #[serde(default)]
    pub invoices: Option<usize>,
    #[serde(default)]
    pub sent: Option<usize>,
    #[serde(default)]
    pub paid: Option<usize>,
    #[serde(default)]
    pub receipts: Option<usize>,
    #[serde(default)]
    pub reports: Option<usize>,
    #[serde(default)]
    pub forks: Option<usize>,
    #[serde(default)]
    pub double_sends: Option<usize>,
    /// What to expect instead when the arm answered a fork and resumed.
    #[serde(default)]
    pub after_resume: Option<Box<Expect>>,
}

impl Expect {
    /// The expectation that applies: the resumed one when a fork was answered, else this.
    pub fn applicable(&self, resumed: bool) -> &Expect {
        match (&self.after_resume, resumed) {
            (Some(e), true) => e,
            _ => self,
        }
    }
}

/// True when the fork was answered: by a planner sample, or by a default the intent declared.
pub fn resumed_after_fork(receipt: &Value) -> bool {
    let answered = receipt
        .pointer("/ledger/samples")
        .and_then(|s| s.as_array())
        .is_some_and(|a| a.iter().any(|x| x.get("kind").and_then(|k| k.as_str()) == Some("fork_answer")));
    let auto = receipt.pointer("/ledger/forks").and_then(|f| f.as_array()).is_some_and(|a| a.iter().any(|x| x.get("auto").is_some_and(|v| !v.is_null())));
    answered || auto
}

fn committed() -> String {
    "committed".into()
}

/// What the script ceiling (arm E) does for this task. A hand-written parallel program,
/// declared rather than coded, so adding a task never adds a match arm.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ScriptSpec {
    /// Customers to invoice, each in its own lane.
    #[serde(default)]
    pub customers: Vec<String>,
    #[serde(default)]
    pub send: bool,
    /// Poll the invoice until paid. The ceiling may poll; it is the speed of light, not the claim.
    #[serde(default)]
    pub wait_paid: bool,
    #[serde(default)]
    pub receipt: bool,
    /// One report per invoice, after that lane's last step.
    #[serde(default)]
    pub report_each: bool,
    /// One report over every invoice, after every lane.
    #[serde(default)]
    pub report_all: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Hooks {
    #[serde(default)]
    pub pay_after_create_ms: Option<u64>,
    /// The payment lands but its webhook is lost, so only reading the world reveals it.
    #[serde(default)]
    pub pay_silently: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    #[serde(default = "two")]
    pub phase: u32,
    /// Length of the longest dependency chain in the task. Ground truth for the samples-vs-depth table,
    /// because a baseline arm has no plan to measure it from.
    #[serde(default)]
    pub depth: u32,
    pub seed: u64,
    pub goal: String,
    #[serde(default)]
    pub wants: Vec<String>,
    #[serde(default)]
    pub constraints: Constraints,
    #[serde(default)]
    pub forks: Vec<IntentFork>,
    #[serde(default)]
    pub chaos: Value,
    #[serde(default)]
    pub hooks: Hooks,
    #[serde(default)]
    pub script: Option<ScriptSpec>,
    #[serde(default)]
    pub expect: Expect,
}

fn two() -> u32 {
    2
}

impl Task {
    pub fn load(path: &Path) -> anyhow::Result<Task> {
        let text = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }

    pub fn load_dir(dir: &Path) -> anyhow::Result<Vec<Task>> {
        let mut out = Vec::new();
        let mut paths: Vec<_> =
            std::fs::read_dir(dir)?.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "toml")).collect();
        paths.sort();
        for p in paths {
            out.push(Task::load(&p)?);
        }
        Ok(out)
    }

    pub fn intent(&self) -> Intent {
        Intent { goal: self.goal.clone(), wants: self.wants.clone(), constraints: self.constraints.clone(), forks: self.forks.clone() }
    }
}

/// Every check the oracle can make, with what was expected and what was seen.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub expected: Value,
    pub actual: Value,
    pub ok: bool,
}

pub fn check(expect: &Expect, status: &str, forks: usize, snapshot: &Value, double_sends: usize) -> Vec<Check> {
    let invoices = snapshot.get("invoices").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let count = |f: &dyn Fn(&Value) -> bool| invoices.iter().filter(|i| f(i)).count();
    let sent = count(&|i| i.get("status").and_then(|s| s.as_str()).is_some_and(|s| s != "draft"));
    let paid = count(&|i| i.get("status").and_then(|s| s.as_str()) == Some("paid"));
    let receipts = count(&|i| i.get("receipt_sent").and_then(|b| b.as_bool()).unwrap_or(false));
    let reports = snapshot.get("reports").and_then(|v| v.as_array()).map_or(0, |a| a.len());

    let mut out = vec![Check {
        name: "status".into(),
        expected: Value::String(expect.status.clone()),
        actual: Value::String(status.into()),
        ok: expect.status == status,
    }];
    let mut num = |name: &str, exp: Option<usize>, act: usize| {
        if let Some(e) = exp {
            out.push(Check { name: name.into(), expected: Value::from(e), actual: Value::from(act), ok: e == act });
        }
    };
    num("invoices", expect.invoices, invoices.len());
    num("sent", expect.sent, sent);
    num("paid", expect.paid, paid);
    num("receipts", expect.receipts, receipts);
    num("reports", expect.reports, reports);
    num("forks", expect.forks, forks);
    num("double_sends", expect.double_sends, double_sends);
    out
}
