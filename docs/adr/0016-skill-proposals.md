# 0016 — Proposals Are Rows, and Promotion Is a Declaration

> **Languages:** [English](0016-skill-proposals.md) (primary) · [中文](0016-skill-proposals.zh-CN.md)

**Status:** Accepted (2026-10-26) — landed the same day: `mem-core`
(`proposals.rs`), three doors (`POST /skills/{scope}/propose`,
`GET /skills/{scope}/proposals`, `POST /skills/{scope}/{node_id}/promote`), and
gravity's half (the turn reports a composition when it walked two different code
rows).
**Date:** 2026-10-26

## Context

ADR-0015 landed the read side of the skill loop: a skill is a node, `resolve`
lists candidates, `invoke_skill` walks a step. The write side did not exist —
skills were declared by a human with curl, and nothing in a real turn ever
suggested one. The loop therefore had a resolver with nothing to resolve in
production.

gravity ADR-0003 §5 already fixed the shape of the answer: *a recurring
composition is PROPOSED for promotion, never self-parented*. What was missing is
where a proposal lives, who writes it, and what "promotion" is.

## Decision

**1. A proposal is an entity node of kind `skill_proposal`, in the same scope as
the facts and the skills.**

Same graph, same store, same scope — no second collection, no side table. The
kind is what keeps it out of sight: `resolve_skills` keeps only the `skill` kind
(ADR-0015 §4), so a proposal is invisible to the model until a human promotes it.
That is the whole mechanism that makes "proposed, never self-parented" true rather
than decorative: the model cannot choose a draft, because a draft is not a
candidate.

**2. gravity is the only writer, because gravity is the only observer.**

A turn is where an ordered run of code rows actually happens — gravity dispatched
each step, so gravity knows the order. It reports the composition at finish time:
`{key, steps, ts_ms}`, where `key` is the code ids in the order they ran (`4+7`)
and `steps` is the same JSON document a skill's `steps` takes.

The key is the WALKER's spelling, not something k10r derives, because the store
does not parse `steps` (ADR-0015 §3: the producer owns the shape). Making the
store parse it here would be exactly the second schema that decision avoided.

**3. k10r counts; nobody promotes.**

`POST /skills/{scope}/propose` is idempotent on `key`: the same run again is the
same row with `count + 1`. A different order is a different composition (`4+7` ≠
`7+4`), which is why the key is ordered rather than a set.

`count` is a PROP, not a weight. The weight system's counters ride EDGE rows
(ADR-0005) and a proposal is a node; more importantly, counting here answers a
different question — *did this run repeat* — which is what a human reviews. A
weight face would rank by retrieval, which is the wrong question for a queue.

Two rows walked in one turn is the minimum for a composition; one row twice is a
retry. gravity decides that (it has the run), and reports nothing for it.

**4. Promotion is a declaration, not a rename.**

`POST /skills/{scope}/{node_id}/promote {name, title?, content?}` reads the
proposal's steps, declares a skill with the HUMAN's name, title and content, and
records `promoted_to` on the proposal.

It is deliberately not "re-label this node as a skill": the graph has no write
path for re-labelling an entity (identity is the name, and re-labelling would
change it), and the record of *what the run suggested* would be lost. Two rows —
the proposal with its count and its outcome, the skill with its name — are the
honest shape. A second promotion of the same row is refused by name, because a
proposal that produced two skills is a human error worth reporting, not a
silent merge.

**5. The queue is a scan and a human's door.**

`GET /skills/{scope}/proposals?min_count=N` walks the proposal kind, drops
promoted rows, and orders by count. It is a scan rather than a declared weight
face: the queue is small by construction (one row per distinct composition),
nothing on the model's path reads it, and repetition ordering is a sort, not a
retrieval.

## Consequences

- **The write half of the loop exists.** A turn that did something worth
  repeating leaves a row; a human reads the queue and names the skill; the next
  turn's `resolve_skills` can offer it. No step of that path needs a new tool
  name — the fixed list is unchanged (gravity ADR-0004).
- **Nothing is auto-promoted, so a bad turn cannot poison the skill corpus.** A
  proposal is capped by review, not by cleverness.
- **A proposal carries the descriptions of the steps it names** (gravity records
  each code row's `description` as it walks), so a reviewer sees what each step
  does without opening every code row.
- **Two things remain open on this path**: (a) a human-facing verb — today the
  queue is read with `curl`, and a `promote` needs a hand-written POST; (b) the
  count grows without ever decaying, so a composition repeated in one noisy week
  looks the same as one repeated steadily for a month. The weight system's decay
  is edge-shaped and cannot be borrowed as-is.
- **Nothing here touches retrieval ranking.** A promoted skill is an ordinary
  skill node from the moment it lands; the count that suggested it stays on the
  proposal.

## Related records

ADR-0015 (the skill node and `resolve`), gravity ADR-0003 §5 (proposed, never
self-parented), gravity ADR-0004 (the fixed tool list — unchanged by this),
ADR-0005 (weights are edge-shaped), ADR-0012 §2/§3 (the code row: producer-owned
shape, language never defaults).