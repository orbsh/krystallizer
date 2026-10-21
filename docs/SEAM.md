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

**Two classes of door, and they are not interchangeable.**

- **The CORE API** — what a consumer's operation actually runs on: the loop's
  context read and appends, the memory answer, the exact identity entrance, the
  counter a caller reports, the code store. These doors' SHAPES are contract:
  a change is an ADR-level decision, and a consumer's turn path may only use
  these.
- **The ANALYSIS API** — read-only inspection for audit, visualization and
  debugging: the raw graph, the landing in isolation, one hop, the unrouted
  core reads. It is a SECOND-CLASS surface: its shape may change as the
  analysis needs it, it is never on the model's tool list, and nothing in a
  consumer's turn loop may depend on it.

The line exists because internal structure may appear INSIDE a core result
(node ids, edge ids, the community id) as handles, while MAINTAINING that
structure is never a consumer's chore. A door that asks the consumer to list,
pull, fix up or write back k10r's own indexing is therefore not a door of
either class — it is a design error, and the consolidation doors that used to
sit here were exactly that (retired 2026-10-20).

### 1a. Core API — `/channels`, `/memories`, `/code`

| Door (HTTP) | mem-core primitive | Table (ns) | Status |
|---|---|---|---|
| `POST /channels/{c}/messages` | `ChannelStore::append` | messages (33) | landed |
| `GET /channels/{c}/messages` | `ChannelStore::history` — the PEOPLE's view (ADR-0009 §4): every record in log order, attributed by name, no checkpoint, no summary, no merging | messages (33) + members (42) | landed (2026-10-20) |
| `GET /channels/{c}/members/{m}/session?format=&ts_ms=` | `channel::fetch_full` + `view::assemble` (head → checkpoint → increment) + `render::render_head`; `compress::evaluate` runs at the read and its verdict rides `compress: {checkpoint, prompt}` — the engine first opens the in-flight node at that split (empty summary = the marker, surfaced as the DTO's `compressing`); `note_read` stamps THAT member's cursor (the cache clock measures against it; single-user + agent: the log's last record when no row exists) | messages (33) + checkpoints (34) + cursors (39, multi-party) + the head's facts (36/37) | landed (view shape 2026-10-20; compression verdict 2026-10-20) |
| `POST /channels/{c}/members/{m}/checkpoint` | `ChannelStore::complete_checkpoint` (the body is the SUMMARY alone; it fills the open row at the split k10r served; 400 when no row is open) | checkpoints (34) | landed (2026-10-20) |
| `POST /memories/{scope}` | `GraphStore::ingest` (the body IS the 8-slot tuple; `scope` is the path) | entities (36) + facts (37) | landed (2026-10-17) |
| `GET /memories/{scope}/recall?q=&k=&budget=&ts_ms=&format=` | `GraphStore::recall` (landing → the landed COMMUNITY's bucket → one representation: the rendered, id-bearing text or, under `format=struct`, the data alone; ranked by weight, capped by `budget`; no engine = an empty answer) | entities (36) + facts (37) | landed (2026-10-20) |
| `GET /memories/{scope}/entities/{name}` | `GraphStore::entity_by_name` (the exact name/alias entrance; 404 on an unknown name) | entities (36) | landed (2026-10-17) |
| `POST /memories/{scope}/edges/{id}/touch` | `GraphStore::touch` (a caller-reported TOOL consult: counters bump, weight folds; 404 for an unknown edge or another scope's) | facts (37) | landed (2026-10-19) |
| `POST /code` | `CodeStore::put` | code (35) | landed |
| `GET /code` | `CodeStore::list` (pool read: id + language + description, no source) | code (35) | landed |
| `GET /code/{id}` | `CodeStore::get` (the whole row, source included) | code (35) | landed |

Withdrawn from this class (2026-10-19, with flat memory): `POST /memory`,
`GET /memory/{user_id}/search?q=`, and `MemoryStore::forget` / `list` /
`stats` — the graph is the carrier (ADR-0001).

Declared, not yet a door (core side, later phases): initialization-memory
assembly (rides the session projection's head, ADR-0001/0008), task vertices +
`task_links` edges and task instance state (the third interface surface,
ADR-0001; gravity ADR-0003 §3/§4).

**Internal diagnostics have NO door.** The structured log (ns 41, ADR-0011) is
written by k10r itself and read only through a future analysis-class door; it is
never on a consumer's turn path and never a tool.

### 1b. Analysis API — `/graph`

| Door (HTTP) | mem-core primitive | Table (ns) | Status |
|---|---|---|---|
| `GET /graph/{scope}` | `GraphStore::scope_entities` + `scope_facts` (one scope as nodes + links — the renderer's bulk read; nodes carry `community_id`) | entities (36) + facts (37) | landed (2026-10-17) |
| `GET /graph/{scope}/search?q=&k=` | `GraphStore::retrieve` (the landing ALONE: node vectors, inner product, the 0.35 floor; no engine = 200 + `[]`) — what `recall` does before it answers | entities (36) | landed (2026-10-19) |
| `GET /graph/{scope}/entities/{node_id}/edges` | `GraphStore::out_facts` (one hop, both endpoints resolved) — inspection, and a caller's own composition starts here | facts (37) + entities (36) | landed (2026-10-17) |
| `GET /graph/{scope}/entities/{node_id}/anchored` | `GraphStore::anchored` — the entity-anchored entrance (PLAN 4b): the facts touching the node, then its `community_id` slice, heaviest first; 404 for an unknown node | entities (36) + facts (37) | landed (2026-10-20) |
| `GET /graph/{scope}/timeline?k=` | `GraphStore::timeline` — the temporal entrance: the scope's facts by `since`, newest first, walked over the `by_since` face (scope-blind: other scopes' keys are skipped) | facts (37) | landed (2026-10-20) |
| `GET /graph/{scope}/chain?from=&kind=&hops=` | `GraphStore::chain` — the causal entrance: one named predicate followed hop by hop, cycle-safe and bounded; the `depth` in each row is the walk's order | facts (37) + entities (36) | landed (2026-10-20) |
| `GET /graph/{scope}/facts/{edge_id}` | `GraphStore::fact_in_scope` — the DETERMINISTIC read: the same `edge_id` the write answered with, no query and no embedding; the scope is checked against the edge's own endpoint, so another scope's id is a 404 (the address of a reference link) | facts (37) | landed (2026-10-20) |
| `DELETE /graph/{scope}/facts/{edge_id}` | `GraphStore::withdraw` (okm's `unlink`: row + every derived face entry in ONE batch; 204, or 404 when the id names nothing here) | facts (37) | landed (2026-10-20) |
| — no route | `entity` / `entity_props` / `fact` / `fact_props` / `typed_out` (the rest of the core's READ primitives) | fact graph (36/37) | core landed, no route — audit reads |
| `GET /graph/{scope}/top?k=` | `GraphStore::top_by_weight` (the scope's heaviest facts, weight-descending, ties by edge id; the ordering is the declared `by_weight` face — ADR-0005's `W:{weight}:{edge_id}`, maintained on every write — and the RESULT is bounded by `k`) | facts (37) | landed (2026-10-20; O(k rows) + O(scope keys) via the `by_scope_weight` face — the lazy key walk is still owed in okm-core) |

`in-process` is not a second door: `InProcessSurface` links the same
primitives in the caller's process (a same-machine deployment detail); HTTP is
the single cross-process form (ADR-0007 as revised).

## 2. gravity's operations → the door each one uses

Every operation in this table rides a CORE door (§1a); the analysis class
(§1b) appears nowhere in a consumer's turn loop.

| gravity operation | gravity-side name | Door | mem-core primitive | Status |
|---|---|---|---|---|
| Turn-start projection read (the head is TEXT k10r rendered, in the format gravity's config names; `struct` is refused there) | `MemorySurface::fetch_session` | `GET /channels/{c}/members/{m}/session?format=` | `ChannelStore::fetch_full` + `render::render_head` | landed; the signature does NOT move with the pointer ruling — the read is an act BY that member (its cursor is stamped, its projection is the answer) |
| Turn-end delta append (user + reply, one `ts_ms`, `sub` 0/1) | `MemorySurface::append_message` | `POST /channels/{c}/messages` | `ChannelStore::append` | landed |
| Compression landing (summarize) | not wired | `POST /channels/{c}/members/{m}/checkpoint` | `ChannelStore::complete_checkpoint` | door landed, gravity side owed (Phase 2/2.6) |
| Task progress (one task row per step; read with the turn-start projection, completion written at the call site) | ADR-0003 §3/§4 | read side rides `GET /channels/{c}/members/{m}/session`; the write face has no route yet | task vertices + `task_links` edges in the same graph as the facts | Declared — the third interface surface (ADR-0001; gravity Phase 2.6 |
| `recall_memory` — long-term facts | tool face (not in code yet) | `GET /memories/{scope}/recall?q=` (landing + the landed community, rendered) beside `GET /memories/{scope}/entities/{name}` (the exact entrance) | `GraphStore::recall` / `entity_by_name` | graph doors landed (2026-10-20); the tool face is a FOLLOW-UP (gravity side) |
| `commit_memory` — write facts | tool face | `POST /memories/{scope}` | `GraphStore::ingest(fact)` | door landed (2026-10-17); the tool itself is a FOLLOW-UP (gravity side) |
| `resolve_skills` — the selectable skill list | tool face | `GET /code` (bring-up: the pool read) | `CodeStore::list` | bring-up form landed; the skill NODE (preconditions / steps / experience / verification) is Phase 5+ deferred |
| `invoke_skill` — plan step: fetch the code row | tool face | `GET /code/{id}` | `CodeStore::get` | landed |
| `invoke_skill` — the call itself | plan step | **not k10r** (see §3) | — | gravity Phase 4.6 |
| Initialization memory (persona / style) | — the caller performs no operation: it arrives inside the projection | — | assembly is k10r-internal (a traversal from the user node, injected at the head of the context) | Declared (ADR-0001 |

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
  (ADR-0002: appending is the default wherever a table holds
  knowledge or data (node/edge rows, weights, derived entries, code rows) — no
  read-modify-write race, replayable, a bad derivation is a retry — and a
  modification is allowed where the ban would cost more than the update itself.
  The log is not that case: it stays append-only, and the compression point
  lives in its own table as a `(ts_ms, sub)` pointer at one record.

- **The caller owns the clock.** `ts_ms` / `sub` ride the request; the server
  never assigns (ADR-0008 §3). Any new door carries the same rule.
- **One database, two keyspaces, split by write shape** (one per process, one
  lock): `channel` (ns 33/34 — queue-like, its own LSM tree) and `memory`
  (ns 35/36/37/38 today, the skills later; 32 was flat memory, withdrawn).
  A new table joins the keyspace its write shape belongs to; a batch never
  spans keyspaces.
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
  primitive + ns + status **and its class** — core §1a or analysis §1b),
  normally alongside its PLAN phase sub-step and its ADR. A door whose class is
  unclear is not ready to land.
- A door's status moves only with evidence: the integration test that
  exercises it through a real server, or the core test that locks the
  primitive.
- Design questions do not live here. If a sentence would need justification,
  it belongs in the ADR, and this page points at it.