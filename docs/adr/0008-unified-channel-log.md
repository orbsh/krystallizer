# ADR-0008: Unified Channel Log — Agent Context as a Projection, Heuristic Compression

**Status**: Partially landed (2026-10-09: the minimal slice — channel messages ns 33 + per-member checkpoints ns 34, `ChannelStore::append`/`fetch_full`/`append_checkpoint`, serving gravity Phase 0; the compression point is its OWN table (ns 34), pointing into the log by `coverage_ts` and latest-wins inside that table, while the log itself stays append-only (ADR-0002); pending: threads, topic tags, members, cursors, the policy engine)
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
  checkpoint record (coverage point + summary text) → the projection
  now reads checkpoint + post-coverage increment. The log itself
  never shrinks; a bad summary can be re-run, and history can be
  re-segmented and re-compressed at any time. This breaks the
  "compression must swallow all history" convention: each compression
  covers a bounded span, so summary quality stays bounded.
- **The compression point stands BESIDE the log, not inside it.** The
  checkpoint lives in its OWN table (ns 34), pointing into the log by
  `coverage_ts` — latest-wins inside that table, so the overwrite's
  mutability never touches a log row. Append-first (ADR-0002) covers the
  graph and the data side; it does not apply to the log, which is
  append-only outright.
- Checkpoints are keyed per member — `(ns, channel_id, member_id)` — they
  are private projection state, not shared log content, and the row is
  latest-wins: a checkpoint is a cache, not an audit log (history is the
  log). Two agents on one channel compress independently.
- **Assembly is ordered, head first**: the global layer (persona / style —
  the initialization memory, ADR-0001) → the compression point → the messages
  after it. The global layer takes the head position the summary used to
  occupy alone, because it is long-horizon content riding the passive channel;
  the increment follows in key order. Assembly stays a k10r-internal detail —
  the caller receives the projection and never composes it, and the head
  arrives RENDERED (ADR-0010 §5's `format` knob): the caller injects that text
  as-is, it never assembles a head fact list of its own.
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
  the key appends a tiebreaker — `sub` (`u16`, the writer's sequential
  counter within one millisecond). The writer's own counter is the
  tiebreaker because the channel has ONE writer at a time: a channel's
  gravity instance owns its clock (below), so two appends into one channel
  are sequential, and `u16` overflow at 65 536 messages in one
  millisecond is a pathological signal, not a re-keying need. The landed
  key is `[channel_id 8B][ts_ms 8B][sub 2B]` (MODELING, locked by the hex
  test); a sender segment was considered for the many-writers-per-channel
  case and is not in the key, because no such case exists in the shape
  this ADR defines.
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

**The engine's inputs are k10r's own, and the verdict rides the
projection.** There is no compression interface on the agent's side —
the caller has no door that requests compression, and the judgment is
not the caller's. The signals are the ones k10r holds: the increment's
timestamps (max-gap), the assembled context's size (the budget
percentage and the floor), and the member's read cursor (the cache
clock — the prompt cache sits idle exactly while the member's
projection has not been read, and §2's cursor IS that timestamp). The
evaluation point is therefore the projection READ: the engine runs
while the assembly is produced and returns its verdict WITH the
assembly, never through a second call.

**The compression point may sit mid-log, and the decision is a control
field, never a marked record.** The point is the split the heuristics
chose, not the tail; everything before it is what gets compressed. It
travels beside the records the caller receives — a control field on the
projection, shaped once as an object that holds the conversation and
the control side by side (`{ records: [...], compress: { checkpoint:
30, prompt: "…" } }`): an annotation smeared over the log rows is messy
and leaves no room for the other tail-prompt and control purposes the
same field is for, whereas one structured field absorbs them all
without re-spelling the record shape. The tail prompt's TEXT rides that
field, which makes it a k10r-declared artifact (PROMPTS) rather than
something the caller invents.

**The turn that compresses is a second branch, and coverage stays
k10r's arithmetic.** On the directive the caller runs an extra turn —
the records before the point plus the tail prompt — out of band, as a
branch beside the user's turn: the user's interaction neither waits for
it nor sees it. That turn's answer IS the summary and comes back
through the ordinary append path; k10r takes the compressed span's own
last record as the compression point, so the caller never states a
coverage timestamp. This is §2's "tail (single-turn branch)" written
concretely, not a second mechanism.

### 5. Division of labor with the memory add

Both consume the same log; they serve different layers and do not
merge:

- **summarize** serves the CONTEXT: lossy, for the model's working set,
  within the turn's latency budget.
- **The memory add** serves CROSS-SESSION MEMORY: atomic facts into the
  graph — produced by the model's OWN active memory add (the tool call the
  caller routes to `ingest(fact)`), never by a compression pass. No trailing
  extraction worker exists or is scheduled. The log being append-only means a
  caller MAY lag, batch or re-run over the same bytes; it is an available
  property, not a mechanism. So summarize does not gate the add: the summary
  serves the context, the active add serves cross-session memory, and nothing
  obliges them to be one pass.

The root cause of keeping them separate: different quality/latency
budgets (summary in-turn, the fact's producer is whoever is reasoning).

**Multi-party participation** — member identity (display name / role
in the members table, attribution applied at serialization), profile
knowledge (graph facts about a member — a member node plus its type label
and edges; there is no `kind=profile` flat category, ADR-0009), and the
interrupt policy (seconds-scale passive trigger through the same policy
engine, silent outcomes logged) — is specified in ADR-0009. The
single-user shape needs nothing extra: one member, every trigger on the
turn boundary.

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