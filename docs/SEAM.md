# SEAM — a Consumer's Operations Against k10r's Interfaces

> **Languages:** [English](SEAM.md) (primary) · [中文](SEAM.zh-CN.md)

What this page is: the **inventory**. Which door a consumer knocks on, what
that door maps to inside k10r, and whether the door exists yet. The design
behind each door lives in the ADRs (`docs/adr/`); the schemas live in
`docs/MODELING.md`; this page only states the correspondence, so that a
change on either side has one place to look.

Today there is exactly one consumer: **gravity** (the turn executor). Its
operations are listed below; a second consumer (a memory-primary program, an
agent with no turn loop) joins by adding a section, not a new mechanism.

## 1. k10r's doors, as implemented

Routes are declared in `crates/mem-server/src/lib.rs` (the router block);
primitives in `crates/mem-core/src/`.

| Door (HTTP) | mem-core primitive | Table (ns) | Status |
|---|---|---|---|
| `POST /channels/{c}/messages` | `ChannelStore::append` | messages (33) | landed |
| `GET /channels/{c}/members/{m}/session` | `ChannelStore::fetch_full` | messages + checkpoints (33/34) | landed |
| `POST /channels/{c}/members/{m}/checkpoint` | `ChannelStore::append_checkpoint` | checkpoints (34) | landed |
| `POST /code` | `CodeStore::put` | code (35) | landed |
| `GET /code` | `CodeStore::list` (pool read: id + language + description, no source) | code (35) | landed |
| `GET /code/{id}` | `CodeStore::get` (the whole row, source included) | code (35) | landed |
| — no route | `MemoryStore::store` / `search` / `forget` / `list` | flat memory (32) | core landed, no endpoints (PLAN 1e) |
| — no route | `ingest(fact)` + graph reads (traversal, weight Top-K) | fact graph | Declared (ADR-0002/0003), no ns yet |
| — no route | initialization-memory assembly (traversal from the user node) | initialization memory | Declared (ADR-0001 Update 2026-10-16), no ns yet |
| — no route | task vertices + `task_links` edges (the third interface surface — a graph, sharing the facts' graph) | task graph | Declared (ADR-0001 Update 2026-10-16), no ns yet |
| rides the session projection (read); no write route | task instance state (open / done — a structured field for exact enumeration) | task state | Declared (gravity ADR-0003 §3/§4) |

`in-process` is not a second door: `InProcessSurface` links the same
primitives in the caller's process (a same-machine deployment detail); HTTP is
the single cross-process form (ADR-0007 as revised).

## 2. gravity's operations → the door each one uses

| gravity operation | gravity-side name | Door | mem-core primitive | Status |
|---|---|---|---|---|
| Turn-start projection read | `MemorySurface::fetch_session` | `GET /channels/{c}/members/{m}/session` | `ChannelStore::fetch_full` | landed |
| Turn-end delta append (user + reply, one `ts_ms`, `sub` 0/1) | `MemorySurface::append_message` | `POST /channels/{c}/messages` | `ChannelStore::append` | landed |
| Compression landing (summarize) | not wired | `POST /channels/{c}/members/{m}/checkpoint` | `ChannelStore::append_checkpoint` | door landed, gravity side owed (Phase 2/2.6) |
| Task progress (one task row per step; read with the turn-start projection, completion written at the call site) | ADR-0003 §3/§4 | read side rides `GET /channels/{c}/members/{m}/session`; the write face has no route yet | task vertices + `task_links` edges in the same graph as the facts | Declared — the third interface surface (ADR-0001 Update 2026-10-16); gravity Phase 2.6 |
| `recall_memory` — long-term facts | tool face (not in code yet) | — no route | `MemoryStore::search`; later the graph's retrieval (landing + traversal + weight Top-K) | k10r door owed (PLAN 1e); tool owed |
| `commit_memory` — write facts | tool face | — no route | `MemoryStore::store` (flat) / `ingest(fact)` (graph) | same |
| `resolve_skills` — the selectable skill list | tool face | `GET /code` (bring-up: the pool read) | `CodeStore::list` | bring-up form landed; the skill NODE (preconditions / steps / experience / verification) is Phase 5+ deferred |
| `invoke_skill` — plan step: fetch the code row | tool face | `GET /code/{id}` | `CodeStore::get` | landed |
| `invoke_skill` — the call itself | plan step | **not k10r** (see §3) | — | gravity Phase 4.6 |
| Initialization memory (persona / style) | — the caller performs no operation: it arrives inside the projection | — | assembly is k10r-internal (a traversal from the user node, injected at the head of the context) | Declared (ADR-0001 Update 2026-10-16) |

## 3. What is not a k10r door

Execution never reaches k10r. `invoke_skill` dispatches through the
framework's governed call — `invoke(mount_type, session_key, "execute",
{"code": <source>, "args": <model args>})` — to an **effector node mount**;
the script was fetched from k10r one step earlier (`GET /code/{id}`) and
travels as a call argument. The mount registration face is prism's
(`crates/prism/src/mounts.rs`). So the two families part company at the edge:
the memory family (and the skill family's *resolution*) are calls to k10r,
the skill family's *execution* is a call to a node. No k10r route ever becomes
an LLM tool target, and k10r never calls back — it owns no policy and is not
an actor (ADR-0007).

## 4. Invariants that make this table readable

- **Append-first is the GRAPH's rule; the session log is append-only**
  (ADR-0002 Update 2026-10-16): appending is the default wherever a table holds
  knowledge or data (node/edge rows, weights, derived entries, code rows) — no
  read-modify-write race, replayable, a bad derivation is a retry — and a
  modification is allowed where the ban would cost more than the update itself.
  The log is not that case: it stays append-only, and the compression point
  lives in its own table pointing into the log by `coverage_ts`.

- **The caller owns the clock.** `ts_ms` / `sub` ride the request; the server
  never assigns (ADR-0008 §3). Any new door carries the same rule.
- **One database, two keyspaces, split by write shape** (one per process, one
  lock): `channel` (ns 33/34 — queue-like, its own LSM tree) and `memory`
  (ns 32/35 today, the graph and skills later). A new table joins the
  keyspace its write shape belongs to; a batch never spans keyspaces.
- **gravity speaks mem-server's wire itself, in two places**: `HttpSurface`
  (`crates/core/src/surface.rs`) and the booth's turn guest
  (`crates/booth/src/bin/turn.rs`, which builds the same URLs) — one
  contract, two call sites, kept in step by hand.
- **Counters are k10r's, the act is gravity's**: `tool_invoke_count` rides the
  weight system (ADR-0005, Phase 5), and each tool's fast/idempotent vs
  human-touch/external nature is declared at registration on gravity's side
  (gravity Phase 2.5).
- **A failure is a value**, not a torn write: a failed turn appends nothing;
  every door is either called with a complete exchange or not at all.

## 5. Keeping this page true

- A new door is registered here **in the same pass** that lands it (route +
  primitive + ns + status), normally alongside its PLAN phase sub-step and its
  ADR Update.
- A door's status moves only with evidence: the integration test that
  exercises it through a real server, or the core test that locks the
  primitive.
- Design questions do not live here. If a sentence would need justification,
  it belongs in the ADR, and this page points at it.