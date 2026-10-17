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

Krystallizer provides the opposite primitive set (ADR-0001): the
conversation container is an append-only channel log (ADR-0008) whose full
history stays retrievable at any time, and the agent's context is assembled
at read time as a projection — checkpoint records + the post-coverage
increment + the unread tail. Given that, the agent loop does not need to
maintain anything — it can be a pure function:

```
f(projection, user_input) -> records'
```

This is the same shape as Aura's "API is stateful, operations are
stateless" position (aura-architecture.md): session persistence and
recovery sink into the storage layer (krystallizer), the actor/executor
holds no state between calls, and scale-to-zero falls out for free.

## Decision

1. **Session as data.** The agent reads its own projection and appends its
   own records; it never mutates history, never assembles context from
   parts, and holds no session state between invocations. Its only write is
   *append* (tool results included). All function calls — memory ones
   included — are ordinary records: the loop stays blind to call semantics,
   and there is exactly one call mode. The container being a channel log
   means this is structural, not discipline: the log itself forbids
   mutation.

2. **View-layer pruning is a separate mechanism, not loop logic.**
   Trimming tool-call formatting out of the serialized view happens in
   krystallizer's serialization path. The storage layer always holds
   the full, unpruned session; views are trimmed, storage is not.
   (Memory-call *resolution* — replacing the call record in the view
   with the retrieved context — was considered and rejected: the
   savings are one round-trip, while it would force the loop to know
   which calls are memory calls and rewrite the mid-turn context,
   privileging one call class inside the loop and breaking the
   "records belong to the data layer, the loop only appends" invariant.)

3. **Loop decomposition.** The agent separates into four parts — UI,
   intelligent session context (krystallizer's session+memory surfaces),
   execution (tools), and loop. The loop shrinks to:
   read the projection → run turns → append the records — plus, when the
   projection carries a control field, the out-of-band branch turn it asks
   for (2.6's compression turn, ADR-0008 §4) beside the user's turn.

## Consequences

- The loop becomes trivial and portable — integration into Aura's actor
  model is direct: the actor wraps the loop, krystallizer is the
  sleep/wake persistence substrate. Scale-to-zero carries over: nothing
  in the process survives a call, and the projection is re-read on wake.
- Transport volume grows: the projection is read each turn and the new
  records are appended (tens of records on average, tool-call formatting
  pruned in the view). LLM requests are full-context anyway; this is a
  serialization cost, not a scaling one.
- State did not disappear — it moved. Krystallizer owns persistence,
  concurrent writers, view-time pruning and compression (as a checkpoint
  record pointing into the log, ADR-0008 — never a rewrite). It is the
  single owner of session truth; multi-writer semantics are its concern,
  not a blocking concern for this decision.
- View-layer trimming must stay strictly read-side: any write-path
  trimming would fork session truth between what is stored and what
  agents see.