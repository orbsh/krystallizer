# ADR-0005: Weight System — Counters on the Edge Row, Read-Write Quadrants, Tool Index

**Status**: Landed 2026-10-19 (the counters ride the edge row; the delta-append table survives only as the multi-writer escape)
**Date**: 2026-09-08
**Chinese**: [0005-weight-system.zh-CN.md](0005-weight-system.zh-CN.md)

## Context

The graph-memory design assigns every fact edge a system weight metadata
layer (distinct from business properties): read_count, write_count,
tool_invoke_count, last_read, last_write. The read-write ratio defines four
quadrants (core / deep-water / noise / cold), and retrieval order is
weight-ordered Top-K (progressive loading).

Two engineering questions: how to count without read-modify-write
contention, and how to serve a weight-ordered Top-K without full scans.

## Decision

**Counting: the edge ROW is the count key.** `read_count` / `write_count` /
`tool_count` / `last_read` / `last_write` are declared hot fields of
`FactEdge` (ns 37, payload layout v2), with the folded `weight` beside them.
One identity, one write — and because okm's declared faces are derived from
the row, a bump rewrites one row and everything derived from it follows.

- **Who bumps what.** `write_count` is the fact's own assertion: `ingest`
  bumps it when the fact links or wins the adjudication (a dropped fact is
  not a write). `read_count` is a served recall — `recall` / `expand` bump
  it only when the caller's clock rides the call (`ts_ms`), because a read
  nobody stamped is not a read and an absent timestamp must not invent
  decay. `tool_count` is a caller-reported tool consult
  (`POST /memories/{scope}/edges/{id}/touch`): k10r cannot see a tool call from
  inside, so the consumer that used the fact reports it.
- **read_count and write_count stay physically separate** — the atomic-fact
  append model guarantees read/write independence, which is what makes the
  2D quadrant classification stable at the storage layer.
- **Fold points, not background passes.** A touch rewrites the row in one
  batch. The decay the gap accrued is charged AT that fold, measured against
  the PREVIOUS access, and the same fold then re-bases the clock, so the gap
  is charged once rather than twice. The order is load-bearing: fold against
  the previous access, THEN bump the clock and the counter — folding after
  the clock update makes `time_decay` permanently zero, a mistake invisible
  in the numbers (everything simply sorts by raw counts). The delta-append
  table stays the multi-writer ESCAPE: it folds into these same fields, so
  switching to it changes no schema, and its key layout is the one thing to
  fix when it lands.

**Weight formula** (from the wiki, including the tool index):

```
weight = read_score + write_score + tool_score + rarity_bonus - time_decay

tool_score  = log(1 + tool_invoke_count) × 1.5
read_score  = log(1 + read_count)
write_score = log(1 + write_count) × 0.5
rarity_bonus = 1.0 if read_count > R AND write_count < W else 0
time_decay  = days_since_last_access × 0.01
```

Deep-water bonus (high read, low write) is load-bearing: write-threshold
penalties structurally punish high-entry-cost expert knowledge; the
rarity_bonus corrects that.

- **The bucket.** The stored value is the formula in MILLI-units (`× 1000`,
  rounded, clamped at 0): a small float needs a stable ordered integer to be
  compared by integer, and 0.001 is finer than the per-day decay step, so the
  bucket never reorders two facts the formula would order differently. The
  coefficients are named constants in `mem-core/src/graph.rs` (read 1.0,
  write 0.5, tool 1.5, rarity 1.0, decay 0.01/day, deep-water floors
  read > 3 and write < 3). They are the formula, not deployment knobs; a
  policy layer would be a config node, and nothing reads one yet.
- **Ordering.** `recall` returns the seeds in similarity order and the facts
  ranked by weight, highest first (stable, so equal weights keep the seed
  order that pulled them in). Landing stays ADR-0003's; weight is this
  ADR's.

**Top-K: no weight face is declared, and no index exists.** `W:{weight}:
{edge_id}` would answer "the store's heaviest edges", not "this user's
heaviest facts": edges carry no `scope` segment (ADR-0010 §1 — ownership
rides the endpoints). A scope-wide Top-K is therefore a PROJECTION table
keyed `[scope][weight][edge_id]`, written on the same touch — a follow-up
named with the read that consumes it (Phase 4's progressive loading), not a
face to pre-pay for. Recall's ranking needs none of it: the set it ranks is
already in hand. Two further reasons the face stays undeclared: a weight
change rewrites the row anyway, so the face would be maintained for a reader
that does not exist; and okm's document overwrite does not delete an entry
keyed by the OLD value, so every bump would owe an explicit delete for a
query nobody makes.

**Erratum (2026-10-20): the scope-wide Top-K IS a declared face, and the paragraph
above was wrong about why it could not be.** Two premises turned out adjustable. The
scope does not have to ride only the endpoints — the edge row now carries a
denormalized `scope` (a copy of its source endpoint's, written once), so a face CAN
be probed per scope. And the entry does not dangle on a weight bump: okm's
`link_into` unlinks what the row used to derive, a rewrite is one batch, and a test
pins one fact to one ranking entry. So the face exists:
`by_scope_weight { fields(scope, weight) }`, keyed `[scope][weight][edge_id]`, and
`top_by_weight(scope, k)` probes it and decodes k rows — O(k rows) + O(scope keys),
no projection table to maintain beside the row. The bare weight face remains
undeclared, and so does `by_confidence`: neither has a reader.

## Consequences

- Today's ranking is an in-memory sort over the set recall already fetched —
  no index, no full-table job, no second engine.
- Cold-tier work (Phase 5+ fallout) and progressive loading want the same
  scope-keyed projection; when either arrives, one table serves both, and the
  decision above names its shape.
- Time decay makes a fact's weight non-monotonic between folds, which is
  acceptable: nothing derived is kept in step today, and the ordering
  tolerates day-scale staleness (same eventual-consistency posture as skill
  boundary reconsolidation).

## History

**The 2026-09-08 draft, retired when the counting half landed (2026-10-19).**
It counted by appending deltas — `put edge:{src}:{label}:{dst}/read/{ts} →
+1`, folded in the background into `put edge:{src}:{label}:{dst}/count/read
→ N` — and served Top-K from a declared weight secondary index `W:{weight}:
{edge_id}`, writing the new key and deleting the old (tombstone) per weight
change.

- The delta keys were wrong twice over: they were spelled in a retired
  LAYER (`edge:{src}:{label}:{dst}` is not a k10r key shape — an edge's
  identity is `[edge_id]`, with endpoints and the allocated `predicate_id`
  carried as refs), and the delta table itself buys nothing for a single
  serial writer, which is what the store is.
- The count key needed no side table either: the counters are fields of the
  row they count, so a fold is one row rewrite.
- The declared weight face died for the reason the Top-K section states: it
  addresses the whole store rather than one scope, and it would have been
  maintained for readers that do not exist. The tombstone rule went with it —
  there is no face left to dangle.