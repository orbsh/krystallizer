# ADR-0003: Retrieval — Built-In Embedding Model, Vector Landing + Graph Traversal

**Status**: Accepted (design; implementation pending)
**Date**: 2026-09-08
**Chinese**: [0003-retrieval-self-built.zh-CN.md](0003-retrieval-self-built.zh-CN.md)

## Context

The legacy retrieval path is ParadeDB SQL: pgvector cosine + pdb.bm25 full
text + RRF fusion in one query. Moving off PG (ADR-0002) removes those
extensions, so retrieval had to be rebuilt on the KV side.

## Decision

**Embedding: a built-in local model, run in-process.** A small model runs on
CPU inside the k10r process. Model identity is locked with the data (vectors
from different models do not share a space, so the memory system — not any
caller — owns embedding consistency), and there is no endpoint, no key and no
transport hop: this is what makes mem-core's "zero transport dependency"
literal. Vectors are written as `Vector<f32>` frames.

**Vector search: a per-ns in-memory table + brute-force inner product.** The
corpus is per-user; tens of thousands of vectors fit memory by orders of
magnitude. ANN is re-opened only when a MEASURED corpus crosses the threshold,
and arroy is not the default shape for it: arroy 0.6 is an LMDB-backed
(memory-mapped) index, so it cannot be "serialized into Fjall" — it brings a
second storage engine along, exactly what this ADR rules out. Not deferred
below the threshold: dropped.

**Retrieval is vector landing + graph traversal.** An exact name/alias point
lookup (entity name / alias → node id, one prefix scan — the graph's own
identity face) and vector top-k seeds enter ONE traversal, which expands the
neighborhood and ranks by weight (ADR-0005, phases 3/4). A landing confidence
floor plus a legal "nothing landed" outcome (the caller injects no memory)
replace the old fusion score.

**The lexical branch is withdrawn: jieba, the inverted index, BM25, RRF and
tantivy are all dropped.**

- It buys top-k precision in FLAT retrieval. Once the answer is produced by
  neighborhood expansion + weight-ordered Top-K, landing only has to enter the
  right region, so that precision is not what the read path needs.
- It charges two structural prices for it: tokenization bound to one language,
  and exact surface forms defeated by synonyms (recoverable only through a
  maintained synonym table — the thing this design refuses to keep).
- The one capability only it held — verbatim rare tokens (`ADR-0016`,
  `ERR_021`, identifiers), which a small model does not represent — moves to
  the key-space point lookup named above. That is the graph's identity face,
  not a third retrieval mechanism.
- tantivy is not adopted for the same reason: the branch it would serve no
  longer exists.

The standing selection rule that shaped the earlier draft still holds and is
now spent on the graph side: simple algorithms are implemented directly, and
only complex ones (PageRank, community detection) earn a dependency.

## Consequences

- "Functions without boundaries" is exercised on its home turf: what was a UDF
  inside PG and a SQL CTE (hybrid search) is now ordinary library code.
- No HTTP hop at all: the model is in-process, so writes and queries embed
  locally. Dropping the remote embedding call is what makes the core's
  outbound surface empty (ADR-0007).
- Bring-up posture: brute-force inner product stands in for ANN below the
  measured threshold; ANN integration must not be a Phase-1 blocker.
- There is no tokenizer to control, so "tokenization and scoring fully under
  our control" is no longer a trade-off claim this route makes.
- The PG exit gate becomes an end-to-end recall comparison on a fixed query set,
  not a per-branch comparison against pgvector + pdb.bm25 + RRF (PLAN 1d).
- `config/krystallizer.kdl`'s `embedding` node loses base-url / api-key-env and
  gains model + tokenizer paths when Phase 1c lands.