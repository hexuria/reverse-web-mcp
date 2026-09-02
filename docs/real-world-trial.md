# The real-world trial: where rwmcp works, and where it stops

**Status:** not started. This is the brief, written while the benchmark was fresh so the next
session does not have to reconstruct the reasoning.

Everything measured so far runs against an app we wrote, which publishes a perfect
`x-reverse-webmcp` world model because we wrote that too. That is the right way to measure a
compiler and the wrong way to find out whether anyone else can use it. This trial answers one
question in two halves:

> Does rwmcp work on a site we do not control — one without WebMCP, and one with it — and where
> exactly does it stop?

The deliverable is **a limitations section we can publish**, backed by attempts, not by
reasoning. A limitation we predicted and never hit is a guess; a limitation we hit is evidence.

---

## The thing to keep hold of

rwmcp does not discover what an app's operations *mean*. It consumes a declaration of what they
mean and compiles a plan from it. So every real-world question reduces to two:

1. **How expensive is the world model to obtain** for an app nobody annotated for us?
2. **How wrong can it be before plans go wrong**, and does anything catch that in time?

WebMCP does not answer either. WebMCP declares an app's *actions* — name, description,
parameters. It does not declare postconditions, requirements, read/write footprints, or
precedence. So the honest hypothesis going in is: **a WebMCP site is a shortcut for the
parameter half of the annotation and no help at all for the semantic half.** The trial should
try to falsify that, and report it either way.

---

## Choosing the two targets

Both targets must be things we may legitimately hammer. Rules, in order of importance:

- **Never a live account with real money, real customers or real email.** Sandbox, test mode, or
  self-hosted only.
- **Resettable**, or the footprint check in step 3 is impossible and every footprint stays a
  claim. This is a hard requirement, not a nice-to-have.
- **Not written by us.** The whole point is somebody else's naming, somebody else's shape.

### Target A — no WebMCP

Something real, self-hostable, with a genuine REST API and, ideally, an OpenAPI document we did
not write. Strong candidates, roughly in order:

| candidate | why | watch out for |
|---|---|---|
| **Vikunja** | todo/project app, real OpenAPI, docker-compose, trivially resettable | small surface — may be too easy |
| **Gitea** | large real OpenAPI, self-hosted, resettable via volume | write ops have real dependency depth (issue → label → milestone → PR) |
| **Stripe test mode** | the OpenAPI document the industry actually uses; genuine idempotency keys | not resettable — good for a read-only planning test, bad for the footprint check |
| **WooCommerce / WordPress REST** | messy, real, widely deployed | auth is fiddly; the spec is partial |

Recommendation: **Gitea** as the primary — it is big enough to hurt, its dependency structure is
real, and a docker volume reset gives us an oracle. Use Stripe test mode as a *second, planning
only* target, because its idempotency-key support is exactly the thing we want to know about and
its spec is the best-quality one we will find.

### Target B — with WebMCP

WebMCP is young; there may be no third-party site worth using. In order of preference:

1. A genuine third-party WebMCP site, if one exists by then. Search first; do not assume.
2. A WebMCP-enabled build of Target A, wired by someone other than us if possible.
3. Failing both: add a WebMCP layer to Gitea ourselves and **say so in the writeup** — it is then
   a test of the WebMCP *shape*, not of somebody else's WebMCP judgement, and that is a weaker
   claim that must be labelled as one.

---

## Prerequisite: `--verify-footprints`

This was Stage 3 of the CLI plan and was explicitly struck to keep that stage small. The
real-world trial cannot be honest without it, so build it first.

```
rwmcp --app URL --verify-footprints [--reset-url U] [--state-url U]
```

For each write operation: reset, snapshot state, call the operation once, snapshot again, diff.
Every entity row that changed must appear in `writes`. Anything that changed and is not declared
is a bug found. It defaults to this repo's `/oracle/*` convention and must say plainly, not
silently pass, when an app has no reset or state endpoint — because "cannot check" and "checked
and clean" are the two things that must never look alike.

On a target with no such endpoints, fall back to diffing the database directly, and record in the
writeup which operations were verified that way and which were left as claims.

---

## The procedure, per target

Run it the way a new user would, and **time each step** — the cost of onboarding is a result, not
an overhead.

1. **Get a world model.** `rwmcp --app openapi.json --init` for the skeleton, then the
   `annotate-world-model` skill fills the blocks. Record: how many operations, how many
   annotated, how long, how many needed a human to disambiguate.
2. **`rwmcp --app … --validate`.** Record every finding. This is the first real test of whether
   the check is useful on a document we did not write.
3. **`rwmcp --app … --verify-footprints`.** Record which operations verified, which failed, and
   which could not be checked at all.
4. **Write five wants of increasing depth**, `--order-check` each. Depth 1, 2, 3, and two that
   need a join and a wait.
5. **Plan them.** `rwmcp --app URL --goal "…"`. Record model calls. The benchmark's claim is
   one call at any depth — does it survive a world model with two hundred operations in the
   prompt rather than eleven?
6. **Run them** against the sandbox, with `--yes` and eyes open.
7. **Run them a second time** under the same `--plan-id` and confirm nothing happens twice. This
   is the guarantee most likely to break outside our own app.

---

## The limitations to go looking for

These are hypotheses. Each needs to be confirmed, refuted, or marked untested — a hypothesis we
did not reach is not a limitation we found.

**L1 · No declared semantics, no plan.** The load-bearing one. Quantify it: operations per hour
an agent can annotate, and the share needing human judgement.

**L2 · A footprint is a claim until state-diffed.** `--validate` catches *incoherent* footprints,
never *wrong* ones. On an app that cannot be reset, every footprint stays a claim, and a missing
write token means two operations that conflict get planned as independent — the failure is a
silent race, not an error. Expect this to be the sharpest real limitation.

**L3 · Idempotency needs the app's cooperation.** Our keys are content-addressed and ours; they
stop *us* re-sending within a plan. They do not stop a duplicate if the app has no idempotency
key of its own and we crash between the write and the ledger row. Check what each target
actually offers (Stripe: real keys; Gitea: none) and state honestly which guarantee survives.

**L4 · A wait needs an event.** No webhook and no SSE means falling back to `check` polling, at
whatever `--check-every` costs. Measure it; T13 already shows the shape (3.1 s against 0.7 s) on
an app that *does* have events.

**L5 · Resolvers paginate.** `each(customer())` compiles from whatever the resolver returned. A
real API returns page one. Either `expand_selectors` learns to page, or `each()` over a large
collection is quietly wrong — which is worse than an error. **Check this early; it may be a bug
rather than a limitation.**

**L6 · Read-your-writes.** A resolve immediately after a create may not see it on an eventually
consistent backend. Our scheduler assumes it will.

**L7 · Auth and rate limits.** T12 shows we obey a `Retry-After`. A real API's 401/403/429
handling is not covered by anything measured so far.

**L8 · Prompt size.** Two hundred operations in `world.summary()` is a very different planning
prompt from eleven. Watch for the model call count rising with world size — that would qualify
the headline claim materially, and it is better for us to find it than for a reader to.

---

## What to publish

A limitations section on the results page, and a short honest verdict of this shape:

> rwmcp works on an app that declares what its operations mean. Getting that declaration cost
> **N hours for M operations** on a site we did not write; **K of them could not be verified**
> because the app cannot be reset. With WebMCP present it cost **N′** instead of **N**, because
> WebMCP gave us *[whatever it actually gave us]* and did not give us *[the rest]*. It does not
> work at all on *[the cases we hit]*.

Fill every bracket from an attempt. If a bracket cannot be filled, say the trial did not reach it
rather than reasoning a value into place — the benchmark has already had to retract one published
claim, and the whole value of this project is that its numbers are evidence.
