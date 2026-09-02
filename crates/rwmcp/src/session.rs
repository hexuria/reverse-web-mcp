//! One object for the whole job.
//!
//! Embedding the engine used to mean assembling `Scheduler { effectors, bus, pools, policy,
//! recorder, progress }` alongside `EventBus::connect`, `default_effectors`, `Recorder::new`,
//! `Ledger::new` and `ledger.receipt(...)` — six concepts to answer one question. A `Session`
//! holds the app: its world model, its doors, its event stream, and the limits a run may use.
//!
//! ```no_run
//! # async fn f() -> anyhow::Result<()> {
//! use rwmcp::{Intent, Session};
//! let app = Session::connect("http://localhost:47310").await?;
//! let intent = Intent { goal: "invoice Acme".into(), wants: vec!["invoice(customer=customer(name='Acme')).exists".into()], ..Default::default() };
//! let plan = app.plan(&intent)?;
//! let receipt = app.run(&plan).await?;
//! # Ok(()) }
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use thiserror::Error;

use crate::effectors::Effector;
use crate::events::EventBus;
use crate::intent::{lint, LintError};
use crate::ledger::{Ledger, Receipt, Recorder};
use crate::scheduler::Progress;
use crate::{compile, compiler::CompileError, default_effectors, CompileOptions, Intent, Plan, Policy, Pools, Scheduler, World};

/// Why a goal did not become a plan. Both halves are worth telling apart: wants that do not hold
/// up are the author's to fix, and an intent that cannot be compiled is the world model's.
#[derive(Debug, Error)]
pub enum PlanError {
    #[error("{} want(s) do not hold up", .0.len())]
    Wants(Vec<LintError>),
    #[error(transparent)]
    Compile(#[from] CompileError),
}

/// An app, ready to be asked for things.
pub struct Session {
    pub world: Arc<World>,
    /// Which surfaces a plan may use, and the id its idempotency keys are scoped to.
    pub opts: CompileOptions,
    pub pools: Pools,
    pub policy: Policy,
    /// Called as each step finishes. `None` runs silently.
    pub progress: Option<Progress>,
    effectors: HashMap<String, Arc<dyn Effector>>,
    bus: Option<Arc<EventBus>>,
    /// Where the app is, when there is one. `None` is a world model with nothing behind it.
    base: Option<String>,
}

impl Session {
    /// Read the app's world model, open its doors and subscribe to its events.
    pub async fn connect(base: &str) -> anyhow::Result<Session> {
        let world = Arc::new(crate::world_from(base).await?);
        let opts = CompileOptions::default();
        let effectors = default_effectors(base, world.clone(), &opts.surfaces);
        let bus = Some(EventBus::connect(base).await?);
        Ok(Session { world, opts, pools: Pools::default(), policy: Policy::default(), progress: None, effectors, bus, base: Some(base.to_string()) })
    }

    /// A world model with no app behind it: enough to lint, compile and inspect, never to run.
    pub fn offline(world: World) -> Session {
        Session {
            world: Arc::new(world),
            opts: CompileOptions::default(),
            pools: Pools::default(),
            policy: Policy::default(),
            progress: None,
            effectors: HashMap::new(),
            bus: None,
            base: None,
        }
    }

    /// Which surfaces the plan may use. Doors are reopened, since which surfaces are allowed
    /// decides which ones exist.
    pub fn surfaces(mut self, surfaces: &[String]) -> Session {
        self.opts.surfaces = surfaces.to_vec();
        if let Some(base) = &self.base {
            self.effectors = default_effectors(base, self.world.clone(), surfaces);
        }
        self
    }

    /// The name idempotency keys are scoped to. Two runs under the same id share their committed
    /// work; two runs under different ids do not.
    pub fn plan_id(mut self, id: impl Into<String>) -> Session {
        self.opts.plan_id = id.into();
        self
    }

    pub fn limits(mut self, pools: Pools, policy: Policy) -> Session {
        self.pools = pools;
        self.policy = policy;
        self
    }

    pub fn watching(mut self, progress: Progress) -> Session {
        self.progress = Some(progress);
        self
    }

    /// Everything wrong with these wants, against this app, under these surfaces.
    pub fn lint(&self, intent: &Intent) -> Vec<LintError> {
        lint(intent, &self.world, &self.opts)
    }

    /// Lint, then compile. A plan that comes back from here is one this app can carry out.
    pub fn plan(&self, intent: &Intent) -> Result<Plan, PlanError> {
        let errs = self.lint(intent);
        if !errs.is_empty() {
            return Err(PlanError::Wants(errs));
        }
        Ok(compile(intent, &self.world, &self.opts)?)
    }

    /// Run it, skipping whatever a previous run already committed under this plan id.
    pub async fn run(&self, plan: &Plan) -> anyhow::Result<Receipt> {
        self.resume(plan, Ledger::new()).await
    }

    /// The same, carrying a ledger from an earlier run — from another process, if it was saved.
    pub async fn resume(&self, plan: &Plan, mut ledger: Ledger) -> anyhow::Result<Receipt> {
        let outcome = self.run_with(plan, &mut ledger).await?;
        Ok(ledger.receipt(plan, outcome.status, outcome.yield_reason, outcome.evidence, outcome.error))
    }

    /// The same again, writing into a ledger the caller keeps: for anyone who wants the rows and
    /// the outcome rather than the summary, or who means to answer a fork and carry on.
    pub async fn run_with(&self, plan: &Plan, ledger: &mut Ledger) -> anyhow::Result<crate::scheduler::Outcome> {
        if self.bus.is_none() {
            anyhow::bail!("this session has no app behind it, so there is nothing to run against");
        }
        let sched = Scheduler {
            effectors: self.effectors.clone(),
            bus: self.bus.clone(),
            pools: self.pools.clone(),
            policy: self.policy.clone(),
            recorder: Recorder::new(self.world.clone()),
            progress: self.progress.clone(),
        };
        let done = ledger.completed(plan);
        Ok(sched.resume(plan, ledger, &done).await)
    }
}
