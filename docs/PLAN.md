# PLAN

Phases reference ADRs; design content lives in the ADRs, never here.

- [x] Phase 0 — Skeleton: mem-core crate + Fjall engine + PyO3 binding channel smoke test (one store / one search). (ADR-0002)
- [ ] Phase 1 — Flat memory parity: store / search / forget / list / stats semantics on Fjall; retrieval = built-in local embedding model (in-process, no API, model locked with the data) landing into graph traversal, plus an exact name/alias point lookup as the second entrance. PG exit gate: end-to-end recall comparison on a fixed query set. (ADR-0002, ADR-0003 + Update 2026-10-14, ADR-0007)
  - [x] 1a. Semantics: store / search (user-prefix scan) / forget / list / stats; config loading (`config/krystallizer.kdl`, knus, env-resolved secrets)
  - [ ] 1b. Built-in embedder: local small model loaded in-process (no endpoint, no key), embeddings written as `Vector<f32>` frames; name/alias → node id point lookup
  - [ ] 1c. In-memory vector search: per-ns float table loaded from okm, brute-force inner-product Top-K (ANN re-opened only when a measured corpus crosses the threshold)
  - [ ] 1d. PG exit gate: end-to-end recall comparison on a fixed query set (landing confidence floor + a legal "nothing landed" outcome)
- [ ] Phase 2 — Channel log & agent projection: messages / members /
  cursors / checkpoints (per member) tables with gravity-assigned
  timestamp ordering; session-control primitives (tail / summarize-as-
  append) over the projection; thread = parented sub-channel; topic tag
  index. (ADR-0001, ADR-0002, ADR-0008)
  - [x] Minimal slice LANDED (2026-10-09): ns 33 messages + ns 34
    checkpoints, `ChannelStore::append`/`fetch_full`/`append_checkpoint`.
  - [x] HTTP surface LANDED (2026-10-10): mem-server carries the three
    channel-log endpoints over axum (`POST /channels/{id}/messages`,
    `GET /channels/{id}/members/{m}/session`,
    `POST /channels/{id}/members/{m}/checkpoint`); caller owns the
    clock (ts_ms/sub ride the request); 4 integration tests against a
    real server (key echo, checkpoint/increment semantics, channel +
    member isolation).
- [ ] Phase 2.5 — View-layer serialization: projection read path
  (checkpoint + increment assembly) with tool-call pruning in the view
  (storage always full; read-side only), enabling the stateless-agent
  integration contract (append → read projection → run). (ADR-0006, ADR-0008)
- [ ] Phase 2.6 — Compression policy engine: parameterized triggers
  (cache-clock, budget percentage, max-gap split — all from
  `krystallizer.kdl`, per-user override layer reserved), structured
  decision logs; regression tuning deferred until feedback labels exist.
  (ADR-0008)
- [ ] Phase 2.7 — Multi-party participation: member display-name/role
  fields + per-message attribution in serialization; `kind=profile`
  flat-memory category (extraction-routed, weight-governed); interrupt
  trigger (seconds-scale passive window) with silent outcomes in the
  decision log. (ADR-0009)
- [ ] Phase 3 — Atomic-fact graph: fact node/edge tables, KDL ingestion, write-time fusion (in-domain id / out-of-domain embedding convergence + small-candidate LLM decision — the LLM decision is gravity's, per ADR-0007's boundary), weight system (delta counts, read-write quadrants, tool_invoke_count, W: index). (ADR-0004, ADR-0005, ADR-0007)
- [ ] Phase 4 — Clustered retrieval: entity-anchored / temporal / causal-chain views; progressive loading (Top-K by weight). (ADR-0005)
- [ ] Phase 5+ — Deferred (do not start early): dreaming consolidation (petgraph community detection + human-reviewed op set), SKILL topological four-part nodes, Aura Actor nodes. (mem-server left this list 2026-10-10 — it is the single cross-process form, ADR-0007 as revised.)

Deferred gates:

- Multi-consumer service shell: mem-server is the single cross-process
  form (ADR-0007 as revised 2026-10-10); it graduates from deferred to
  deliverable with the channel-log endpoints (append / fetch_full /
  append_checkpoint) first, grown as consumers need; core is
  transport-free so the server is additive.
- Delta-append weight counts: single-process serial start uses direct RMW;
  key layout stays delta-compatible so multi-agent switchover changes no schema.
- Backup export endpoint: one HTTP endpoint that exports the okm data
  as Parquet for direct download (backup = bulk read over the same
  HTTP boundary, no admin CLI). Deferred until the flat-memory tables
  are real (Phase 1); format/open questions — table granularity
  (per-ns vs per-collection), whether to reuse okm's export surface if
  one lands — recorded here so the endpoint grows with consumers, not
  ahead of them.
