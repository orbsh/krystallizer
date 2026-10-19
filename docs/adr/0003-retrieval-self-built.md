# ADR-0003: Retrieval — Built-In Embedding Model, Vector Landing + the Landed Community

**Status**: Partially landed (2026-10-17: the embedding engine — a built-in
local model loaded in-process from a model directory — its config surface, and
the row's vector carrier; the in-memory vector table and the read path are
owed, PLAN 1c/1d)
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

Landed 2026-10-17: **BAAI/bge-small-zh-v1.5** (512-dim, CLS pooling, ~96 MB
fp32), run by a pure-Rust BERT (`candle`) from a model DIRECTORY named by
`config/krystallizer.kdl`'s `embedding` node — the three artifact files are
found by name inside it, and `KRYSTALLIZER_MODEL_DIR` overrides the value,
since where the artifact lives is a deployment fact rather than a repo fact.
The pick is provisional: the recall comparison (PLAN 1d) is the instrument
that can overturn it, and a swap is a directory swap PLUS an embed of the
whole corpus — vectors do not carry across models, which is exactly why the
model is locked with the data rather than chosen per call.

The vector's carrier landed with it: a row's `embedding: Vector<f32>` is a cold
frame of ANY length (its own count rides the frame, so a swap changes the data
and not the wire — no `#[ok_len]` contract, deliberately), L2-normalized at the
source so `cosine` is an inner product. The vector is computed INSIDE k10r, by
the engine the serving process attached, so the HTTP client never sends one;
a row written with no engine attached carries an EMPTY frame — a legal row the
retrieval side skips, never a zero vector.

**Vector search: a per-ns in-memory table + brute-force inner product.** The
corpus is per-user; tens of thousands of vectors fit memory by orders of
magnitude. ANN is re-opened only when a MEASURED corpus crosses the threshold,
and arroy is not the default shape for it: arroy 0.6 is an LMDB-backed
(memory-mapped) index, so it cannot be "serialized into Fjall" — it brings a
second storage engine along, exactly what this ADR rules out. Not deferred
below the threshold: dropped.

**Retrieval is vector landing + the landed community.** An exact name/alias
point lookup (entity name / alias → node id, one prefix scan — the graph's own
identity face) and vector top-k seeds both land; the answer is the COMMUNITY
those seeds belong to (ADR-0010 §5): its members through the `by_community`
face, the facts whose endpoints are both members, ranked by weight (ADR-0005)
and rendered as id-bearing text. A landing confidence floor plus a legal
"nothing landed" outcome (the caller injects no memory) replace the old fusion
score.

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
  **Landed 2026-10-19:** `GraphStore::retrieve` + `GET /facts/{scope}/search`
  — per-scope in-memory table (loaded from the KV on first use, dropped on
  every write, byte-budgeted with LRU eviction), inner product over
  unit-length vectors, `LANDING_FLOOR` (0.35) and a legal empty landing —
  plus the read answer (`GraphStore::recall`, `GET /facts/{scope}/recall`):
  the seeds, the communities they landed in, and those communities' facts
  rendered as id-bearing text under a fact budget (ADR-0010 §3–§5).
  Weight-ordered ranking is ADR-0005's. `/search` is the ANALYSIS door (the
  landing alone, for inspection); `/recall` is the CORE door (the memory
  answer) — ADR-0007 owns the two-class split.
- **The three costs, and the shape past the threshold (2026-10-19).** A scan
  strategy pays in compute, memory or I/O, and the choice is a MEASURED one:
  an in-memory table pays memory (~2 KB/vector) and scans in O(N·d) — at
  10⁴ rows that is sub-millisecond, at 10⁵ ~6 ms with AVX2; IVF pays training
  plus a retrain-and-rewrite cycle but probes only nprobe buckets; HNSW pays
  a persisted graph (deletes become tombstones, model swaps a rebuild) for
  logarithmic hops, and buys nothing while the whole path is already a few
  milliseconds. When one scope's table outgrows the budget, the next shape is
  **IVF with the bucket encoded in the KV key** (`[ns][bucket][node_id]`) —
  no second storage engine, deletes stay natural. arroy stays rejected for
  the LMDB reason above; a persisted HNSW only if a MEASURED latency budget
  still fails after that, and then as DERIVED state rebuilt from the rows,
  never as a second source of truth.
- There is no tokenizer to control, so "tokenization and scoring fully under
  our control" is no longer a trade-off claim this route makes.
- The PG exit gate becomes an end-to-end recall comparison on a fixed query set,
  not a per-branch comparison against pgvector + pdb.bm25 + RRF (PLAN 1d).
  **Measured baseline 2026-10-19** (the PG stack is not runnable here, so the
  fixture is what a comparison is measured against): 14 nodes × 14 queries,
  precision@1 0.929, precision@3 1.000, lowest true-hit score 0.637 (floor
  0.35), off-corpus query lands nothing. The floor is therefore not cutting
  true hits — the whole point the constant exists to be checked against.
- The `embedding` node is LOCAL (landed 2026-10-17): `base-url` / `endpoint` /
  `api-key-env` / `timeout-ms` are gone, and `model-dir` names the artifact
  directory (`config.json` / `model.safetensors` / `tokenizer.json` are found
  by name inside it) with `KRYSTALLIZER_MODEL_DIR` overriding it. The artifact
  itself is host-local and NOT in the repo — this is the standing rule for
  anything of model size.