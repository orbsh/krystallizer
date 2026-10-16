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

**mem-core receives structured facts (the 9-slot tuple); it never calls an LLM.**

- The two-stage pipeline, entity fusion's LLM decision step, active-memory
  judging criteria (specificity × generality), and any prompt engineering
  live in the Python layer (skillforge side, later the mem adapter
  package).
- The core's ingestion surface is deterministic: `ingest(fact)` where
  `fact` is already the 9-slot tuple (head value / head type / head props /
  rel value / rel type / rel props / tail value / tail type / tail props).
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
- The fact tuple is the contract across the boundary. It is the
  wiki's authoritative serialization for atomic facts, renders
  deterministically to Mermaid/Cytoscape for inspection, and reading it in
  Rust is plain `serde_json` (already a workspace dependency).

## Consequences

- mem-core has zero LLM dependency, zero network dependency for ingestion;
  the only network call in the core is the embedding API (ADR-0003).
- The Python adapter is part of the shipped surface (not a disposable
  demo): pipeline orchestration, fact serialization, fusion LLM decisions,
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

## Update (2026-10-16, facts come from the model's active add, not from an extraction stage)

The two-stage pipeline in Context (normalization → classified extraction, each
with its own LLM call) is retired as the mechanism: facts are not produced by a
separate extraction pass. **The model adds memory actively, in-turn** — the
memory tool is the producer, and how that call is routed is the caller's
business (ADR-0007). What this ADR settled does not move: intake stays the
deterministic `ingest(fact)` over the tuple, the fusion split stands (in-domain
ids resolved deterministically; the out-of-domain cheap half is embedding search
plus anchored entity nodes, the expensive half is the caller's decision), and
the specificity × generality criterion is now simply a rule the model applies
when it decides to add — not a stage of a pipeline.

## Update (2026-10-16, the fact's serialization is a fixed-length 9-slot tuple)

The fact that crosses the boundary is no longer KDL: it is **a fixed-length
9-slot tuple — a JSON list of three roles in order, head / rel / tail, each
`[value, type, props]`** — and the `category` field is gone: a fact's form is
those three roles and nothing else.

- head value / head type / head props, rel value / rel type / rel props, tail
  value / tail type / tail props — positions 0-8.
- Empty props are written `{}`; the tuple is fixed-length, so no slot is
  omitted.
- The relation carries a type label of its own (position 4) — the same register
  a node's type label uses (a lowercase single token). What makes a relation
  kind functional vs multi-valued is still open (ADR-0010).

What it changes elsewhere: nothing on the fact path needs a KDL parser any more
(the config file stays KDL, ADR-0007), and the packet handed to the capsule LLM
is a list of these tuples (ADR-0010). What does not move: this ADR's decision —
the core receives already-structured facts through a deterministic
`ingest(fact)` and calls no LLM — and the fusion split.

Where the form is written down: the wiki's authoritative serialization section
(krystallizer, both languages), the knowledge-triplets skill's format section,
and this repo's PROMPTS pair. ADR-0010's owed list now records that position 4
has no counterpart in the edge body yet (it carries one `kind_id` for the
predicate).
