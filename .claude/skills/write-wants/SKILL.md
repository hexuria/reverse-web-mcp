---
name: write-wants
description: Turn a goal into rwmcp wants — the predicates that make up an Intent. Use when writing a task TOML, hand-writing an Intent to embed the engine, debugging a plan that came out wrong, or reproducing what the runtime planner would have emitted.
---

# Writing wants

A **want** is one fact that must be true when the work is done. A list of wants plus a goal is
an `Intent`; the compiler turns that into the plan. You never write actions, an order, or a
loop — those are derived.

At runtime the planner emits these from `PLANNER_SYSTEM` in `crates/bench/src/planner.rs`. This
skill is the same job done by hand, so keep the two in agreement: if you change the rules here,
change them there.

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
   produce the same plan as listing it last — if it does not, that is a compiler bug worth
   reporting, not something to work around by reordering.
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

When a name may match zero or several rows, keep the want as it is and declare a fork:

```toml
[[forks]]
when = "result.count != 1"
ask = "Two customers are named Acme. Which one?"
default = "lowest_id"     # omit to stop and ask instead of resolving
```

Only `result.count != 1` and `result.count == 0` are understood. With a `default`, the scheduler
resolves it alone; without one, the run ends `need_think` with the evidence attached.

## Check before you ship

Run `lint(&intent, &world, &opts)` — it parses every want, checks entities and fields, rejects
variables and unexpanded selectors, and then tries a full compile. An empty error list means the
intent is compilable, not that it is *right*.

For right, read the plan:

```rust
println!("{}", plan.render());
```

Confirm three things: the node count matches the number of effects you expect; every join waits
for every branch it should; and nothing waits on something it does not need. Then compile the
same wants **in a different order** and diff — the plans must be identical.

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
| `Empty` | no wants at all |
