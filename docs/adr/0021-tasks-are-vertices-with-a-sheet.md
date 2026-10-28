# 0021 — Tasks Are Vertices With A Sheet

> **Languages:** [English](0021-tasks-are-vertices-with-a-sheet.md) (primary) · [中文](0021-tasks-are-vertices-with-a-sheet.zh-CN.md)

**Status:** Accepted and LANDED (2026-10-27). Doors, tools and the turn's opening
listing ship with it.
**Date:** 2026-10-27

## Context

The fixed tool list (§ADR-0004) names three groups: memory, skills, tasks. Two
existed. The third had no storage anywhere in the stack — `gravity` is glue with no
store, `prism` owns identity only, `aura` is the realm — so a task had no home but
the memory core, and nothing in k10r knew what one was.

The shape was not obvious, and the first two proposals here were wrong.

**A row with a state face** (a task table, one row per task, an index on state) is
what a conventional task manager looks like. It answers "what is open" perfectly and
cannot answer "what does this block" — and dependencies, parents, tags and
cross-references ARE what a task is. Those are edges.

**A separate collection for tasks** (ns 44 as a vertex table) would have given them
edges, at the price of a second vertex type: every edge writer, every resolver and
every deletion path would have to learn a second kind of endpoint.

## Decision

**1. A task is an ordinary vertex of the fact graph.** `kind = "task"` in ns 36, the
same collection as every other node and the same one skills use. A dependency is
`link`, a parent is `link`, a tag is `link`, a reference to an issue or a file is
`link` — and a fact about a task is a fact:

```json
["task-12", "task", {}, "depends_on", {}, "task-9", "task", {}]
```

Nothing new was needed for any of that: the resolver interns a node of a named type
on demand, which is exactly how `skill` works (ADR-0015). The task's NAME is its
identity, and a fact naming a `task` that does not exist yet creates it — so being
mentioned as work IS being work.

**2. What changes about a task lives in a side sheet (ns 44).** Same `EntityKey`, the
`NodeVector` pattern (ADR-0019): byte-identical key, no join table, one batch.

```text
ns 44  TaskSheet  [scope][node_id]
         state       open | doing | blocked | done     (a closed set, refused if not)
         next        the next action, the producer's words
         created_at  the caller's clock when it was opened
         updated_at  the caller's clock at the last move
         #[ok_index(by_state { fields(state) })]  ->  [state][scope][node_id]
```

Three reasons this is a sheet and not props or columns:

- **Presence is the meaning.** The sheet exists iff the node is a task. The vertex
  keeps the same fields for every kind — the alternative gives every node in the
  corpus a task's columns, which is the mistake ADR-0019 undid for the vector.
- **The state is enumerated, not retrieved.** "Which are open" is an exact question;
  a face answers it, and okm has no index over dynamic props. A state in props would
  mean scanning every task and filtering in memory, forever.
- **The lifecycles differ.** The vertex lives as long as facts mention it; the sheet
  lives as long as the work does. One can be retired without the other, and neither
  write touches the other's row.

**3. Three doors, because three things are not edges.**

```text
POST /tasks/{scope}                     open (idempotent on the name)
POST /tasks/{scope}/{node_id}/move      state and/or next; absent = left alone
GET  /tasks/{scope}?state=open          the enumeration
```

There is deliberately NO dependency door, parent door or tag door: those are facts,
and a second vocabulary for one graph is a second thing to keep in step. `/move`
takes a verb like `/touch` and `/promote` — one door shape for "do something to this
node", so a caller never has to know which HTTP verb an action chose.

**4. The states are a closed set.** `open`, `doing`, `blocked`, `done`; anything else
is refused at the door. A task stored under a state no listing looks for is a task
nobody finds — the failure is silent, and silence is what a typo produces.

**5. A turn starts by knowing what is open.** `gravity` lists open tasks and puts
them in front of the model as one system block before the new message. Best effort:
a store that cannot answer does not fail the turn, and the model can still ask with
`list_tasks`.

## Consequences

- **The agent can drive itself.** Until this, every turn started from zero: it knew
  what was said and not what was still owed. That is the gap the third tool group
  existed to close.
- **Cross-session work is one graph.** A task mentioned in a conversation, a
  dependency written by a fact, a tag, a parent and a note all live in one place —
  and a recall about a task finds its facts, because they ARE facts.
- **`state` is not a fact.** Moving a task overwrites the sheet and indexes nothing
  else; the graph does not accumulate states, so "how long was it blocked" is not
  answerable by history. If that is ever wanted it is a new edge kind, not a state
  column with a log.
- **`next` is free text the store never parses.** Same rule as skill steps
  (ADR-0012 §2): the producer owns the shape.
- **A task has no owner field.** Ownership is a person node and an edge when it
  matters, which is what keeps the sheet from growing into a task tracker.

## Related records

ADR-0004 (the fixed tool list — the gaps this closes), ADR-0015 (the skill node: the
kind-entity pattern tasks follow), ADR-0019 (the side table this sheet copies), okm
ADR-0041 (the pair-addressable faces that make the identity check one seek), okm
ADR-0042 (layout versions are stamps — why the entity row's counters could move to
the front instead of being appended).