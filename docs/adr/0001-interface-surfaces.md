# ADR-0001: Interface Surfaces — Session Control, Memory, Tasks

**Status**: Accepted (design; implementation pending)
**Date**: 2026-09-08
**Chinese**: [0001-interface-surfaces.zh-CN.md](0001-interface-surfaces.zh-CN.md)

## Context

The legacy skillforge `mem` module conflated two things that change for
different reasons:

1. **Conversation plumbing** — appending messages, prefix checkpoints,
   compression triggering. The auto-trigger lived inside
   `ConversationMemory.__init__` (`checkpoint_threshold`): the threshold
   check and tail-prompt injection were hard-wired into `get_context()`.
2. **Memory retrieval** — flat long-term facts with hybrid search, injected
   as auxiliary context.

The auto-trigger is a *policy*, not a *mechanism*. Burying policy in the
storage layer means the caller cannot turn it off, change the threshold
per-session, or trigger compression at a different moment (e.g. end of a
work unit rather than a message count).

## Decision

The core exposes three interface surfaces. A surface is a family of CORE
doors: the read-only ANALYSIS doors (the bulk graph render, the landing in
isolation, one hop, the unrouted read primitives) belong to none of them —
ADR-0007 owns the two-class split, and `docs/SEAM.md` keeps the two lists.

### Surface 1: Session control (the passive side · short-term)

Three primitives, composable, agent-driven:

```
branch(fork_point)    Fork at any prefix. Checkpoints are immutable (prefix
                      model), so forking is zero-copy: the fork shares the
                      checkpoint prefix, increments diverge per branch. Two
                      branch kinds stay distinct — an agent-private
                      checkpoint-prefix tree and a thread (a public
                      sub-channel): both parented logs, differing in
                      visibility.

tail(prompt)          A one-turn branch. Fork → inject the prompt at the tail →
                      the turn runs → the branch is discarded. The main
                      history is untouched (the "history purity" guarantee,
                      now structural rather than a coding convention).

summarize(...)        A composite: inject the compression instruction (via
                      tail) → collect the summary → append a checkpoint record
                      and advance coverage. Compression is APPEND over an
                      immutable log — nothing is truncated, so a bad summary is
                      a retry, not a permanent loss.
```

The container is a **channel log** (ADR-0008): the human chat and the LLM
conversation are one shape, and the agent's context is a **projection** —
checkpoint records + the post-coverage increment + the unread tail — assembled
at read time.

**Trigger policy moves to the caller.** The core provides the primitives;
*when* to summarize (threshold, session-end, explicit command) is agent-side
strategy. The legacy auto-trigger becomes one optional caller-side policy,
not core behavior.

### Surface 2: Memory (the active side · long-term)

The memory family is a **standing capability of every consumer**, offered AS
TOOLS. The two memory-bearing surfaces are named by **who initiates** — 被动 /
主动. The pair *full control mode / assist mode* (完全控制模式 / 辅助模式) is
retired: it reads as "full mode makes assist redundant", as if a chat-shaped
consumer needed no memory retrieval, which is false.

| | 被动面 (passive) | 主动面 (active) |
|---|---|---|
| who drives | the loop, once per turn | the model, as a tool call |
| horizon | 短期 (short): the current context | 长期 (long): facts outside it |
| carrier | the channel log + its projection | the fact graph (flat memory was its pre-graph form) |

短期 / 长期 name the CONTENT horizon and ride along as the qualifier; they
cannot name the surfaces by themselves, because the third kind below is
long-horizon content travelling on the passive channel.

Three kinds:

- **被动 (short-term)** — the loop's own current context: the channel log plus
  its checkpoint (a pointer into the log + that span's summary). The session
  itself is the memory; retrieval is not
  involved, and the model never touches it.
- **主动 (long-term)** — facts outside the context, queried by the model AS A
  TOOL: `store` / `search` / `forget` over the fact graph. Flat memory — the
  pre-graph carrier of the same knowledge — was withdrawn with the graph's
  landing (2026-10-19): its table, `MemoryStore` and the `/memory` door are
  gone, and ns 32 is free. Retrieved context is
  *injected*: alongside the session, never replacing it.
- **Initialization (persona / style)** — graph-derived: k10r assembles it as a
  traversal from the user node and injects it at the HEAD of the context, the
  position the projection's summary already occupies. It therefore rides the
  passive channel while carrying long-horizon content, which is exactly the
  case that separates "passive" from "short-term". The assembly is a k10r
  internal detail: the caller receives it inside the projection and never
  composes it. It is NOT the profile knowledge of ADR-0009 — that is a set of
  graph facts about a member (a member node plus its edges: who a person in a
  channel is), a different content kind.

### Surface 3: Tasks (the third surface)

- **Its carrier is a graph, not a table.** Task vertices plus `task_links`
  dependency edges — the shape the task-forest already assumes (a task row per
  step, bound to the call that completes it — gravity ADR-0003 §3) and the
  medium multi-agent coordination runs on (the graph replaces message passing;
  parallelism comes from topology, not from a scheduler).
- **Tasks and facts share ONE graph.** Same trust domain: don't split what can
  be joined. A task node type is a collection (ns IS the type marker); its ns
  is not assigned yet.
- **Instance state is structured and exact.** Open / done is a declared field
  for enumeration ("how many open tasks") — never an approximate-retrieval
  fact; it travels with the projection, so every in-turn pre-dispatch check on
  gravity's side is a local read (gravity ADR-0003 §4).
- **Why this is a surface and not a fourth memory kind:** the memory surface
  answers *what is known*, by approximate retrieval; the task surface is the
  coordination and progress medium — dependency order, what may run in
  parallel, and, when work goes through it, the organizational ledger of who
  did what. Different question, different access shape.
- **Where it sits on the 被动 / 主动 axis:** task reads ride the passive channel
  (they arrive with the projection) and completion is written at the call site
  by the executing side — completion is never a model judgement. k10r holds the
  task definition, its links and its structured state.

### Application shapes (a consumption stance, not a surface)

Three shapes, divided by which surface stars:

| Shape | Surface 1 (session) | Surface 2 (memory) |
|---|---|---|
| Session-primary (gravity / chat) | The main surface: channel log + projection | Assist: retrieval injected alongside the session, never replacing it |
| Memory-primary (mudra / task management) | Unused, or residual use only (below) | **Everything**: store / search / forget IS the application's memory |

Memory-primary is **a consumption stance, not a surface of its own** — the
existing primitives cover it, and what changes is the stance, not the
interface. This retroactively validates the core boundary: the core decides
*how*, never *when*; for a memory-primary shape, when-to-extract /
when-to-store is likewise caller-side policy (the chat side delegates it to
gravity; the task side to the application's own agent or deterministic rules).

**Structured application state does not enter the memory pipeline.** Task
records, status fields, due dates are the application's own KV/tables — exactly
enumerable queries ("how many open tasks") do not go through the
approximate-retrieval pipeline, or "search for one memory" gets mixed up with
"count one status". What k10r gives these applications is **cross-session
factual memory**: user preferences, past decisions, experience with how things
were done. The approximate-retrieval vs exact-enumeration line sits here and is
not caller-configurable.

**Surface 1's residual use**: if the application has agentic turn sequences
(multi-turn LLM work units), those turns are a sequential log and can ride the
channel log — keyed by task id rather than session id. That is mechanism reuse,
not chat semantics; purely retrieval-style applications never touch Surface 1
at all.

## Consequences

- The storage core never decides anything about *when* — only *how*. This keeps
  mem-core free of policy and trivially testable.
- The legacy `checkpoint_threshold` constructor parameter and the tail-prompt
  branch inside `get_context()` disappear; callers compose `tail()` /
  `summarize()` explicitly.
- Branching requires the checkpoint-prefix model to be preserved exactly
  (immutable checkpoints, ordering by the caller's timestamps per ADR-0008).
  This is a constraint the storage schema (ADR-0002) honors.
- The "history purity" property (tail prompts never persisted) is a structural
  property of one-turn branches, not a convention enforced by careful code.
- The two memory-bearing surfaces are named by who initiates (被动 / 主动); the
  consumer's stance never adds a surface, and the memory family is usable from
  every shape.