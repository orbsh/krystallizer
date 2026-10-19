# PROMPTS — The Live Prompt Surface

> **Languages:** [English](PROMPTS.md) (primary) · [中文](PROMPTS.zh-CN.md)

Status: **Declared** (a draft; nothing here is wired). Prompts are text the
caller owns; k10r holds no LLM (ADR-0007), so the prompt below runs on the
caller's side and is not a k10r asset.

One surface. The draft this page came from also carried a write-side
"capsule generator" (community slice in, title + summary out); it retired with
the capsule table (ADR-0010 §4, 2026-10-20) — the read side renders the
community's nodes and edges instead of asking a model to summarize them.

Names below follow the settled vocabulary. Where the source draft invented a
name or a field, it is marked **[not in the design]** and left visible rather
than silently adopted — see §3.

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
  omitted; the type vocabulary is open, with ONE reserved token: `literal`
  marks a value that has no node identity of its own (a date, a number, a
  short string), and k10r lands it as a literal node whose exact text is its
  name.

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
  8-slot tuple). The flat-memory `store` door is gone (2026-10-19). A tool
  name is never a k10r function name.
- **The model does not stamp the clock.** `ts_ms`/`sub` are the caller's
  (ADR-0008 §3). If a fact carries a date, the caller supplies it — a model
  asked for "the current ISO timestamp" invents one **[open ruling: who stamps
  a fact's `since`]**.
- **`confidence` is [not in the design] as written.** The design's ranking
  input is the weight system (ADR-0005: read/write counts, `tool_invoke_count`,
  the read-write quadrants) plus the landing confidence floor on the READ side.
  A model-declared confidence per fact is a proposal; if it lands, it is a
  declared row attribute, not a key.
- **Nothing here says where the fact goes.** Where a fact is filed is decided
  by the caller's routing and by write-time fusion (ADR-0004/0007), not by the
  model.

## 2. Where the block is deployed

| Block | Owner | Where it runs | Wiring status |
|---|---|---|---|
| §1 tool definition | gravity (the caller) | inserted into the turn's system prompt alongside the fixed tool list | not wired (the tool list itself is still Phase 0: plain chat, no tools) |

## 3. Provenance and the to-verify list

This page integrates an external AI-generated draft suite. Integration rule:
keep the drafts' prompt TEXT (it is decent craft) but never its invented
architecture. Items flagged, in one place:

- `k10r.store()` as a tool name — not in the design; the tool faces are
  `commit_memory` / `recall_memory`, the k10r door is `ingest(fact)`.
- `confidence` per fact — not in the design (weight system + read-side landing
  floor instead).
- `since` supplied by the model — the caller owns the clock; needs a ruling for
  facts.
- "Layer 3 Semantic Capsule" — the draft's numbering, and its whole
  consolidation pass is retired (ADR-0010 §4): there is no capsule, and the
  read renders the community. The draft's capsule prompt is therefore not
  carried here at all.
- Sample entities (`galaxy_repo`, `k10r_core`) and the dropped-dependency
  example are illustrative, not measurements.