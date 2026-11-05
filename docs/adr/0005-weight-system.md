# ADR-0005: Weight System — Counters on the Edge Row, Read-Write Quadrants, Tool Index

**Status**: Landed 2026-10-19 (the counters ride the edge row; the delta-append table survives only as the multi-writer escape); 2026-10-22 appended the recency ring (payload layout v3); 2026-10-23 widened it to two rings at payload v4 and moved time to the query — accepted, not yet landed
**Date**: 2026-09-08
**Chinese**: [0005-weight-system.zh-CN.md](0005-weight-system.zh-CN.md)

## Context

The graph-memory design assigns every fact edge a system weight metadata
layer (distinct from business properties): read_count, write_count,
tool_invoke_count, and the access rings that date them. The read-write ratio
defines four quadrants (core / deep-water / noise / cold), and retrieval order
is weight-ordered Top-K (progressive loading).

Two engineering questions: how to count without read-modify-write
contention, and how to serve a weight-ordered Top-K without full scans.

## Decision

**Counting: the edge ROW is the count key.** `read_count` / `write_count` /
`tool_count` are declared hot fields of `Facts` (ns 37, payload layout v4),
with the folded `weight` beside them. One identity, one write — and because
okm's declared faces are derived from the row, a bump rewrites one row and
everything derived from it follows.

**Recent accesses: two fixed-size rings at the row's tail (layout v4).** The
counters answer "how many" and do not answer "what did the distribution look
like" — 50 reads bunched into one week and 50 spread over two years bill very
differently under the query-time decay. `recent_read: Vector<u32>` and
`recent_write: Vector<u32>` sit at the payload's tail, one ring per direction:
8 slots each, a SECOND offset from `1_767_225_600` (2026-01-01T00:00:00Z),
ascending, `0` = unused, shifted right by one on write. Two rings rather than
one because a write's ring answers "when was this fact last re-asserted", which
a single last-access stamp could not separate from "when was it last read".
`#[ok_len(8)]` pins "always 8 slots" on both at the write boundary (`Default` is
hand-written to pad them, and a row written before the rings existed decodes
with 8 zeros by default, so no store rebuild is involved); the cost is 32 more
bytes per row.
These two are the only time-bearing fields on the row — no `last_read` /
`last_write` beside them — and the re-rank takes its decay base from each
ring's newest used slot. The judgement behind the shape — not `Offset`, not
`Bytes`, no cursor, no okm wrapper — is recorded in okm MODELING's k10r case.

- **Who bumps what.** `write_count` is the fact's own assertion: `ingest`
  bumps it when the fact links or wins the adjudication (a dropped fact is
  not a write). `read_count` is a served recall — `recall` / `expand` bump
  it only when the caller's clock rides the call (`ts_ms`), because a read
  nobody stamped is not a read and an absent timestamp must not invent
  recency. `tool_count` is a caller-reported tool consult
  (`POST /memories/{scope}/edges/{id}/touch`): k10r cannot see a tool call from
  inside, so the consumer that used the fact reports it.
- **read_count and write_count stay physically separate** — the atomic-fact
  append model guarantees read/write independence, which is what makes the
  2D quadrant classification stable at the storage layer.
- **Fold points, not background passes.** A touch rewrites the row in one
  batch: the counter goes up and the matching ring shifts right by one slot,
  same row, same write. The delta-append table stays the multi-writer ESCAPE:
  it folds into these same fields, so switching to it changes no schema, and
  its key layout is the one thing to fix when it lands.

**Weight formula** (from the wiki, including the tool index):

```
weight = read_score + write_score + tool_score

tool_score  = log(1 + tool_invoke_count) × 1.5
read_score  = log(1 + read_count)
write_score = log(1 + write_count) × 0.5
```

Deep water (high read, low write) is load-bearing: a hard write threshold
would structurally punish high-entry-cost expert knowledge. The 1.0 read
coefficient against write's 0.5 is that correction already expressed — a
threshold pair of its own would only restate the gap.

- **`weight` is the ranking key, not a derived cache.** The formula is its
  default producer; an outside writer may set an absolute value (0 = unset,
  i.e. fold the formula). No second field. The subjective value does not enter
  the formula at this stage — the data a later formula needs exists; the
  formula is not final.
- **The bucket.** The stored value is the formula in MILLI-units (`× 1000`,
  rounded, clamped at 0): a small float needs a stable ordered integer to be
  compared by integer, and 0.001 is finer than the smallest step the log terms
  can take, so the bucket never reorders two facts the formula would order
  differently. The coefficients are named constants in `mem-core/src/graph.rs`
  (read 1.0, write 0.5, tool 1.5). They are the formula, not deployment knobs; a
  policy layer would be a config node, and nothing reads one yet.
- **Ordering.** `recall` returns the seeds in similarity order and the facts
  ranked by weight, highest first (stable, so equal weights keep the seed
  order that pulled them in); the set it fetched is then re-ranked by recency
  at query time from the two rings. Landing stays ADR-0003's; weight is this
  ADR's.

**What the row carries for a later formula.** The widening lands before the
formula settles on purpose: the three counters, the two access rings,
`confidence` (already declared; today only the LPA vote reads it), and the
ranking key. Anything a future term needs beyond these — a citation count for
fact-to-fact references, a separate count for by-id fetches — is added when the
reader that consumes it arrives: a field nobody scans is a write nobody asked
for.

**Top-K: the scope-wide face is declared, the bare weight face is not.** The
bare `W:{weight}:{edge_id}` would answer "the store's heaviest edges", not
"this user's heaviest facts": an edge's identity carries no `scope` segment
(ADR-0010 §1 — ownership rides the endpoints). The scope-wide face carries it
instead: the edge row holds a denormalized `scope` (a copy of its source
endpoint's, written once), so `by_scope_weight { fields(scope, weight) }`,
keyed `[scope][weight][edge_id]`, is probed per scope, and
`top_by_weight(scope, k)` decodes k rows — O(k rows) + O(scope keys), no
projection table to maintain beside the row. Nor does a weight bump leave it
dangling: okm's raw-collection overwrite does not delete an entry keyed by the
OLD value, but a graph write goes through `link_into`, which unlinks what the
row used to derive, so a rewrite is one batch and a test pins one fact to one
ranking entry. Recall's ranking needs none of it: the set it ranks is already
in hand, and its recency re-rank happens there. `by_confidence` stays
undeclared on the same ground as the bare weight face: neither has a reader.

## Consequences

- Today's ranking is an in-memory sort over the set recall already fetched —
  no index, no full-table job, no second engine. The recency re-rank rides
  that same pass.
- Cold-tier work (Phase 5+ fallout) and progressive loading (Phase 4) want the
  same scope-keyed face (`by_scope_weight`); when either arrives, one face
  serves both, and the decision above names its shape.
- The stored weight carries no time term, so it cannot go stale between writes
  and nothing derived can. Recency is a query-time re-rank over the set recall
  already fetched; a scope-wide Top-K ordered by recent activity therefore has
  no face — it would be stale by construction. Add one only with a reader that
  accepts that.

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