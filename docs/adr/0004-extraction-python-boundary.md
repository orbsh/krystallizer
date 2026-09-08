# ADR-0004: Extraction Pipeline Stays in Python — Core Is LLM-Free

**Status**: Accepted (design; implementation pending)
**Date**: 2026-09-08
**Chinese**: [0004-extraction-python-boundary.zh-CN.md](0004-extraction-python-boundary.zh-CN.md)

## Context

The graph-memory design defines a two-stage extraction pipeline
(normalization → classified extraction into atomic facts), with LLM calls
at both stages and cost strategies (small model for stage 1, strong model
for stage 2, merged single call for short inputs). The write-time fusion
flow (out-of-domain entity convergence) also involves one LLM decision over
a small candidate set.

The question: does the LLM orchestration live inside the Rust core, or at
the Python layer?

## Decision

**mem-core receives structured KDL facts; it never calls an LLM.**

- The two-stage pipeline, entity fusion's LLM decision step, active-memory
  judging criteria (specificity × generality), and any prompt engineering
  live in the Python layer (skillforge side, later the mem adapter
  package).
- The core's ingestion surface is deterministic: `ingest(fact)` where
  `fact` is already parsed from KDL
  (`head <type> <value> [props] / rel <RELATION> [props] / tail <type>
  <value> [props]`).
- Fusion inside the core is split accordingly:
  - *In-domain entities* (org members, projects, tasks, products): resolved
    deterministically by stable id — zero embedding, zero search, zero LLM.
  - *Out-of-domain entities*: the core provides the cheap half — embedding
    search returning candidate facts **plus their anchored entity nodes**
    — and the caller (Python) runs the expensive half (LLM decides:
    reuse node / merge-duplicate / new node), then calls
    `ingest`/`link` with the decision applied.

## Rationale

- The core stays deterministic and fully testable without mocks or recorded
  LLM fixtures. Every property of the storage layer can be asserted
  exactly.
- LLM orchestration is a fast-moving concern (models, prompts, cost
  policies change weekly); storage schemas are slow-moving. Separating
  them lets each move at its own speed.
- Python already owns the agent integration (tool injection, tail-prompt
  authoring, checkpoint policies). Extraction is the same kind of work —
  it belongs there.
- The KDL fact format is the contract across the boundary. It is the
  wiki's authoritative serialization for atomic facts, renders
  deterministically to Mermaid/Cytoscape for inspection, and parsing it in
  Rust is a plain `kdl` crate call.

## Consequences

- mem-core has zero LLM dependency, zero network dependency for ingestion;
  the only network call in the core is the embedding API (ADR-0003).
- The Python adapter is part of the shipped surface (not a disposable
  demo): pipeline orchestration, KDL generation, fusion LLM decisions,
  caller-side trigger policies.
- If a future non-Python consumer appears (mem-server), it re-implements
  the orchestration layer or speaks to a Python sidecar; the core API does
  not change.
