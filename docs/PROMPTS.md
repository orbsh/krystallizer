# PROMPTS — The Two Prompt Surfaces

> **Languages:** [English](PROMPTS.md) (primary) · [中文](PROMPTS.zh-CN.md)

Status: **Declared** (drafts; nothing here is wired). Prompts are text the
caller owns; k10r holds no LLM (ADR-0007), so both prompts below run on the
caller's side and neither is a k10r asset.

Two isolated paths, and they must not be conflated (they were, in the draft
this page is built from):

1. **The live path** — the model's ACTIVE memory add: a tool in gravity's fixed
   tool list, called during a user turn (ADR-0008: facts come
   from the active add, not from a compression pass).
2. **The write-side pass** — the capsule generator: it runs after a fact
   lands, inside the same background pass as ADR-0010's write-time resolution
   and binning. It is NOT the compression bypass: the bypass only compresses
   (ADR-0008.

Names below follow the settled vocabulary. Where the source draft invented a
name or a field, it is marked **[not in the design]** and left visible rather
than silently adopted — see §4.

## 1. Live path: the tool definition (gravity's side)

Appended to the agent's system prompt. It exists to make the add a reflex
rather than a session-end flush, and to keep the model from storing filler.

```
# Memory: active add

You have a graph memory engine behind a tool. Preserve critical atomic facts,
architectural decisions and explicit user preferences as they are disclosed, by
calling the memory-add tool during the turn.

## What to add
Do NOT add chit-chat, filler, or trial reasoning. Add when the user declares:
- Core engineering decisions — framework, storage, protocol, performance
  constraints ("we chose Rust + Fjall for local storage to avoid
  write-amplification").
- Immutable rules and axioms — domain logic or formatting constraints.
- Permanent user preferences — explicit states, identities, rigid style
  preferences.

## Shape
Break the statement into subject–predicate–object triples, serialized as a
**fixed-length 8-slot tuple** (a JSON list): the three roles in order head /
rel / tail — head and tail as `[value, type, props]`, the relation as
`[RELATION, props]` (its name IS its kind, so it carries no type label) — with
the caller routing that tuple to `ingest(fact)`.
- head / tail — value = the entity (short, snake_case: `galaxy_repo`,
  `fjall_engine`); type = the entity's type label (lowercase single token);
  props = the entity's attributes.
- rel — value = the relation or constraint (uppercase snake_case: `USES`,
  `IMPLEMENTED_IN`, `PREVENTS`); props = the relation's modifiers (time /
  condition / cause / strength).
- Empty props are written `{}` — the tuple is fixed-length, so no slot is
  omitted; the type vocabulary is open.

## Examples
User: "We're pivoting the Galaxy repo to an embedded Rust memory core named
k10r — network graph databases are too heavy for a stateless architecture."
  ["galaxy_repo", "repository", {}, "PIVOTED_TO", {"reason": "network_graph_too_heavy"}, "k10r_core", "system", {}]

User: "I like an iced americano while coding, but no milk in coffee, ever."
  ["user", "person", {}, "PREFERS", {"context": "while_coding"}, "iced_americano", "coffee", {}]
```

Notes that keep this prompt honest:

- **The tool name is the memory family's, not `k10r.store()`**
  **[not in the design]**. The caller's names are `recall_memory` /
  `commit_memory` (gravity's fixed tool list, one (retrieve, act) pair per
  family); inside k10r the write door is `ingest(fact)` (deterministic, the
  8-slot tuple) and `store` (flat memory). A tool name is never a k10r
  function name.
- **The model does not stamp the clock.** `ts_ms`/`sub` are the caller's
  (ADR-0008 §3). If a fact carries a date, the caller supplies it — a model
  asked for "the current ISO timestamp" invents one **[open ruling: who stamps
  a fact's `since`]**.
- **`confidence` is [not in the design] as written.** The design's ranking
  input is the weight system (ADR-0005: read/write counts, `tool_invoke_count`,
  the read-write quadrants) plus the landing confidence floor on the READ side.
  A model-declared confidence per fact is a proposal; if it lands, it is a
  declared row attribute, not a key.
- **Nothing here says where the fact goes.** Flat memory vs graph is decided by
  the caller's routing and by write-time fusion (ADR-0004/0007), not by the
  model.

## 2. Write-side pass: the capsule generator

Input: ONE community's slice of atomic facts (a list of 8-slot tuples),
produced by ADR-0010's incremental LPA. This prompt runs in the write-side
background pass, after the fact write — never on the conversational critical
path.

```
You are a graph-consolidation step inside an embedded memory core's
write-side pass.

## Input
A raw, isolated sub-graph cluster of atomic facts in 8-slot tuple notation (one
tuple per line): a tightly bound semantic community selected by local label
propagation.

## Tasks
Read the triples, their properties and their timestamps. Invent nothing and
extrapolate nothing beyond the literal assertions. Do exactly two things:
1. Title — a concept title, at most 6 words, that names this community.
2. Capsule — a 1–2 sentence high-density summary capturing the causality, the
   settled choice, or the behavioral pattern. It becomes a cache anchor so
   downstream reads do not re-traverse the whole community.

## Output
Raw JSON only — no markdown fence, no preamble, no greeting:

{"title": "string (max 6 words)", "summary": "string (1-2 sentences)"}

## Example
<the tuple slice from §4's sample>
-> {"title": "K10r Core Embedded Storage Selection", "summary": "The k10r core is an embedded Rust memory system using the Fjall KV engine to eliminate runtime write-amplification; the arroy vector-search dependency was dropped to keep execution local."}
```

Where the capsule lives is ADR-0010's business (community id on the node row +
a community reverse index + the capsule); "Layer 3" **[not in the design]** is
the draft's numbering, not this design's vocabulary (the paradigm doc's layers
are write / organize / retrieve).

## 3. Where each block is deployed

| Block | Owner | Where it runs | Wiring status |
|---|---|---|---|
| §1 tool definition | gravity (the caller) | inserted into the turn's system prompt alongside the fixed tool list | not wired (the tool list itself is still Phase 0: plain chat, no tools) |
| §2 capsule generator | whichever consumer owns the write-side pass (gravity for chat; a memory-primary consumer's own worker otherwise) | the background pass after a fact write | not wired (ADR-0010 is Declared) |

## 4. Provenance and the to-verify list

This page integrates an external AI-generated draft suite. Integration rule:
keep the drafts' prompt TEXT (it is decent craft) but never its invented
architecture. Items flagged, in one place:

- `k10r.store()` as a tool name — not in the design; the tool faces are
  `commit_memory` / `recall_memory`, the k10r doors are `ingest(fact)` / `store`.
- `confidence` per fact — not in the design (weight system + read-side landing
  floor instead).
- `since` supplied by the model — the caller owns the clock; needs a ruling for
  facts.
- "Layer 3 Semantic Capsule" — the draft's numbering; ADR-0010 names the
  artifacts (community, capsule).
- "the asynchronous bypass pipeline" as the capsule's home — conflated: the
  bypass compresses; the fact write and the binning ride the write-side pass.
- Sample entities (`galaxy_repo`, `k10r_core`) and the dropped-dependency
  example are illustrative, not measurements.