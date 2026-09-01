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

The base URL, model and effort are recorded in every result file, so a report always says which
model produced it. Pin one model for a whole comparison; never mix providers across arms.

Every knob is on `bench run --help`: `--latency-ms` (default 25, added to every write so the app
behaves like a network service), `--surfaces`, `--model`, `--effort`, `--planner`.

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
| A | CUA click loop: screenshot → model → one action, on a sandbox display | needs the sandbox and a screen driver; not wired yet |
| B | MCP loop, one tool call per turn | wired (`loops.rs`) |
| B2 | MCP loop, the model may emit several tool calls per turn, run concurrently | wired; the honest baseline |
| C | WebMCP loop inside a headless browser | needs a browser driver; not wired yet |
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

## What a result contains

Per run: status, model samples, tokens, wall time, **max parallel** (a sweep over the ledger's
microsecond spans), correctness against the task's expected end state as read from the oracle,
double-sends from the app's outbox, forks taken, the full ledger, the oracle snapshot, and for
arm D the rendered plan. `bench verify` recomputes the first three from the raw rows and fails if
anything stored disagrees.

## The sandbox

```sh
make sandbox     # docker compose: app + N virtual displays with Chromium + ffmpeg recording + the bench
```

Your machine runs the command. The app, the browsers, the displays and the recordings live in
the container. Each display has its own Chromium profile directory so two screen lanes never
collide on a profile lock. The scheduler's screen pool is exactly the display count.

## Where this is on the road

- Phase 0, the app and oracle: done.
- Phase 1, baselines: B and B2 wired and run through opencodex. B2 with parallel tool calls matches our max parallel on fan-out (T2: 10 wide in 4 samples) and is the bar to beat; it lost T3 (no report) and T4 (did not wait for payment) in the smoke run. A and C wait on the sandbox's browser driver.
- Phase 2, compiler and scheduler on the API door: done. T2 and T3 run wide and match the ceiling.
- Phase 3, planner sample, event edges, retries with keys, forks: done. With `--planner model` through opencodex and grok-4.6, arm D passes T1 to T6 on exactly one sample each. The planner gets the world model plus one read of current facts (the customer list); it never sees actions.
- Phase 4, mixed surfaces: the compiler already places `approveInvoice` on `a11y`; the effector that drives the page is the next piece. A driver that speaks MCP can plug straight into `McpEffector`.
- Phase 5, harden and publish: not started.

## Tests

```sh
cargo test --workspace
```

The compiler tests are the ones that matter: two customers compile to two lanes with no edge
between them; a report waits for every send while each send depends only on its own create;
a receipt waits on the payment event; a UI-only action needs a screen surface or the compile fails.
