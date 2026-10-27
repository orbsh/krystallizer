# 0018 — Counting Happens at the Call, and the Model Creates Skills

> **Languages:** [English](0018-counts-and-create.md) (primary) · [中文](0018-counts-and-create.zh-CN.md)

**Status:** Accepted (2026-10-26) — landed the same day: `touch_skill` + the touch
door, recall's count made best-effort, and gravity's fifth tool `create_skill`.
**Date:** 2026-10-26

## Context

Two gaps, found by reading the code rather than the plan.

**Nothing counted a skill.** The weight system's counters ride EDGE rows
(ADR-0005), and a skill is a NODE — so `invoke_skill` left no trace at all. "How
often does this skill get used, and does it still work" was unanswerable, and
ADR-0016's proposal queue could only be fed by gravity's turn-end report of a
whole composition, never by the calls themselves.

**Counting was load-bearing where it should not be.** Recall bumped the edge's
`read_count` inline and propagated the error (`?`) — a failed counter write failed
the read. A by-product of serving was holding the answer hostage.

**A skill could be run but not written.** `commit_memory` is the model's act: it
decides, mid-turn, that something deserves to be kept. Skills had no counterpart
— they arrived only from a human's curl or from a statistical proposal. Memory and
skill were two different kinds of thing for the same kind of judgement.

## Decision

**1. Two records per invocation, at the two moments the caller knows something.**

`attempt` when the call is dispatched (outcome unknown), `success` when the answer
comes back. The store derives nothing: `calls` and `ok` are props on the node, and
their difference is the failure count. Recording the attempt BEFORE the outcome is
known is what makes "tried and broke" a visible state — the alternative (one
record carrying an outcome) can only be written by a call that returned, which is
exactly the case that needs no diagnosis.

**2. Node counters and edge counters answer different questions.**

A node counter is usage telemetry: is this thing alive, is it worth keeping, has
it been tried and abandoned. An edge counter is association weight: how strongly
do these two belong together, which is a ranking input. Neither substitutes for
the other, and the fact that both are "counts" is a coincidence of vocabulary.

**3. Counting never fails what it counts.**

Recall's bump answers the fact as it stands when the count cannot be written; the
touch door's answer is a receipt, and gravity ignores it. The counters are props
merged into the dynamic segment: no declared field, no new column, no re-label. The
bump is still a whole-document write under okm ADR-0035 — which is why the vector
has to leave the row (ADR-0019), so a count never re-encodes an embedding it cannot
change.

**4. The model creates skills, symmetrically with memory.**

`create_skill` is the fifth fixed tool (gravity ADR-0004): name (the identity),
title, content, and the three step-shaped parts. Declaring twice merges
(ADR-0015). Nothing else creates a skill — the statistical proposal path stays,
because it covers what the model did NOT notice, and it can still only PROPOSE.

**5. The step-shaped parts cross the boundary as JSON text, produced there.**

The model writes arrays; k10r stores TEXT and parses none of it (ADR-0012 §2). The
conversion belongs to the caller, which owns the wire — teaching the store a
model's habits is how a store acquires a second schema.

## Consequences

- **The corpus can grow from inside a conversation.** A procedure worked out in a
  turn is a skill before the turn ends; nothing waits for a batch pass.
- **Two counts are now visible per skill** (`calls`, `ok`) plus the clock of the
  last one. What they do NOT say is when: they are totals, so "hot for a week" and
  "hot for a month" look alike. Decay is edge-shaped (ADR-0005) and cannot be
  borrowed as-is.
- **A bump is a whole-document write.** Until the vector leaves the row (ADR-0019)
  it also re-encodes the embedding; only after that is a count genuinely a small
  write.
- **The counters are unbounded and never pruned.** That is acceptable while the
  only reader is a human or a review pass; it becomes a problem when something
  ranks by them, which is the point at which a half-life has to be chosen.
- **ADR-0016's turn-end filing is now redundant as the ONLY writer.** The counts
  at the call site already carry repetition, and the model can create directly.
  Re-cutting that record is the next step, not this one: what a proposal is FOR
  changes when the counts exist.

## Related records

ADR-0005 (weights are edge-shaped), ADR-0012 §2 (the producer owns the shape),
ADR-0015 (the skill node), ADR-0016 (proposals), ADR-0017 (tail operations),
gravity ADR-0004 (the fixed tool list, now five).