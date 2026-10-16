# ADR-0001: Two Interface Surfaces — Session Control and Memory

**Status**: Accepted (design; implementation pending)
**Date**: 2026-09-08
**Chinese**: [0001-two-interface-surfaces.zh-CN.md](0001-two-interface-surfaces.zh-CN.md)

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

The core exposes exactly two interface surfaces.

### Surface 1: Session control (full control mode)

Three primitives, composable, agent-driven:

```
branch(fork_point)    Fork the session at any prefix. Checkpoints are
                      immutable (prefix model), so forking is zero-copy:
                      the fork shares the checkpoint prefix; increments
                      diverge per branch.

tail(prompt)          A one-turn branch. Fork → inject prompt at the tail →
                      the turn runs → the branch is discarded. The main
                      session history is untouched (the historical
                      "history purity" guarantee, now structural).

summarize(...)        A composite operation: inject compression instruction
                      (via tail) → collect the summary → truncate the
                      compressed messages → write a new checkpoint. The
                      session shrinks.
```

Tail-prompt injection and summarization are both just branch operations:
a tail is a single-turn branch; summarization is inject+truncate. One
mechanism, no special cases.

**Trigger policy moves to the caller.** The core provides the primitives;
*when* to summarize (threshold, session-end, explicit command) is agent-side
strategy. The legacy auto-trigger becomes one optional caller-side policy,
not core behavior.

### Surface 2: Memory (assist mode)

Two modes:

- **Full-session mode** — `get_context` returns the complete context
  (checkpoint summary + DB increments + pending user messages). The session
  itself is the memory; retrieval is not involved.
- **Assist mode** — `store` / `search` / `forget` over flat memories and
  graph facts. Retrieved context is *auxiliary*: injected alongside the
  session, never replacing it.

## Consequences

- The storage core never decides anything about *when* — only *how*.
  This keeps mem-core free of policy and trivially testable.
- The legacy `checkpoint_threshold` constructor parameter and the
  tail-prompt branch inside `get_context()` disappear; callers compose
  `tail()` / `summarize()` explicitly.
- Branching requires the checkpoint-prefix model to be preserved exactly
  (immutable checkpoints, message ordering by creation). This is a
  constraint the storage schema (ADR-0002) must honor.
- The "history purity" property (tail prompts never persisted) stops being
  a convention enforced by careful code and becomes a structural property
  of one-turn branches.

## Update (2026-09-22, ADR-0008)

The session document becomes a **channel log** with the agent's context
as a projection. summarize loses its truncation step: compression is
append (checkpoint record + coverage advance) over an immutable log,
so a bad summary is a retry, not a permanent loss. branch (agent-
private checkpoint-prefix tree) and thread (public sub-channel) stay
distinct — both parented logs differing in visibility.

## Update (2026-10-10, third application shape: memory-primary)

The interface surfaces are unchanged; a third consumption stance
appears: **programs that are not chat applications but need memory —
mudra, task managers.**

Three shapes, divided by which surface stars:

| Shape | Surface 1 (session) | Surface 2 (memory) |
|---|---|---|
| Session-primary (gravity / chat) | The main surface: channel log + projection | Assist: retrieval injected alongside the session, never replacing it |
| Memory-primary (mudra / task management) | Unused, or residual use only (below) | **Everything**: store / search / forget IS the application's memory |

Memory-primary is **not a third interface surface** — the two surfaces'
primitives already cover it; the third shape is a change of consumption
stance. This retroactively validates this ADR's core boundary: the core
decides *how*, never *when*; in shape B, when-to-extract / when-to-
store is likewise caller-side policy (the chat side delegates it to
gravity; the task side to the application's own agent or deterministic
rules).

**Structured application state does not enter the memory pipeline.**
Task records, status fields, due dates are the application's own KV/
tables — exactly enumerable queries ("how many open tasks") do not go
through the approximate-retrieval pipeline, or "search for one memory"
gets mixed up with "count one status". What k10r gives these
applications is **cross-session factual memory**: user preferences,
past decisions, experience with how things were done. The approximate-
retrieval vs exact-enumeration line sits here and is not caller-
configurable.

**Surface 1's residual use**: if the application has agentic turn
sequences (multi-turn LLM work units), those turns are a sequential log
and can ride the channel log — keyed by task id rather than session
id. That is mechanism reuse, not chat semantics; purely retrieval-style
applications never touch Surface 1 at all.

## Update (2026-10-16, memory kinds; the stance naming is retired)

The pair **full control mode / assist mode** (完全控制模式 / 辅助模式) is
retired as the naming of the two surfaces: it reads as "full mode makes
assist redundant" — as if a chat-shaped consumer needed no memory
retrieval. That is false, so the surfaces are named by **who initiates**:

| | 被动面 (passive) | 主动面 (active) |
|---|---|---|
| who drives | the loop, once per turn | the model, as a tool call |
| horizon | 短期 (short): the current context | 长期 (long): facts outside it |
| carrier | the channel log + its projection | flat memory, then the graph |

短期 / 长期 name the CONTENT horizon and ride along as the qualifier; they
cannot name the surfaces by themselves, because the third kind below is
long-horizon content travelling on the passive channel.

**The memory family is a standing capability, not a mode.** Every consumer
has it — a chat-shaped agent (gravity) exactly as a memory-primary program
— and it is offered AS TOOLS, so any other agent can use it the same way.
Nothing about it is gated on the consumption stance; the 2026-10-10 Update
above already established that the stance is not a third surface.

**A third content kind gets its home here: initialization memory
(persona / style).** It is graph-derived — k10r assembles it as a traversal
starting from the user node — and it is injected at the HEAD of the context,
the position the projection's summary already occupies. It therefore rides
the passive channel while carrying long-horizon content, which is exactly
the case that separates "passive" from "short-term". The assembly is a k10r
internal detail: the caller receives it inside the projection and never
composes it. It is NOT ADR-0009's `kind=profile` category — that one holds
member profile knowledge (who a person in a channel is), not the agent's
own persona/style.

Surface 1's primitives (branch / tail / summarize) and Surface 2's
(store / search / forget) are unchanged; this note renames and adds a
content kind, nothing else.
