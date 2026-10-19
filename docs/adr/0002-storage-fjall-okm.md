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
for the atomic-fact graph; the remaining channel-log and task tables are
CRUD-shaped and map directly. (Flat memory did too, and was withdrawn once
the graph became the carrier — 2026-10-19.)

## Decision

**Fjall is the storage engine. All schemas are declared with OKM derive
macros.**

- Fixed-width keys for identity (`user_id`, `channel_id`, table ns) —
  compile-time `KEY_LEN`, hex stability tests lock the layout.
- Payloads as TLV via `RowEncode`; variable-length content (message text,
  entity names) as `String` payload fields, never in keys.
- Messages (ns 33): key `(ns, channel_id, ts_ms + tiebreaker)` — ordering by
  gravity-assigned timestamps (ADR-0008). A reply provably postdates what it
  answers, so a physical clock preserves causal order where a shared seq
  counter does not; same-millisecond bursts are resolved by the tiebreaker
  segment. The prefix-checkpoint model keeps its stable total order, and the
  checkpoint itself (ns 34, per member) is immutable.
- Tasks and the fact graph are ONE graph (ADR-0001: the third interface
  surface shares the facts' graph — task vertices plus `task_links`
  dependency edges). The legacy PG schema's `(task_id, seq)` composite stays
  on record as the migration mapping.
- Atomic-fact graph: an OKM edge collection with declared faces, per
  ADR-0010 §1 — entities as a node collection (ns 36), facts as an
  edge collection (ns 37) carrying `predicate_id` plus the declared `since` /
  `confidence` / `weight` fields, with the kind and traversal faces. NOT an
  `edge:{src}:{label}:{dst}` key layout, and `EdgeTable` is a former name:
  the layer above is `Graph<S, E: KvGraph>` + `NodeRef` + the kind
  dictionary (okm ADR-0017).
- **Append-first, not append-only — scoped by table.** The channel log
  (ns 33) is append-only outright: extraction, replay and audit all read it,
  and a log that can be corrected loses all three at once. The graph and the
  data side are append-first: node/edge rows, weights, derived index entries
  and code rows may be corrected where forbidding it would cost more
  complexity than the update itself — the live case is the code store, whose
  `put(id)` rewrites a row identified by that id. The reason appends are the
  default is SIMPLICITY (no read-modify-write race, replayable, a bad
  derivation is a retry), not purity.
- One WriteBatch = one transaction: checkpoint write + fact ingestion +
  weight deltas commit atomically or not at all; a batch that fails leaves
  nothing.
- Nothing truncates the log: the compression point lives in its own table
  (ns 34), pointing into the log by `coverage_ts` — latest-wins there, that
  mutability confined to that table. A bad summary is a retry, not a loss.
- The store never splits into two truths: view trimming is read-side only,
  and whoever owns the write owns the record.
- SlateDB+S3 backend behind the same engine trait, deferred until
  multi-machine sharing is real (same gate as mem-server).

## PG exit and migration

- **Export**: one-time JSONL dump from PG (`memories`, `session_messages`,
  `session_checkpoints`, `tasks`, `task_items`).
- **Import**: through the normal write API only — no second write channel
  (same discipline as OKM's parquet restore path).
- **Legacy flat memories stay flat — and the table they imported into is gone (2026-10-19); the import target is the migration's call.** They have no triple structure; forcing
  them into graph facts would fabricate relations. They load as-is into the
  flat table — the pre-graph carrier of long-term memory, which is withdrawn
  together with the graph (ADR-0001; PLAN Phase 3). What happens to imported
  legacy rows when that lands is an open migration question, not a schema one.
- **Quality gate**: Phase 1 is not "done" until retrieval on the new
  core matches or beats PG retrieval on a fixed query set (recall-oriented;
  exact ordering may differ). Per ADR-0003, the comparison is end-to-end
  retrieval quality, not a per-branch recall comparison against pgvector +
  pdb.bm25 + RRF (PLAN 1d).

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