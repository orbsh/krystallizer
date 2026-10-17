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
| 32  | Flat memory   | Implemented (Phase 0)   | ADR-0002 |
| 33  | Channel messages | Implemented (minimal slice, 2026-10-09) | ADR-0008 |
| 34  | Checkpoints (per member) | Implemented (minimal slice, 2026-10-09) | ADR-0008 |
| 35  | Code (delivered scripts) | Implemented (bring-up slice, 2026-10-15) | gravity ADR-0001/0003 |
| —   | Session messages | superseded by the channel log | ADR-0008 |
| —   | Tasks / task_links DAG | Declared (ADR-0001 — the third interface surface; same graph as the facts) | ADR-0001, ADR-0002 |
| 36  | Entities (fact graph) | Implemented (2026-10-17) | ADR-0010 |
| 37  | Facts (fact graph edges) | Implemented (2026-10-17) | ADR-0010 |
| —   | Initialization memory (persona / style) | Declared (ADR-0001 | ADR-0001 |

The graph's read side (LPA, binning, the capsules), the session log's
missing slices, and the tasks land in later phases; a row above marked
`Declared` is an ADR commitment rather than working code.

## Memory kinds (who initiates)

Three kinds, split by who drives them (ADR-0001; the
naming is 被动 / 主动, with 短期 / 长期 as the content horizon:

- **被动 (short-term)** — the current context: the channel log (ns 33) plus
  its per-member projection (ns 34). The loop fetches and appends; never a
  tool.
- **主动 (long-term)** — facts outside the context, reached as a TOOL by
  any agent. The carrier is the fact graph; flat memory (ns 32) is its
  pre-graph form — the same knowledge held as text rows, which is why it goes
  away when the graph lands (a text blob has no per-fact identity: it cannot be
  updated, weighted or enumerated per fact). Raw content never needed it — the
  channel log (ns 33) already holds every message.
- **初始化 (persona / style)** — graph-derived: a traversal from the user
  node, delivered at the head of the projection. No ns assigned yet.

## Flat memory (ns 32)

Implemented (Phase 1a) — the pre-graph carrier of extracted long-term memory, withdrawn when the fact graph lands (ADR-0001's "flat memory, then the graph"; PLAN Phase 3). Declared in `crates/mem-core/src/memory.rs`.

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

### Row: `MemoryRow` (`RowEncode`)

```rust
#[derive(RowEncode, ...)]
#[kv_ref(MemoryKey)]
#[kv_ns(32)] // the row is the table's declaration point
pub struct MemoryRow {
    pub text: String,       // cold TLV segment
    pub created_at: u64,    // hot segment (fixed width)
}
```

- ns 32 is declared on the row (`#[kv_ns]`); the key type carries none
  (new OKM API: `Table::new(store)` takes no ns; assembly uses
  `MemoryRow::table(store)`).
- Payload: fixed-width fields contiguous (hot), variable text as TLV
  (cold). Layout version defaults to 1.
- No `#[kv_index]` access methods yet — retrieval is the Phase 0
  brute-force substring scan (`scan_keys` + filter). This is the
  declared bring-up posture (ADR-0003), not the end state; Phase 1
  adds the built-in local embedding model (vectors as `Vector<f32>`
  frames, searched from an in-memory table) + name/alias point lookup
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

Mandatory-per-OKM discipline: every entity's queries must be declared
access methods, not scans. Current standing:

- Flat memory (ns 32): the user-prefix scan (key prefix). The table is the
  PRE-GRAPH carrier of long-term memory and is withdrawn with the graph
  (ADR-0001, PLAN Phase 3), so the vector landing and the name/alias point
  lookup ADR-0003 specifies are declared on the graph's collections, not
  here.
- Channel messages (ns 33): prefix scan by `[channel_id][ts_ms][sub]` —
  replay, increment and max-gap reads are one prefix scan (ADR-0008 §3).
  Checkpoints (ns 34): point reads by `[channel_id][member_id]`.
- Fact graph: an OKM node collection (entities) plus an edge collection
  (facts) with declared faces, per ADR-0010 §1 — NOT an
  `edge:{src}:{label}:{dst}` key layout (`EdgeTable` is a former name; the
  layer above is `Graph<S, E: KvGraph>` + `NodeRef` + the kind dictionary,
  okm ADR-0017). The weight secondary index is the edge collection's
  `weight` declared field's face (ADR-0005, ADR-0010 §1).

## Hex stability

Key layout is locked by `key_layout_hex_stability`
(`crates/mem-core/tests/smoke_test.rs`): `[user_id 8B BE][id 8B BE]`,
ns header excluded (`KeyEncode::encode()` returns payload without the
ns; the Table prepends `[ns 2B][slot 1B]`).

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
    by_community { fields(community_id) },
)]
pub struct EntityRow {
    pub kind_id: u16,
    pub community_id: u64,
    pub literal_num: i64,
    pub name: String,
}
```

- Key `[scope 8B BE][node_id 8B BE]` — `scope` leads because the corpus is
  per-user (ADR-0003), the same ownership boundary flat memory uses. Ids are
  store-assigned surrogates, recovered on reopen from the last key.
- **A face carries the payload field first and the primary key last**: okm
  joins the key tail itself, and the derive accepts payload fields only (a
  key field inside `fields(...)` is a compile error). The entries land as
  `[kind_id][scope][node_id]`, `[name][scope][node_id]` and
  `[community_id][scope][node_id]` — ADR-0010 §1's three faces.
- **Text-first regime**: `name` is variable-width and sits last among the
  indexed fields, so a prefix scan for `alpha` also reaches `alphabet`. An
  exact lookup therefore re-verifies the row's name AND the decoded key's
  scope — the price of keeping the name out of the key.
- `name` is an ATTRIBUTE (never key material); `kind_id` allocates through
  this collection's own 0x2/0x3 dictionary — the same vocabulary dynamic
  field names use (harmless: an id is only ever read back through the field
  it was written into).
- `literal_num` is a literal node's integer projection (0 otherwise): a
  declared, fixed-width, ordered field, so a numeric read later stays a range
  scan over a face rather than a vector-distance guess.
- **Entity props merge.** A term's props (the tuple's head/tail props) land
  on the node row's DYNAMIC segment (slot 1) — the entity's attributes,
  shared by every fact about it — and they MERGE: a same-named attribute
  takes the newer value, the rest survive.
- `community_id` is written by ADR-0010's LPA pass (steps 3–4, not landed);
  `0` is a legal community and means "not binned yet" (the `UNBINNED`
  constant).

### Facts (ns 37) — edge collection

An okm edge collection (`Graph<FjallStore, FactEdge>`), one ns per graph (okm
ADR-0017). The derive supplies `NS_PREFIX`, `ATTR_SLOTS`, `attrs()` and one
0x1 face per declared field; okm builds the primary and the four traversal
faces itself.

| Face | Key |
|---|---|
| 0x0 primary | `[edge_id 8B BE]` → `[src NodeRef][dst NodeRef][kind_id][attrs]` |
| 0x1 declared field (`since` / `confidence` / `weight`) | `[field value][edge_id]` |
| 0x4 kind index | `[kind_id][edge_id]` |
| 0x5 out-edges | `[src NodeRef][edge_id]` |
| 0x6 in-edges | `[dst NodeRef][edge_id]` |
| 0x7 kind+out | `[kind_id][src NodeRef][edge_id]` |
| 0x8 kind+in | `[kind_id][dst NodeRef][edge_id]` |

- **No scope segment on this side.** The edge collection builds its own keys
  (okm's edge shape), so the ownership boundary rides the ENDPOINTS: a
  `NodeRef` is `[ns 2B][len varint][entity key]`, and the entity key carries
  the scope. A traversal starts at a scoped node and stays in scope; only a
  whole-ns scan (0x4, "every edge of kind K") crosses scopes, and filtering
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
- **`from_attrs` is k10r's** — `[since u64][confidence u8][weight u32]`, 13
  bytes in the derive's declaration order. The `EdgeEncode` derive emits the
  writer half only, so the reader carries the widths and a test locks it
  against `attrs()`.
- **Dictionary discipline (two vocabularies in one ns).** The edge ns holds
  relation kinds (allocated by okm's `Graph` through its own long-lived
  `DictCache`) AND the prop names k10r allocates. Two caches allocating in
  one dictionary hand the same id to different names, and the frame then
  reads back under the wrong name — silently. k10r therefore allocates
  through a fresh cache per call, builds the edge handle per call, and primes
  it (`kind_name`) before a typed read, because `Graph::typed_*` /
  `edges_of_kind` answer from the non-allocating `by_name_get` and never fall
  back to the engine. The owed okm-side items are in ADR-0010's owed list.

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
`out_edges` and `typed_out` (the traversal currency). A functional rewrite is
`unlink` + `link` — two engine commits, because okm's `unlink` has no
into-batch form (owed, ADR-0010).

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
