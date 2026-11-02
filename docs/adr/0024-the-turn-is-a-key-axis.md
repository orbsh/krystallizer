# 0024 — The turn is part of the message key, and the store mints it

> **Languages:** [English](0024-the-turn-is-a-key-axis.md) (primary) · [中文](0024-the-turn-is-a-key-axis.zh-CN.md)

**Status:** Accepted (2026-10-31). Code pending: the layout below is what the key
and the write path move to (per round, then one closing record).
**Date:** 2026-10-31

## Context

`MessageKey` is `[channel_id][ts_ms][sub]` (ADR-0008 §3), and the choice was made
for reading: byte order is `(ts, sub)` order, so replay, the increment and the
max-gap read are one prefix scan in the log's own sort order — no sort, no
secondary index. Nothing in it identifies a TURN, because nothing needed to: a
turn wrote its rows once, at the end, and the only consumer was the next turn's
read.

Three things pull that apart.

**1. A turn's rows must exist before it ends.** A `human`-nature call can take
minutes, and the entry that dispatched it may be suspended and resumed (aura's
cold tier). A resumed turn needs its own working set back — the question, the
rounds already run, what is still pending. A write path that lands the exchange
only when the whole turn succeeds leaves nothing to resume from.

**2. Several turns can be open at once.** Once a user may interject — and once a
suspended turn is waiting on a result — a channel has more than one turn in
flight. Time alone then cannot delimit a turn: two open turns interleave their
rows, and "everything since ts X" is no longer "this turn".

**3. A turn has to be collapsible exactly.** Writing every round means a
completed turn's rows must be reduced to the net effect (one row per successful
tool call, gravity's current shape). A collapse that works on a time range can
delete a neighbour's rows; one that works on a turn cannot.

## Decision

### 1. The key carries the turn: `[channel_id][turn_id][ts_ms][sub]`

All fixed width, BE — so a turn is a contiguous prefix range, and within it
`(ts, sub)` order is byte order. Collapsing a turn is a prefix scan; reading one
turn is a prefix scan; nothing needs a new index.

The read semantics change with it, and this is the part to state loudly:
**key order is turn creation order, then in-turn time order — it is NOT global
time order.** A turn suspended for an hour resumes under its original id with
fresh `ts_ms`, so its segment sits before turns created while it slept while its
rows are newer. Any view that wants strict chronological order must merge turn
segments by their rows' `ts_ms`. For a conversation with suspensions and
interjections that merge is the honest reconstruction, not a workaround.

### 2. The STORE mints `turn_id`, per channel, at the turn's open

Not the caller. A caller-side counter collides across devices, across a retry,
and across two turns the same client opened; the store already owns the one
counter that cannot. The same record is the turn's open marker — so "an open
turn" is a fact the store knows without asking anyone.

### 3. Every round is written as it happens

The model asks for a tool, the call runs, the round lands — one write per round
rather than one at the end. This is what makes resumption possible and what makes
the turn's rows visible while it is in flight. It also means an abandoned turn
leaves rows behind; see §5.

### 4. The turn closes with ONE record of an explicit kind

When the loop finishes and the answer is complete, the writer submits the full
final output once, with `kind = FINAL` (a new code in the row vocabulary; the
writer names the code, the labels stay here) and, in that same submission, the
subs to KEEP — the question, the net-effect tool rows, the answer.

### 5. k10r collapses the turn itself, on receipt

No HTTP surface, no caller cooperation: on a `FINAL` write, k10r prefix-scans the
turn, keeps the subs the record names, and purges the rest (`Collection::purge`,
the primitive the queue compaction already uses). One scan, one turn's rows, once
per turn. The keeplist is explicit data rather than an inference from role or
kind, because "which rows were redundant" is the writer's judgement and guessing
it here would be a second rule to keep in sync.

### 6. Open turns are marked, and the projection skips them

A turn whose open record exists and whose `FINAL` does not is OPEN: its rows are
working state, not history. The projection marks the segment and leaves it out of
the model's view. This is also what makes multiple suspended turns work — nothing
special is needed for interjection, because each open turn is its own segment.

### 7. The checkpoint's pointer names a turn; its KEY still orders by compression time

A checkpoint row says which log record a summary ends at, and until now the key
IS the pointer (`[channel][ts_ms][sub]`, ADR-0008 §4): one address, no second
field to keep in step. The turn axis breaks that identity in a way worth spelling
out — `sub` breaks ties INSIDE a turn, so `(ts_ms, sub)` alone is no longer unique
across a channel once two turns can be open at once. A pointer of `(ts, sub)` would
resolve to two records.

So the pointer becomes `(turn_id, ts_ms, sub)` — an address in the log's own key
space, which is also the space the increment reads "strictly after" — carried in
the row. The checkpoint's own KEY stays `[channel][ts_ms][sub]`: it orders
COMPRESSIONS, and compressions must stay ordered by when they happened, not by
which turn they cover. A turn suspended for an hour can compress long after a
newer turn already did; keying the checkpoint by turn would make that late row
sort before the newer one and the reader's "latest = largest key" rule would pick
the older compression.

The two axes are therefore separate on purpose: the ROW knows the log position
(the turn included), the KEY knows the compression's own order.

### 8. Compression can never point into a collapsible set

A tail operation's span is computed over CLOSED turns only: a compression covers
`(coverage, verified checkpoint]`, and an open turn is after every checkpoint by
construction — it has no `FINAL`, so no pointer can name it, and the collapse
deletes only rows inside a turn that just closed. The invariant that makes
deletion safe is this one, and it must be stated where the span is computed, not
only here.

## Consequences

- **Chronology is a merge, not a scan.** Any reader that wants global order (the
  view building a transcript, a UI paging history) sorts turn segments by their
  rows' `ts_ms` or by the open record's timestamp. One extra ordering step over a
  handful of turns, not a sort of the log.
- **The log becomes mutable — inside one closed turn.** Nothing else is ever
  purged, and the deletion set is bounded by a turn that already ended. A
  checkpoint's span never covers it.
- **Abandoned turns are permanent unless a policy is chosen.** An open turn whose
  entry died keeps its rows, skipped by the projection forever.
- **The increment's boundary is still the checkpoint**, not the turn: turns are
  the write and delete unit, checkpoints are the read cut. They are different
  axes and the code must not conflate them.
- **`sub` keeps its meaning** (tie-break inside one millisecond, one writer), but
  it is no longer the only ordering inside a channel.

## Not decided here

- **Retention of open turns.** A crashed turn's rows stay until someone sweeps
  them. Whether the store sweeps on the next turn's open, on a clock, or never,
  is a policy this record does not pick.
- **Whether `FINAL` also carries the turn's metadata** (model, mount table,
  latency) or is the answer row plus a keeplist and nothing else.
- **The id's width and exhaustion.** `u64` per channel is the working assumption;
  nothing depends on it yet.