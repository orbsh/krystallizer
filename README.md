# Krystallizer — Agent Memory Core

Graph-native agent memory: session control, atomic-fact graph, vector-landing retrieval — as an embedded Rust core with Python bindings.

## What it is

A standalone memory system extracted from skillforge's `src/mem`, rebuilt on
the KV architecture described in [KV 存储引擎](https://github.com/orbsh/wiki/blob/main/kv-storage-engine-en.md)
and [图谱化记忆](https://github.com/orbsh/wiki/blob/main/graph-memory.md):

- **Session control surface** — full conversation control as primitives:
  `branch` (fork at any prefix; checkpoint sharing makes forks zero-copy),
  `tail` (one-turn branch: inject a prompt, discard after the turn), and
  `summarize` (inject + truncate + checkpoint). Trigger policy lives with the
  caller; the core only provides primitives.
- **Memory surface** — a standing capability offered as tools, in three kinds
  (ADR-0001: 被动 / short-term (the loop's own context —
  the channel log and its projection), 主动 / long-term
  (`store` / `search` / `forget` over flat memory and graph facts), and
  initialization memory (persona / style — graph-derived, injected at the
  head of the context).
- **Task surface** — tasks are a graph too: task vertices + `task_links`
  dependency edges, sharing the same graph as the facts; instance state is
  structured (exact enumeration) and travels with the projection. The third
  interface surface (ADR-0001.
- **Atomic-fact graph** — append-first facts, a fixed-length 8-slot tuple
  (roles `head` / `rel` / `tail` — head/tail `[value, type, props]`, the
  relation `[name, props]`); the write side's collections and
  `ingest(fact)` landed 2026-10-17 (entities ns 36 + facts ns 37,
  ADR-0010). Entity-anchored
  / temporal / causal-chain
  clustering as views, weight system (read-write quadrants,
  `tool_invoke_count`), fusion on write (in-domain id lookup / out-of-domain
  embedding convergence).
- **Built-in retrieval** — a small local embedding model runs in-process
  (no API, no key; the vector space is locked with the data), landing into
  graph traversal alongside an exact name/alias lookup and weight-ordered
  Top-K. No lexical index, no fusion stage, no second storage engine.

## Crates

| Crate | Role |
|---|---|
| `mem-core` | Storage + retrieval core. Zero LLM dependency, zero transport dependency. |
| `mem-ffi` | PyO3 bindings (maturin). |
| `mem-server` | axum HTTP surface — the single cross-process form (ADR-0007 as revised; channel-log endpoints landed 2026-10-10, the code store 2026-10-15, the fact graph 2026-10-17 — the write door `POST /facts/{scope}`, the exact name entrance, one-hop edges, and the render graph `GET /graph/{scope}`). |

Storage: Fjall, with schemas declared via OKM derive macros (compile-time key
encoding, hex stability tests).

## Status

Design phase — see `docs/PLAN.md` (phases), `docs/adr/` (design),
`docs/MODELING.md` (schemas), `docs/SEAM.md` (a consumer's operations
against the interfaces above), and `docs/PROMPTS.md` (the two prompt
surfaces: the live active-add tool definition and the write-side capsule
generator).
