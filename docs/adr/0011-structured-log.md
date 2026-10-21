# ADR-0011: Structured Log — Internal State as Disposable Diagnostic Rows

**Status**: Design settled (implementation pending; ns 41 declared, no code yet)
**Date**: 2026-10-20
**Chinese**: [0011-structured-log.zh-CN.md](0011-structured-log.zh-CN.md)
**Related**: ADR-0008 (the compression verdict, §4's "no pre-paid training set"), ADR-0007 (core and analysis API classes never mix), ADR-0002 (one table, one ns)

## Context

k10r holds a class of data that is neither knowledge nor conversation: why the
compression cut where it did, when a retrieval index was rebuilt, what an engine
error said. It has no domain identity — no entity, no edge, no message — so it
belongs neither in the graph nor in the channel log (which is user-visible
conversation). But it is **uncertain yet potentially useful**: today for
debugging, later maybe as tuning samples.

The earlier shape gave each kind of record its own table; the decision log (once
ns 40) was that, and it was withdrawn: it pre-paid a table for "fit the policy
later" while labels and readers did not exist, and the question that must be
answered ("up to which record is this compressed") is answered by the checkpoint
row itself. Withdrawing it was right, but the conclusion was pushed one step too
far — **internal state does need a home**; what it must not be is "one table per
kind of record", and it must not pretend it will be trained on tomorrow.

## Decision

### 1. One table, keyed by time

ns 41, table `log`, append-only, key `[ts_ms 8B][seq 8B]` (BE fixed width, so byte
order IS time order: replay and range scans are prefix scans).

- `ts_ms` is **k10r's own clock**: the writer of this table IS the process, so
  ADR-0008 §3's "the caller owns the clock" does not apply — that rule governs
  caller-supplied business data, not the process's own observations.
- `seq` is a process-local monotonic counter (`u64`), seeded at open from the
  table's `seq` WATERMARK (`#[ok_reduce(HighWater(seq))]`, ADR-0023's preset) —
  the same recovery pattern the code store uses for its ids, and unlike a scan of
  the last key it cannot rewind when the trim deletes from the front (the
  ids), so a restart continues instead of colliding. It separates several rows in
  one millisecond; two processes cannot write at once (Fjall's directory lock
  keeps a single writer).
- No scope and no channel in the key: this is the program's internal state, not
  one user's memory. Finding "rows for channel X" uses the LOCATOR FIELDS on the
  row (§3) — a scan, which is cheap on a bounded table.

### 2. The fixed part is the low-cardinality set, and "declared" is not "indexed"

- `level u8`: trace / debug / info / warn / error, values from a constants module
  (`level::WARN` and friends) so no call site invents a number.
- `event Vocab<u16>`: the event name, from a constants vocabulary
  (`event::COMPRESSION_VERDICT`, …), two bytes on disk — the typo is a compile
  error at the call site, and one event cannot be spelled three ways.
- `source Vocab<u16>` and `note Str` (§8): where the row came from, and the one
  free-text field for the rows whose fields cannot say it.
- All four are DECLARED fields (hot segment, fixed encoding) and NONE carries a
  0x1 face: a face is a real entry per (value, pkey), so a face here costs one
  extra index write per row in exchange for a query nobody issues yet. The table
  is bounded and a time-range scan is cheap; add the face when a query becomes
  hot — the same discipline that removed ADR-0005's three unscanned faces.

The OKM mapping, so the vocabulary stays straight: there is no "low-cardinality
type". Low-cardinality FIELDS are declared fields (U8..U64 / FixedBytes / Str)
that MAY carry a face; low-cardinality SETS are the multi-value func index
(element → pkey), with an application-level bloom as the high-cardinality
fallback; `#[ok_nodes(Enum)]` types node SETS, not fields.

### 3. The variable part rides the dynamic segment: locator + payload

Slot 1's dynamic segment carries the open vocabulary (UInt / Int / F64 / Str /
Bool / Bytes / Array / Obj / Null) under arbitrary names:

- **Locator**: `channel` / `scope` / `entity` / `edge` / `msg` — whichever
  applies, names from a fixed vocabulary, values of the existing key types
  (u64 / the composite key's fields). Unavailable means absent, never a
  placeholder.
- **Payload**: parameter snapshots, the three signals' measured values, the split
  point, error text, timings. Nested objects MUST go through `put_frame_named`
  (a bare `put_frame` has no dictionary to resolve names).

### 4. What goes in, what stays out

In: decisions and state changes **off the core path** — the compression verdict
with its whole parameter/signal snapshot (this is where the withdrawn decision
log's training samples live now: it has a reader today, the person debugging the
engine), index rebuilds and evictions, engine-level errors, maintenance actions
other than trimming.

Out: message content, user data, anything a domain row already holds; no
per-call tracing on the hot path. The test is not "might it be useful" but "would
I want to look at this line when something breaks".

k10r never ACTS on a log row: a level is not a policy, it triggers no alert and
changes no behaviour. It only lets a scan look at errors first.

### 5. The table is bounded; trimming happens on the write

This table grows without limit and its contents are disposable.
`logs { max-rows }` in `krystallizer.kdl` sets the ceiling; a write deletes from
the oldest key until the table fits, in the SAME batch.

The trim is NOT itself logged — otherwise trimming produces rows, and rows
trigger trimming. Loss is an accepted price: this is a diagnostic table, not a
ledger.

### 6. No HTTP door

It is not one of the interface surfaces (ADR-0001's session / memory / task) and
not part of the core API (ADR-0007). When a viewer or an analysis script exists,
it opens as an **analysis-class** door — that class's shapes may drift with the
analysis need, never appear on a model's tool list, and never become a turn
loop's dependency.

### 7. The first writer: the compression verdict

The verdict was the reason this table exists — a decision, the trigger that fired,
the split it named and the numbers it was made from, with nowhere to live once the
decision log was withdrawn (ADR-0008). `LogStore::record_verdict` writes exactly
that row: `channel`, `split_ts` + `split_sub` (the pointer, both halves), `trigger`
as a WORD (`budget` / `cache_clock` / `both` — a log row is read by a person, and a
code nobody remembers is not a record), `assembled_bytes`, and whichever signals
fired. An absent signal writes no field at all: absence must not read as zero,
which is the difference between "the cache clock stayed silent" and "it said 0".

The read side is a window, not a query language: `since(ts_ms)` returns every row
at or after a lower bound, in key order — which byte order already gives — and
`max_rows` keeps it bounded.

### 8. The field shape, ruled

`ts_ms u64` keys the row. The declared fields are then four, and everything else
rides the dynamic segment:

- `level u8` — five values, ordered, compared in code.
- `event Vocab<u16>` — a label in memory, a 2-byte id on disk (okm ADR-0032).
- `source Vocab<u16>` — the module or access path that emitted the row; `0` = none.
- `note Str` — the ONE free-text field: a human sentence about this row, not a
  payload. A payload belongs in the dynamic segment with a name of its own.

`event` and `source` are Vocab because they are a CLOSED, small, endlessly repeated
set — the one shape a vocabulary is for. A `String` spends the same bytes on every
row and admits three spellings of one event; two bytes cannot be misspelled, and a
typo becomes a compile error at the call site instead of a row nobody can group by.
`note` stays a string for the opposite reason: it is the one field whose content is
not a name. `level` stays `u8` because it is a tiny ordered domain the code compares,
not a label it prints.

Status: LANDED 2026-10-20. What the change was: the row's FIELD
TYPES, nothing else. Interning is the type's own business — okm's write path interns
a `Vocab` field when the row type declares one (`Collection::put` clones the document
and runs `__okm_intern`), and the read path resolves the label back (`get` runs
`__okm_deref_vocab`), so neither k10r's store nor its doors allocate or look anything
up. `append` keeps taking `&str` and builds `Vocab::of(event)` internally, so no call
site changes. The one honest cost: a table that declares a Vocab field pays a document
clone on every write (the price of `put(&R)`'s immutable signature). The layout break
was a clean one (pre-production, ADR-0018 — no migration). It waited on the pinned okm
rev, which predated Vocab document fields; the lock moved `e0842a7` → `49c7406` and the
row compiled as ruled. `LogStore::row(key)` returns the declared fields, which is how a
reader gets the level without walking the dynamic map. PLAN 2.9c.

## Why Not

- **One table per kind of record (the decision log's shape)**: every new question
  would cost a schema change, while all of them share one property — "uncertain
  but potentially useful". One table plus a dynamic segment settles that once.
- **Writing internal state into the channel log or the graph**: the channel log
  is conversation (user-visible), the graph is knowledge (things with entity
  identity). Internal state is neither; mixing it in would let compression and
  retrieval see bytes they should ignore.
- **Faces on level / event**: one extra index write per row for a query nobody
  issues today. Not pre-paid.
- **Relying on process logs alone (stderr / tracing)**: once this runs as a
  service, process output dies with the container or the terminal, while "why did
  that compression cut there" has to be retrievable from the store afterwards.
  tracing stays for operations; the bytes an analysis needs land in the store.
- **Unbounded growth plus a TTL daemon**: one more resident mechanism and one more
  thing that can fail; write-side trimming is a few deletes in a batch already
  being written, with no second lifecycle.
- **Giving the log table domain vocabularies (one per subsystem)**: that is
  "one table per kind of record" again. Event names are a vocabulary, not a
  schema.

## Consequences

- ns 41 = `log`; ns 40 stays free (the decision log was withdrawn and nothing
  took its place).
- `logs { max-rows }` joins `krystallizer.kdl` as a new subsystem node (a node in
  the one config file, never a new file).
- The compression verdict goes from "rides the projection only" to "rides the
  projection AND lands a row": ADR-0008 §4's "no pre-paid training set" still
  holds — that row serves debugging first, and tuning would merely be an
  incidental use of it, needing no new table.
- No consumer contract moves: no door, no signature change; gravity / prism /
  booth cannot tell it exists.
- Self-observation is deliberately limited: the row count and the trim are not
  observable objects (the trim does not log itself), so "what was dropped" is
  inferable only from the configured ceiling — that is the point, and it is what
  keeps the mechanism from feeding on itself.