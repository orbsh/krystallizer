# ADR-0005: Weight System — Delta Counts, Read-Write Quadrants, Tool Index

**Status**: Accepted (the counting half LANDED 2026-10-19 — see Updates; the delta table waits for the multi-writer switch)
**Date**: 2026-09-08
**Chinese**: [0005-weight-system.zh-CN.md](0005-weight-system.zh-CN.md)

## Context

The graph-memory design assigns every fact edge a system weight metadata
layer (distinct from business properties): read_count, write_count,
tool_invoke_count, last_read, last_write. The read-write ratio defines four
quadrants (core / deep-water / noise / cold), and retrieval order is
weight-ordered Top-K (progressive loading).

Two engineering questions: how to count without read-modify-write
contention, and how to serve Top-K without full scans.

## Decision

**Counting: delta-append keys, fold to a count key in the background.**

```
put edge:{src}:{label}:{dst}/read/{ts}  → +1        # append-first: this path stays an append
put edge:{src}:{label}:{dst}/count/read → N         # folded, whole-value put
```

The key on the left is spelled in an earlier pass's notation: `edge:{src}:
{label}:{dst}` is a **former LAYER**, not a layout to restore — the graph's
edges are an okm edge collection keyed by `[edge_id]` with the endpoints and the
kind carried as refs and an allocated `predicate_id` (ADR-0010 §1), so the two
counting keys land on that identity when this system is built (Phase 5). What
this decision fixes is unchanged: count by appending deltas, fold to a
whole-value count key in the background, and keep the layout
delta-compatible from day one so the multi-writer switch changes no schema.

- Reads merge the delta chain for the current count; low-peak fold writes
  the merged value to the count key.
- The single-process start can use direct RMW on the count key (per the
  wiki's concurrency-model criterion: same WriteBatch, serial updates —
  delta machinery buys nothing yet). **The key layout is delta-compatible
  from day one**, so the multi-agent switch (SlateDB+S3 shared truth,
  concurrent writers) changes no schema.
- read_count and write_count are physically separate objects — the
  atomic-fact append model guarantees read/write independence, which is
  what makes the 2D quadrant classification stable at the storage layer.

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

**Top-K: weight secondary index `W:{weight}:{edge_id}`.**

- Weight changes write the new key + delete the old (tombstone) in one
  WriteBatch — memory weight updates are low-frequency batched flushes, so
  tombstone overhead is negligible (standard put+delete per the wiki's
  index-update criterion).
- Progressive loading reads the index in descending order, fetches back
  edge payloads, stops at K.
- Weight is stored at the precision the index needs (bucketed): recompute
  order must match index order, so the indexed value is the same value the
  formula produces at fold time.

## Consequences

- Hot path (search) never scans all edges; cold data identification
  (noise/cold quadrants) is an index range read, not a full-table job.
- The S3 cold-tier fallout (Phase 5+) reads the same index from the bottom
  of the weight range; no separate "cold data" bookkeeping.
- Time decay makes weight non-monotonic between folds — acceptable: the
  index refreshes at fold cadence, and retrieval ordering tolerates
  day-scale staleness (same eventual-consistency posture as skill boundary
  reconsolidation).

## Updates

**2026-10-19 — the counting half landed, in okm's terms.** The counter keys
are the edge ROW, not a side table: `read_count` / `write_count` /
`tool_count` / `last_read` / `last_write` are declared hot fields of
`FactEdge` (ns 37, payload layout v2), and `weight` is the folded field
beside them. One identity, one write.

- **Who bumps what.** `write_count` is the fact's own assertion: `ingest`
  bumps it when a fact links or wins the adjudication (a dropped fact is not
  a write). `read_count` is a served recall — `recall`/`expand` bump it only
  when the caller's clock rides the call (`ts_ms`), because a read nobody
  stamped is not a read and an absent timestamp must not invent decay.
  `tool_count` is a caller-reported tool consult
  (`POST /facts/{scope}/edges/{id}/touch`): k10r cannot see a tool call from
  inside, so the consumer that used the fact reports it.
- **Fold points, not background passes.** The single-process start pays a
  direct RMW per access — this ADR's own allowance. A touch rewrites the row
  in one batch. The decay the gap accrued is charged AT that fold, measured
  against the PREVIOUS access, and the same fold re-bases the clock, so it is
  charged once rather than twice. The delta-append table stays the
  multi-writer escape; it folds into these same fields, so switching to it
  changes no schema — its key layout is the one thing to fix when it lands.
- **The bucket.** The stored value is the formula in MILLI-units (`× 1000`,
  rounded, clamped at 0): a small float needs a stable ordered integer for
  the index, and 0.001 is finer than the per-day decay step, so the bucket
  never reorders two facts the formula would order differently. The
  coefficients are named constants in `mem-core/src/graph.rs` (read 1.0,
  write 0.5, tool 1.5, rarity 1.0, decay 0.01/day, deep-water floors
  read > 3 and write < 3). They are the formula, not deployment knobs; a
  policy layer would be a config node, and nothing reads one yet.
- **Ordering.** `recall` returns the seeds in similarity order and the facts
  ranked by weight, highest first (stable, so equal weights keep the seed
  order that pulled them in). Landing stays ADR-0003's; weight is this
  ADR's.

**The W index — corrected.** `W:{weight}:{edge_id}` is a declared-field face
like any other, and it is deliberately NOT declared today: weight changes
already rewrite the row, so the face would be maintained for a reader that
does not exist. The sketch named two readers — a scope-wide top-K /
progressive load, and the Phase-5 cold-tier range read. Neither is callable
now, and the first is not even shaped right: edges carry no `scope` segment
(ADR-0010 §1 — ownership rides the endpoints), so `[weight][edge_id]`
answers "the store's heaviest edges", not "this user's heaviest facts". A
per-scope top-K would be a projection table keyed `[scope][weight][edge_id]`,
written on the same touch — a design item to name when the read exists, not
a face to pre-pay for. Recall's in-memory ranking needs none of it: the set
it ranks is already in hand.
