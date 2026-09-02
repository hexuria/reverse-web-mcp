# rwmcp

**Reverse-WebMCP: the execution layer.** WebMCP declares an app's *actions* and lets a model
pick the order. rwmcp declares what those actions *mean* and lets a compiler pick both.

One model sample in, a parallel plan out, a receipt that proves it. And a benchmark anyone can
rerun.

A **compiler** turns a goal's intent graph into a dependency DAG with a surface, an idempotency
key and a read/write footprint per node. A **scheduler** runs that DAG as wide as the data
allows, waits on events instead of polling, and never needs the model again unless the plan
stops to ask. Every claim below is falsifiable from the ledger — max concurrent effects, model
calls, tokens, double-sends — and never taken from an arm's own word.

Everything is Rust. Nothing runs on your screen.

## The command

Install once, then never write Rust:

```sh
cargo install --path crates/cli    # gives you `rwmcp`
```

One command, options only, so an agent composes a single line instead of a session.

```sh
# What can this app be asked for?
rwmcp --app http://localhost:8000 --world

# Plan it. One model call, in 53 of 60 measured runs. Nothing happens.
rwmcp --app http://localhost:8000 --goal "invoice every customer and send it"

# Run what an agent already wrote. No model call at all.
rwmcp --app http://localhost:8000 --wants billing.wants --run --yes
```

`--wants` is a plain text file, one predicate per line, `#` for comments. That is the artefact
the `write-wants` skill produces, and the artefact you keep in version control:

```
# billing.wants
invoice(customer=each(customer())).exists
invoice(customer=each(customer())).status='sent'
report(invoices=[all(invoice(customer=each(customer())))]).exists
```

`--wants -` reads the same thing from stdin, so an agent needs no temp file.

Nothing is executed unless you pass `--run`, and a plan with steps that leave the system —
email, money — refuses to run until you also pass `--yes`. The plan is printed first, in full,
every time, with each outgoing step named by who it is for:

```
10 steps leave the system (email, money):
  sendInvoice ← 'Acme', 10000
  sendInvoice ← 'Globex', 10000
  …
```

### Checking things before you run them

Three commands, none of which needs Rust and two of which need no app running:

```sh
rwmcp --app URL --validate                     # is the world model coherent?
rwmcp --app URL --wants w.wants --order-check  # does the plan depend on want order?
rwmcp --app openapi.json --init                # what blocks does my OpenAPI doc still need?
```

`--validate` is the review the `annotate-world-model` skill describes: entities and fields that do
not exist, `before` pointing nowhere, an operation no surface can call, a footprint selector that
silently widens to `entity:*`, and two operations writing the same thing with no declared order.
`--order-check` compiles the wants, compiles them reversed, and diffs the two graphs.

### When it stops to ask

A name that matches two rows is a fork: the run stops, exit code **11**, with the rows attached.

```sh
rwmcp --app URL --wants w.wants --run --yes --answer "customer(name='Acme')=>customer(id=11)"
rwmcp --app URL --wants w.wants --run --yes --answer-with-model    # one model call
```

Only the ambiguous want changes, so everything already done keeps its key and is not done twice.
`--receipt-out FILE` saves a run and `--resume FILE` picks it back up in another process, skipping
whatever committed.

### Exit codes

An agent branches on the number, never on the prose. `--json` prints exactly one object on every
path, `{"ok":true,…}` or `{"ok":false,"code":"…","message":"…"}`.

| code | meaning |
|---|---|
| 0 | done |
| 2 | bad usage (clap) |
| 10 | the wants, the recipe or the world model did not hold up |
| 11 | it stopped to ask; answer with `--answer` or `--answer-with-model` |
| 12 | steps leave the system and `--yes` was not given |
| 13 | the app or its world model could not be reached |
| 14 | the planner failed |
| 15 | a step failed while running |

`RWMCP_APP`, `RWMCP_SURFACES`, `RWMCP_RECIPES_DIR`, `RWMCP_BASE_URL` and `RWMCP_MODEL` fill their
flags, so the app need be named once per shell rather than once per command.

### A plan that worked is a recipe

Once a goal has been planned successfully, the shape does not change; only the values do. So
save it, leave the changing parts as `$placeholders`, and re-run it for the cost of nothing:

```sh
rwmcp --app URL --goal "invoice Acme and send it" --save billing --run --yes
rwmcp --app URL --recipe billing --set who=Globex --run --yes    # 0 model calls
rwmcp --list
```

A recipe is a small JSON file — written by `--save`, or by an agent, or by you:

```json
{
  "name": "billing",
  "app": "http://localhost:8000",
  "goal": "invoice every customer and send it",
  "params": ["who"],
  "wants": [
    "invoice(customer=customer(name=$who)).exists",
    "invoice(customer=customer(name=$who)).status='sent'"
  ],
  "world_fingerprint": "9f1c…",
  "created_at": "2026-09-02T14:20:11Z",
  "rwmcp_version": "0.1.0"
}
```

The fingerprint is a hash of the app's world model when the recipe was saved. Re-annotate the app
and the recipe stops rather than running wants that may no longer mean what they meant; `--force`
proceeds anyway.

`--recipe billing` looks for `billing.json` in the recipes directory (`~/.rwmcp/recipes` by
default, or `--recipes-dir`). `--recipe ./ops/billing.json` takes a path instead, so recipes can
live in your repository next to the code they drive. Forget a parameter and the command tells you
which `--set` is missing rather than guessing.

This is the intended steady state: the model is paid once, at design time, and the thing that
runs in production is a compiled plan with arguments.

## Does it work

Twelve tasks against an app we own, 168 runs across three campaigns, every number recomputed
from the raw ledgers by `make verify`. The four tasks below are the ones every arm ran.

| | ours | parallel MCP loop | WebMCP loop | hand-written script |
|---|---|---|---|---|
| ten invoices | **1** call · 1,531 tok | 4 calls · 7,095 tok | 7 calls · 14,755 tok | 0 calls |
| wait for the payment, then receipt | **1** · 1,535 | 5 · 6,082 | 5 · 5,802 | 0 |
| ten chains, six levels deep | **1** · 1,547 | 6 · 14,245 | 10 · 26,993 | 0 |
| three hundred invoices from one want | **1** · 1,563 | 14 · 357,927 | 40 · 824,014 | 0 |

Model calls and input tokens, medians, grok-4.6. The last row is the shape of the whole thing:
the loop's bill grows with the work because it re-reads its transcript every turn; a compiled
plan reads the world model once. On that row the WebMCP loop hit its 40-turn budget in all three
runs, having created up to 283 of the 300 invoices and sent none.

- **120/120 correct** for ours across twelve tasks, five runs each, checked by the app's own oracle.
- **0 double-sends** across all 168 runs of every arm — the compiler inserts the idempotency key, so no model drift can skip it.
- **One call is a median, not a guarantee.** 53 of 60 planning runs took one; seven took a second repair call after the planner returned an empty intent. None took three. On the second model, `gpt-5.6-luna`, all twelve planning runs took one.
- **Execution matches the hand-written script** to within tens of milliseconds on nine of twelve tasks. It does not on three, and the results page says why rather than dropping them.

The caveats travel with the numbers: five runs per cell is a smoke signal, not a confidence
interval; the loops ran four tasks to our twelve; the chaos tasks are seeded, so repeating them
tests jitter and not resilience; and T7 is excluded because it needs a screen. All of that, plus
the five things this benchmark caught us getting wrong, is on the
[results page](docs/results.html).

And the largest caveat of all: **every number above is against an app we wrote, which publishes a
world model we also wrote.** That is the right way to measure a compiler and the wrong way to
find out whether anyone else can use it. The trial that would answer the second question — a real
site without WebMCP, a real site with it, and the limitations that fall out — is specified in
[`docs/real-world-trial.md`](docs/real-world-trial.md) and has not been run.

## Embedding it

The CLI is a shell over the library, and the library is one object.

```rust
use rwmcp::{Intent, Session};

let app = Session::connect("http://localhost:8000").await?;
let intent = Intent { goal: "invoice Acme".into(), wants: vec!["invoice(customer=customer(name='Acme')).status='sent'".into()], ..Default::default() };
let plan = app.plan(&intent)?;              // lint, then compile
let receipt = app.run(&plan).await?;        // run, then receipt
```

`plan()` returns `PlanError::Wants(Vec<LintError>)` or `PlanError::Compile(CompileError)`, so the
errors an embedder gets are the ones the CLI prints. `Session::offline(world)` plans against a
world model with nothing behind it. `plan_id` scopes the idempotency keys: two sessions under the
same id share their committed work, which is what makes a crashed run safe to start again.

## Guides

The long-form documentation is six illustrated guides in [`docs/guides/`](docs/guides/index.html).
Open `docs/guides/index.html` in a browser, or read them online:

| guide | answers |
|---|---|
| [rwmcp in Pictures](docs/guides/pictures.html) · [online](https://claude.ai/code/artifact/9032509f-35de-4b2c-b63a-5120711710c4) | **What does this do?** The idea, the doors your app needs, the annotation, and a receipt — for someone who has never seen it. |
| [Reverse-WebMCP](docs/guides/reverse-webmcp.html) · [online](https://claude.ai/code/artifact/b53128ad-21db-48ae-acc3-c647f382f325) | **How is this different from MCP and WebMCP?** The same operation declared both ways, one goal run both ways, and when to pick each. |
| [Zero to First Plan](docs/guides/setup.html) · [online](https://claude.ai/code/artifact/974551b9-1e84-4568-9fbf-a07157d055fd) | **How do I use it on my own app?** What to install, which skill does which job, and five steps end to end. |
| [rwmcp Stack Map](docs/guides/stack-map.html) · [online](https://claude.ai/code/artifact/8e01b51e-1bf2-4460-b185-4f4a8ae495b5) | **What is the code?** Five crates, three layers, the process boundary, and every external dependency with what breaks without it. |
| [The Falsifiable Number](docs/guides/results.html) · [online](https://claude.ai/code/artifact/a2b0d412-41df-46ab-a493-ef75f71e8717) | **Does it work?** The benchmark results, with every caveat that comes with them. |
| [The Benchmark Bench](docs/guides/benchmark.html) · [online](https://claude.ai/code/artifact/6b0c12fa-6677-4c71-ad4b-beec10788d33) | **How is it measured?** The target app and its oracle, the six arms, the thirteen tasks, what a result file holds. |

The full results page is [`docs/results.html`](docs/results.html) · [online](https://claude.ai/code/artifact/2ab0387c-161a-4e55-927b-2e53ef5e12da).

New here: start with the first. Already know MCP: the second. Wiring your own app: the third.
Reading the source: the fourth.

## Running the benchmark

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
crates/cli        the `rwmcp` command: plan, check, run
crates/bench      the arms, the tasks runner, the report, verify
crates/driver     a CDP page pool: the screen surfaces
tasks/            T1..T13 as TOML: goal, wants, seed, chaos, hooks, expected end state
results/          one JSON per (task, arm, run) plus summary.json and report.html
docs/guides/      the six guides, as static HTML
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
| `annotate-world-model` | write the `x-reverse-webmcp` blocks for an app | `rwmcp --validate` for coherence, then call each operation against a reset app and diff the state; every row that changed must appear in `writes` |
| `write-wants` | turn a goal into wants and forks | `rwmcp --wants w.wants` to lint and read the plan, then `--order-check` to prove it does not depend on want order |

The principle in both: the agent drafts, the machine verifies. A drafted footprint is a guess,
and a wrong `writes` list is the worst failure this design has — it deletes an edge, the plan
looks *more* parallel, and two writes race with nothing in the output saying so.

## Renamed from chiffon / zerohuman

| was | now |
|---|---|
| the OpenAPI extension `x-zerohuman` (and `-entities`, `-events`, `-ui`) | `x-reverse-webmcp` (same suffixes) |
| `crates/zerohuman`, package `zerohuman` | `crates/rwmcp`, package `rwmcp` |
| the project name `chiffon` | `rwmcp` |
| the `CHIFFON_CHROME` environment variable | `RWMCP_CHROME` |

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
