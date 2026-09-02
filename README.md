# rwmcp

**Reverse-WebMCP: the execution layer.** WebMCP declares an app's *actions* and lets a model
pick the order. rwmcp declares what those actions *mean* and lets a compiler pick both.

One model sample in, a parallel plan out, a receipt that proves it. And a benchmark anyone can rerun.

The claim: a **compiler** turns a goal's intent graph into a dependency DAG with a surface, an
idempotency key and a read/write footprint per node; a **scheduler** runs that DAG as wide as
the data allows, waits on events instead of polling, and calls the model only at declared
forks. The claim is falsifiable on one number, **max concurrent effects**, measured from the
ledger and never taken from an arm's own word.

Everything is Rust. Nothing runs on your screen.

## Layout

```
crates/app         the target app we own: invoicing, one handler set, five doors, an oracle
crates/rwmcp   world model deriver · predicate language · compiler · scheduler · ledger · effectors · event bus
crates/bench       the arms, the tasks runner, the report, verify
tasks/             T1..T7 as TOML: goal, wants, seed, chaos, hooks, expected end state
docker/            the sandbox: app + headless Chromium + one virtual display per screen lane
results/           one JSON per (task, arm, run) plus summary.json and report.html
```

## One command

```sh
make bench                      # build, spawn the app, run arms D and E on every phase ≤3 task, 5 runs each
make report RUN=results/<stamp> # rebuild summary.json + report.html from the stored ledgers
make verify RUN=results/<stamp> # recompute max parallel, correctness and double-sends from raw data
make app                        # run the target app on http://127.0.0.1:47310 and use the UI yourself
```

Model-driven arms need a server that speaks the Messages shape. No Anthropic key is required:
a local gateway works as-is. With opencodex on `http://localhost:8080` routing to xAI Grok:

```sh
make bench-ocx ARMS=D,E,B,B2 PLANNER=model RUNS=5     # everything through opencodex + grok-4.6
./target/release/bench run --spawn --arms B2 --base-url http://localhost:8080 --model grok-4.6 --tasks T2
```

Against Anthropic directly (`ANTHROPIC_API_KEY`, or `ant auth login`):

```sh
make bench ARMS=D,E,B,B2 RUNS=5                       # baselines + ours, claude-opus-5
./target/release/bench run --spawn --arms D --planner model --tasks T2,T3   # ours with one real planner sample
```

Every run directory gets a `config.json` with the exact options used (the API key is never
written), and every result file records model, effort, base URL, latency and surfaces. Pin one
model for a whole comparison; never mix providers across arms.

Every knob is on `bench run --help`: `--latency-ms` (default 25, added to every write so the app
behaves like a network service), `--surfaces`, `--model`, `--effort`, `--planner`,
`--planner-effort` (default `low`). Through opencodex with grok-4.6 the effort knob is real but
small: on T3 the planner emitted 143–163 output tokens in 5–6 s at low and 172–175 in 7–8 s at
medium, all correct. Treat effort as a cost lever, not a correctness lever, on that route.

## The target app

Customers, invoices, an outbox, payments. The same handlers behind five doors:

| door | path | who uses it |
|---|---|---|
| REST + OpenAPI | `/api/*`, `/openapi.json` | ours, the script ceiling |
| MCP over HTTP | `/mcp` (initialize, tools/list, tools/call) | the MCP loops, ours |
| WebMCP | the page registers the same ops on `navigator.modelContext`, and always on `window.__webmcp` | the WebMCP loop |
| accessibility | every control on the page has a role and a name | ours via a computer-use driver |
| pixels | the same page | the CUA loop |

The oracle: `POST /oracle/reset?seed=`, `GET /oracle/state`, `GET /oracle/effects` (every write
with its key; double-sends counted here), `POST /oracle/chaos` (latency, send failure rate, rate
limit, require approval, a UI modal), `POST /oracle/pay` (the outside world paying), `GET /events`
(server-sent events for every state change).

**Approve** is deliberately UI-only. There is no API for it and the server refuses the call
without the page's header, so every arm has to prove it can mix one screen node into a plan.

The `x-reverse-webmcp` blocks in `crates/app/static/openapi.json` are the world model: postcondition,
requirements, read/write footprint, and which surfaces expose each operation at what cost.
The compiler derives everything from that file. It is never hand-authored anywhere else.

## What "reverse" means

Reversed **arrow**, not reversed effort. Both approaches need the app's author to declare
something and neither infers anything, so if you came here expecting an engine that reads your
API and guesses: it does not, and nothing could do that safely.

What is reversed is the direction the work is derived in.

- **WebMCP / MCP:** the app declares its *actions*. The model picks which one to call, and in
  what order, one turn at a time. Actions in, outcome hopefully out.
- **Reverse-WebMCP:** the app declares what each action *means* — its postcondition, its
  requirements, its read/write footprint. The model declares the *outcome* once. A compiler
  derives the actions and their order from the footprints. Outcome in, actions out.

That inversion is the whole reason one model call covers depth 3 to 6 and width 1 to 300: there
is no per-level decision left for a model to make.

| | MCP / WebMCP | rwmcp |
|---|---|---|
| the app declares | callable functions: name, prose, arg schema | per operation: `post`, `requires`, `reads`/`writes`, surfaces and their cost |
| the model produces | the next tool call, every turn | a set of end-state facts, once |
| ordering comes from | the model's judgment | the footprints, mechanically |
| model calls | roughly one per dependency level | one, whatever the depth or width |
| doing it twice | whatever key the model remembers to reuse | a content-addressed key stamped by the compiler |
| resuming | replay the transcript | skip every node whose key already has an ok row |
| the artifact | a conversation | a plan you can read before it runs, and a receipt after |
| who writes the declaration | the site author | the site author, or you about someone else's API — drafted by the `annotate-world-model` skill, then verified against the running app |
| works when | the app ships MCP tools | the app has an HTTP API, a web page, or events |

They are not rivals. WebMCP is one of rwmcp's five doors: an operation exposed several ways
carries a cost per surface (`"surfaces": {"api": 1, "webmcp": 2, "mcp": 3, "a11y": 50}`) and the
compiler picks the cheapest one the run is allowed to use. Arm C in the benchmark *is* a WebMCP
loop, run as an honest baseline.

Reach for a tool loop when the job is one or two calls, genuinely open-ended, or when each
result changes what should happen next. Reach for rwmcp when the same shape of job runs
repeatedly, when doing something twice costs money or sends an email, when someone will ask what
happened and "the transcript" is not an answer, or when part of the job is waiting on the
outside world.

## Renamed from chiffon / zerohuman

This project was called **chiffon**, and its engine crate **zerohuman**, until the rebrand to
reverse-WebMCP. Three things changed at once:

| was | now |
|---|---|
| the OpenAPI extension `x-zerohuman` (and `-entities`, `-events`, `-ui`) | `x-reverse-webmcp` (same suffixes) |
| `crates/zerohuman`, package `zerohuman` | `crates/rwmcp`, package `rwmcp` |
| the project name `chiffon` | `rwmcp` |

**Migrating a document.** The extension key is the only breaking change, and it fails loudly:
an old `x-zerohuman` document derives an empty world model, every want becomes unsatisfiable,
and since `7ff8216` a zero-node plan is `CompileError::Empty` rather than a run that commits
having done nothing. Rename the four keys and you are done.

**Reading older results.** Everything under `results/` predating the rebrand was produced under
the old names, and those files are left exactly as they were: their provenance is accurate for
when they ran. `bench verify` still recomputes every number in them.

## The arms

| arm | what | status |
|---|---|---|
| A | CUA click loop: screenshot → model → one action, on a headless page | wired (`loops.rs`); the screen is a CDP viewport, never the host |
| B | MCP loop, one tool call per turn | wired (`loops.rs`) |
| B2 | MCP loop, the model may emit several tool calls per turn, run concurrently | wired; the honest baseline |
| C | WebMCP loop inside a headless browser | wired (`loops.rs`), pages from the driver pool |
| D | ours: intent → compiler → scheduler → receipt | wired; `--planner handwritten` or `model` |
| E | script ceiling: a hand-written parallel program, no model | wired |

## The tasks

| id | task | isolates | phase |
|---|---|---|---|
| T1 | one invoice, sent | sanity | 2 |
| T2 | invoice and send for 10 customers | fan-out with disjoint writes | 2 |
| T3 | 3 invoices, email each, one report over all three | a real DAG with a join | 2 |
| T4 | send the receipt only after the payment arrives | waiting as an edge | 3 |
| T5 | T2 with a 30% send failure rate | retries with the same key, zero double-sends | 3 |
| T6 | invoice Acme when two customers are named Acme | a declared fork, exactly one yield | 3 |
| T7 | T3 plus Approve on each invoice, UI-only | one screen node per invoice in a mixed plan | 4 |
| T8 | 3 invoices: send, paid, receipt, one report | six deep and three wide | 3 |
| T9 | 10 invoices: send, paid, receipt, one report | six deep and ten wide | 3 |
| T10 | T9 plus a report per invoice | six deep, ten wide, eleven joins | 3 |
| T11 | three hundred invoices, all sent, from one `each(...)` want | fan-out without naming every row | 3 |
| T12 | T2 under a limit of eight writes per second | 429s, backoff, keys: twenty writes, none doubled | 3 |
| T13 | T4 with the payment webhook lost | a wait that reads the world when the event never comes | 3 |

A want may contain `each([...])`; the compiler unrolls it into one want per element before
compiling, so three hundred rows are still one sample. Keys are identical to the written-out form.

Each task declares its `depth` (the longest dependency chain) so the report can plot samples
against depth for every arm, and a `[script]` block that arm E interprets as its hand-written
parallel program: customers, send, wait for payment, receipt, a report per invoice, a report over all.

## Results

Full page with charts: `docs/results.html`. Every number is recomputed from raw ledgers by
`bench verify`; every run directory below verifies clean.

**The final pass** (`results/campaign-5-final`, grok-4.6, five runs per task, eleven tasks):
110 of 110 runs correct, one model call per task (two on T8), zero double-sends.

**Same commit, same tasks, ours against the loops** (`campaign-5-loops` for the loops on grok,
`campaign-4-luna` for the second model). Medians.

| task | depth | arm | model | correct | model calls | thinking | executing | max parallel |
|---|---|---|---|---|---|---|---|---|
| T2 ten invoices | 3 | **ours** | grok-4.6 | 5/5 | **1** | 3.5 s | 57 ms | 10 |
| | | B2 parallel MCP | grok-4.6 | 3/3 | 4 | 16 s | 9.3 s | 10 |
| | | C WebMCP | grok-4.6 | 3/3 | 7 | 32 s | 25 s | 10 |
| | | **ours** | gpt-5.6-luna | 3/3 | **1** | 2.4 s | 56 ms | 10 |
| | | B2 | gpt-5.6-luna | 3/3 | 4 | 13 s | 8.2 s | 10 |
| T4 wait for payment | 5 | **ours** | grok-4.6 | 5/5 | **1** | 3.4 s | 664 ms | 1 |
| | | B2 | grok-4.6 | 3/3 | 5 | 11 s | 6.4 s | 1 |
| | | **ours** | gpt-5.6-luna | 3/3 | **1** | 4.0 s | 672 ms | 1 |
| | | B2 | gpt-5.6-luna | 3/3 | 5 | 7.8 s | 5.1 s | 1 |
| T9 ten chains, six deep | 6 | **ours** | grok-4.6 | 5/5 | **1** | 3.9 s | 693 ms | 10 |
| | | B2 | grok-4.6 | 3/3 | 6 | 21 s | 15 s | 10 |
| | | C | grok-4.6 | 3/3 | 9 | 40 s | 34 s | 10 |
| | | **ours** | gpt-5.6-luna | 3/3 | **2** | 11 s | 687 ms | 10 |
| | | B2 | gpt-5.6-luna | 2/3 | 6 | 22 s | 17 s | 11 |
| T11 three hundred from one want | 3 | **ours** | grok-4.6 | 5/5 | **1** | 3.2 s | 289 ms | 64 |
| | | B2 | grok-4.6 | 3/3 | 14 | 202 s | 196 s | 50 |
| | | C | grok-4.6 | 0/3 | 40 (cap) | 201 s | 202 s | 50 |
| | | **ours** | gpt-5.6-luna | 3/3 | **1** | 3.1 s | 300 ms | 64 |
| | | B2 | gpt-5.6-luna | 3/3 | 14 | 223 s | 216 s | 50 |

What it says:

- **Model calls do not grow with the work.** One for ours at depth 3 to 6 and width 1 to 300.
  The parallel MCP loop pays roughly one per dependency level, and fourteen at three hundred rows.
- **The shape survives a model swap.** Ours is one call on both models; the loop is four to
  fourteen on both.
- **Width is not ours to claim.** Parallel tool calls parallelise fine; the loop went 50 wide.
  What it cannot do is stop thinking between levels.
- **Execution matches the hand-written script** to within tens of milliseconds on every task.
  The single planning call is nearly all our wall time, which is why the plan cache matters:
  a repeat goal costs zero calls.

Earlier campaigns are kept for provenance: `campaign-1`, `campaign-2*` (the first four-arm
sweep), `campaign-3*` (after the wait, planner and selector fixes), `campaign-2d-ratelimit`.
The loop cells on wait tasks in `campaign-1` and `campaign-2b-loops` are **void**: an app bug
overwrote a payment with a later send, and only the slower arms ever sent late. It is fixed,
pinned by a test, and those cells were rerun.

### What the benchmark caught us doing wrong

A stale binary that made a smoke test measure old code. A silent fallback to the hand-written
plan when the planner failed, scoring a model failure as a success. The payment bug above. And a
quantifier whose meaning depended on position, so "one report over ten invoices" compiled into
ten reports. Each is fixed, pinned by a test, and disclosed rather than quietly rerun.

## What a result contains

Per run: status, model samples, tokens, wall time, **max parallel** (a sweep over the ledger's
microsecond spans), correctness against the task's expected end state as read from the oracle,
double-sends from the app's outbox, forks taken, the full ledger, the oracle snapshot, and for
arm D the rendered plan. `bench verify` recomputes the first three from the raw rows and fails if
anything stored disagrees.

## The sandbox

```sh
make sandbox     # docker compose: the app + Chromium + the bench, results mounted back
```

Every screen arm drives a headless Chromium over CDP, on the Mac or in the container, so no run
anywhere can touch your screen, mouse or keyboard. Arm A saves the screenshot it acted on each
turn under `results/<run>/shots/`, which is the recording. Point `BASE_URL` at
`http://host.docker.internal:8080` to reach opencodex from inside the container.

## Where this is on the road

- Phase 0, the app and oracle: done, with in-process door tests.
- Phase 1, baselines: B, B2, C and A all run through one `run_tool_loop` or the pixel loop, get the same
  world facts and worked example as the planner, and a bail-out is scored as an error.
- Phase 2, compiler and scheduler: done. Content-addressed keys, a resumable scheduler, one
  Recorder for every arm, samples as rows, and `verify` recomputing every headline number.
- Phase 3, planner: done. Lint with one re-ask, fork answers linted and resumed, an intent cache,
  `each(...)` fan-out, depth tasks T8-T11. Campaign-1 above is the first 5-run evidence.
- Phase 4, screens: done. The driver crate leases headless pages; the accessibility effector makes
  T7 green with one screen lane inside a wide API plan; arm C runs WebMCP in a page; arm A is a
  pixel loop on screenshots, saved per turn.
- Phase 5, harden and publish: the gate (`make gate`) runs fmt, clippy and every test; CI does the
  same on push. Still open: a campaign over T1-T11 with all six arms, and a cold reproduction on a
  second machine.

Not built, on purpose: a stdio MCP client for a desktop computer-use driver. The a11y door drives
the page through CDP instead, which works identically on the Mac and in the container.

## Layers and boundaries

Four crates, three layers, and a short list of things outside the process.

```
L2  crates/bench      the harness: planner (goal → wants), arms, tasks, report, verify
                      the ONLY layer that talks to a model
        │ Intent
L1  crates/rwmcp  the engine: compiler (pred · world · intent · plan) + scheduler
                      + ledger + event bus.  "Zero model calls in here."
        │ Node + args
L0  effectors.rs      one adapter per surface: ApiEffector, McpEffector, Unavailable
    crates/driver     A11yEffector — a CDP page pool, the screen surfaces
- - - - - - - - - - - - - - - - - - - - - - - - - - - - - - - - process boundary
    your app (HTTP · /mcp · /events · a page)   Chromium over CDP
    the model endpoint (/v1/messages)           the disk (results/, plan cache)
```

The dependency graph is `bench → {rwmcp, driver}`, `driver → rwmcp`, and `rwmcp`
depends on nothing of ours. `crates/app` is the target we own; it is a **dev-dependency only**
of the other three, for in-process door tests, and never a runtime one. Embedding rwmcp means
depending on `rwmcp` alone: `tokio`, `reqwest`, `serde`, `sha2`. No database, no broker, no
agent framework, and no browser unless you add `driver`.

The model endpoint touches L2 only. The engine takes an `Intent` struct and has no idea where it
came from, which is why "one sample per goal" is structural rather than a discipline someone has
to maintain. Credentials are read once in `ModelClient::from_env` and each run directory's
`config.json` is written with the key omitted; the engine crate has no notion of a credential.

### External dependencies, and what breaks without each

| external | reached by | needed for | if it is missing |
|---|---|---|---|
| your HTTP API | `ApiEffector` → `reqwest` | everything | the node fails `Fatal` and the plan stops |
| `GET /openapi.json` | `world_from(base)`, one GET at startup | the world model | `WorldError::Fetch`. Bypass it with `World::from_openapi(&json)`, which takes any JSON you supply |
| `GET /events` (SSE) | `EventBus::connect`, one shared stream | waits | falls back to a read every `check_every` (3 s); with no `check` declared, times out at `wait_timeout` (30 s) |
| a model endpoint | `ModelClient` → `POST /v1/messages` | goal → wants | not needed if you write the wants yourself |
| Chromium | `chromiumoxide` over CDP | `a11y`, `pixels` | compile fails with `NoSurface` rather than silently using another door |
| `POST /mcp` | `McpEffector` → JSON-RPC | the `mcp` surface, arms B/B2 | the compiler picks another allowed surface |
| the filesystem | `std::fs` | results, plan cache, screenshots | benchmark only; the engine writes nothing to disk |
| the system clock | `SystemTime::now`, microseconds | every ledger row's span | `max_parallel` is only as good as this clock's resolution |

What you write is the `x-reverse-webmcp` notes (once per operation), `idempotency-key` handling (once
per write), an SSE endpoint if anything waits, and the wants (per goal) — the first and last of
those have skills that draft and then verify them; see **Skills** below. The DAG, the edges, the
keys, the ordering, the concurrency and the receipt are all derived — there is no place to
hand-write a plan, on purpose.

## Skills: the setup work an agent should do for you

Two jobs used to be described here as "write this by hand". Both are now agent jobs with a
machine check at the end, packaged as skills in `.claude/skills/`. Claude Code picks them up
automatically inside this repo; any other agent can read the same `SKILL.md` as a prompt.

| skill | the job | the check that ends it |
|---|---|---|
| `annotate-world-model` | write the `x-reverse-webmcp` blocks for an app | call each operation against a reset app and diff the state; every row that changed must appear in `writes` |
| `write-wants` | turn a goal into wants and forks | `lint`, read `plan.render()`, then compile the same wants in a different order and diff the two plans |

The principle in both: **the agent drafts, the machine verifies.** A drafted footprint is a
guess, and a wrong `writes` list is the worst failure this design has — it deletes an edge, the
plan looks *more* parallel, and two writes race with nothing in the output saying so. So neither
skill ends at "looks right". In this repo the app does the checking for you: every write logs
its own footprint into `GET /oracle/effects`, so the diff is a direct comparison rather than a
judgement.

### Two different agents

Keep these apart when reading anything below.

| | the planner | your coding agent |
|---|---|---|
| what it is | a model call rwmcp makes at runtime, `crates/bench/src/planner.rs` | Claude Code, Codex, Grok Build |
| when | once per goal | once, at setup |
| writes | the wants and forks | the annotations, task files, glue code |
| skills apply | no — a fixed system prompt over `/v1/messages` | yes |

### Words, precisely

- **goal** — your sentence. Always yours.
- **want** — one predicate: one fact that must be true at the end.
- **fork** — a declared question the planner agrees to be woken for.
- **Intent** — the struct holding goal, wants, constraints and forks.
- **plan** — what the compiler derives. Nobody writes this, by design.

The model's tool call is `emit_intent` and it returns `{wants, forks}` only; the goal and
constraints are stapled on by `intent_from()`. So "the agent writes the intent" is loose — it
writes the wants and the forks.

## Using rwmcp on your own app

rwmcp is two things: a Rust **engine** (`crates/rwmcp`) and a **benchmark** around it. The
engine never touches your code. It drives your app through HTTP, the way any client would, so
the app can be Rust, Node, Python, PHP, Go, or something that already exists. The engine itself
is Rust today: embed the crate, or run the `bench` binary. There is no JavaScript or Python SDK,
no hosted service, and no HTTP wrapper around the engine yet.

### Requirements

- Rust (stable, `cargo`).
- A model endpoint for the planner: `ANTHROPIC_API_KEY`, or any local gateway that speaks the
  Messages shape (`--base-url http://localhost:8080` with opencodex needs no key). Skip this if
  you write the wants yourself.
- Chrome only for screen steps (the a11y and pixel doors); Docker only for the sandbox.

### Install and see it work

```sh
git clone <rwmcp> && cd rwmcp
cargo build --release
make app                      # the sample invoicing app on http://127.0.0.1:47310
make bench                    # ours + the script ceiling on every phase ≤3 task
```

### What your app must provide

| door | required | what |
|---|---|---|
| `GET /openapi.json` | yes | an OpenAPI document with an `operationId` on every operation and an `x-reverse-webmcp` block per operation (below) |
| `idempotency-key` header on writes | yes | the same key twice returns the first response and changes nothing; this is where "zero double-sends" comes from |
| `GET /events` | for waits | server-sent events, one `data: {"seq","kind","entity","id","data"}` per state change, so a "wait for payment" is an edge instead of a poll |
| `POST /mcp` | optional | JSON-RPC `initialize`, `tools/list`, `tools/call`; writes take an `idempotency_key` argument |
| a web page | for UI-only steps | every control has a role and a name; the engine clicks it through headless Chrome |

The `x-reverse-webmcp` block is the world model. One per operation:

```json
"x-reverse-webmcp": {
  "post":     "invoice(id=$id).status='sent'",        what becomes true
  "requires": ["invoice(id=$id).exists"],             what must be true first
  "reads":    ["invoice:$id", "customer:*"],          footprint: reads
  "writes":   ["invoice:$id", "outbox:new"],          footprint: writes (a write gets a key)
  "produces": "invoice",                              for creators and finders
  "defaults": {"amount_cents": 10000},                body fields the goal need not mention
  "fork":     {"when": "result.count != 1", "ask": "which customer named $name"},
  "external": true,                                   leaves the system (email, money)
  "surfaces": {"api": 1, "mcp": 3, "a11y": 50}        which doors expose it, at what cost
}
```

Reads and writes decide the parallelism: two operations with disjoint footprints get no edge and
run together; a shared token serialises them. Three document-level blocks complete the model:
`x-reverse-webmcp-entities` (each noun, its id field and fields), `x-reverse-webmcp-events` (things the
outside world does, with a `check` read for a lost webhook) and `x-reverse-webmcp-ui` (button-only
actions with a route and a control). `crates/app/static/openapi.json` is a complete example.

### Embedding the engine

```rust
use std::sync::Arc;
use rwmcp::{compile, CompileOptions, Intent, Ledger, Scheduler};
use rwmcp::{events::EventBus, ledger::Recorder};

let base = "http://localhost:8000";
let world = Arc::new(rwmcp::world_from(base).await?);           // reads /openapi.json
let intent = Intent {
    goal: "invoice Acme and send it".into(),
    wants: vec!["invoice(customer=customer(name='Acme')).status='sent'".into()],
    ..Default::default()
};
let opts = CompileOptions { plan_id: "job-42".into(), surfaces: vec!["api".into()] };
let plan = compile(&intent, &world, &opts)?;                          // the DAG, keys included
let sched = Scheduler {
    effectors: rwmcp::default_effectors(base, world.clone(), &opts.surfaces),
    bus: Some(EventBus::connect(base).await?),
    pools: Default::default(),
    policy: Default::default(),
    recorder: Recorder::new(world.clone()),
};
let mut ledger = Ledger::new();
let outcome = sched.run(&plan, &mut ledger).await;
let receipt = ledger.receipt(&plan, outcome.status, outcome.yield_reason, outcome.evidence, outcome.error);
```

`receipt.status` is `committed` (every keyed node has an ok row), `need_think` (stopped at a
declared fork or gate; `yield_reason` says which) or `error`. Write the wants yourself, as above
(the `write-wants` skill is this job with the lint and plan-diff checks attached), or let a model
write them at runtime: the planner (goal → wants, lint, one re-ask) lives in
`crates/bench/src/planner.rs` behind the `Sampler` trait and works with any Messages-shaped
endpoint through `loops::ModelClient`. To resume after answering a fork, recompile and call
`sched.resume(&plan, &mut ledger, &ledger.completed(&plan))`: nothing already proven done by
its key is re-sent.

### Framework notes

- **Already-built app:** add the `x-reverse-webmcp` notes to the OpenAPI you already serve and honor
  `idempotency-key` on writes. No rewrite.
- **React Native / mobile:** rwmcp drives the app's backend API. It can also click a *web*
  page through headless Chrome; it does not drive a phone screen.
- **Rust project:** add `rwmcp` as a path dependency and use the snippet above.
- **Anything else (Node, Python, PHP, Go):** your app is the target; run the engine as a
  separate Rust process. A non-Rust SDK is not built yet.

## Tests

```sh
cargo test --workspace
```

The compiler tests are the ones that matter: two customers compile to two lanes with no edge
between them; a report waits for every send while each send depends only on its own create;
a receipt waits on the payment event; a UI-only action needs a screen surface or the compile fails.
