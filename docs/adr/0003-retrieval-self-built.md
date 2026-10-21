# ADR-0003: Retrieval — Built-In Embedding Model, Vector Landing + the Landed Community

**Status**: Landed (2026-10-17: the embedding engine — a built-in local model
loaded in-process from a model directory — its config surface, and the row's
vector carrier; 2026-10-19: the landing table, `/search` and `/recall`, PLAN
1c/1d. The model pick stays provisional: the recall comparison against the
legacy PG stack is still not runnable here, so PLAN 1d's fixture is the
baseline)
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

**The engine is a MODE, not one implementation (ruled 2026-10-21).** What the
store needs is a contract: one text in, one UNIT-length vector out at the
declared `dimensions`. WHO computes it is a deployment property, so the
`embedding` node states it:

- `mode "local"` — the built-in model in this process. The default, unchanged:
  no endpoint, no key, no transport hop.
- `mode "remote"` — an OpenAI-shaped embeddings service
  (`POST {base-url}/embeddings`), so the vectors can be computed on a machine
  with a GPU and k10r keeps its own storage, its own clock and its own
  contract. That shape is chosen because this operation has effectively ONE
  wire form (vLLM, sglang, Ollama, LM Studio and the hosted APIs all speak it);
  a per-provider adapter would buy maintenance and nothing else.

The tag chooses a SHAPE, not a branch: `mode` is the tag of the node, and each
mode's field set is its own type, so a field of the other mode is a decode error
on that line rather than a name a validator has to remember to refuse.

The seam is a trait in mem-core (`Embedder`) with the local engine as one
implementation, and the client for the remote one lives in `mem-server`: the
core keeps its zero-transport surface (it owns the contract, not a client), and
the shell that already speaks HTTP to consumers speaks HTTP to a model. Both
modes are PROVEN at boot — a missing artifact, an unreachable service, or a
width that disagrees with the config is a start failure, because two vector
spaces in one table is not a condition the store can survive. The remote
answer is normalized HERE rather than trusted, the bearer token is named by
`api-key-env` (a variable name; the key itself never enters the file), and
requests are serialized on one client thread — the store's own single-writer
discipline, not a new bottleneck. The trade the mode buys with is latency: an
in-process embed is a memory operation, a remote one adds a round trip to every
ingest and every landing.

**Vector search: `okm-vector`'s index over a per-ns in-memory table.** The
corpus is per-user; tens of thousands of vectors fit memory by orders of
magnitude, so the exact path answers in sub-millisecond time and no other
machinery is needed. **The index is `okm-vector`'s end to end** (ruling
2026-10-20): exact today (`ExactIndex`), and an approximate form would be that
same crate's business — adopted from it rather than designed beside it, so this
repo plans no approximate index of its own, holds no threshold parameter and
offers no index option. arroy is rejected: arroy 0.6 is an LMDB-backed
(memory-mapped) index, so it cannot be "serialized into Fjall" — it brings a
second storage engine along, exactly what this ADR rules out.

**Retrieval is vector landing + the landed community.** An exact name/alias
point lookup (entity name / alias → node id, one prefix scan — the graph's own
identity face) and vector top-k seeds both land; the answer is the COMMUNITY
those seeds belong to (ADR-0010 §5): its members through the `by_community`
face, the facts whose endpoints are both members, ranked by weight (ADR-0005)
and rendered as id-bearing text (the representation is `?format=`:
`plaintext` / `markdown` / `mermaid`, or `struct` for the data alone — ADR-0010
§5). A landing confidence floor plus a legal "nothing landed" outcome (the
caller injects no memory) replace the old fusion score.

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
- Bring-up posture: the scan is EXACT — brute-force inner product over the
  scope's table — and that is what retrieval costs at this scale; nothing
  approximate sits behind it waiting, because the index form is `okm-vector`'s
  call and never a phase of this repo.
  **Landed 2026-10-19:** `GraphStore::retrieve` + `GET /graph/{scope}/search`
  — per-scope in-memory table (loaded from the KV on first use, dropped on
  every write, byte-budgeted with LRU eviction), inner product over
  unit-length vectors, `LANDING_FLOOR` (0.35) and a legal empty landing —
  plus the read answer (`GraphStore::recall`, `GET /memories/{scope}/recall`):
  the seeds, the communities they landed in, and ONE representation of those
  communities' facts under a fact budget — the id-bearing text, or the data
  alone under `format=struct` (ADR-0010 §3–§5).
  Weight-ordered ranking is ADR-0005's. `/search` is the ANALYSIS door (the
  landing alone, for inspection); `/recall` is the CORE door (the memory
  answer) — ADR-0007 owns the two-class split.
- **The three costs, and whose call the shape is (2026-10-19).** A scan strategy
  pays in compute, memory or I/O, and the choice is a MEASURED one: an in-memory
  table pays memory (~2 KB/vector) and scans in O(N·d) — at 10⁴ rows that is
  sub-millisecond, at 10⁵ ~6 ms with AVX2; IVF pays training plus a
  retrain-and-rewrite cycle but probes only nprobe buckets; HNSW pays a
  persisted graph (deletes become tombstones, model swaps a rebuild) for
  logarithmic hops, and buys nothing while the whole path is already a few
  milliseconds.
  **Which index k10r gets is that crate's call, not this repo's** (ruling
  2026-10-20): the corpus is queried through `okm-vector`, and whatever form it
  carries — exact today, approximate whenever it grows one — is taken from
  there, its snapshot blobs living at okm's own faces. What stays on this side
  is the ACCOUNT, not the algorithm: when the derived table is rebuilt (a write
  drops it), how much of it may sit in memory (byte budget + LRU), and the
  landing floor. An index of any kind is DERIVED state rebuilt from the rows,
  never a second source of truth, and arroy stays rejected for the LMDB reason
  above — it would bring a second storage engine, which is exactly what the
  crate spares us.
- There is no tokenizer to control, so "tokenization and scoring fully under
  our control" is no longer a trade-off claim this route makes.
- The PG exit gate becomes an end-to-end recall comparison on a fixed query set,
  not a per-branch comparison against pgvector + pdb.bm25 + RRF (PLAN 1d).
  **Measured baseline 2026-10-19** (the PG stack is not runnable here, so the
  fixture is what a comparison is measured against): 14 nodes × 14 queries,
  precision@1 0.929, precision@3 1.000, lowest true-hit score 0.637 (floor
  0.35), off-corpus query lands nothing. The floor is therefore not cutting
  true hits — the whole point the constant exists to be checked against.
- The `embedding` node became LOCAL on 2026-10-17 (landed): `base-url` /
  `endpoint` / `api-key-env` / `timeout-ms` were removed, and `model-dir` named
  the artifact directory (`config.json` / `model.safetensors` / `tokenizer.json`
  are found by name inside it) with `KRYSTALLIZER_MODEL_DIR` overriding it. The
  artifact itself is host-local and NOT in the repo — this is the standing rule
  for anything of model size. **Reopened 2026-10-21 as a MODE**: the same node
  now also accepts `base-url` / `model` / `api-key-env` / `timeout-ms` under
  `mode "remote"`. The local-only shape had quietly made "no external compute"
  a repo-level assumption, and the alternative to naming it — a second memory
  backend for GPU hosts — would have broken the ONE thing this ADR binds: the
  vector space belongs to the memory system. The removed fields come back for
  the mode that needs them; the local mode's fields are refused there, so a
  half-edited file cannot serve.