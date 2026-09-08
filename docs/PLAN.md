# PLAN

Phases reference ADRs; design content lives in the ADRs, never here.

- [ ] Phase 0 — Skeleton: mem-core crate + Fjall engine + PyO3 binding channel smoke test (one store / one search). (ADR-0002)
- [ ] Phase 1 — Flat memory parity: store / search / forget / list / stats semantics on Fjall; hybrid retrieval = arroy + BM25 + RRF. Python-side API equivalent. PG exit gate: retrieval-quality comparison (recall not worse on a fixed query set). (ADR-0002, ADR-0003)
- [ ] Phase 2 — Session & task migration: ConversationMemory (append + prefix checkpoint, WriteBatch atomic) with session-control primitives (branch / tail / summarize); TaskList + task_links DAG edges. (ADR-0001, ADR-0002)
- [ ] Phase 2.5 — View-layer serialization: full-session read path with tool-call pruning in the view (storage always full; read-side only), enabling the stateless-agent integration contract (fetch session → run → persist). (ADR-0006)
- [ ] Phase 3 — Atomic-fact graph: fact node/edge tables, KDL ingestion, write-time fusion (in-domain id / out-of-domain embedding convergence + small-candidate LLM decision), weight system (delta counts, read-write quadrants, tool_invoke_count, W: index). (ADR-0004, ADR-0005)
- [ ] Phase 4 — Clustered retrieval: entity-anchored / temporal / causal-chain views; progressive loading (Top-K by weight). (ADR-0005)
- [ ] Phase 5+ — Deferred (do not start early): dreaming consolidation (petgraph community detection + human-reviewed op set), SKILL topological four-part nodes, Aura Actor nodes, mem-server transport shell.

Deferred gates:

- Multi-consumer service shell (mem-server): deferred until shared-memory-graph
  becomes a real deployment need; core is transport-free so the shell is
  additive.
- Delta-append weight counts: single-process serial start uses direct RMW;
  key layout stays delta-compatible so multi-agent switchover changes no schema.
- tantivy: not adopted (see ADR-0003); revisit only if hand-written BM25 hits
  a structural wall, not a tuning one.
