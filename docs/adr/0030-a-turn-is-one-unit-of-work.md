# ADR-0030: What an in-flight turn may assume about memory

**Status**: Accepted (design; the long-turn declaration is not implemented)
**Date**: 2026-11-17
**Chinese**: [0030-a-turn-is-one-unit-of-work.zh-CN.md](0030-a-turn-is-one-unit-of-work.zh-CN.md)

## Context

An agent turn is a stretch of work with a beginning and an end: it reads memory, decides, calls tools, and appends. Memory keeps changing while it runs — another turn in another channel writes a fact, a re-cut merges two entities, a weight decays. Two operational questions follow, and they look unrelated until they are not:

1. **Push.** A turn depends on facts `a`, `b`, `c`. If `a` changes mid-turn, does k10r push the change into the running turn?
2. **Append.** The turn is still running and the user sends a new instruction. Does the new turn run concurrently with the old one, does it cancel and merge with it, or does an LLM decide by topic whether the two interfere?

Both are questions about the same thing: whether work already in flight is a unit that something outside it can interrupt.

## Decision

**An in-flight turn is ONE unit of work.** That single sentence answers both questions, and the answers below are its consequences rather than separate policies.

1. **A read is a snapshot, and freshness has the granularity of the read.** Nothing is pushed into a running turn. Tracking what a turn used means maintaining a reverse-use index on the read path, and the payoff covers one turn's time window — after that the index describes a turn that already ended. A turn's plan is built from what it read; handing it a changed fact does not let it rebuild that plan, because rebuilding is a replanning capability, not a notification. So the boundary is the read: what a turn read is what it had, until it reads again.

2. **A turn that acts on a premise re-checks it at the action boundary.** The premise was true when read and may be false when acted on. The reread belongs to the step that depends on it, not to a push that guesses which steps do. This costs one read at the point where being wrong is expensive, which is where it belongs.

3. **A long turn may declare the keys it depends on.** This is the one exception, and it is the caller's declaration rather than the system's inference: a caller that knows it will hold a premise for minutes says which keys, and the answer tells it whether any of them moved. A declaration given by the caller is strictly better than a guess made by the system about what the caller cares about.

4. **An append queues by default; an interrupt is explicit.** A new instruction arriving mid-turn does not destroy the work in flight — queued, it runs when the turn closes. Overwriting is available, but as a deliberate act by whoever sends the instruction, never as a race between two turns. Concurrent non-destructive execution of two appends to the same session is refused: interleaved writes have no transaction and no ordering, and the second turn would silently change the goal of the first.

5. **No relevance classification.** Having an LLM decide by topic whether a new instruction interferes with the running one is the option that looks cheapest and is the least reliable: it depends on the running turn's private state (which the classifier cannot see), it is wrong exactly in the interesting case (the instruction that does not look related but changes the goal), and it makes the outcome of the same two messages depend on a sampling.

6. **Writes serialize per turn; reads may run in parallel.** One turn's rows land in batches it owns (ADR-0024 closes them together), so two turns writing the same session would need an ordering the log does not have. Reads take a snapshot and are free to run beside each other.

## Consequences

- A turn's answer is consistent with a moment: the reads it made. It is never half-updated, because nothing updates it.
- Stale premises surface where they cost something — at the action — rather than as a stream of small notifications a turn cannot act on.
- The append path is deterministic: the same two messages produce the same ordering regardless of scheduling, so a bug in it is reproducible.
- A turn that holds a premise across a long stretch needs to declare it or re-check it. Both are visible decisions; neither is automatic.
- Nothing in the store changes: this is a statement about the interface a caller sees, not a new mechanism. The declaration in (3) is the only piece that would add one, and it is not implemented.

## Rejected

- **Push changed facts into in-flight turns.** The tracking is a reverse index built on the read path, the value expires with the turn, and the act of receiving a change cannot restore the plan the turn was following.
- **Cancel the in-flight turn and merge when a new instruction arrives.** It destroys work nothing asked to destroy, and the merged result has two goals — the old turn's plan and the new instruction's — with no rule for which yields.
- **An LLM relevance gate before appending.** See (5): it needs state it does not have, and it fails where the decision matters.
- **A transaction spanning two turns.** The log gives a turn its own closure (ADR-0024) and nothing broader; a cross-turn transaction would make an unfinished turn block a new one, which is the same mistake as (4) with more machinery.