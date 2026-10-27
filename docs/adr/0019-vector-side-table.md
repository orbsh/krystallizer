# 0019 — The Embedding Leaves the Entity Row

> **Languages:** [English](0019-vector-side-table.md) (primary) · [中文](0019-vector-side-table.zh-CN.md)

**Status:** Accepted (2026-10-26). **Not yet implemented** — the row still carries
`embedding: Vector<f32>`; the list of what changes is at the end.
**Date:** 2026-10-26

## Context

ADR-0018 put a skill's usage counters on the node as props. Props live in the row's
dynamic segment, and a prop write is a whole-document write (okm ADR-0035): the row
is decoded, merged, re-encoded. That is fine for a row of small fields — and wrong
for this row, because the same payload also carries the node's vector.

The two fields have opposite life cycles:

| field | size | how often it changes |
|---|---|---|
| `embedding: Vector<f32>` | 1.5 KB at 384 dims, 2 KB at 512 | almost never — it changes only when `name`/`title`/`content` change |
| `invoke_calls` / `invoke_ok` / `last_invoked` | eight bytes each | on every call |

Sharing one payload means the frequent writer pays for the rarely-written large
field: counting an invocation re-encodes an embedding that a count cannot possibly
change. The cost is not theoretical — it is a copy of the vector on every tool call,
and it grows with the width of the embedding model.

## Decision

**The vector moves to its own table, keyed by the entity's own key type.**

```rust
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(EntityKey)]          // the SAME key type: byte-identical key payload
#[ok_ns(43)]                  // one ns of its own
pub struct NodeVector { pub embedding: Vector<f32> }
```

This is okm's existing side-table pattern ("A value too big for the row: its own ns,
the same key"), not a new mechanism. Four reasons, in the order they decided it:

**1. One row, two writers with nothing in common.** A vector is large and nearly
immutable; a counter is tiny and changes constantly. Any payload shared by both is
priced at the sum for the writer that touches it more often — which is the counter.

**2. Off-row was considered and rejected.** An in-row annotation that extracts one
field to its own storage (`#[ok_offrow]`) can hold a vector, but it costs a
field-level selector threaded through `FieldDesc`, `FieldSchema`, `split_names`, the
four whole-document verbs and the derive walks — and it forbids indexing that field.
A second table with the parent's key type does the same job with no new machinery
(okm ADR-0036's rejection, reaffirmed from this side).

**3. The main table's writes stop touching the vector at all.** This is the
advantage that had not been stated: before the split, EVERY write to the node —
including a count — re-encoded the embedding. After it, the hot row has no large
field, so a hot write is small by construction, not by discipline.

**4. Index maintenance stops needing precision.** With off-row storage, an update to
one field has to establish which indexes that field feeds and update exactly those;
otherwise a heavyweight index — the vector is the worst case — is rewritten on every
write of anything. The side table's indexes are its OWN: they are rebuilt from that
table alone, so an index update is wholesale, and no field-level bookkeeping is
required to keep it honest.

## Shape

- **Presence is the side row.** No `Option<Vector<f32>>`, no zero-length sentinel: a
  node with no vector has no row in ns 43.
- **No faces on it.** A vector has no key order; search stays `okm-vector`'s
  `ExactIndex`, now rebuilt from ns 43 instead of from the entity rows' frames.
- **Retrieval reads two tables**: the entity rows answer the query's landing and its
  filters, then the vectors are fetched by id. That is the deliberate price of the
  split, and it is the one thing worth watching: a landed set of `k` nodes becomes
  `k` point reads (or one batched multi-get).
- **Deletion is the caller's**, as everywhere in this model: nothing cascades, so the
  pair is written together and deleted together.

## Consequences

- **A count costs a small row write.** ADR-0018's counters become cheap to bump for
  the reason it assumed they were, rather than because the segment is small.
- **A re-embedding pass writes one table.** Changing the model's width touches ns 43
  only, not every entity row.
- **"Derived data stays off the graph" becomes "beside the graph, in its own table".**
  okm's MODELING gains the write-frequency asymmetry as a second split trigger
  alongside "large AND accessed on its own": a large column that shares a row with a
  frequent small writer is split even though every read wants both.
- **Not implemented yet.** `/graph/{scope}` currently reads the vector off the row;
  the retrieval path, the embedding write, and the existing stores' migration have to
  follow. Until they do, this record describes the target, and the code is one
  ADR behind — recorded rather than silent.

## What implementation must touch

1. `EntityRow` loses `embedding`; `NodeVector` is declared at ns 43 with `EntityKey`.
2. Every write of a vector becomes a side-table put, committed in the same batch as
   the row (okm ADR-0003: one WriteBatch = one transaction).
3. `retrieve` (and the resolver that rides it) fetches vectors by id after landing.
4. `ExactIndex` is built from ns 43.
5. An existing store's rows keep their vectors in the old frame until a one-shot
   migration rewrite; until then both shapes must be readable, or the store is
   rebuilt (acceptable now: the corpus is small and regenerable).

## Related records

ADR-0015 (the skill node), ADR-0018 (counters on the node), okm MODELING §"A value
too big for the row" and §"derived data stays off the graph", okm ADR-0035
(whole-document writes), okm ADR-0036 (off-row, rejected).