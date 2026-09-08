# ADR-0002: Storage — Fjall + OKM Declarative Encoding, PG Exit and Migration

**Status**: Accepted (design; implementation pending)
**Date**: 2026-09-08
**Chinese**: [0002-storage-fjall-okm.zh-CN.md](0002-storage-fjall-okm.zh-CN.md)

## Context

The legacy mem module runs on PostgreSQL (ParadeDB) with embedding computed
in-process by plpython3u triggers. That architecture's reason to exist was
"keep computation next to the data" — the data lived in PG, so embedding ran
inside PG at 1 RTT. Once the whole memory system moves into the application
process (Rust), that tradeoff inverts: the closest place to the data *is*
the process, and PG adds a network hop, runtime DDL locks, SQL parsing
taxes, and plugin version coupling (pgvector / pg_search) — for a dataset
that is a per-user corpus in the tens of thousands of records.

The graph-memory design (wiki) already specifies the KV encoding pattern
for the atomic-fact graph; the remaining flat memory, session, and task
tables are CRUD-shaped and map directly.

## Decision

**Fjall is the storage engine. All schemas are declared with OKM derive
macros.**

- Fixed-width keys for identity (`session_id`, `user_id` hashed to u64,
  table ns) — compile-time `KEY_LEN`, hex stability tests lock the layout.
- Payloads as TLV via `RowEncode`; variable-length content (message text,
  fact summary) as `String` payload fields, never in keys.
- Session messages: key `(ns, user_id, session_id, seq)` — ordering by a
  per-session monotonic seq assigned in the same WriteBatch as the insert
  (not wall-clock timestamps: two messages in the same millisecond must not
  reorder). The prefix-checkpoint model requires stable total order.
- Tasks: `(task_id, seq)` composite as in the legacy PG schema; task_links
  DAG edges via OKM `EdgeTable` (direction-bit header).
- Atomic-fact graph: `edge:{src}:{label}:{dst}` per the wiki encoding, OKM
  `EdgeTable` for bidirectional adjacency.
- One WriteBatch = one transaction: checkpoint write + message truncation +
  fact ingestion + weight deltas commit atomically or not at all.
- SlateDB+S3 backend behind the same engine trait, deferred until
  multi-machine sharing is real (same gate as mem-server).

## PG exit and migration

- **Export**: one-time JSONL dump from PG (`memories`, `session_messages`,
  `session_checkpoints`, `tasks`, `task_items`).
- **Import**: through the normal write API only — no second write channel
  (same discipline as OKM's parquet restore path).
- **Legacy flat memories stay flat.** They have no triple structure; forcing
  them into graph facts would fabricate relations. They load into the Phase-1
  flat table as-is; only new knowledge goes through extraction. Old data
  keeps its shape.
- **Quality gate**: Phase 1 is not "done" until hybrid retrieval on the new
  core matches or beats PG retrieval on a fixed query set (recall-oriented;
  exact ordering may differ).

## Consequences

- plpython3u trigger architecture is retired with PG; embedding moves into
  the process (ADR-0003). The API key single-source problem simplifies from
  "PG process env" to an ordinary process env var.
- Schema evolution is the OKM versioned-enum path, not DDL migration.
- Ops surface shrinks: no PG instance, no ParadeDB image, no extension
  upgrades — a directory on disk.
- Fjall's single-writer model is acceptable at single-agent scale; the
  multi-agent shared-memory future is gated behind SlateDB+S3, not fought
  with Fjall.
