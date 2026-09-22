# PLAN

Phases reference ADRs; design content lives in the ADRs, never here.

- [x] Phase 0 — Skeleton: mem-core crate + Fjall engine + PyO3 binding channel smoke test (one store / one search). (ADR-0002)
- [ ] Phase 1 — Flat memory parity: store / search / forget / list / stats semantics on Fjall; hybrid retrieval = arroy + BM25 + RRF, embeddings computed in-process (internal detail, model locked). PG exit gate: retrieval-quality comparison (recall not worse on a fixed query set). (ADR-0002, ADR-0003, ADR-0007)
  - [x] 1a. Semantics: store / search (user-prefix scan) / forget / list / stats; config loading (`config/krystallizer.kdl`, knus, env-resolved secrets)
  - [ ] 1b. BM25: jieba tokenization, inverted index as a multi-value function index, BM25 scoring
  - [ ] 1c. arroy: vectors written/queried in-process via the embedding API
  - [ ] 1d. RRF fusion (k=60) + PG exit gate (recall comparison on a fixed query set)
- [ ] Phase 2 — Channel log & agent projection: messages / members /
  cursors / checkpoints (per member) tables with gravity-assigned
  timestamp ordering; session-control primitives (tail / summarize-as-
  append) over the projection; thread = parented sub-channel; topic tag
  index. (ADR-0001, ADR-0002, ADR-0008)
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
- [ ] Phase 3 — Atomic-fact graph: fact node/edge tables, KDL ingestion, write-time fusion (in-domain id / out-of-domain embedding convergence + small-candidate LLM decision — the LLM decision is gravity's, via the access modes of ADR-0007), weight system (delta counts, read-write quadrants, tool_invoke_count, W: index). (ADR-0004, ADR-0005, ADR-0007)
- [ ] Phase 4 — Clustered retrieval: entity-anchored / temporal / causal-chain views; progressive loading (Top-K by weight). (ADR-0005)
- [ ] Phase 5+ — Deferred (do not start early): dreaming consolidation (petgraph community detection + human-reviewed op set), SKILL topological four-part nodes, Aura Actor nodes, mem-server transport shell.

Deferred gates:

- Multi-consumer service shell: mem-server carries the HTTP access mode
  (ADR-0007); its build-out is deferred until the memory graph is a real
  multi-consumer deployment need; core is transport-free (embedding
  excepted) so the shell is additive.
- Delta-append weight counts: single-process serial start uses direct RMW;
  key layout stays delta-compatible so multi-agent switchover changes no schema.
- tantivy: not adopted (see ADR-0003); revisit only if hand-written BM25 hits
  a structural wall, not a tuning one.
