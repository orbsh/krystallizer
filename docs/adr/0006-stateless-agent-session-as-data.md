# ADR-0006: Stateless Agent Integration — Session as Data

**Status**: Accepted (design; implementation pending)
**Date**: 2026-09-08
**Chinese**: [0006-stateless-agent-session-as-data.zh-CN.md](0006-stateless-agent-session-as-data.zh-CN.md)

## Context

The conventional agent architecture keeps the session as *runtime state*
inside the agent process: the loop owns the message list, mutates it
in place, and is responsible for persistence, checkpointing, and
context assembly. This makes the loop the coupling point for everything
that touches history — memory injection, compression triggers, and
framework-specific context plumbing.

Krystallizer already provides the opposite primitive set (ADR-0001):
the session is an append-only, checkpoint-prefixed *document* addressed
by `session_id`, with the full history retrievable at any time. Given
that, the agent loop does not need to maintain anything — it can be a
pure function:

```
f(session, user_input) -> session'
```

This is the same shape as Aura's "API is stateful, operations are
stateless" position (aura-architecture.md): session persistence and
recovery sink into the storage layer (krystallizer), the actor/executor
holds no state between calls, and scale-to-zero falls out for free.

## Decision

1. **Session as data.** The agent fetches the full session by
   `session_id` from krystallizer, executes, and writes the new session
   back. The loop's only session responsibility is *append* (plus
   writing tool results). It never mutates history, never assembles
   context from parts, and holds no session state between invocations.
   All function calls — memory ones included — are ordinary records:
   the loop stays blind to call semantics, and there is exactly one
   call mode.

2. **View-layer pruning is a separate mechanism, not loop logic.**
   Trimming tool-call formatting out of the serialized view happens in
   krystallizer's serialization path. The storage layer always holds
   the full, unpruned session; views are trimmed, storage is not.
   (Memory-call *resolution* — replacing the call record in the view
   with the retrieved context — was considered and rejected: the
   savings are one round-trip, while it would force the loop to know
   which calls are memory calls and rewrite the mid-turn context,
   privileging one call class inside the loop and breaking the
   append-only invariant.)

3. **Loop decomposition.** The agent separates into four parts — UI,
   intelligent session context (krystallizer's session+memory surfaces),
   execution (tools), and loop. The loop shrinks to:
   fetch session → run turns → persist session.

## Consequences

- The loop becomes trivial and portable — integration into Aura's actor
  model is direct: the actor wraps the loop, krystallizer is the
  sleep/wake persistence substrate.
- Transport volume grows: the full session is shipped each call
  (tens of records on average, with tool-call formatting pruned in the
  view). LLM requests are full-context anyway; this is a serialization
  cost, not a scaling one.
- State did not disappear — it moved. Krystallizer now owns
  persistence, concurrent writers, and view-time pruning. It becomes
  the single owner of session truth; multi-writer semantics and
  compression-at-read are its concerns, not blocking concerns for this
  decision.
- View-layer trimming must stay strictly read-side: any write-path
  trimming would fork session truth between what is stored and what
  agents see.
