# ADR-0003: Self-Built Retrieval — arroy Vectors + Hand-Written BM25 + RRF

**Status**: Accepted (design; implementation pending)
**Date**: 2026-09-08
**Chinese**: [0003-retrieval-self-built.zh-CN.md](0003-retrieval-self-built.zh-CN.md)

## Context

The legacy retrieval path is ParadeDB SQL: pgvector cosine + pdb.bm25 full
text + RRF fusion in one query. Moving off PG (ADR-0002) removes those
extensions. The options are: bring a SQL-capable engine along, adopt Rust
search frameworks, or build the two indices directly.

## Decision

**Vector: arroy (HNSW, Rust), graph serialized into Fjall, loaded into
memory at startup.**

- Corpus is per-user; tens of thousands of vectors fit memory by orders of
  magnitude. This matches the standing selection principle: in-memory → HNSW.
- Insert/delete per user on write; rebuild is a local operation.

**Full text: hand-written inverted index + BM25. jieba-rs for Chinese
tokenization. tantivy is NOT adopted.**

- BM25 over a user-scoped corpus is a few hundred lines: term frequencies,
  document lengths, IDF, scoring. This falls under the standing rule —
  simple algorithms are implemented directly; only complex ones (PageRank,
  community detection) earn a dependency.
- The wiki names "tokenization and scoring fully under our control" as the
  core thing the KV route buys over pg_search's black-box tokenizer —
  adopting tantivy would hand both back to a Lucene-shaped dependency tree
  (multi-MB, its own runtime, its own index format) to solve a problem we
  do not have at this corpus scale.
- The inverted index itself is KV-shaped (term → postings) and lives in
  Fjall alongside everything else; no second storage engine.

**Fusion: RRF in-process.** The legacy SQL CTE (two ranked branches, RRF
weights, join) becomes a plain function — the same k=60 RRF, one function
call instead of one query.

## Consequences

- "functions without boundaries" is exercised on its home turf: what was a
  UDF inside PG (plpython3u embedding) and a SQL CTE (hybrid search) is
  now ordinary library code with database-kernel-adjacent performance.
- Embedding calls: if the embed model stays a remote API, writes/queries
  still make that HTTP call — unchanged from the PG arrangement (PG
  forwarded the same call). Zeroing this out means a local embed model,
  an independent decision, out of scope here.
- Fallback posture: below ~10k edges per user, brute-force scan can stand in
  for arroy during bring-up; arroy integration must not be a Phase-1
  blocker.
- Tuning hooks (tokenizer choice, BM25 k1/b) are code-level constants, not
  a black box.

## Update (2026-10-14): built-in embedding model; the lexical branch is withdrawn

Two amendments; the 2026-09-08 body above stays as the record.

**Embedding is a built-in local model, not a remote API.** The
Consequences' "a local embed model, an independent decision, out of scope
here" is decided: a small model runs on CPU inside the process. Model
identity stays locked with the data and the vector space stays
krystallizer's (ADR-0007); dropping that HTTP hop is what makes
mem-core's "zero transport dependency" literally true. This lands before
Phase 1c, so the swap costs nothing — no stored vector anywhere was
produced under the remote model.

**Retrieval is vector landing + graph traversal. jieba, the inverted
index, BM25, RRF and arroy are all withdrawn:**

- arroy was mis-specified here: arroy 0.6 is an LMDB-backed
  (memory-mapped) index and cannot be "serialized into Fjall" — it brings
  heed/LMDB with it, i.e. a second storage engine, which this ADR itself
  rules out. Not deferred; dropped.
- The lexical branch buys top-k precision in flat retrieval. Once the
  answer is produced by neighborhood expansion + weight-ordered Top-K
  (ADR-0005, Phase 3/4), landing only has to enter the right region — and
  the branch charges two structural prices for that precision:
  tokenization bound to one language, and exact surface forms defeated by
  synonyms (recoverable only through a maintained synonym table, the thing
  we refuse to keep).
- The one capability only the lexical branch held — verbatim rare tokens
  (`ADR-0016`, `ERR_021`, identifiers), which a small model does not
  represent — moves to a key-space point lookup: entity name / alias →
  node id, one prefix scan. That is the graph's own identity face, not a
  third retrieval mechanism.
- Vector search is an in-memory table per ns with brute-force inner
  product at bring-up scale; ANN is re-opened only when a measured corpus
  crosses the threshold, and arroy is not the default shape for it.

Read path: exact lookup (name/alias) and vector top-k seeds enter one
traversal, which expands the neighborhood and ranks by weight. A landing
confidence floor and a legal "nothing landed" outcome (the caller injects
no memory) replace the fusion score.

Consequences: the PG exit gate becomes an end-to-end recall comparison on
a fixed query set, no longer a per-branch comparison against pgvector +
pdb.bm25 + RRF; `tantivy` leaves the deferred list together with the
branch that justified it; "full-chain control over tokenization and
scoring" is no longer a KV-route trade-off claim (there is no tokenizer to
control); the `embedding` node in `config/krystallizer.kdl` loses
base-url / api-key-env and gains model + tokenizer paths when Phase 1c
lands.
