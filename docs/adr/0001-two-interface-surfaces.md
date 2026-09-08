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
