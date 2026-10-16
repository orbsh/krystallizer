# ADR-0004: The Fact Contract — Core Is LLM-Free, Orchestration Is the Caller's

**Status**: Accepted. The layer ownership is [ADR-0007](0007-consumption-boundary.md)'s
(there is no Python layer; LLM orchestration belongs to the caller — gravity for
chat consumers). What stays this ADR's: the deterministic `ingest(fact)` fact
contract and the fusion split.
**Date**: 2026-09-08
**Chinese**: [0004-fact-contract.zh-CN.md](0004-fact-contract.zh-CN.md)

## Context

The graph-memory design's two-stage extraction pipeline (normalization →
classified extraction, an LLM call per stage) is retired as the mechanism:
facts are not produced by a separate extraction pass. **The model adds memory
actively, in-turn** — the memory tool is the producer, and how that call is
routed is the caller's business (ADR-0007/0008). What remains is the boundary
question this ADR answers: does the LLM orchestration live inside the Rust
core, or on the caller's side? The write-time fusion flow (out-of-domain entity
convergence) similarly involves one LLM decision over a small candidate set.

## Decision

**mem-core receives structured facts (the 8-slot tuple); it never calls an LLM.**

- The LLM decisions — the intake judgment (specificity × generality), the
  out-of-domain fusion decision, summarize-when-policy-needs-one, and any
  prompt engineering — live on the CALLER's side (ADR-0007: gravity for chat
  consumers; a memory-primary program runs its own single-pass reasoning),
  never inside the core.
- The core's ingestion surface is deterministic: `ingest(fact)`, where `fact`
  is already the 8-slot tuple — head value / head type / head props, rel name /
  rel props, tail value / tail type / tail props (positions 0-7). Head and tail
  carry a name AND a type label; the relation carries only its name and props —
  its name IS its kind (the edge collection's own dictionary allocates it as
  `kind_id`, ADR-0010 §1), so a second, coarser label would have a face nobody
  reads. Empty props are written `{}` — the tuple is fixed length, no slot is
  omitted — and `category` is not part of a fact's form. The form is written
  down in the wiki's authoritative serialization section (krystallizer, both
  languages), the knowledge-triplets skill's format section, and this repo's
  PROMPTS pair; reading it is plain `serde_json` (already a workspace
  dependency).
- **Fusion inside the core is split accordingly:**
  - *In-domain entities* (org members, projects, tasks, products): resolved
    deterministically by stable id — zero embedding, zero search, zero LLM. The
    caller's stable id reaches the graph as an attribute with a derived entry
    (the exact entrance), never as key material.
  - *Out-of-domain entities*: the core provides the cheap half — embedding
    search returning candidate facts **plus their anchored entity nodes** — and
    the caller runs the expensive half (the model decides: reuse node /
    merge-duplicate / new node), then calls `ingest`/`link` with the decision
    applied.
- **The intake criterion.** A memory earns a graph node when it can be coded:
  it has a canonical name and at least one typable relation. The reason is an
  argument, not a taste — the code domain is where graph retrieval demonstrably
  works, and what code hands over for free is exactly (a) one canonical identity
  per thing and (b) typed, directed relations (wiki graph-memory, "Why Graphs
  Work: the Existence Proof in Code"). A memory with neither — chat chatter,
  unanchored impressions — enters the graph only as weight dilution: nothing can
  link to it by identity and its relations cannot be typed. What it does have,
  semantic similarity, belongs to the entrance layer, not the graph.

## Rationale

- The core stays deterministic and fully testable without mocks or recorded
  LLM fixtures. Every property of the storage layer can be asserted exactly.
- LLM orchestration is a fast-moving concern (models, prompts, cost policies
  change weekly); storage schemas are slow-moving. Separating them lets each
  move at its own speed.
- The caller already owns the agent integration (tool injection, prompt
  authoring, checkpoint policy). Producing facts is the same kind of work.

## Consequences

- mem-core has zero LLM dependency and no network surface at all — the
  embedding model is built in (ADR-0003), so ingestion is local and so is
  embedding.
- The orchestration surface is the caller's: fact production, the fusion
  decision, trigger policies. Chat consumers route it through gravity
  (ADR-0007); a memory-primary program needs no agent middle layer.
- **The graph's cost condition is CONSTRUCTION**: identity (canonical name +
  alias entries) and relation typing are where the work sits, so ingestion that
  skips them produces isolated nodes rather than "slightly worse retrieval".
  This gate restricts what earns a node, never what may be stored — raw
  content's home is the channel log (ADR-0008: every message, append-only), and
  the gate governs the long-term memory built from it. The intake decision stays
  caller-side (a judging criterion, like specificity × generality).
- A second consumer shape (a memory-primary program) re-implements nothing: it
  calls the same HTTP doors and runs its own single-pass reasoning (ADR-0007).
- Nothing on the fact path needs a KDL parser; the config file stays KDL
  (ADR-0007).
