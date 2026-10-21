# Krystallizer Data Modeling

The memory core's schemas in one place: what tables exist today, the key
and row shapes, and which access methods are declared. Generic OKM
modeling rules (four layers, access-method discipline, key-tail rules)
are NOT repeated here — they live in okm's
[MODELING.md](https://github.com/orbsh/okm/blob/main/docs/MODELING.md) and are normative for this
repo too. Design decisions behind each table live in the ADRs.

> **Languages:** [English](MODELING.md) (primary) · [中文](MODELING.zh-CN.md)

## Tables

One entity class = one ns (OKM rule). Current state:

| ns  | Entity        | Status                  | ADR      |
|-----|---------------|-------------------------|----------|
| 33  | Channel messages | Implemented (minimal slice, 2026-10-09) | ADR-0008 |
| 34  | Checkpoint (channel pointer → summary + persona snapshot) | Implemented (minimal slice, 2026-10-09) | ADR-0008 |
| 35  | Code (delivered scripts) | Implemented (bring-up slice, 2026-10-15) | gravity ADR-0001/0003 |
| —   | Tasks / task_links DAG | Declared (ADR-0001 — the third interface surface; same graph as the facts) | ADR-0001, ADR-0002 |
| 36  | Entities (fact graph) | Implemented (2026-10-17) | ADR-0010 |
| 37  | Facts (fact graph edges) | Implemented (2026-10-17) | ADR-0010 |
| 38  | Community sequence (the id issuer, one row per scope) | Implemented (2026-10-20) | ADR-0010 |
| 39  | Read cursor (a channel pointer; multi-party only) | Implemented (2026-10-20) | ADR-0008 §2 |
| 41  | Structured log (diagnostics; internal, no door) | Implemented (2026-10-20) | ADR-0011 |
| 42  | Channel members (identity: display name + human/agent), and the graph's user node set | Implemented (2026-10-20; graph participation 2026-10-21) | ADR-0009 §1, ADR-0014 |
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
  its checkpoint (ns 34: a pointer into the log + that span's summary). The loop
  fetches and appends; never a tool.
- **主动 (long-term)** — facts outside the context, reached as a TOOL by
  any agent. The carrier is the fact graph (ns 36/37). Raw conversation is not
  this kind's business: the channel log (ns 33) already holds every message.
- **初始化 (persona / style)** — graph-derived: a traversal from the user
  node (the member's node, ns 42), delivered at the head of the projection. No
  ns assigned yet.

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

- Channel messages (ns 33): a **named face** — prefix scan by
  `[channel_id][ts_ms][sub]`: replay, increment and max-gap reads are one prefix
  scan, and the interval rides the main table's own sort segment (sequential, no
  fetch-back), per ADR-0008 §3. The checkpoint (ns 34) is read in KEY ORDER —
  `[channel_id][ts_ms][sub]`, newest = the last key of one prefix range — so
  "the latest compression" costs no sort; ns 40 (the withdrawn decision log) is
  free.
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
`edge_payload_layout_is_locked` in `tests/graph.rs`. ns header excluded (`KeyEncode::encode()` returns
payload without the ns; the Table prepends `[ns 2B][slot 1B]`).

## Channel log (ns 33)

The append-only message table plus the three tables the projection and the
compression engine read (ADR-0008). Declared in
`crates/mem-core/src/channel.rs`.

**No member dimension, and no third table.** The two tables here belong to the
CHANNEL, not to a reader: the checkpoint (the channel's pointer + that span's
summary + the persona snapshot) and — only in the MULTI-PARTY shape — the read
cursor. One log has no second compression subject: an agent's boundary moves with
its long-term memory, and one channel has one voice, so a member segment could
only be a constant, and a constant is not a dimension (ADR-0008 §2 and its Why
Not). A MESSAGE has no member either: a message belongs to the log. A member is a
SENDER (display name / role, Phase 2.7) — a ROW FIELD plus serialization
attribution, never a key here. The decision log that used to sit beside these was
WITHDRAWN (2026-10-20): the checkpoint row already answers "which span was
compressed", and the parameter snapshots it kept had no reader (no labels → no
fitting). `DEFAULT_MEMBER = 1` is the placeholder the code still carries and is
on its way out.

### `MessageKey` / `MessageRow` (ns 33)

```rust
#[derive(KeyEncode, ...)]
#[ok_ns(33)]
pub struct MessageKey {
    pub channel_id: u64,
    pub ts_ms: u64,
    pub sub: u16,
}
```

- Key `[channel_id 8B][ts_ms 8B][sub 2B]`, BE fixed width, so byte order IS
  `(ts_ms, sub)` order: replay, the increment and the max-gap read are one
  prefix scan in the log's own sort order — no sort step, no secondary index.
- `ts_ms` is the CALLER's clock (ADR-0008 §3); `sub` breaks ties within one
  millisecond (one writer, sequential appends).
- Row: `role u8` (user/assistant/tool/system), `kind u8` (text;
  tool_call/tool_result reserved for view-layer pruning), `content String`.
  Storage always holds the full row — pruning is the view's job (ADR-0006).

### `CheckpointKey` / `CheckpointRow` (ns 34) — the channel's pointer

- Key `[channel_id 8B][ts_ms 8B][sub 2B]` — the KEY IS THE POINTER: one row per
  compression, naming the log record its summary ends at. Reads take the
  HIGHEST-keyed row that carries a summary — byte order is `(ts, sub)` order, so
  "the latest compression" is a key comparison, never a sort.
- Row: `summary String` (EMPTY = a compression in flight; that row is what tells
  the engine to propose nothing on the next read) and `prompt_snapshot String`
  (the initial prompt / persona text as of that compression — stored, not
  recomputed, so the context prefix stays byte-stable and the prompt cache
  holds).
- Rows accumulate instead of overwriting: a span compressed twice writes its own
  rows at the same pointer (same read result, one wasted model call), and a
  summary returning for an EARLIER pointer lands below the newest row —
  invisible to reads, kept as free history, never deleted.

### `CursorKey` / `CursorRow` (ns 39)

- Key `[channel_id 8B][member_id 8B]` — a participant's READ CLOCK, stamped at a
  projection read: the cache clock's only observable. A row exists only in the
  MULTI-PARTY shape; with a single user + agent nothing is stored and the idle
  time is "now − the last record's ts" outright. Latest-wins, and an earlier read
  leaves it alone — the clock never rewinds. No POSITION is kept: every read
  answers up to the newest record, and the checkpoint is what cuts the increment.
- Row: `read_at u64` — the caller's clock at that read, and nothing else. The
  model's read answers up to the newest record, so a stored position would be a
  constant; what the cache clock needs is when this member last touched the model.

### Access (`ChannelStore`)

| Call | What it does |
|---|---|
| `append(channel, ts_ms, sub, role, kind, content)` | one log entry; returns its key |
| `fetch_full(channel, member)` | `SessionView { coverage, summary, head, compressing, increment, last_read }` — summary/head/cut point from the newest node that HAS a summary; an empty-summary node above it only sets `compressing` |
| `open_checkpoint(channel, pointer, head)` | put the EMPTY-summary node at the split (the "compressing" marker) carrying the frozen head edge ids; the read keeps answering from the node below |
| `complete_checkpoint(channel, summary)` | fill the newest open node — the only checkpoint write a door makes |
| `append_checkpoint(channel, pointer, summary, head)` | raw single-shot write (tests, backfill) — the door does NOT call it |
| `channel::member_node(channel, member)` | the graph node name a member's facts hang on — `member-{channel}-{member}`, a rule not a table (a rename moves no fact) |
| `note_read(channel, member, read_at)` / `read_at` | stamp / read the read clock (a cursor row is the multi-party shape's; no position is stored) |

`view::assemble(channels, graph, channel, member, scope)` returns
`Projection { head, view }` — the head is the user node's incident facts
(36/37), which the door renders per `?format=` before it goes on the wire.

Not yet: threads, topic tags, the members table (ADR-0008/0009) — that table is
what finally gives the cursor its people.

The CODE follows the model (2.6d-i/ii landed 2026-10-20): the key IS the pointer,
nodes accumulate, `open_checkpoint` / `complete_checkpoint` are the two writes, ns 40
is gone, and the head is the frozen edge-id set. What is still owed is the cursor's
second segment (2.6d-iii: `ts_ms + sub`, and only the multi-party shape stores a row).

## Structured log (ns 41) — diagnostics, no door

ADR-0011. `crates/mem-core/src/log.rs` holds the table and the store. The first
writer is the compression verdict — `record_verdict(now_ms, channel, &verdict)`
writes `channel` / `split_ts` / `split_sub` / `trigger` (a word) /
`assembled_bytes` plus whichever signals fired, keyed by K10R's own clock; a
signal that did not fire writes no field. Reads are windows (`since(ts_ms)`), not
a query language.

The field shape (ADR-0011 §8, landed 2026-10-20): `event` and `source`
become `Vocab<u16>` — a closed, small, endlessly repeated set is what a vocabulary
is for, two bytes cannot be misspelled, and the typo moves from a row nobody can
group by to a compile error — while `note Str` stays the one free-text field and
`level u8` stays a tiny ordered domain the code compares. Interning is the TYPE's
business, not the store's: okm interns on write (`Collection::put` → `__okm_intern`)
and resolves the label on read (`get` → `__okm_deref_vocab`), so `append` still
takes `&str`, builds `Vocab::of(..)` internally, and no call site changes. The cost
is one document clone per write on a table that declares a Vocab field.
`LogStore::row(key)` returns the declared fields — the map read gives the dynamic
segment — so a reader gets the level without walking names. PLAN 2.9c.

### `LogKey` / `LogRow` (ns 41)

```rust
pub struct LogKey { pub ts_ms: u64, pub seq: u64 }
pub struct LogRow { pub level: u8, pub event: Vocab<u16>,
                    pub source: Vocab<u16>, pub note: String }   // + the dynamic segment
```

- Key `[ts_ms 8B][seq 8B]` — BE fixed width, so byte order IS time order: "the
  last N rows", "the window around T" and a replay are prefix/range scans.
  `ts_ms` is k10r's OWN clock (the writer is the process; ADR-0008 §3's
  caller-clock rule governs caller data, not the process's observations). `seq`
  is a process-local monotonic counter seeded at open from its `seq` watermark — a
  declared `HighWater(seq)` reduce, fold = highest ever written, unfold a NO-OP —
  so a restart continues instead of overwriting, and a trim cannot hand a number
  out twice.
- Row: `level u8` (trace/debug/info/warn/error), `event Vocab<u16>`,
  `source Vocab<u16>` (module or access path; empty = none) and `note Str` (the one
  free-text field). The labels come from constants in `log::event` / `log::source`
  and no field carries a 0x1 face — a face is one extra index write per row for a
  query nobody issues yet; add it when a query turns hot.
- Everything variable rides slot 1's dynamic segment: the LOCATOR (`channel` /
  `scope` / `entity` / `edge` / `msg`, whichever applies) and the PAYLOAD
  (parameter snapshots, measured signals, the split point, error text). Nested
  values go through `put_frame_named`.
- Bounded by `logs { max-rows }`: a write trims oldest-first in the SAME batch,
  and the trim is never logged (trimming would emit rows that trigger trimming).

## Code store (ns 35)

The delivered script's home — the one table an executor's call reads.
Declared in `crates/mem-core/src/code.rs`; the design is gravity's
(ADR-0001/0003: the node mount reads a script that travels as a call
argument), and this repo's own decision is ADR-0012 — a bare surrogate id
(no name, no hash), the language as data that never defaults, and a row
that is code rather than a skill.

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
`code_id` (a surrogate id: recovered on reopen from its `HighWater(code_id)` watermark, so a withdrawn row never frees its id); `get(code_id)` → the row, or absent for an
unknown id; `list()` → `(code_id, language, description)` in id order —
what a resolver or a linker picks from, and the declared bring-up
posture (a whole-table scan that the graph's retrieval replaces,
ADR-0003/0018). Source and parameter definition stay out of `list()`.

## Fact graph (ns 36 entities, ns 37 facts)

**What `scope` is.** One user's memory corpus. Every graph table below puts it
in the FIRST key segment (`u64` user identity; a single-user deployment is `1`,
i.e. `view::DEFAULT_SCOPE`), and it is the boundary of retrieval and traversal:
one scope's corpus goes into one scope's index, and a bucket read stays inside
one scope. The channel side has no scope — a channel already IS one scope's
conversation.

The 主动 (long-term) side's carrier: okm node collections (entities, plus the
members table as the user node set — ADR-0014) and an okm edge collection, and the
write-time resolution that lands one fact (ADR-0010 §1; landed 2026-10-17).
Declared in `crates/mem-core/src/graph.rs`; the members table it references is
declared in `crates/mem-core/src/channel.rs`.

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
    by_kind { fields(kind) },        // the node-kind face
    by_name { fields(name) },
)]
pub struct EntityRow {
    pub kind: Vocab<u16>,            // a categorical field: interned on write
    pub literal_num: i64,
    pub name: String,
    pub embedding: Vector<f32>, // NO #[ok_index]: a vector has no key order, see below
}
```

**A vector declares no face.** An `#[ok_index]` face is a derived entry for
key-ordered reads (prefix / range / covering scans); a vector has no key order,
so there is no face to declare for it. Vector search runs on `okm-vector`'s
`ExactIndex` — a DERIVED index rebuilt from the rows' `embedding` frames on
first use and dropped on every write, not a declared face.

- Key `[scope 8B BE][node_id 8B BE]` — `scope` leads because the corpus is
  per-user (ADR-0003). Ids are
  store-assigned surrogates, recovered on reopen from the last key.
- **A face carries the payload field first and the primary key last**: okm
  joins the key tail itself, and the derive accepts payload fields only (a
  key field inside `fields(...)` is a compile error). The entries land as
  `[kind][scope][node_id]` and `[name][scope][node_id]`.
- **Text-first regime**: `name` is variable-width and sits last among the
  indexed fields, so a prefix scan for `alpha` also reaches `alphabet`. An
  exact lookup therefore re-verifies the row's name AND the decoded key's
  scope — the price of keeping the name out of the key.
- `name` is an ATTRIBUTE (never key material); the kind is a CATEGORICAL FIELD —
  `kind: Vocab<u16>` (okm ADR-0032): `put` interns the label on write and `get`
  resolves it back through that field's own data vocabulary, so a kind needs no
  allocation code and no reserved faces. The field is named `kind`, not
  `kind_id`: the value IS the label and the id is only its encoding. The layout
  is version 3 (the field's meaning changed, its wire width did not: two bytes).
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
  eviction here because the budget is about k10r's memory, not the search. Whether
  that index is exact or approximate is `okm-vector`'s own call — the same crate
  already carries any such form — never a k10r phase, a threshold parameter, or
  a hand-rolled graph here (ADR-0003).
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

### Users (ns 42) — the identity collection, as a node set

The one node collection k10r does NOT declare itself: `MemberRow` (ns 42,
`crates/mem-core/src/channel.rs`) is the identity layer ADR-0009 §1 put on the
channel side — `[channel_id][member_id]` → display name + human/agent. ADR-0014
makes it a graph participant rather than a proxy row:

```rust
#[derive(KeyEncode, ...)]
#[ok_ns(42)]
pub struct MemberKey { pub channel_id: u64, pub member_id: u64 }

#[derive(DocumentEncode, ...)]
#[ok_ref(MemberKey)]
#[ok_ns(42)]
pub struct MemberRow { pub display_name: String, pub role: u8 }
```

- **CLOSED type**: "user" is one type, declared once, so the collection is the
  type marker and the row needs no `kind_id`. Entities keep the open-kind shape
  because their kinds arrive at ingest; people are registered, not discovered.
- **The key needs no change to become a ref**: `[42][len][channel_id][member_id]`
  is a self-describing endpoint like any other. One type name joins the edge's
  `#[ok_nodes(EntityRow, MemberRow)]` and nothing else in the declaration moves:
  the member-set enum and its ns list are generated from those tables.
- **No faces, no vector.** The only readers are a point read (the key) and one
  channel's members (a primary-key prefix scan); a declared face would have no
  scanner of its own. And with no `embedding` — in a different collection — a user
  is never a landing seed: retrieval lands on knowledge, never on people.
- **The reference syntax is `member-{channel}-{member}`**, parsed by `ingest` when
  the tuple's type is the reserved token `user` (ADR-0004's open vocabulary, second
  reserved token after `literal`). The string is wire, never a stored name, so a
  rename cannot move a fact. An unregistered member fails the fact — nothing is
  created.
- **The row is read across keyspaces**: members live in the `channel` keyspace,
  the facts in `memory`. A point read through the channel handle is fine; a fact's
  batch never spans the two (okm's `commit_batch` writes into the handle it was
  called on).

### Facts (ns 37) — edge collection

One fact = two nodes + one edge; this is the EDGE table (ADR-0010 §1). Declared:

```rust
#[derive(GraphEncode, ...)]
#[ok_ns(37)]
#[ok_nodes(EntityRow, MemberRow)]   // the member tables: entities (36), people (42)
#[ok_index(
    by_since { fields(since) },              // two faces, each naming its reader
    by_scope_weight { fields(scope, weight) },  // the scope-prefixed ranking (PLAN 4a)
)]
pub struct FactEdge { /* scope / since / confidence / weight / five counters */ }
```

- **The edge collection builds its own keys, so the KEY carries no scope
  segment.** The ROW does carry a `scope` — a denormalized copy of its source
  endpoint's scope, written once — because a face can only be probed by what the
  index encodes, and the weight ranking is asked per scope (PLAN 4a). The endpoints
  stay the authority for membership; the copy is what makes the face usable.
  Scope otherwise rides the ENDPOINTS: an endpoint is a reference to
  `(ns, primary key)`, and an entity's primary key carries the scope. Traversal
  starts at a scoped node, so it stays inside one scope; only a whole-table scan
  ("every edge of one predicate") crosses scopes, and filtering by endpoint is
  the caller's composition. The USER endpoint is the one unscoped endpoint: a
  member is store-global (a channel belongs to one scope), and the edge's own
  denormalized `scope` is what keeps it in the scope the fact was written to.
  Putting a per-scope prefix in the edge key would mean
  hand-writing this collection instead of using the library's edge graph — not
  worth it.
- The declared fields are PROJECTIONS of the relation's props (one datum, one
  home): `since` (the caller's clock, the adjudication input), `confidence`
  (write-side; 255 = absent, i.e. no penalty) and `weight` (the weight system's
  milli-units), plus the five counters the weight system folds in (listed with
  the payload fields above).
- **A relation's open props ride the edge's dynamic segment**, committed in the
  same batch as the link (ADR-0002: one WriteBatch = one transaction). The
  per-assertion `functional` flag stays there — the assertion's own record of how
  it declared its kind's cardinality.
- Face numbers, vocabularies and the dynamic segment's frame construction are
  **okm's internals** and are not repeated here: see okm's
  [MODELING](https://github.com/orbsh/okm/blob/main/docs/MODELING.md). This page
  registers what k10r itself declares (ns, key fields, declared faces, payload
  fields) and the one conclusion it depends on.

### Access (mem-core `GraphStore`)

`ingest(scope, fact)` decodes one 8-slot tuple and runs ADR-0010 §1's steps
1–2: an endpoint resolves by its type slot — `entity_by_name` for an ordinary
type (created when absent, allocating the kind), and for the reserved token
`user` a PARSE of `member-{channel}-{member}` into a `NodeRef` at ns 42
(ADR-0014: nothing is created, an unregistered member fails the fact). Then the
relation either links a new edge or — when the
fact declares `functional` — is adjudicated against the incumbent edge of
that kind out of that source (`since >` or `confidence >=` wins: the row is
rewritten in place with the same `edge_id`, otherwise the fact is dropped).
It returns `IngestOutcome::{Linked, Rewritten, Dropped}`.

Reads: `entity(scope, node_id)`, `entity_by_name(scope, name)` →
`(node_id, row)` (the kind's label rides the row — `row.kind.as_str()`),
`entity_props` / `fact_props` (the dynamic segments), `fact(edge_id)` → the
decoded edge,
`fact_in_scope(scope, edge_id)` → the deterministic point read (see below),
`neighbours(scope, node_id)` → both directions out of one node (degree-cost, both
sides are ref lists on the node rows), `anchored(scope, node_id)` → that
neighbourhood then the node's community slice, `timeline(scope, limit)` → facts by
`since`, newest first, walked over the `by_since` face (scope-blind, so other
scopes' keys are skipped), `chain(scope, from, kind, hops)` → one predicate followed
hop by hop,
`out_edges` and `typed_out` (the traversal currency), plus their
ref-taking forms `out_edges_of` / `in_edges_of` / `typed_out_of` and
`user_ref(channel, member)` — the entry a USER-anchored walk starts from
(ADR-0014: identity is not scoped to the corpus, so those take a `NodeRef`),
and `resolve_ref` answers a `NodeId` (`Entity { scope, node_id }` or
`User { channel, member }`), never a bare id. For a bulk consumer
(the visualization door) `scope_entities(scope)` and `scope_facts(scope)`
return one scope whole — the scan is bounded by the primary-key prefix
`[scope]`, and each edge is reached once because `ingest` puts both
endpoints in the scope it was called with. A functional rewrite is ONE engine
batch — okm's `unlink_into` + `link_into` plus k10r's own dynamic-segment
entry, same `edge_id`, no window between two commits (ADR-0010, paid
2026-10-19).

Three verbs, and the doors mirror them: `ingest` LINKS (or rewrites in place
when the relation is functional), `withdraw(scope, edge_id)` removes one fact —
row and every derived face entry in one batch (okm's `unlink`) — and
`fact_in_scope(scope, edge_id)` is the deterministic read that answers with the
same `edge_id` the write returned. Nothing in that loop needs a search: a
reference is an id.

**Reference links.** A fact names another fact's id in its props under `ref` —
one id or a list — when the other one is not central enough to sit in the head;
the reader then decides whether to follow it (`fact_in_scope`, one id) or not.
`FactView.refs` carries the ids, the doors carry them as `refs`, and the renderers
print them as addresses (`· refs=#41,#57`): a pointer the reader may take, never a
subtree the writer forces in — the shape skills use one level up. Nothing
validates the target: a dangling link is the reader's 404, and a `ref` that is not
an id is not a reference at all.

### Storage assembly

One Fjall **database** per process (one directory, one lock file), and
**two keyspaces**: `channel` (the log is queue-like and takes the store's
highest write rate, so it gets its own LSM tree and its compaction never
drags the other tables along) and `memory` (code + the fact
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
