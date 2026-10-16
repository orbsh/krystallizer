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
| —   | Tasks / task_links DAG | Declared (ADR-0001 Update 2026-10-16 — the third interface surface; same graph as the facts) | ADR-0001, ADR-0002 |
| —   | Fact graph edges | Declared (ADR-0002) | ADR-0002 |
| —   | Initialization memory (persona / style) | Declared (ADR-0001 Update 2026-10-16) | ADR-0001 |

Session, tasks, and the fact graph land in later phases; their shapes
below are the ADR commitments, not working code.

## Memory kinds (who initiates)

Three kinds, split by who drives them (ADR-0001 Update 2026-10-16); the
naming is 被动 / 主动, with 短期 / 长期 as the content horizon:

- **被动 (short-term)** — the current context: the channel log (ns 33) plus
  its per-member projection (ns 34). The loop fetches and appends; never a
  tool.
- **主动 (long-term)** — facts outside the context: flat memory (ns 32)
  today, the fact graph once it lands. Reached as a TOOL, by any agent.
- **初始化 (persona / style)** — graph-derived: a traversal from the user
  node, delivered at the head of the projection. No ns assigned yet.

## Flat memory (ns 32)

The only implemented table. Declared in `crates/mem-core/src/memory.rs`.

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
  index / BM25 / RRF / arroy) is withdrawn — ADR-0003 Update
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

- Flat memory: none declared (brute-force scan is the Phase 0 posture).
  Phase 1 commitments: user-prefix scan (key prefix), name/alias point
  lookup (entity identity), embedding vectors (`Vector<f32>` frames +
  an in-memory search table), weight secondary index
  `W:{weight}:{edge_id}` (ADR-0005).
- Session messages: prefix scan by `[user_id][session_id]` with `seq`
  in the key tail — replay/range reads are one prefix scan (ADR-0002).
- Fact graph: `edge:{src}:{label}:{dst}` + EdgeTable bidirectional
  adjacency (ADR-0002), weight index per ADR-0005.

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

### Storage assembly

One Fjall **database** per process (one directory, one lock file), and
**two keyspaces**: `channel` (the log is queue-like and takes the store's
highest write rate, so it gets its own LSM tree and its compaction never
drags the other tables along) and `memory` (flat memory + code, and later
the fact graph and the skills — those tables are discriminated by ns).
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
