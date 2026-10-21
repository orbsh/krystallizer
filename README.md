# Krystallizer — Agent Memory Core

Graph-native agent memory: session control, atomic-fact graph, vector-landing retrieval — as a Rust core, reached over HTTP by a process you can run (`krystallizer`) or linked in-process as a crate.

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
  (`store` / `search` / `forget` over the fact graph — flat memory was its
  pre-graph form and is withdrawn), and
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
  (no API, no key; the vector space is locked with the data). The query lands,
  the answer is the landed COMMUNITY (its nodes and edges, weight-ordered,
  rendered as id-bearing text under a fact budget, in the representation
  `?format=` names — `plaintext` / `markdown` / `mermaid`, or `struct` for the
  data alone), and an exact name/alias point lookup is the second entrance. No lexical index, no fusion stage, no
  summarization pass, no second storage engine.

## Crates

| Crate | Role |
|---|---|
| `mem-core` | Storage + retrieval core. Zero LLM dependency, zero transport dependency. |
| `mem-server` | axum HTTP surface — the single cross-process form (ADR-0007 as revised). It serves TWO classes of door, kept apart in `docs/SEAM.md`: the CORE API a consumer runs on (session projection and appends, `POST /memories/{scope}`, `GET /memories/{scope}/recall`, the exact name entrance, `.../edges/{id}/touch`, the code store) and the ANALYSIS API for audit and visualization (the render graph `GET /graph/{scope}`, the landing alone, one hop). It also owns the ONLY outbound client in the project: the embeddings client for `mode "remote"` (ADR-0003), which is why `mem-core` can stay transport-free. |

Storage: Fjall, with schemas declared via OKM derive macros (compile-time key
encoding, hex stability tests).

## Run

```
cargo build --release
./target/release/krystallizer [--config <path>] [--addr 127.0.0.1:7760]
```

The service's parameters are ONE config file, `krystallizer.kdl`: `storage.data-dir`
(the database's directory, created if absent, `$HOME` allowed), the `embedding`
engine — the built-in model in-process (`mode "local"`), or an OpenAI-shaped
embeddings service on another machine (`mode "remote"`: `base-url` / `model` /
`api-key-env` / `timeout-ms`) — the `compression` numbers and the `logs` ceiling.
`--config` names the file; without it the binary reads the one BESIDE ITSELF, so a
deployment ships the two together (`config/krystallizer.kdl` in this repo, copied
next to the binary). A missing config is a boot failure, never a fallback: a service
that creates a database in whatever directory it happened to start from loses data
the next time it starts somewhere else. The address it prints is the BOUND one, so
`--addr 127.0.0.1:0` (OS-picked port) still tells a supervisor where to look.

Both embedding modes are proven at boot: a missing artifact, an unreachable service,
or a width that disagrees with `dimensions` stops the process, because rows written
without a working engine would be rows nobody can retrieve.

## Status

Design phase — see `docs/PLAN.md` (phases), `docs/adr/` (design),
`docs/MODELING.md` (schemas), `docs/SEAM.md` (a consumer's operations
against the interfaces above), and `docs/PROMPTS.md` (the live active-add
tool definition — the one prompt surface; the write-side capsule generator
retired with ADR-0010's consolidation pass).
