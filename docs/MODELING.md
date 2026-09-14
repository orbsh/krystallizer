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
| —   | Session messages | Declared (ADR-0002) | ADR-0001, 0002 |
| —   | Tasks / task_links DAG | Declared (ADR-0002) | ADR-0002 |
| —   | Fact graph edges | Declared (ADR-0002) | ADR-0002 |

Session, tasks, and the fact graph land in later phases; their shapes
below are the ADR commitments, not working code.

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
  adds BM25 inverted index (a function index over tokens) + arroy
  vectors + weight metadata (ADR-0005).

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
  Phase 1 commitments: user-prefix scan (key prefix), token function
  index (BM25 postings), weight secondary index `W:{weight}:{edge_id}`
  (ADR-0005).
- Session messages: prefix scan by `[user_id][session_id]` with `seq`
  in the key tail — replay/range reads are one prefix scan (ADR-0002).
- Fact graph: `edge:{src}:{label}:{dst}` + EdgeTable bidirectional
  adjacency (ADR-0002), weight index per ADR-0005.

## Hex stability

Key layout is locked by `key_layout_hex_stability`
(`crates/mem-core/tests/smoke_test.rs`): `[user_id 8B BE][id 8B BE]`,
ns header excluded (`KeyEncode::encode()` returns payload without the
ns; the Table prepends `[ns 2B][slot 1B]`).
