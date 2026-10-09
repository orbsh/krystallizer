# ADR-0007: Consumption Boundary — Three Access Modes, No Python Layer

**Status**: Accepted
**Date**: 2026-09-14
**Chinese**: [0007-consumption-boundary.zh-CN.md](0007-consumption-boundary.zh-CN.md)
**Supersedes**: ADR-0004's layer-ownership decision (the Python adapter layer)

## Context

ADR-0004 placed the extraction pipeline's LLM orchestration in "the
Python layer (skillforge side, later the mem adapter package)". That
reflected how skillforge happened to be implemented, not a property of
this project. Krystallizer's direction is **Python-free**: it is a
Rust memory core consumed by other projects; mem-ffi is one binding,
not an architecture layer.

Meanwhile the agent that consumes k10r settled into a definite shape
(ADR-0006): gravity, the stateless agent. Gravity runs in two modes —
as a CLI process, and inside Aura as an actor. Each mode reaches k10r
differently, and the wiki's stateless-agent page already fixes one
boundary precisely: LLM calls belong to gravity (model identity is
bound to the side that initiates inference); the single exception is
krystallizer's internal embedding (the embedding space is bound to
data correctness, so the memory system holds it).

## Decision

**Krystallizer is a Rust-only memory service with three access modes,
and no Python layer.**

1. **FFI** (in-process): mem-ffi (PyO3) — one binding shell among
   possible consumers. It is a consumption channel, not a layer of
   krystallizer; other bindings (or none) are equally legitimate.
2. **HTTP** (mem-server): when gravity executes as a CLI process, it
   talks to k10r over its HTTP interface. mem-server graduates from
   "deferred shell" to the carrier of the HTTP access mode.
3. **Aura invoke** (actor-to-actor): when gravity runs inside Aura as
   an actor, k10r is itself an actor; gravity calls it via invoke. The
   carrier form is **bgi (a standalone k10r process spawned by aura)** —
   revised 2026-10-09, see Consequences.

**LLM orchestration belongs to gravity.** Extraction-pipeline LLM
decisions (stage classification, out-of-domain fusion decisions,
summarize when policy needs one) are gravity's work — k10r hands over
deterministic primitives and cheap half-steps (candidate sets), the
model-bound decisions happen on the side that owns the model
identity.

**Embedding is internal to krystallizer** — the only network call in
the core (already conceded by ADR-0003). The embedding model is locked
with the data: vectors from different models do not share a space, so
the memory system, not any caller, owns embedding consistency. Write
and query paths compute embeddings in-process.

**Secrets never enter the config file.** Configuration is one KDL
document (`config/krystallizer.kdl`, one top-level node per
subsystem); every secret is referenced by the environment variable
name that holds it.

## Consequences

- ADR-0004's ownership claim ("extraction lives in the Python layer")
  is superseded; its remaining content — the deterministic
  `ingest(fact)` KDL surface, the fusion split (core = cheap half,
  caller = expensive half) — carries over with the caller now being
  gravity.
- mem-ffi stops being a deliverable surface; it stays as the Phase 0
  binding smoke test and a usable consumption mode.
- The core's network surface is exactly one call: the embedding API.
  No LLM client, no transport framework.
- A private config-parsing crate (knus-based, env-merge included) is
  planned once the third consumer need (KDL-shaped CLI) is real; for
  now knus alone covers decode, env reads stay one `std::env::var`
  away.
- **Storage carriage splits by run form.** CLI/standalone form: k10r
  owns its Fjall directory (ADR-0002) — "one directory on disk" stands.
  **Aura-actor form (revised 2026-10-09): k10r stays a STANDALONE
  PROCESS carried by aura's bgi carrier** — aura registers a booth type
  with `language: "bgi"` whose spawn spec launches the k10r binary; the
  fifo line protocol carries the invoke round trip. The original draft
  compiled the OKM schema into a wasm module with storage moved to aura
  via a NestStorage frame up-call; that shape is superseded — it had an
  unresolvable hole: a wasm sandbox holds no filesystem (fine, storage
  moved) but ALSO no outbound network, and the embedding call is the
  one network surface k10r owns (ADR-0003: the embedding space is bound
  to the data — moving it out is a correctness violation, not a
  preference). ADR-0034's withdrawal of wasi-http closed the only
  planned wasm egress path. Under bgi everything stays where it
  belongs: k10r keeps its Fjall directory AND its in-process embedding
  calls; aura allocates no ns for it and needs no Storage Actor
  (aura PLAN Phase 6.6's NestStorage executor stays deferred until a
  consumer actually wants hosted storage). The cost is honest: k10r's
  data is process-local, so aura booths reach it ONLY through invoke —
  which is already ADR-0007's contract (access mode 3), not a loss.
