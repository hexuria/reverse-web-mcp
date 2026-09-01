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

A want may contain `each([...])`; the compiler unrolls it into one want per element before
compiling, so three hundred rows are still one sample. Keys are identical to the written-out form.

Each task declares its `depth` (the longest dependency chain) so the report can plot samples
against depth for every arm, and a `[script]` block that arm E interprets as its hand-written
parallel program: customers, send, wait for payment, receipt, a report per invoice, a report over all.

## First campaign, 2026-09-02

`results/campaign-1`: grok-4.6 through opencodex, planner at low effort, loops at medium, 25 ms per
write, 5 runs per cell, 120 results, `bench verify` clean. Medians.

| task | depth | arm | correct | samples | plan ms | run ms | max parallel |
|---|---|---|---|---|---|---|---|
| T1 one invoice | 3 | **D ours** | 5/5 | 2 | 4849 | 54 | 1 |
| | | B2 parallel MCP | 5/5 | 4 | 8646 | 5609 | 1 |
| | | B sequential MCP | 5/5 | 4 | 10453 | 6055 | 1 |
| T2 ten invoices | 3 | **D ours** | 5/5 | 1 | 5729 | 57 | 10 |
| | | B2 | 5/5 | 4 | 12995 | 7886 | 10 |
| | | B | 5/5 | 22 | 48767 | 44818 | 1 |
| T3 three + report | 4 | **D ours** | 5/5 | 1 | 4120 | 98 | 3 |
| | | B2 | 4/5 | 5 | 9859 | 6451 | 3 |
| | | B | 5/5 | 11 | 21981 | 18005 | 1 |
| T4 wait for payment | 5 | **D ours** | 5/5 | 1 | 4866 | 679 | 1 |
| | | B2 | 0/5 | 6 | 18994 | 9604 | 1 |
| | | B | 0/5 | 60 | 77651 | 75030 | 1 |
| T5 ten, 30% failures | 3 | **D ours** | 5/5 | 1 | 6262 | 100 | 10 |
| | | B2 | 5/5 | 5 | 17057 | 12274 | 10 |
| | | B | 5/5 | 24 | 44654 | 40501 | 1 |
| T6 two Acmes | 3 | **D ours** | 3/5 | 2 | 5755 | 2608 | 1 |
| | | B2 | 5/5 | 2 | 5037 | 1 | 1 |
| | | B | 5/5 | 2 | 5695 | 1 | 1 |

Arm E, the script ceiling, is 5/5 everywhere at 0 samples; its run time equals ours to within a
few milliseconds on every task. Double-sends were zero for every arm on every task.

What the table says:

- **Samples.** Ours is 1 on every task that needs no question. B2 pays a turn per dependency
  level (4 at depth 3, 5 at depth 4, 6 at depth 5); B pays a turn per action.
- **Wall time.** Ours is dominated by the one planner sample; the kitchen itself runs in tens of
  milliseconds. B2's run time is its own thinking between tool calls.
- **Waiting.** T4 separates the arms completely: the wait is an edge for us and a thing to
  remember for a loop. Neither loop managed it in five tries.
- **The honest losses.** T6: two of our five runs returned an empty fork answer that was compiled
  as-is; that is fixed in the commit after this campaign (fork answers are linted). T1: the planner
  needed a lint re-ask in three of five runs, which is why its median is 2 samples.

`results/campaign-1/report.html` has the full table with spread, and every result file carries
its ledger, snapshot and intent.

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
