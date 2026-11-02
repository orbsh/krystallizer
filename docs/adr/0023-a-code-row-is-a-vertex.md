# 0023 — A Code Row Is A Vertex

> **Languages:** [English](0023-a-code-row-is-a-vertex.md) (primary) · [中文](0023-a-code-row-is-a-vertex.zh-CN.md)

**Status:** Accepted and LANDED (2026-10-31). Doors, gates and tests ship with it.
**Date:** 2026-10-31
**Amends:** ADR-0012 (the code store's identity).

## Context

gravity ADR-0003 §3 says completion is written at the CALL SITE, never judged by the
model: before dispatching a step, gravity must know which open tasks that code row
finishes. The binding it wants is a fact —

```json
["run the migration", "task", {}, "finishes_on", {}, "code-7", "code", {}]
```

— and a fact reaches ENTITIES. A code row was not one: ADR-0012 gave it a bare
surrogate id in ns 35, addressed by number and by nothing else. So the binding had
nowhere to land, and every way around that looked like a bargain with the address
space:

- **A second endpoint kind** (`Endpoint::Code(u64)` in mem-core): one consumer's
  relation would have taught the resolver, the renderer, the deletion paths and every
  endpoint decoder a third vocabulary.
- **Bind to the skill instead** (skills ARE vertices): the step index has no home — a
  multi-step skill would close a task on step 1 because step 3 was meant.
- **A field on the task sheet**: a silent layout change to a table that already has
  rows, and a relation demoted to a column.
- **A third table** (`[scope][node_id] → code_id`): k10r's idiom, no migration, and
  still a parallel address space for something the graph already knows how to hold.

Each was a way to avoid moving code into the graph. The move is cheaper than any of
them, and it closes the question instead of parking it.

## Decision

**1. A code row is an ordinary vertex.** `kind = "code"` in ns 36, the same collection
every other node lives in — tasks and skills included (ADR-0021, ADR-0015). The vertex
is named `code-{id}`, where `{id}` is the code id: the graph addresses nodes by name,
the caller addresses code by number, and the name carries nothing but the number.

**2. `code_id` does not change.** A skill's `steps[].code_id` still holds it, gravity
still asks for it by number, and a row keeps the id it was written with. What changed is
where the number lives: it is on a node now, so facts can point at it.

**3. The payload moves beside the vertex (ns 35, sheet).** Key `[scope][code_id]`:
language, description, parameters, source. Presence is the meaning — a node with no
sheet is not code — and the payload stays out of the vertex's props, because a script's
source is not a dynamic segment every reader of the entity table should decode. The id
allocator is the same watermark as before: the highest `code_id` ever written, so a
deleted row does not free its id.

**4. The doors are scoped.** `POST /code/{scope}` writes and answers the id,
`GET /code/{scope}/{code_id}` reads, and the export/import tables carry the sheet as
`code` with the rest of the corpus. Scope is not decoration: the vertex lives in that
scope's graph, so a row without a scope would be a node with nowhere to be.

**5. The binding is stated at open and written as a fact.**

```text
POST /tasks/{scope}  { "name": ..., "finishes_on": [code_id, ...] }
```

The door writes one `finishes_on` edge per code id — an ordinary fact, no new verb and
no new table. Three manners matter:

- A code id nobody delivered is REFUSED by name (400). Binding to nothing is a caller
  bug, and a phantom `code-99` vertex would be a worse answer than an error.
- Re-stating a binding is idempotent (the ingest answers `Bumped`), and opening an
  existing task still never moves its state — which is what lets a later turn bind a
  task it opened in an earlier one.
- A binding is ADDED, never rewritten. Removing one is an edge withdrawal, and no door
  pretends to do it.

**6. k10r does not close anything.** The listing reports `finishes_on: [code_id…]` per
task — resolved from the edges, never stored twice — and gravity closes the task at the
call site that succeeded. The store answers; the executor writes.

## Why Not

- **`Endpoint::Code`**: see Context. A new endpoint kind is a change to the graph's
  address space, paid by every reader so one relation can be written.
- **Source in the vertex's props**: props are decoded by everyone who reads a node; a
  script is payload, and the sheet is where payload goes (ADR-0019's rule for the
  vector side table, applied again).
- **A `name` with meaning** (a slug of the description): a rename would break the edges
  that name it, and the number already IS the identity.

## Consequences

- The entity table gains one row per code row, and that row is in every graph read and
  every backup. A bundle's entity count is "facts' nodes + code vertices", which is one
  thing to remember when reading a restore report.
- `graph --code vertex` is now a traversal, not a lookup: a fact can say what code
  belongs to what, and gravity's Phase 5 counters (how often a step ran) have a place
  to land — on the code row that actually ran, not only on the skill.
- A code row is created by two writes (vertex, then sheet). The order is deliberate: a
  failure can leave an inert node, never a sheet nobody can reach.
- ADR-0012's reason for a nameless identity stands — nothing caches a name, the caller
  re-reads the row every call — but the name is now the graph's, synthetic and derived,
  not a field a caller proposes.