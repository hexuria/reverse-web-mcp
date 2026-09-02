# chiffon

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
crates/zerohuman   world model deriver · predicate language · compiler · scheduler · ledger · effectors · event bus
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

The `x-zerohuman` blocks in `crates/app/static/openapi.json` are the world model: postcondition,
requirements, read/write footprint, and which surfaces expose each operation at what cost.
The compiler derives everything from that file. It is never hand-authored anywhere else.

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

## Tests

```sh
cargo test --workspace
```

The compiler tests are the ones that matter: two customers compile to two lanes with no edge
between them; a report waits for every send while each send depends only on its own create;
a receipt waits on the payment event; a UI-only action needs a screen surface or the compile fails.
