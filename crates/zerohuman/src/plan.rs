//! The physical plan: what the compiler emits and the scheduler runs. Static, inspectable, diffable.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::world::{Check, Fork, OpKind, UiSpec};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", content = "v", rename_all = "lowercase")]
pub enum Arg {
    Lit(Value),
    /// The field of another node's output.
    Ref {
        node: String,
        field: String,
    },
    List(Vec<Arg>),
}

impl Arg {
    pub fn refs(&self) -> Vec<String> {
        match self {
            Arg::Lit(_) => vec![],
            Arg::Ref { node, .. } => vec![node.clone()],
            Arg::List(xs) => xs.iter().flat_map(|x| x.refs()).collect(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub op: String,
    pub kind: OpKind,
    pub args: BTreeMap<String, Arg>,
    pub surface: String,
    pub key: Option<String>,
    pub reads: Vec<String>,
    pub writes: Vec<String>,
    pub external: bool,
    pub produces: Option<String>,
    pub fork: Option<Fork>,
    pub ui: Option<UiSpec>,
    /// For a wait: how to confirm the fact by reading, if the event never arrives.
    #[serde(default)]
    pub check: Option<Check>,
    /// Operations this node must precede when both write the same entity.
    #[serde(default)]
    pub before: Vec<String>,
    /// What this node makes true, for the receipt and for humans.
    pub post: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum GateKind {
    External,
    Spend,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Gate {
    pub node: String,
    pub kind: GateKind,
    pub allowed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub plan_id: String,
    pub goal: String,
    pub nodes: Vec<Node>,
    pub edges: Vec<(String, String)>,
    pub gates: Vec<Gate>,
}

impl Plan {
    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    pub fn preds_of(&self, id: &str) -> Vec<&str> {
        self.edges.iter().filter(|(_, b)| b == id).map(|(a, _)| a.as_str()).collect()
    }

    /// Longest path in nodes: how deep the plan is. Width is nodes minus that, roughly.
    pub fn depth(&self) -> usize {
        let mut memo: BTreeMap<&str, usize> = BTreeMap::new();
        fn d<'a>(p: &'a Plan, id: &'a str, memo: &mut BTreeMap<&'a str, usize>) -> usize {
            if let Some(v) = memo.get(id) {
                return *v;
            }
            let v = 1 + p.preds_of(id).into_iter().map(|q| d(p, q, memo)).max().unwrap_or(0);
            memo.insert(id, v);
            v
        }
        self.nodes.iter().map(|n| d(self, &n.id, &mut memo)).max().unwrap_or(0)
    }

    /// A one-screen rendering for logs and the report.
    pub fn render(&self) -> String {
        let mut out = format!("plan {}  ({} nodes, depth {})\n", self.plan_id, self.nodes.len(), self.depth());
        for n in &self.nodes {
            let preds = self.preds_of(&n.id);
            let after = if preds.is_empty() { String::new() } else { format!("  after {}", preds.join(",")) };
            let key = n.key.as_deref().map(|k| format!("  key {k}")).unwrap_or_default();
            out.push_str(&format!("  {:<4} {:<16} {:<8} {}{}{}\n", n.id, n.op, n.surface, n.post, after, key));
        }
        out
    }
}
