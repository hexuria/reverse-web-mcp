---
name: write-wants
description: Turn a goal into rwmcp wants — the predicates that make up an Intent. Use when writing a task TOML, hand-writing an Intent to embed the engine, debugging a plan that came out wrong, or reproducing what the runtime planner would have emitted.
---

# Writing wants

A **want** is one fact that must be true when the work is done. A list of wants plus a goal is
an `Intent`; the compiler turns that into the plan. You never write actions, an order, or a
loop — those are derived.

At runtime the planner emits these from `PLANNER_SYSTEM` in `crates/rwmcp/src/planner.rs`. This
skill is the same job done by hand, so keep the two in agreement: if you change the rules here,
change them there.

What you write is a **`.wants` file**: one want per line, `#` for a comment, blank lines ignored.
That is what `rwmcp --wants` reads, and `-` reads the same thing from stdin. Saving one with
`--save NAME` turns it into a recipe: the same wants with `$placeholders` where the values change,
which then runs with `--set name=value` and costs no model calls ever again.

## The shape

```
entity(arg=value, ...).field=value
entity(arg=value, ...).exists
```

- Strings use single quotes: `customer(name='Acme')`.
- Lists use brackets: `report(invoices=[a,b])`.
- A nested `entity(...)` as a value is a **reference** — the compiler resolves it to a node and
  wires the edge: `invoice(customer=customer(name='Acme'))`.
- `.exists` for "this must be there"; a named field for a state: `.status='sent'`.

## The rules

1. **One want per fact that must hold at the end.** Not per step.
2. **Never want a read or a lookup.** "list the customers" is not a want; the compiler adds the
   lookup because something downstream needs the id.
3. **Never want an entity that is found rather than made.** `customer(name='Acme').exists` is
   rejected (`WantsAnEntityThatAlreadyExists`) because the world model has a *resolver* for
   customers. Refer to it inside another predicate instead.
4. **Identify things by what you know, never by an id you invented.** `invoice(customer=customer(name='Acme'))`, not `invoice(id=7)`.
5. **Do not imply order.** Order comes from the footprints. Listing the report first must
   produce the same plan as listing it last — `rwmcp --wants w.wants --order-check` proves it. If
   it does not, that is a compiler bug worth reporting, not something to work around by
   reordering.
6. **No variables.** `$name` in a want is rejected (`VariableInWant`). Wants are concrete.
7. **Still want a fact that depends on the outside world.** `invoice(...).receipt_sent=true`
   even though a payment must land first; the engine inserts the wait. A fork is for ambiguity,
   never for waiting or retrying.
8. **Every field you name must be in `x-reverse-webmcp-entities`.** Otherwise `UnknownField`.

## Many rows: each and all

`each(...)` fans a want out, one per element. `all(X)` collects X's expansion into one list.
Choose deliberately — this is the single easiest thing to get wrong.

```
invoice(customer=each([customer(name='Acme'),customer(name='Globex')])).status='sent'
    → two wants, two lanes

report(invoices=[invoice(customer=each([...]))]).exists
    → ONE REPORT PER invoice

report(invoices=[all(invoice(customer=each([...])))]).exists
    → ONE report over every invoice
```

Selectors, for when you will not name every row: `each(customer(name_prefix='Bulk '))` or
`each(customer())` for all of them. These are **expanded by a read against the app** before
compiling (`expand_selectors`), so keys come out identical to the written-out form. An
unexpanded selector is a lint error, never a silent single lane.

Two cautions the language does not catch for you in every version: keep every `each(...)` in one
want the same length, and make sure a selector actually matches something — an empty expansion
means zero wants, which compiles to an empty plan.

## Forks

A fork is what happens when a name matches zero or several rows. You do not write one into a
`.wants` file: the world model declares it per operation, and the ambiguity is discovered while
running. Keep the want as it is.

When one fires, the run stops with exit code **11** and the evidence attached — the rows it could
not choose between. Resolve it in one of two ways:

```
rwmcp --app URL --wants w.wants --run --yes --answer "customer(name='Acme')=>customer(id=11)"
rwmcp --app URL --wants w.wants --run --yes --answer-with-model
```

`--answer OLD=>NEW` rewrites that text in every want and costs nothing; `--answer-with-model`
spends one call letting the model choose. Either way only the ambiguous want changes, so every
other want keeps its text, keeps its content-addressed key, and is not done twice.

When embedding the engine, a fork declared in the world model can carry `default = "lowest_id"`,
in which case the scheduler resolves it alone. Only `result.count != 1` and `result.count == 0`
are understood.

## Check before you ship

Three commands, no Rust:

```
rwmcp --app URL --wants w.wants                 lint, compile, print the plan, change nothing
rwmcp --app URL --wants w.wants --order-check   compile forwards and backwards, prove they match
rwmcp --app URL --validate                      check the world model itself
```

The first parses every want, checks entities and fields, rejects variables and unexpanded
selectors, tries a full compile, and prints the plan. It exits **0** when the plan is good and
**10** when the wants are not, with `--json` giving `{"ok":false,"code":"wants_rejected",...}` and
a `code` on every error.

A clean lint means the intent is *compilable*, not that it is *right*. For right, read the plan it
printed and confirm three things: the node count matches the number of effects you expect; every
join waits for every branch it should; and nothing waits on something it does not need.

`--order-check` is the fourth thing, and the one nobody does by hand: it compiles the wants, then
compiles them reversed, and diffs the two graphs. A plan that changes is a plan that depends on
the order you happened to type in.

## Worked example

Goal: *invoice Acme, Globex and Initech, send each, then one report over all three.*

```
invoice(customer=customer(name='Acme')).exists
invoice(customer=customer(name='Acme')).status='sent'
invoice(customer=customer(name='Globex')).exists
invoice(customer=customer(name='Globex')).status='sent'
invoice(customer=customer(name='Initech')).exists
invoice(customer=customer(name='Initech')).status='sent'
report(invoices=[invoice(customer=customer(name='Acme')),invoice(customer=customer(name='Globex')),invoice(customer=customer(name='Initech'))]).exists
```

Nine nodes: a lookup, a create and a send per customer, plus the report. Depth 4, three lanes
wide, no edge between lanes. Written with `each` and `all` it is three wants and the same keys.

## Lint errors and what they mean

| error | cause |
|---|---|
| `Unparseable` | syntax: unbalanced parens, a missing quote |
| `VariableInWant` | a `$name` survived; resolve it first |
| `UnknownEntity` / `UnknownField` | not in `x-reverse-webmcp-entities` |
| `WantsAnEntityThatAlreadyExists` | you wanted a found entity; nest it instead |
| `UnexpandedSelector` | `each(customer(...))` never got expanded against the app |
| `Unsatisfiable` | nothing in the world model has a matching `post` — wrong field, wrong entity, or the annotation is missing |
| `MatchesNothing` | a selector expanded to nothing, so the want asks for no work |
| `Empty` | no wants at all |

Every one of these has a stable `code` in `--json` output (`unparseable`, `variable_in_want`,
`unknown_entity`, `unknown_field`, `wants_an_entity_that_already_exists`, `unsatisfiable`, `empty`,
`unexpanded_selector`, `matches_nothing`), so branch on that rather than on the prose.
