# 0019 — The Embedding Leaves the Entity Row

> **Languages:** [English](0019-vector-side-table.md) (primary) · [中文](0019-vector-side-table.zh-CN.md)

**Status:** Accepted and LANDED (2026-10-26) — `EntityRow` carries no vector, the
side table is `NodeVector` (ns 43), and the pair commits in one batch.
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
- **Deletion is the caller's**, as everywhere in this model: nothing cascades, so
  whoever deletes a node has to delete its vector in the same breath. Nothing deletes
  nodes today (a `purge` exists only for the log), so this is the contract a delete
  path owes, not a path that runs.

## Why not a `Ref` field

The side table is keyed by the entity's key; there is no field-level reference between
the two tables. The other shape that looks natural is an entity row carrying
`Ref<NodeVector, EntityKey>`; four costs reject it.

- **Read amplification comes back.** `Collection::get` dereferences, so every read of an
  entity row also point-reads ns 43 — 2 KB per read, growing with the model's width.
  Half the point of the split is that a read which does not need the vector does not
  pay for it (a skill read, a name lookup, a viz card); with a `Ref` it always does.
- **`Ref::own` gives the write coupling back.** The parent's `put` writes the child, so
  a counter bump re-writes the vector — exactly what this ADR removes. `Ref::ref_key`
  stores the key alone and touches nothing, but the row then carries an extra
  hot-segment field, and a key change still triggers a release.
- **There is no cascade delete, and what does exist points the wrong way.** `purge`
  deletes the primary entry, the faces and the reduce ledgers; it never touches the
  child. Delete the row and the side row stays behind as an orphan — which the ns 43
  rebuild then indexes, landing on ids that no longer exist, so "presence is the side
  row" fails on the spot. The release that does exist (a changed key deletes the old
  child) runs the opposite direction from "delete the parent, delete the child".
- **That release does not distinguish sharing.** The derive pushes a key for both the
  `own` and the `ref_key` form, so a parent that overwrites and drops a key deletes a
  child another parent still references (it reads back as `None`). okm MODELING says a
  shared child survives; the code does not back that sentence.

One criterion decides it: `Ref` says "the child belongs to the parent" — identity,
lifetime and dereference bound together; a side table says "two parts of one identity,
each with its own writer". The vector's key IS the entity key; it needs no second
identity, and the two things it does need — write isolation and read-on-demand — are
precisely what `Ref`'s two semantics break. Neither existence nor deletion has a
foreign key behind it: the caller writes the pair together and deletes it together.

## Consequences

- **A count costs a small row write.** ADR-0018's counters become cheap to bump for
  the reason it assumed they were, rather than because the segment is small.
- **A re-embedding pass writes one table.** Changing the model's width touches ns 43
  only, not every entity row.
- **"Derived data stays off the graph" becomes "beside the graph, in its own table".**
  The split has a second trigger, recorded here: **write-frequency asymmetry** — a
  large column that shares a row with a frequent small writer is split even though
  every read wants both. It sits alongside "large AND accessed on its own"; neither
  replaces the other.
- **A pre-split store is rebuilt, not migrated.** The row's layout went to v4 and
  `embedding` left the middle of the payload, so a v3 row decodes as garbage under
  the new schema; there is no tolerant decoder, by design — a decoder that shrugs at
  a shifted layout reads corruption silently. `data/` was empty when this landed, so
  the upgrade path is a fresh directory. If a store ever has to be carried over, the
  surface is `scan_documents_raw` plus a legacy struct decoded into the two tables.

## What landed

1. `EntityRow` lost `embedding` (layout v4); `NodeVector { embedding }` is declared at
   ns 43 with `#[ok_ref(EntityKey)]` and no face.
2. A new node's row and vector are written into ONE batch (`save_with_prev_into` for
   both, one `commit_batch`), so the pair cannot be torn apart by a crash. No engine =
   no side row at all, rather than an EMPTY frame nothing reads.
3. `GraphStore::node_vector(scope, node_id)` answers `Option<Vector<f32>>`: presence is
   the boolean.
4. `load_vector_table` reads `scope_vectors` (one scan of ns 43) and joins it to the
   entity rows it already scanned; `ExactIndex` is therefore built from this table.
5. Tested: a prop merge leaves the vector byte-identical, the pair survives a reopen,
   and retrieval rebuilds from ns 43 (`mem-core/tests/graph.rs`).

## Related records

ADR-0015 (the skill node), ADR-0018 (counters on the node), okm MODELING §"A value
too big for the row" and §"derived data stays off the graph", okm ADR-0035
(whole-document writes), okm ADR-0036 (off-row, rejected).