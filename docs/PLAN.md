# PLAN

Phases reference ADRs; design content lives in the ADRs, never here. This file is the index: what landed, when, and what is still owed.

## Milestones

- [x] Phase 0 — Skeleton: `mem-core` + fjall + a PyO3 binding smoke test (one store, one search) — ADR-0002
- [x] Phase 1 — Flat memory parity — SUPERSEDED 2026-10-20: every goal was re-answered on the graph by Phase 3 — ADR-0002, ADR-0003, ADR-0007
  - [x] 1a — store / search / forget / list / stats; the KDL config
  - [x] 1b — built-in embedder — CLOSED 2026-10-20, both halves landed elsewhere
  - [x] 1c — in-memory vector search, on the graph's nodes — ADR-0003, ADR-0005
  - [x] 1d — recall gate as a measured baseline: p@1 0.929, p@3 1.000
  - [x] 1e — the HTTP face for the active side — ADR-0001
- [x] Phase 2 — Channel log and the agent projection — ADR-0001, ADR-0002, ADR-0008
  - [x] the minimal slice — 2026-10-09: ns 33 messages, ns 34 checkpoints
  - [x] the HTTP surface — 2026-10-10: three channel-log endpoints, the caller owns the clock
- [x] Phase 2.5 — View-layer serialization: the projection read path, pruning at the read side only
- [x] Phase 2.6 — The compression policy engine — ADR-0008
  - [x] 2.6a — the cursor state (ns 39)
  - [x] 2.6b — the engine (`mem-core/src/compress.rs`)
  - [x] 2.6c — the checkpoint door takes the summary alone
  - [x] 2.6d — the pointer and the shape: coverage is a record, `CheckpointKey` is the pointer, the cursor is a clock — ADR-0008 §2/§4
- [x] Phase 2.7 — Multi-party participation — 2026-10-20
  - [x] 2.7a — the identity layer (ns 42)
  - [x] 2.7b — the two views (`MessageRow.end_ts_ms`)
  - [x] 2.7c — the member → node mapping
- [x] Phase 2.8 — The code store: the delivered script's home
- [x] Phase 2.9 — The structured log at ns 41 — ADR-0011
  - [x] 2.9a — the table (`mem-core/src/log.rs`)
  - [x] 2.9b — the first writer (`record_verdict`)
  - [x] 2.9c — the field shape
  - [x] 2.9d — internal counters on `ok_reduce`'s HighWater
- [x] Phase 3 — The atomic-fact graph — ADR-0001, ADR-0002, ADR-0004, ADR-0005, ADR-0007, ADR-0008, ADR-0010
  - [x] 3a — the write side, steps 1-2
  - [x] 3b — the weight system — ADR-0005
  - [x] 3c — steps 3-4, reshaped 2026-10-20 — ADR-0010
  - [x] 3d — flat memory (ns 32) and its HTTP door, withdrawn 2026-10-19
  - [x] 3e — the deterministic read and the third write verb
- [x] Phase 4 — Clustered retrieval — 2026-10-20 — ADR-0005, ADR-0010
  - [x] 4a — the weight ranking (`GraphStore::top_by_weight`)
  - [x] 4b — the three entrances
  - [x] 4c — reference links
- [x] Phase 4.5 — The user node — ADR-0014
- [x] Phase 4.6 — The kind field's vocabulary — okm ADR-0032
- [x] Phase 4.7 — The backup boundary closes: restore — ADR-0013 §6
- [x] Phase 4.8 — The knowledge pipeline's pilot: this repo's own docs into the graph
- [x] Phase 5's SKILL slice — the skill node — 2026-10-26 — ADR-0015
- [x] Counts at the call — 2026-10-26 — ADR-0018
- [x] The vector side table — 2026-10-26 — ADR-0019

## Repo hygiene (maintenance, not phase work)

- [x] The store's error face — 2026-10-24: every door says "the store could not answer" instead of folding that into an empty page — okm ADR-0037/0038/0039
- [x] The lint gate — 2026-10-24: the workspace denies `clippy::unwrap_used`
- [x] okm followed to `ba0fb72` and past it — `MemBatch` is `Batch`, and the write primitives are `Result`
- [x] The namespace probe follows okm's scan — 2026-11-09: `scan_range` IS the streaming shape, so emptiness is read off the first item
- [x] Restore from the lake — ADR-0026; a restore lands only into empty namespaces, and the edge set is published by endpoint pair — ADR-0027

## Open

- [ ] Phase 5+ — deferred, do not start early: dreaming consolidation (community detection plus a human-reviewed op set), the SKILL topological walk, Actor nodes, and document → graph population
- [ ] Populating relations from documents — deferred 2026-10-31 — `docs/ideas/hypergraph-population.md`
- [x] A real-bucket rehearsal of the restore — 2026-11-21, `scripts/lake-s3-rehearsal.sh` against a rustfs bucket: two publish intervals landed 79 objects across 15 tables (delta logs at versions 0–3), the restore read them back into an empty directory, and every endpoint answered identically to the snapshot taken from the live store (8 records; channel 7's three rows and the member list included). The harness stays as the gate for this claim, with a directory target when no credential is at hand. It found three defects on the way, all fixed in the same commit: the restore panicked on a nested runtime, it opened a bucket anonymously, and `CATALOG` published neither `channels` nor `memberships` (ADR-0026)