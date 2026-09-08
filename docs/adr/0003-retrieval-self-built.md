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
