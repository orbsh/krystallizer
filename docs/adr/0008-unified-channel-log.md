# ADR-0008: Unified Channel Log — Agent Context as a Projection, Heuristic Compression

**Status**: Accepted (design; implementation pending)
**Date**: 2026-09-22
**Chinese**: [0008-unified-channel-log.zh-CN.md](0008-unified-channel-log.zh-CN.md)
**Amends**: ADR-0001 (summarize's truncation step), ADR-0002 (session message key), ADR-0006 (session read/write shape)

## Context

ADR-0001 defined session control (branch/tail/summarize) over an
agent-owned session document, and ADR-0002 keyed session messages as
`(ns, user_id, session_id, seq)`. Two consumer shapes were left as
separate containers:

1. **Human chat** — multi-party channels, private chats (a two-member
   channel), read cursors, threads anchored on messages, topic tags.
2. **LLM conversation** — an agent's context window, its length control
   (compression), and cooperation with active memory extraction.

Both shapes consume the same underlying object — an ordered message
log — but the draft carried them as two containers. That meant
implementing "pull increments", "compression", and "read tracking"
twice, and the differences between the two implementations (coverage
vs unread cursor, checkpoint vs history) are exactly the differences a
projection layer absorbs.

A second defect surfaced in the same area: ADR-0002 rejected wall-clock
timestamps for message ordering ("two messages in the same millisecond
must not reorder") in favor of a per-session monotonic seq. The
rejection considered only cross-sender disorder; it missed the reverse
risk — a seq assigned by a shared counter has no relationship to
causality, while a reply's timestamp provably postdates the message it
replies to (the replier must have seen it).

## Decision

### 1. One container: the channel log

The human chat and the LLM conversation share ONE storage primitive:
an **append-only channel log**, keyed `(ns, channel_id, timestamp)`.
Nothing else is a container.

- Private chat = a channel with two members. A group chat = N members.
- An agent's "session" is NOT a container. It is the agent's **own
  projection** onto the channel: its checkpoint records + the
  increment after its coverage point + its unread tail.
- Threads are sub-channels anchored at `(channel_id, timestamp)` — the
  same messages table with a parent pointer, a public second dimension.
  They are NOT agent branches.
- Topic tags are a SECONDARY INDEX over message rows (OKM index
  entries; tag trees per the existing tag-tree design), never a third
  container. Forum-style consumption (pull by topic) and IM-style
  consumption (follow by time) are two read paths over one log.

### 2. The agent's context is a projection

The agent never owns, truncates, or rewrites the log. Its context is
assembled at read time:

```text
channel:  [m1][m2][m3][m4][m5][m6][m7]...
                        ^
agent A: checkpoint(c1: summary of m1..m4) + [m5][m6][m7]
agent B: cursor at m3 → unread [m4][m5]...
```

- **Compression is append, never truncate.** summarize = run the
  summary through a tail (ADR-0001's single-turn branch) → append a
  checkpoint record (coverage seq + summary text) → the projection
  now reads checkpoint + post-coverage increment. The log itself
  never shrinks; a bad summary can be re-run, and history can be
  re-segmented and re-compressed at any time. This breaks the
  "compression must swallow all history" convention: each compression
  covers a bounded span, so summary quality stays bounded.
- Checkpoints are keyed per member — `(ns, channel_id, member_id,
  timestamp)` — they are private projection state, not shared log
  content. Two agents on one channel compress independently.
- Read cursors `(ns, channel_id, member_id) -> last-read timestamp`
  serve BOTH the IM unread badge and the agent's incremental pull:
  one mechanism, two consumers.

This amends ADR-0006: "fetch full session, write back session"
becomes "append own messages, read own projection". The stateless-loop
property is unchanged — persistence and assembly still sink into the
storage layer; only the read/write shape changes. The loop still never
mutates history; now the storage layer structurally forbids it too.

### 3. Order by timestamp (amends ADR-0002's seq)

Message ordering uses **timestamps assigned by gravity at write time**,
not a monotonic counter.

- **Causal consistency for free**: a reply must postdate the message it
  answers (the replier has seen it), so a physical clock gives a total
  order consistent with the causal partial order — a Lamport-clock
  argument with real clocks. A shared seq counter has no such
  property: a reply could land between concurrent messages it
  logically follows.
- **Cross-sender disorder is harmless**: two messages with no causal
  link are concurrent; their relative order changes no semantics.
- **Single clock authority**: gravity's clock IS the channel's time.
  The conversation flow runs on internal time; user client clocks
  never assign order (client timestamps are display metadata at most).
  This removes multi-party clock-skew concerns: only gravity's clock
  needs to be sane, and skew cannot break causality (a reply is always
  written after what it answers was written).
- **Same-millisecond bursts** (gravity serving many users/channels):
  the key appends a tiebreaker — `(ts_ms u48, sender_hash u8, sub u8)`
  — stable width, no impact on max-gap computation.
- **max-gap becomes a time difference**: with time as order, the
  largest gap is the max delta between adjacent entries in one range
  scan — no second index, no seq→time lookup.
- Storage cost is a net save: events must carry timestamps anyway; the
  separate seq column disappears.

### 4. Compression is a parameterized policy engine

Compression triggering is heuristic, never an LLM judgment: deciding
"did the topic change" via LLM costs one inference and still needs the
summary inference — two model calls for one opportunity. The split
point must be free. The heuristics:

- **Cache-clock trigger**: when the prompt-cache TTL approaches expiry
  (e.g. 50 idle minutes of a 1-hour cache) AND the context carries
  weight (above a minimum percentage), compress — the cache is about
  to invalidate anyway, so compression costs nothing. If the context
  is trivial (a few messages, under the floor), skip; the clock is a
  free signal, not an obligation.
- **Budget trigger**: at a mid-band threshold (e.g. 50% of context,
  NOT 90% — tiered pricing makes the low band much cheaper, and
  compression quality degrades as spans grow; compressing early keeps
  each summary small and good).
- **Max-gap split point**: within a bounded window before the
  coverage head, split at the largest inter-message time gap —
  conversation pauses are the observable trace of topic changes, at
  zero model cost. Trigger and split must corroborate: if the max gap
  IS the current silence (the idleness that fired the trigger), do not
  compress.

**All thresholds are parameters, not constants.** A policy configuration
(global defaults in `krystallizer.kdl`) holds every number: cache
fractions, budget percentage, window bounds, minimum context floor.
Per-user overrides are a reserved second layer (a lookup before the
global default) — reserved structurally now, populated manually or by
future tuning.

**Decision logging over model fitting.** A regression model needs
labels ("was this compression good?") that do not exist yet; fitting
without them fits noise. Every compression decision therefore logs a
structured record — parameter snapshot, trigger signal values, split
position, pre/post token counts — which is the future training set.
The learning loop closes when real feedback signals exist; the engine's
"engine" today means parameterizable + auditable, not adaptive.

### 5. Division of labor with active memory extraction

Both consume the same log; they serve different layers and do not
merge:

- **summarize** serves the CONTEXT: lossy, for the model's working set,
  within the turn's latency budget.
- **extraction** serves CROSS-SESSION MEMORY: atomic facts into flat
  memory / the graph, asynchronous, outside the turn's path. The
  extraction worker trails the coverage pointer — the log is
  append-only, so extraction can lag, batch, and re-run freely without
  contending with conversation latency.

The root cause of keeping them separate: different quality/latency
budgets (summary in-turn, extraction out-of-band).

**Multi-party participation** — member identity (display name / role
in the members table, attribution applied at serialization), user
profiles (a managed `kind=profile` category of global memory,
extracted and weight-governed), and the interrupt policy (seconds-scale
passive trigger through the same policy engine, silent outcomes logged)
— is specified in ADR-0009. The single-user shape needs nothing extra:
one member, every trigger on the turn boundary.

## Why Not

- **Session as a second container (kept for LLM chats)**: implements
  pull-increments, compression, and read tracking twice; the differences
  (coverage vs cursor, checkpoint vs history) are all absorbable by the
  projection layer. Rejected.
- **LLM-judged topic boundaries**: one inference to judge + one to
  summarize = two calls per compression; the heuristic (max time gap)
  is free and corroborates the trigger. LLM judgment only pays off when
  it replaces the summary call too — which is "compress on the spot",
  the thing the trigger avoids. Rejected as the split-point mechanism.
- **Truncating compression (mainstream shape)**: destroys re-run
  ability. A bad summary over a truncated log is permanent; over an
  append-only log it is a retry. Rejected.
- **Fit the policy parameters now (regression on logged decisions)**:
  no labels yet; the fit would track noise. Decision logging is the
  cheap, correct first half. Deferred with a reserved structure.

## Consequences

- ADR-0002's `(ns, user_id, session_id, seq)` message key is superseded
  by `(ns, channel_id, timestamp+tiebreaker)`; session_id disappears
  from the key space — the unification lands here.
- ADR-0001's summarize loses its truncation step (replaced by coverage
  advance + checkpoint append); branch/tail are unchanged, and "branch"
  (agent-private, checkpoint-prefix tree) stays distinct from "thread"
  (public sub-channel) — both are parented logs differing in visibility.
- The agent loop's contract (ADR-0006) restates as: append own records,
  read own projection; scale-to-zero and statelessness carry over.
- New tables: messages, members, cursors, checkpoints (per member),
  message tag index. All OKM declares; hex stability tests lock layouts.
- The policy engine is pure functions in k10r (threshold evaluation,
  max-gap split, decision log records); LLM calls (summary) stay on
  gravity's side per ADR-0007.
