# ADR-0005: Weight System — Delta Counts, Read-Write Quadrants, Tool Index

**Status**: Accepted (design; implementation pending)
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
