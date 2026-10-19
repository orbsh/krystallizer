# Krystallizer Data Modeling

The memory core's schemas in one place: what tables exist today, the key
and row shapes, and which access methods are declared. Generic OKM
modeling rules (four layers, access-method discipline, key-tail rules)
are NOT repeated here — they live in okm's
[MODELING.md](../../../okm/docs/MODELING.md) and are normative for this
repo too. Design decisions behind each table live in the ADRs.

> **Languages:** [English](MODELING.md) (primary) · [中文](MODELING.zh-CN.md)

## Tables

One entity class = one ns (OKM rule). Current state:

| ns  | Entity        | Status                  | ADR      |
|-----|---------------|-------------------------|----------|
| 32  | Flat memory   | **Withdrawn 2026-10-19** (the block below is history) | ADR-0002 |
| 33  | Channel messages | Implemented (minimal slice, 2026-10-09) | ADR-0008 |
| 34  | Checkpoints (per member) | Implemented (minimal slice, 2026-10-09) | ADR-0008 |
| 35  | Code (delivered scripts) | Implemented (bring-up slice, 2026-10-15) | gravity ADR-0001/0003 |
| —   | Session messages | superseded by the channel log | ADR-0008 |
| —   | Tasks / task_links DAG | Declared (ADR-0001 — the third interface surface; same graph as the facts) | ADR-0001, ADR-0002 |
| 36  | Entities (fact graph) | Implemented (2026-10-17) | ADR-0010 |
| 37  | Facts (fact graph edges) | Implemented (2026-10-17) | ADR-0010 |
| 38  | Community sequence (the id issuer, one row per scope) | Implemented (2026-10-20) | ADR-0010 |
| —   | Initialization memory (persona / style) | Declared (ADR-0001) | ADR-0001 |

The graph's read side (LPA, binning, the rendered community) landed with it
(2026-10-19/20); the session log's missing slices, the tasks, and the
scope-wide Top-K's `[scope][weight][edge_id]` projection table land in later
phases; a row above marked `Declared` is an ADR commitment rather than
working code.

## Memory kinds (who initiates)

Three kinds, split by who drives them (ADR-0001; the
naming is 被动 / 主动, with 短期 / 长期 as the content horizon:

- **被动 (short-term)** — the current context: the channel log (ns 33) plus
  its per-member projection (ns 34). The loop fetches and appends; never a
  tool.
- **主动 (long-term)** — facts outside the context, reached as a TOOL by
  any agent. The carrier is the fact graph; flat memory (ns 32) WAS its
  pre-graph form — the same knowledge held as text rows, which is why it went
  away when the graph landed (a text blob has no per-fact identity: it cannot
  be updated, weighted or enumerated per fact). Withdrawn 2026-10-19. Raw content never needed it — the
  channel log (ns 33) already holds every message.
- **初始化 (persona / style)** — graph-derived: a traversal from the user
  node, delivered at the head of the projection. No ns assigned yet.

## Fact-graph additions (2026-10-19)

- `EntityRow` gained `community_id` (declared hot, `#[ok_default(0)]`;
  payload layout v2) and its `by_community` face
  (`[community_id][scope][node_id]`) — the bucket read is a face scan, never
  a table filter (ADR-0010 §2).
- `FactEdge` carries the weight system's inputs (`read_count`, `write_count`,
  `tool_count`, `last_read`, `last_write`; layout v2). `weight` is the folded
  formula in milli-units. Its three declared faces (`by_since` /
  `by_confidence` / `by_weight`) are NOT declared: nothing scans them, and
  every touch would maintain them for no reader (ADR-0005).
- `CommunitySeqRow` (ns 38): `[scope]` → `high_water`, the last community id
  issued. One row per scope, overwritten in place, monotonic — the issuer that
  replaced both the `HighWater` reduce and the "max of the live community ids"
  reading (ADR-0010 §1). The capsule table that ns once carried is retired.

## Flat memory (ns 32)

**WITHDRAWN 2026-10-19** — the graph landed and took the carrier with it (ADR-0001's "flat memory, then the graph"; PLAN Phase 3): the table, `MemoryStore` and the `/memory` door are deleted, and ns 32 is free. The schema below is kept as the historical record of the pre-graph form — legacy rows are a migration question (ADR-0002's PG import), not a live table.

### Key: `MemoryKey` (`KeyEncode`)

```rust
#[derive(KeyEncode, ...)]
#[kv_ns(32)] // legacy: declared on the key (pre row-as-declaration-point)
pub struct MemoryKey {
    pub user_id: u64,
    pub id: u64,
}
```

Physical layout: `[user_id 8B BE][id 8B BE]` = 16 bytes, zero padding.
`user_id` is the organizational prefix (user-scoped scans); `id` is a
surrogate identity — attributes are payload, never key.

### Row: `MemoryRow` (`DocumentEncode`)

```rust
#[derive(DocumentEncode, ...)]
#[ok_ref(MemoryKey)]
#[ok_ns(32)] // the row is the table's declaration point
pub struct MemoryRow {
    pub text: String,           // cold TLV segment
    pub created_at: u64,        // hot segment (fixed width)
    pub embedding: Vector<f32>, // cold frame, ANY length — the row's vector
}
```

- ns 32 is declared on the row (`#[ok_ns]`); the key type carries none (the
  collection takes no ns: assembly is
  `<MemoryRow as Document>::collection(store)`).
- Payload: fixed-width fields contiguous (hot), variable text as TLV
  (cold). Layout version defaults to 1. `embedding` is appended at the tail
  of the cold segment — new fields go at the tail (okm's schema-evolution
  rule).
- `embedding` is the built-in engine's output (ADR-0003), L2-normalized at
  the source so the contract's `cosine` is an inner product. The frame
  carries its own count, so a model swap changes the DATA, not the wire; an
  EMPTY frame is a row written without an engine (legal, and the retrieval
  side skips it — it is not a zero vector).
- No `#[ok_index]` access methods yet — retrieval is the Phase 0
  brute-force substring scan (`scan_keys` + filter). This is the
  declared bring-up posture (ADR-0003), not the end state; Phase 1
  searches the built-in local embedding model's vectors (`Vector<f32>`
  frames, read into an in-memory table) + name/alias point lookup
  + weight metadata (ADR-0005). The lexical branch (jieba / inverted
  index / BM25 / RRF / arroy) is withdrawn — ADR-0003
  2026-10-14.

### Identity generation

`id` is an auto-increment surrogate assigned by `MemoryStore`. The
counter is NOT persisted: `open()` recovers it by scanning the last key
in table order (`[user_id][id]` BE → the last key carries the global
max id) and continuing past it. Consequence: ids are unique but not
contiguous per user, and the counter is global across users — do not
assume per-user id density.

## Access-method inventory

okm's discipline: queries go through declared paths only — there is no general
full-table scan query face. This inventory states which KIND each path is:

- **Point read**: `get` (primary key).
- **Named face**: a declared access method (prefix / range / covered scan;
  equality matching is `scan_exact`).
- **Declared degenerate path**: a full scan or an in-table filter. Legal, but it
  must be registered here with its shape, its precondition and its ceiling; when
  the precondition breaks (size or heat) it is promoted to a named face —
  declaring registers itself, so the fix is one index, not the call site.

Current standing:

- Flat memory (ns 32): a **declared degenerate path** — the `[user_id]` prefix
  scan plus an in-table substring filter (ADR-0003's bring-up posture). The table
  WAS the pre-graph carrier of long-term memory and went with the graph's
  landing (ADR-0001, withdrawn 2026-10-19), so the vector landing and the
  name/alias point lookup ADR-0003 specifies are declared on the graph's
  collections, not here.
- Channel messages (ns 33): a **named face** — prefix scan by
  `[channel_id][ts_ms][sub]`: replay, increment and max-gap reads are one prefix
  scan, and the interval rides the main table's own sort segment (sequential, no
  fetch-back), per ADR-0008 §3. Checkpoints (ns 34): point reads by
  `[channel_id][member_id]`.
- Code (ns 35): `get(code_id)` is a **point read**; `list()` is a **declared
  degenerate path** (the whole table in id order, for resolving or linking,
  replaced by graph retrieval — ADR-0003/0018).
- Fact graph: an OKM node collection (entities) plus an edge collection
  (facts) with declared faces, per ADR-0010 §1 — NOT an
  `edge:{src}:{label}:{dst}` key layout (`EdgeTable` is a former name; the
  layer above is `Graph<S, E: GraphEncode>` + `NodeRef` + the predicate vocabulary,
  okm ADR-0017). The weight secondary index is the edge collection's
  `weight` declared field's face (ADR-0005, ADR-0010 §1).

## Hex stability

Key layouts are locked by each suite's own hex test — `key_layout_is_hex_locked`
in `tests/channel.rs` and `tests/code.rs`, `entity_key_layout_is_hex_locked` and
`edge_payload_layout_is_locked` in `tests/graph.rs`; the flat-memory lock went
with its table (2026-10-19). ns header excluded (`KeyEncode::encode()` returns
payload without the ns; the Table prepends `[ns 2B][slot 1B]`).

## Channel log (ns 33) + checkpoints (ns 34)

The unified channel log and the agent's projection (ADR-0008, minimal
slice serving gravity Phase 0). Declared in
`crates/mem-core/src/channel.rs`.

### Keys

- `MessageKey` (ns 33): `[channel_id 8B][ts_ms 8B][sub 2B]` — BE,
  byte order == `(ts_ms, sub)` order; the log IS the index (no sort, no
  secondary index). `sub` = same-millisecond tiebreaker (single writer,
  sequential; u16 overflow at 65 536 msgs/ms is pathological). ns/slot
  join at entry time per ADR-0011.
- `CheckpointKey` (ns 34): `[channel_id 8B][member_id 8B]` — one
  checkpoint per (channel, member), OVERWRITTEN (same-key RMW): the
  projection reads only the latest; audit history belongs to Phase
  2.6's decision log. If re-segmentation later needs checkpoint
  history, this becomes an append-only log keyed by coverage point —
  a pre-production schema change.

### Rows

- `MessageRow`: `role u8` (user/assistant/tool/system), `kind u8`
  (text; tool_call/tool_result reserved for view-layer pruning),
  `content String`. Storage always holds full rows; pruning is view
  side (ADR-0006).
- `CheckpointRow`: `coverage_ts u64` (the projection's increment starts
  STRICTLY after), `summary String`.

### Access (mem-core `ChannelStore`)

`append(channel_id, ts_ms, sub, role, kind, content)` — the caller
(gravity) assigns the clock; `fetch_full(channel_id, member_id)` →
`SessionView { coverage_ts, summary, increment }` — the projection:
checkpoint + increment strictly after coverage, key order;
`append_checkpoint(channel_id, member_id, coverage_ts, summary)`.
`view::assemble(channels, graph, channel, member, scope, user_node)` →
`Projection { head: Vec<FactView>, view: SessionView }` — the head is the user
node's incident facts (36/37), which the door RENDERS (`render::render_head`)
in the requested `format` before it goes on the wire.

Not yet (later slices): threads, topic tags, members, read cursors,
compression policy engine (ADR-0008/0009).

## Code store (ns 35)

The delivered script's home — the one table an executor's call reads.
Declared in `crates/mem-core/src/code.rs`; the design is gravity's
(ADR-0001/0003: the node mount reads a script that travels as a call
argument), and this repo's own ADR for the store is owed.

### Key: `CodeKey` (`KeyEncode`)

```rust
#[derive(KeyEncode, ...)]
#[ok_ns(35)]
pub struct CodeKey {
    pub code_id: u64,
}
```

Physical layout: `[code_id 8B BE]` — identity and nothing else. No name
and no content hash: the caller re-reads the row on every call and the
source travels as a call argument, so there is no cache to stale
(gravity ADR-0003, "Code identity").

### Row: `CodeRow` (`DocumentEncode`)

```rust
#[derive(DocumentEncode, ...)]
#[ok_ref(CodeKey)]
#[ok_ns(35)]
pub struct CodeRow {
    pub language: String,
    pub description: String,
    pub parameters: String,
    pub source: String,
}
```

- **A row is a reusable piece of code, not a skill.** A login script is
  written once and many skills carry a step pointing at it — so the row
  carries a `description` and the parameter definition (`parameters`, a
  JSON document carried as TEXT: the store does not parse it, the caller
  that delivered the code owns its shape). Whoever links a skill to this
  code, or reads the graph afterwards, decides from the row instead of
  reading every source to find out what it takes. Neither field is an LLM
  search surface: what a skill IS lives in the graph (a structured node:
  preconditions / steps / experience / verification).
- **The language is data ON the row.** A script is data, and data has a
  language; nothing defaults it. The caller routes the call to the mount
  serving that language, and a language with no mount is the caller's
  error value — never a fallback here.
- gravity is the only client (ADR-0001: the node never talks to
  krystallizer; the byte path is gravity → control path → node).

### Access (mem-core `CodeStore`)

`put(language, description, parameters, source)` → the store-assigned
`code_id` (a surrogate id recovered on reopen by scanning the last key,
same rule as flat memory); `get(code_id)` → the row, or absent for an
unknown id; `list()` → `(code_id, language, description)` in id order —
what a resolver or a linker picks from, and the declared bring-up
posture (a whole-table scan that the graph's retrieval replaces,
ADR-0003/0018). Source and parameter definition stay out of `list()`.

## Fact graph (ns 36 entities, ns 37 facts)

The 主动 (long-term) side's carrier: an okm node collection plus an okm
edge collection, and the write-time resolution that lands one fact
(ADR-0010 §1; landed 2026-10-17). Declared in
`crates/mem-core/src/graph.rs`.

### Entities (ns 36) — node collection

```rust
#[derive(KeyEncode, ...)]
#[ok_ns(36)]
pub struct EntityKey {
    pub scope: u64,
    pub node_id: u64,
}

#[derive(DocumentEncode, ...)]
#[ok_ref(EntityKey)]
#[ok_ns(36)]
#[ok_index(
    by_kind { fields(kind_id) },
    by_name { fields(name) },
)]
pub struct EntityRow {
    pub kind_id: u16,
    pub literal_num: i64,
    pub name: String,
    pub embedding: Vector<f32>,
}
```

- Key `[scope 8B BE][node_id 8B BE]` — `scope` leads because the corpus is
  per-user (ADR-0003), the same ownership boundary flat memory uses. Ids are
  store-assigned surrogates, recovered on reopen from the last key.
- **A face carries the payload field first and the primary key last**: okm
  joins the key tail itself, and the derive accepts payload fields only (a
  key field inside `fields(...)` is a compile error). The entries land as
  `[kind_id][scope][node_id]` and `[name][scope][node_id]`.
- **Text-first regime**: `name` is variable-width and sits last among the
  indexed fields, so a prefix scan for `alpha` also reaches `alphabet`. An
  exact lookup therefore re-verifies the row's name AND the decoded key's
  scope — the price of keeping the name out of the key.
- `name` is an ATTRIBUTE (never key material); `kind_id` allocates through
  this collection's own PREDICATE VOCABULARY (fixed faces 0x04/0x05, okm ADR-0017
  §3) — separate from the 0x02/0x03 vocabulary its props' names use, so no two
  allocators share one id space.
- `literal_num` is a literal node's integer projection (0 otherwise): a
  declared, fixed-width, ordered field, so a numeric read later stays a range
  scan over a face rather than a vector-distance guess.
- **Entity props merge.** A term's props (the tuple's head/tail props) land
  on the node row's DYNAMIC segment (slot 1) — the entity's attributes,
  shared by every fact about it — and they MERGE: a same-named attribute
  takes the newer value, the rest survive.
- **`embedding` is the node's vector (ADR-0003)**: the built-in engine's
  output over the entity's NAME, computed in-process at ingest — the HTTP
  client never sends a vector. A cold frame of ANY length (its own count
  rides the frame, so a model swap changes the data, not the wire),
  L2-normalized at the source so `cosine` is an inner product. EMPTY is a
  legal row (written with no engine attached) and retrieval skips it — never
  a zero vector.
- **Retrieval (`GraphStore::retrieve`, `GET /graph/{scope}/search?q=&k=` — the
  ANALYSIS class: the landing alone, for inspection; the core answer is
  `recall`, below)**
  is vector landing, not a query language: embed the query, ask the scope's
  DERIVED index for its nearest rows, drop everything under `LANDING_FLOOR`
  (0.35 cosine), return the top `k` seeds `{node_id, score, name, kind}`. An
  empty result is legal (nothing landed — the caller injects no memory). The
  SEARCH is `okm-vector`'s, not k10r's: the index is its `ExactIndex` (one flat
  vector block plus keys), rebuilt from the KV on first use and DROPPED on every
  write, so it can never answer from a stale corpus, and byte-budgeted with LRU
  eviction here because the budget is about k10r's memory, not the search. The
  ANN behind the same call is `okm-vector`'s `VectorIndex` (sharded HNSW, one
  snapshot blob per shard at okm's face 0x0B), taken when ADR-0003's measured
  threshold says so — never a hand-rolled graph.
- **`GraphStore::recall` (`GET /memories/{scope}/recall?q=&k=&budget=&ts_ms=` — the
  CORE class: the memory answer a consumer runs on)**
  is that landing plus the landed COMMUNITY: the seeds' `community_id`s are
  read as buckets (the `by_community` face for members, the traversal faces for
  the facts whose endpoints are both members), ranked by weight (ADR-0005),
  capped at `budget` fact lines, and rendered as id-bearing text — one header,
  one line per node, one line per fact. An empty landing is an empty answer
  (no seeds, no text), never an error. Deeper walks are the caller's
  composition over `out_edges` / `in_edges`.
- **`community_id` and its `by_community` face** belong to ADR-0010's steps
  3–4 (the incremental LPA + binning) and landed with them (2026-10-19): the
  read side consumes the face as the community bucket. ADR-0010 §1 keeps the
  design.

### Facts (ns 37) — edge collection

An okm edge collection (`Graph<FjallStore, FactEdge>`), one ns per graph (okm
ADR-0017 §4). The DECLARATION is the graph — `GraphEncode`: `#[ok_ns(37)]`,
`#[ok_nodes(FactNodes)]` (the member collections — today `Entities`; admitting
another collection is one enum variant plus its ns in `NS_VALUES`, a code
change), the payload's codec,
and one 0x1 face per DECLARED `#[ok_index]`. The handle builds the primary, the
payload's dynamic entry, the predicate vocabulary and the four traversal
faces.

| Face | Key |
|---|---|
| 0x0 primary | `[edge_id 8B BE]` → `[src NodeRef][dst NodeRef][predicate_id][obj payload]` |
| 0x1 dynamic segment | `[edge_id 8B BE]` → the relation's open props (`functional` rides here) |
| 0x1nnn declared index (`since` / `confidence` / `weight`) | `[field value][edge_id]` |
| 0x4/0x5 predicate vocabulary | `[predicate_id]` → name / `[predicate name]` → `predicate_id` |
| 0x6 predicate index | `[predicate_id][edge_id]` |
| 0x7 out-edges | `[src NodeRef][edge_id]` |
| 0x8 in-edges | `[dst NodeRef][edge_id]` |
| 0x9 predicate+out | `[predicate_id][src NodeRef][edge_id]` |
| 0xA predicate+in | `[predicate_id][dst NodeRef][edge_id]` |

- **No scope segment on this side.** The edge collection builds its own keys
  (okm's edge shape), so the ownership boundary rides the ENDPOINTS: a
  `NodeRef` is `[ns 2B][len varint][entity key]`, and the entity key carries
  the scope. A traversal starts at a scoped node and stays in scope; only a
  whole-ns scan (0x6, "every edge of a predicate") crosses scopes, and filtering
  it by endpoints is the caller's job. A per-scope edge prefix would mean
  hand-rolling the collection against okm's `Graph`.
- The three declared fields are the relation props' PROJECTIONS: `since`
  (the caller's clock — the adjudication input), `confidence` (the write
  side's; 255 = absent, i.e. no penalty) and `weight` (the weight system's,
  ADR-0005 — 0 until that system writes it). They are kept OUT of the edge's
  dynamic entry: one datum, one home.
- **The relation's open props ride the edge's dynamic segment** (slot 1 of
  the edge's ns), built with okm's `put_frame_named` and committed in the
  SAME batch as the link (ADR-0002: one WriteBatch = one transaction). The
  per-assertion `functional` flag is kept there — the assertion's own record
  of how it declared its kind's cardinality.
- **The payload codec is the declaration's (LANDED 2026-10-19).** `FactEdge`'s
  payload is an ordinary obj payload (`[version u8][hot_len u16 BE][since u64]
  [confidence u8][weight u32]`), produced and read by the one declaration:
  `encode_payload` / `decode_payload`. k10r's hand-written `from_attrs` and its
  locking test are gone; the adjudication input is
  `FactEdge::decode_payload(&body.payload)`.
- **Dictionary discipline (two vocabularies in one ns) — RETIRED, nothing
  replaces it.** The edge ns used to hold relation kinds (okm's `Graph`,
  long-lived `VocabCache`) AND the prop names k10r allocates; two caches
  allocating in one vocabulary hand the same id to different names, and the
  frame then reads back under the wrong name — silently. okm ADR-0017 §3 split
  the vocabularies (the predicate owns 0x04/0x05; the payload's field names
  keep 0x02/0x03), so the collision is structurally gone; k10r's two
  disciplines — a fresh cache per call, a handle per call — retire with it, and
  so does the read-side priming (okm's `VocabCache::id_get` loads on probe).
  The entity collection rides the same rule — kinds at 0x04/0x05, prop names at
  0x02/0x03.

### Access (mem-core `GraphStore`)

`ingest(scope, fact)` decodes one 8-slot tuple and runs ADR-0010 §1's steps
1–2: both endpoints resolve through `entity_by_name` (created when absent,
allocating the kind), then the relation either links a new edge or — when the
fact declares `functional` — is adjudicated against the incumbent edge of
that kind out of that source (`since >` or `confidence >=` wins: the row is
rewritten in place with the same `edge_id`, otherwise the fact is dropped).
It returns `IngestOutcome::{Linked, Rewritten, Dropped}`.

Reads: `entity(scope, node_id)`, `entity_by_name(scope, name)` →
`(node_id, row)`, `entity_kind(kind_id)` → the token, `entity_props` /
`fact_props` (the dynamic segments), `fact(edge_id)` → the decoded edge,
`out_edges` and `typed_out` (the traversal currency). For a bulk consumer
(the visualization door) `scope_entities(scope)` and `scope_facts(scope)`
return one scope whole — the scan is bounded by the primary-key prefix
`[scope]`, and each edge is reached once because `ingest` puts both
endpoints in the scope it was called with. A functional rewrite is ONE engine
batch — okm's `unlink_into` + `link_into` plus k10r's own dynamic-segment
entry, same `edge_id`, no window between two commits (ADR-0010, paid
2026-10-19).

### Storage assembly

One Fjall **database** per process (one directory, one lock file), and
**two keyspaces**: `channel` (the log is queue-like and takes the store's
highest write rate, so it gets its own LSM tree and its compaction never
drags the other tables along) and `memory` (flat memory + code + the fact
graph, and later the skills — those tables are discriminated by ns).
`crate::service::open_service` opens the database and attaches each group
with `FjallStore::from_db`. Fjall locks the directory, so a second
`FjallStore::open` on it fails — the keyspace attach is what lets the
groups share one directory and one lock. This is how a process arranges
its tables; k10r stays what ADR-0007 made it (a standalone service with
its own storage, reached only over HTTP).

One consequence worth knowing: Fjall's batch can span keyspaces
(`WriteBatch::insert(&keyspace, …)`, one commit = one WAL write), but
okm's `commit_batch` writes every op into the store handle it was called
on — i.e. into ONE keyspace. A batch that mixed channel ops with code ops
would land them in whichever keyspace received the call, so batches stay
within one keyspace (reads and writes never needed to span the two).
