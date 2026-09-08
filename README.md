# Krystallizer — Agent Memory Core

Graph-native agent memory: session control, atomic-fact graph, hybrid retrieval — as an embedded Rust core with Python bindings.

## What it is

A standalone memory system extracted from skillforge's `src/mem`, rebuilt on
the KV architecture described in [KV 存储引擎](https://github.com/orbsh/wiki/blob/main/kv-storage-engine-en.md)
and [图谱化记忆](https://github.com/orbsh/wiki/blob/main/graph-memory.md):

- **Session control surface** — full conversation control as primitives:
  `branch` (fork at any prefix; checkpoint sharing makes forks zero-copy),
  `tail` (one-turn branch: inject a prompt, discard after the turn), and
  `summarize` (inject + truncate + checkpoint). Trigger policy lives with the
  caller; the core only provides primitives.
- **Memory surface** — two modes: *full-session mode* (the session itself is
  the memory: checkpoint + increments + pending) and *assist mode*
  (`store` / `search` / `forget` flat + graph facts).
- **Atomic-fact graph** — append-only KDL facts
  (`head` / `rel` / `tail`), entity-anchored / temporal / causal-chain
  clustering as views, weight system (read-write quadrants,
  `tool_invoke_count`), fusion on write (in-domain id lookup / out-of-domain
  embedding convergence).
- **Self-built retrieval** — arroy (HNSW) vectors + hand-written BM25 inverted
  index + RRF fusion. Full-chain control over tokenization and scoring.

## Crates

| Crate | Role |
|---|---|
| `mem-core` | Storage + retrieval core. Zero LLM dependency, zero transport dependency. |
| `mem-ffi` | PyO3 bindings (maturin). |
| `mem-server` | axum service shell (deferred until multi-consumer sharing is a real need). |

Storage: Fjall, with schemas declared via OKM derive macros (compile-time key
encoding, hex stability tests).

## Status

Design phase — see `docs/PLAN.md` and `docs/adr/`.
