# ADR-0004: Extraction Pipeline Stays in Python — Core Is LLM-Free

**Status**: Accepted; ownership superseded by [ADR-0007](0007-consumption-boundary.md) (no Python layer — LLM orchestration belongs to gravity; the deterministic `ingest(fact)` surface and the fusion split carry over)
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

## Update (2026-10-14): the graph's intake criterion; network-call note

One criterion joins the caller-side set (specificity × generality):
**a memory earns a graph node when it can be coded — it has a canonical
name and at least one typable relation.** The reason is an argument, not a
taste: the code domain is where graph retrieval demonstrably works, and
what code hands over for free is exactly (a) one canonical identity per
thing and (b) typed, directed relations (wiki graph-memory, "Why Graphs
Work: the Existence Proof in Code"). A memory with neither — chat chatter,
unanchored impressions — enters the graph only as weight dilution: nothing
can link to it by identity and its relations cannot be typed. What it does
have, semantic similarity, belongs to the entrance layer, not the graph.

Consequences: the graph's cost condition is *construction* — identity
(canonical name + alias entries) and relation typing are where the work
sits, so ingestion that skips them produces isolated nodes rather than
bad retrieval. This gate restricts what earns a node, never what may be
stored: flat memory keeps everything, and the intake decision itself stays
caller-side (it is a judging criterion, like specificity × generality).

Network-call note: ADR-0003's embedding question is decided as a built-in
local model (ADR-0003 Update 2026-10-14), so the Consequences bullet below
about "the only network call in the core is the embedding API" no longer
holds — the core's outbound surface is now empty.
