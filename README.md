# rwmcp

**Reverse-WebMCP: the execution layer.** WebMCP declares an app's *actions* and lets a model
pick the order. rwmcp declares what those actions *mean* and lets a compiler pick both.

One model sample in, a parallel plan out, a receipt that proves it. And a benchmark anyone can
rerun.

A **compiler** turns a goal's intent graph into a dependency DAG with a surface, an idempotency
key and a read/write footprint per node. A **scheduler** runs that DAG as wide as the data
allows, waits on events instead of polling, and calls the model only at declared forks. The
claim is falsifiable on one number — **max concurrent effects** — measured from the ledger and
never taken from an arm's own word.

Everything is Rust. Nothing runs on your screen.

## Guides

The long-form documentation lives in six illustrated guides. They are private Artifacts: the
links work for the author and for anyone the author shares them with.

| guide | answers |
|---|---|
| [rwmcp in Pictures](https://claude.ai/code/artifact/9032509f-35de-4b2c-b63a-5120711710c4) | **What does this do?** The idea, the doors your app needs, the annotation, and a receipt — for someone who has never seen it. |
| [Reverse-WebMCP](https://claude.ai/code/artifact/b53128ad-21db-48ae-acc3-c647f382f325) | **How is this different from MCP and WebMCP?** The same operation declared both ways, one goal run both ways, and when to pick each. |
| [Zero to First Plan](https://claude.ai/code/artifact/974551b9-1e84-4568-9fbf-a07157d055fd) | **How do I use it on my own app?** What to install, which skill does which job, the command that resets your app, five steps end to end. |
| [rwmcp Stack Map](https://claude.ai/code/artifact/8e01b51e-1bf2-4460-b185-4f4a8ae495b5) | **What is the code?** Four crates, three layers, the process boundary, and every external dependency with what breaks without it. |
| [The Falsifiable Number](https://claude.ai/code/artifact/a2b0d412-41df-46ab-a493-ef75f71e8717) | **Does it work?** The benchmark results, with every caveat that comes with them. |
| [The Benchmark Bench](https://claude.ai/code/artifact/6b0c12fa-6677-4c71-ad4b-beec10788d33) | **How is it measured?** The target app and its oracle, the six arms, the thirteen tasks, what a result file holds. |

New here: start with the first. Already know MCP: the second. Wiring your own app: the third.
Reading the source: the fourth.

## One command

```sh
make bench                      # build, spawn the app, run arms D and E on every phase ≤3 task, 5 runs each
make report RUN=results/<stamp> # rebuild summary.json + report.html from the stored ledgers
make verify RUN=results/<stamp> # recompute max parallel, correctness and double-sends from raw data
make app                        # run the target app on http://127.0.0.1:47310 and use the UI yourself
```

Model-driven arms need a server that speaks the Messages shape. No Anthropic key is required —
a local gateway works as-is:

```sh
make bench-ocx ARMS=D,E,B,B2 PLANNER=model RUNS=5     # through opencodex + grok-4.6
make bench ARMS=D,E,B,B2 RUNS=5                       # against Anthropic, claude-opus-5
```

Every run directory gets a `config.json` with the exact options used (the API key is never
written), and every result file records model, effort, base URL, latency and surfaces. Pin one
model for a whole comparison; never mix providers across arms. Every knob is on
`bench run --help`.

> **`bench run` is a benchmark harness, not a production runner.** It calls `POST /oracle/reset`
> on the target before *every* run. Never point it at an app you care about — to run real work,
> embed the `rwmcp` crate. See [Zero to First Plan](https://claude.ai/code/artifact/974551b9-1e84-4568-9fbf-a07157d055fd).

## Layout

```
crates/rwmcp      the engine: world model · predicates · compiler · scheduler · ledger · effectors · events
crates/app        the target app we own: invoicing, one handler set, five doors, an oracle
crates/bench      the arms, the tasks runner, the report, verify
crates/driver     a CDP page pool: the screen surfaces
tasks/            T1..T13 as TOML: goal, wants, seed, chaos, hooks, expected end state
results/          one JSON per (task, arm, run) plus summary.json and report.html
```

`crates/rwmcp` is the only crate you embed. It pulls in `tokio`, `reqwest`, `serde` and `sha2` —
no database, no broker, no agent framework, and no browser unless you add `driver`. `crates/app`
is a dev-dependency of the others, never a runtime one.

The `x-reverse-webmcp` blocks in `crates/app/static/openapi.json` are the world model:
postcondition, requirements, read/write footprint, and which surfaces expose each operation at
what cost. The compiler derives everything from that file. It is never hand-authored anywhere
else, and there is no place to hand-write a plan.

## Skills

Two setup jobs that used to read "write this by hand" are agent jobs now, each ending in a
machine check. Claude Code loads them from `.claude/skills/`; any other agent reads the same
`SKILL.md` as a prompt.

| skill | the job | the check that ends it |
|---|---|---|
| `annotate-world-model` | write the `x-reverse-webmcp` blocks for an app | call each operation against a reset app and diff the state; every row that changed must appear in `writes` |
| `write-wants` | turn a goal into wants and forks | `lint`, read `plan.render()`, then compile the same wants in a different order and diff the two plans |

The principle in both: the agent drafts, the machine verifies. A drafted footprint is a guess,
and a wrong `writes` list is the worst failure this design has — it deletes an edge, the plan
looks *more* parallel, and two writes race with nothing in the output saying so.

## Renamed from chiffon / zerohuman

| was | now |
|---|---|
| the OpenAPI extension `x-zerohuman` (and `-entities`, `-events`, `-ui`) | `x-reverse-webmcp` (same suffixes) |
| `crates/zerohuman`, package `zerohuman` | `crates/rwmcp`, package `rwmcp` |
| the project name `chiffon` | `rwmcp` |

The extension key is the only breaking change, and it fails loudly: an old document derives an
empty world model, every want becomes unsatisfiable, and a zero-node plan is
`CompileError::Empty` rather than a run that commits having done nothing. Rename the four keys
and you are done.

Everything under `results/` predating the rebrand is left exactly as it was — its provenance is
accurate for when it ran, and `bench verify` still recomputes every number in it.

## Tests

```sh
cargo test --workspace     # or ./scripts/gate.sh for fmt + clippy -D warnings + tests
```

The compiler tests are the ones that matter: two customers compile to two lanes with no edge
between them; a report waits for every send while each send depends only on its own create; the
same wants in a different order produce the same DAG; a receipt waits on the payment event; a
UI-only action needs a screen surface or the compile fails.
