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

The current core has no profile storage at all: the flat memory table
(`MemoryRow`) carries free text only, and no ADR assigns persona
knowledge a home.

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

  Storage: a managed category of flat-memory facts (`kind=profile`),
  maintained incrementally by the existing extraction pipeline
  (ADR-0004) and governed by the existing weight system (ADR-0005) —
  a frequently-cited preference grows heavy, a stale one decays. No new
  table; the agent's context assembly pulls profile facts for the
  channel's current members in one batch.

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
triggers should end without speaking. Interrupt decisions therefore log
too — signal values, the decision, and the reason — because the
negative samples ("did not speak") are exactly the data future tuning
of the speak/annoy tradeoff needs. The decision log from ADR-0008 §4
extends to this trigger unchanged.

### 3. What does NOT change

- One container: the multi-party channel is the same messages table;
  members and cursors are already per-member keyed (ADR-0008 §1/§2).
- The projection contract: assembly reads checkpoint + increment + the
  member-attributed view; attribution is a serialization concern, never
  written back into the log.
- Trigger policy ownership: thresholds and clocks live in
  `krystallizer.kdl` (global defaults, per-user override reserved),
  decisions are logged, learning waits for labels — ADR-0008 §4's
  terms apply verbatim.

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
- Flat memory gains the `kind=profile` managed category; the extraction
  pipeline's output routing gains profile facts as a destination.
- The policy engine gains the interrupt trigger (seconds-scale window)
  and the decision log gains silent outcomes.
- Single-user mode needs no new mechanism: it is the multi-party shape
  with one member and every trigger firing on the turn boundary.
