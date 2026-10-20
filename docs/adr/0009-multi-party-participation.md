# ADR-0009: Multi-Party Participation — Member Identity, Profiles, and the Interrupt Policy

**Status**: Accepted (design; implementation pending)
**Date**: 2026-09-22
**Chinese**: [0009-multi-party-participation.zh-CN.md](0009-multi-party-participation.zh-CN.md)
**Extends**: ADR-0008 (channel log, projection, policy engine)

## Context

ADR-0008 unified the container (channel log) and the agent's context
(projection), and defined the compression policy engine. Two consumer
shapes were left implicit:

1. **Single-user + agent** — the degenerate shape where the agent is
   full-time present: one user message advances the projection, every
   turn runs assemble → infer → reply. Turn boundaries carry all
   triggers (compression, extraction).
2. **Multi-party + agent** — the agent is a MEMBER of the channel, not
   its host. The message flow does not wait for the agent: members talk
   to each other while the agent is present-but-silent. Context
   assembly decouples from speaking turns; before speaking, the agent
   pulls everything it missed since its own last message, not just
   messages addressed to it.

Three needs fall out of the second shape that ADR-0008 does not cover:

- The serialized projection must carry a **sender identity per message**.
  In the one-on-one shape, user/assistant alternation suffices; with N
  members, every "user message" in the model's view is potentially a
  different person, and unattributed messages merge the buyer and the
  support agent into one voice the model cannot tell apart.
- Member identity has a **semantic layer beyond the display name** —
  who is the support agent, who is the buyer — which is user-profile
  knowledge, not channel data.
- Speaking is no longer triggered only by being addressed. An agent in
  a multi-party channel needs an interrupt policy: a passive trigger
  with a latency budget far tighter than compression's.

At the time this was written the core had no profile storage at all: the
flat memory table carried free text only, and no ADR assigned persona
knowledge a home. That table is gone (2026-10-19); the graph is the home
this ADR picks.

## Decision

### 1. Member identity: two layers

- **Identity layer (structured, in the members table)** — member_id →
  display name + role tag (support / buyer / the agent itself). This is
  deterministic data; k10r's serialization path prepends the
  `[display-name]` attribution to each message when assembling a
  projection. No model involvement.
- **Profile layer (semantic, in global memory)** — the long-term
  understanding of a person: preferences, behavioral history,
  relationship context. Profiles are VALID ACROSS CHANNELS (the same
  person in the after-sales channel and the pre-sales channel is one
  profile), so they belong to global memory, not to any channel.

  Storage: the graph — profile knowledge is a set of facts about a member
  (`member -[PREFERS]-> iced_americano`), the member being an in-domain
  entity resolved by its stable id (ADR-0004), maintained by whoever
  reasons (ADR-0004/0007) and governed by the existing weight system
  (ADR-0005) PER FACT: a frequently-cited preference grows heavy, a stale
  one decays. The context assembly pulls profile facts for the channel's
  current members as one traversal from the member nodes. There is no
  `kind=profile` flat category and no profile table: flat memory was the
  pre-graph carrier of long-term memory and went away with the graph's
  landing (ADR-0001; withdrawn 2026-10-19), and its text rows had no per-fact
  identity — updating,
  weighting and enumerating them per fact are all out of reach, where an
  edge does all three.

  The node's NAME is a rule, not a stored mapping: `member-{channel}-{member}`
  (`channel::member_node`). Derived from the ids, so it survives a rename — a
  display name is presentation, and renaming a member must not move a single
  fact. The channel is in the name because member 7 of one channel is not member
  7 of another, and one scope holds many channels.

The two layers stay separate because they change for different reasons:
identity is admin-time and deterministic; profiles are run-time and
extracted. Merging them would put extracted, revisable content into a
table the serialization path treats as ground truth.

### 2. The interrupt policy: same engine, tighter clock

Passive participation reuses the ADR-0008 policy engine with a third
signal source. The three triggers are structurally identical — signal →
threshold → decision (speak / compress / stay silent) — differing only
in latency budget:

- **Explicit (@mention of the agent's name)**: no threshold, the turn
  starts immediately.
- **Passive interrupt**: the check window is SECONDS, not minutes — a
  channel silent for N seconds with new, relevant-enough content makes
  the agent consider speaking; "avoiding dead air" cannot wait for a
  cache-expiry clock. Relevance candidates and silence duration are
  k10r's cheap half-step; the judgment ("does this situation warrant a
  remark, and what should it be") is an LLM call, so it belongs to
  gravity (ADR-0007's boundary).
- **Compression**: the ADR-0008 clock (cache-expiry aligned), unchanged.

**Silence is the common outcome, not the failure case.** Most passive
triggers should end without speaking. No interrupt decision is recorded today —
the decision log was withdrawn with ADR-0008 §4: the negative samples ("did not
speak") only pay off once tuning exists and labels do, so what to record is a
question for that day, not storage paid for now.

### 3. What does NOT change

- One container: the multi-party channel is the same messages table. The members
  table is keyed by member id, and the read cursor is `[channel][member]` — the
  cursor's people dimension is exactly what this table finally supplies
  (ADR-0008 §2). The CHECKPOINT does not gain one: it is the channel's pointer at
  one record, and the summary it holds belongs to the one voice on that log.
- The projection contract: assembly reads checkpoint + increment + the
  member-attributed view; attribution is a serialization concern, never
  written back into the log.
- Trigger policy ownership: thresholds and clocks live in
  `krystallizer.kdl` (global defaults, per-user override reserved),
  decisions are logged, learning waits for labels — ADR-0008 §4's
  terms apply verbatim.

### 4. The model's view and the people's view are different reads

The channel is ONE log, but two readers want different shapes, and neither is a
transformation of the other's output — they are two projections of the same rows.

- **Nobody is unattributed.** A single-user channel still has a user, so member
  `1` is seeded (default display name from one constant, `master` today) the
  first time a channel is touched. There is no sender id 0 and no "unknown"
  author to special-case; multi-party rows use ids from 10 up.
- **People read the channel.** Raw records, in log order, exactly as written —
  no checkpoint, no summary, no merging (`ChannelStore::history`). Reading it is
  a scan, not an assembly.
- **The model reads turns.** Between two replies, several user messages are ONE
  thing said, so they merge into a single turn; every line carries
  `[display-name(id=N)]` — the name so the model can speak about a speaker, the
  id so it can look the member up in the graph. Deterministic data, no model
  involvement (ADR-0009 §1).
- **Order is start, span is real.** A reply is keyed by its FIRST token's clock,
  and carries `end_ts_ms` — the clock it finished on (`0` = instant, which is
  what a user message is). So a message typed while a reply was still streaming
  sorts BEFORE that reply and is still visibly inside its span: the order is
  `user, user, assistant(start 1.1s, end 5.0s), user(1.2s)`, never re-sorted by
  end. Nothing is dropped to make the transcript tidy.
- **Tool rows are not in the conversation yet.** They are in the log (the human
  view shows them) and the turn builder skips them; pruning belongs to the view
  layer once the tool shape is settled.

## Why Not

- **Profiles as a channel attribute**: a profile survives the channel
  (one person, many conversations); channel-scoped storage would fork
  the person into per-channel fragments and re-merge them at query
  time — a derived-merge doing global memory's job. Rejected.
- **Profiles as a dedicated table with fixed fields**: profile content
  is open-ended and evolves (preferences, context, relationships); a
  fixed schema would need migration on every new facet, while
  extracted facts are the format the extraction pipeline already
  produces and the weight system already governs. Rejected.
- **LLM-decided interrupt on every message**: per-message inference is
  the compression trap again (a model call to decide whether a model
  call is warranted). The cheap half-step (silence duration, relevance
  candidates) must gate the expensive one; the LLM judgment runs only
  when the gate opens. Rejected as an unconditional mechanism.
- **Role tags inside the log**: writing `support:`/`buyer:` into
  message payloads makes attribution part of immutable history — a
  role correction would then need log surgery. Attribution rides the
  members table and is applied at serialization; the log stays raw.

## Consequences

- The members table gains the display-name/role fields; the
  serialization path gains per-message attribution.
- Profile knowledge lands in the graph (a member node plus its edges); the
  `kind=profile` flat category and its extraction routing do not exist —
  neither was ever built.
- The policy engine gains the interrupt trigger (seconds-scale window). No
  interrupt outcome is recorded (the decision log is withdrawn, ADR-0008 §4).
- Single-user mode needs no new mechanism: it is the multi-party shape
  with one member and every trigger firing on the turn boundary.
