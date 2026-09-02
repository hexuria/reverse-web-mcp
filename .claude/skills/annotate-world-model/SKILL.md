---
name: annotate-world-model
description: Write the x-reverse-webmcp blocks that make an app compilable by rwmcp. Use when someone wants to point rwmcp at an app for the first time, add a new operation to an annotated app, or fix a plan that compiles wrong (missing edges, wrong parallelism, Unsatisfiable wants).
---

# Annotating an app for rwmcp (reverse-WebMCP)

Where WebMCP publishes an app's *actions*, rwmcp publishes what those actions *mean*, and a
compiler derives the actions and their order from that. These blocks are that declaration: the
whole reverse-WebMCP surface of an app.

The compiler derives every plan from one place: the `x-reverse-webmcp` blocks in the app's
OpenAPI document. This skill produces those blocks and then **proves them against the running
app** rather than trusting the draft.

Draft is cheap. Verification is the point. A wrong `writes` list is the worst failure this
system has: it removes an edge, the plan looks *more* parallel, and two writes race with nothing
in the output saying so.

## Inputs you need

1. The app's OpenAPI document (or enough of the routes to write one).
2. The handler source, if you can read it. This is where `reads`/`writes` really come from.
3. A running instance you may reset and call. Without one, stop after Step 3 and say the
   annotation is unverified.

## Step 1 — the entities

Add `x-reverse-webmcp-entities` at the document root: every noun the API talks about, its id field,
and the fields a goal might mention.

```json
"x-reverse-webmcp-entities": {
  "invoice": { "id": "id", "fields": ["customer_id", "amount_cents", "status", "receipt_sent"] }
}
```

Only list fields a *want* could name. `lint` rejects `invoice(...).colour='red'` because
`colour` is not here, so this list is the spell-checker for goals.

## Step 2 — one block per operation

Every operation needs an `operationId` and a block:

```json
"x-reverse-webmcp": {
  "post":     "invoice(id=$id).status='sent'",
  "requires": ["invoice(id=$id).exists"],
  "reads":    ["invoice:$id", "customer:*"],
  "writes":   ["invoice:$id", "outbox:new"],
  "produces": "invoice",
  "defaults": {"amount_cents": 10000},
  "external": true,
  "surfaces": {"api": 1, "mcp": 3, "a11y": 50}
}
```

Field by field:

- **`post`** — the single fact that is true after a successful call, in the predicate language:
  `entity(arg=value).field=value`. Variables (`$id`) bind to the operation's parameters by name.
  A creator's post is `.exists`; a finder's is `.resolved`. An operation with no `post` can
  never satisfy a want — that is fine for pure reads the compiler pulls in itself.
- **`requires`** — facts that must already hold. The compiler satisfies each one recursively, so
  this is where real dependency edges come from. Keep it minimal and true; a requirement that is
  not really needed serializes work that could run wide.
- **`reads` / `writes`** — resource tokens, `entity:selector`. Three selector forms:
  `entity:$param` (the row named by that parameter), `entity:new` (a row this call creates),
  `entity:*` (all rows of that entity). Get this right; see Step 4.
- **`produces`** — the entity a nested reference resolves through. Set it on creators and finders.
- **`defaults`** — body fields a goal will not mention, so the compiler can fill them.
- **`external`** — true if the effect leaves the system (email, money, a message). Gated by
  `constraints.external_ok`.
- **`surfaces`** — every door that exposes this operation and its relative cost. The compiler
  picks the cheapest one the run allows. Omit a door and the operation cannot use it.
- **`fork`** — for a finder that may match zero or many: `{"when": "result.count != 1", "ask": "..."}`.
  Only `result.count != 1` and `result.count == 0` are understood today.

Two document-root blocks complete the model:

- **`x-reverse-webmcp-ui`** — actions with no API at all. Same fields plus `route` and
  `control: {role, name}`, and `surfaces` limited to screen doors.
- **`x-reverse-webmcp-events`** — things the outside world does. `post` plus a `check`
  (`{op, arg, field, value}`) so a lost webhook can be confirmed by reading.

## Step 3 — the footprint rules

This is the part to be slow about.

- **Write down every row a call touches, not just the obvious one.** `sendInvoice` writes
  `invoice:$id` *and* `outbox:new`. Miss the outbox and two operations that both append there
  will be planned as independent.
- **Reads matter as much as writes.** They are what makes a reader wait for a writer.
- **When unsure, widen, never narrow.** `entity:*` is always safe: it over-serializes, costing
  parallelism. A too-narrow token costs correctness. Prefer a slow correct plan.
- **A parameter that is not in `params` cannot be a selector.** `resource()` falls back to
  `entity:*` for an unbound variable, which is safe but usually means you meant a different name.
  `rwmcp --app URL --validate` reports this one as `unbound_footprint_param`; it is otherwise
  invisible, since the plan is correct and merely slow.
- **`new` is per-node.** `entity:new` becomes `entity:@NodeId`, which never conflicts with
  another node's `new`. That is what lets ten creates run at once.

## Step 4 — prove it

Never ship a draft. Do all four. The first and the last need no app running.

**a. Is the model coherent?**

```
rwmcp --app URL-or-openapi.json --validate
```

It reads the document and reports what cannot be right: an entity or field that does not exist, a
`before` pointing at no operation, an operation no surface can call, a footprint selector naming a
`$param` the operation does not have, and two operations that write the same thing with no
declared order between them. Exit **0** when there is nothing fatal, **10** when there is;
`--json` gives `errors` and `warnings` with a stable `code` on each. Anything under `errors` must
be fixed before going further.

**b. Does it compile at all?** Write one want per operation you annotated and plan it:

```
rwmcp --app URL --wants w.wants
```

An `Unsatisfiable` error means `post` does not unify with the want; `NoSurface` means the door is
missing from `surfaces`; `Unbound` means a `post` variable has no matching parameter.

**c. Does the footprint match reality?** This is the one no command can do for you, and the one
that matters most. For each write operation: reset the app to a seed, read the state, call the
operation once, read the state again, and diff. Every entity row that changed must appear in
`writes`. In this repo the app does the bookkeeping for you — `GET /oracle/effects` returns each
call's own footprint string (`"invoice:12,outbox:13"`), so the diff is a direct comparison.
Against a real app, diff the database or its write-ahead log. Anything that changed and is not in
`writes` is a bug you just found.

**d. Does the order come out right?**

```
rwmcp --app URL --wants w.wants --order-check
```

It compiles the wants, compiles them reversed, and diffs the two graphs. They must be identical.
If they are not, an edge is coming from want order instead of from data, and the annotation (or
the compiler) is wrong. Then read the plan from **b** and check specifically that a join waits for
every branch, and that a step which must precede another actually does.

## Common mistakes

| symptom | usual cause |
|---|---|
| `nothing in the world model can make '…' true` | `post` shape does not match the want: wrong entity, wrong field, or args that do not unify |
| everything runs serially | a token widened to `entity:*` that should name a row, or a `requires` that is not real |
| two writes race, output looks fine | a missing token in `writes` — the dangerous one |
| a want compiles to one node when it should be many | an `each(...)` that never expanded; check the selector |
| `operation X has no available surface` | the door is not in `surfaces`, or the run did not allow it |
| a fork never fires | `when` is not one of the two understood conditions |
| a plan that used to be right goes wrong after re-annotating | keys are scoped to the world model; a saved recipe says so and stops, `--force` overrides |

## Starting from nothing

If the document has no `x-reverse-webmcp` blocks at all:

```
rwmcp --app openapi.json --init
```

prints the skeleton block each operation still needs, with its real parameter names listed, plus
the `x-reverse-webmcp-entities` block for the document root. Fill them in, then go to Step 4.

## What to report back

State plainly which operations were verified against a running app and which are drafts. `rwmcp
--validate` passing is necessary and not sufficient: it proves the model is coherent, never that
a footprint is true. Only step **4c** does that, and only for the operations you actually ran. An
unverified footprint is a claim, not a fact, and the whole design rests on that distinction.
