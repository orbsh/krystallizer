# 0017 — Tail Operations Are a Table, and Compression Answers Twice

> **Languages:** [English](0017-tail-operations.md) (primary) · [中文](0017-tail-operations.zh-CN.md)

**Status:** Accepted (2026-10-26) — stage one landed the same day: the `tails`
table, the two-product prompt, the `suggested_*` fields on the checkpoint row,
and gravity's branch turn reading both products. Stage two (the review pass) is
NOT built; the seam for it is.
**Date:** 2026-10-26

## Context

ADR-0008 §4 wired one tail operation: when the compression policy fires, the
projection carries `compress: { checkpoint, prompt }`, the caller appends that
prompt after the span, and the branch's answer is the summary. It was written as
a single field because there was a single operation.

Two things forced this record.

**The operations are a class.** Compression is not the only thing worth doing
with a whole span: a review pass ("did this turn leave a skill or a memory
behind?") belongs to the same family — the same timing, the same shape (a prompt
appended to a projection read, run as a branch), a different product. A field per
operation would make each one a wire change.

**Compression sees what the budget cannot.** The span is chosen by a byte budget
and an idle clock, so it can end mid-exchange. The pass is the only thing that
reads the whole span, and after reading it a model can see where the conversation
actually breaks. That judgement is worth keeping: as a label for the cutter, and
as a range for whoever reads the span next.

## Decision

**1. Operations are a TABLE, not fields.**

`mem_core::compress::TAIL_OPS: &[TailOp]`, each `{ name, prompt }`, served as
`tails: [{ name, prompt, from_*, to_* }]` on the projection. Adding an operation
is an entry in that table plus its landing arm in the caller — no shape change on
either side, no config knob (there is nothing to configure: an operation is code).
A caller that does not know a name SKIPS it with a log rather than failing: the
store's list can grow before every deployed guest is upgraded, and version skew
must not take a turn down.

Each entry states its own SPAN. Compression's is `from = coverage the caller's
summary already has`, `to = the record the verdict named`; an operation with a
narrower range computes its own.

**2. Compression answers with two products: the summary and the cut it would have
chosen.**

The summary is necessary — the span becomes unreachable raw, so failing to
produce it loses content. The cut is an opinion, and it is recorded BESIDE the
summary, never applied to it: `CheckpointRow.suggested_ts` + `suggested_sub`
(`(0, 0)` = none).

- It does NOT move the coverage point. The node still closes where the verdict
  said, and the increment is still cut there. A suggestion that reached back into
  the span would silently drop records out of the increment — the fault
  `coverage_sub` exists to prevent, one layer up.
- It IS validated against the span the node closes: strictly after the previous
  node's pointer, at or before this one. Equal to this node's pointer is the
  mechanical cut restated, stored as none. Outside the span is a 400 that names
  the range: every later pass reads this value as the start of a range that
  existed, so a bad one cannot be stored.
- Two segments, not one: a position in the log is a RECORD (the lesson of the
  coverage point).

**3. The cut is an INDEX the caller resolves, not a timestamp the model invents.**

The caller numbers the records it sends (`[1]`, `[2]`, …), the model answers
`{"summary": …, "cut": N}`, and the caller — the only component that built those
messages — maps `N` back to a pointer. The store then validates it. A model that
returned a `ts_ms` would be stating a fact it cannot see, and a second
numbering implementation inside the store would be a second source of truth.

**4. Two stages, two cursors.**

The tail operations share a TIMING (a prompt appended to a projection read on the
same verdict) but not a NECESSITY: compression is required — a span that is never
summarized grows without bound — while a review pass is optional, and its
uselessness is not an error. So they advance separate cursors:

- the compression cursor is the checkpoint chain itself (how far summaries cover);
- an analysis pass gets its OWN cursor, and reads from `suggested_*` to the head
  rather than from the mechanical coverage point — the range a model saw break,
  not the range a byte budget chose.

Because the second cursor is separate, a review pass that never runs costs
nothing, and one that runs late still reads the right range. Stage one lands the
first cursor's second product; the second cursor is the next record's business.

**5. A reply that is not the shape is still a summary.**

The prompt asks for one JSON object. A model that answers with prose still
produced a usable summary, so the caller takes the whole text as the summary with
no cut. Losing the only product a necessary pass exists to make, over a format
slip, would be the wrong trade; the format is an optimization, the summary is the
contract.

**6. The pair of cuts is training data.**

`(mechanical cut, suggested cut, span)` is a sample for a future cutter. Nothing
exports it yet — storing it is enough for the set to exist, and a sampler that
runs before the data is collected has nothing to sample.

## Consequences

- **Two operations can now be added without touching the wire**, in either
  direction: a table entry here, a landing arm there. What an operation still
  owes is its own cursor and its own door.
- **The compression pass pays one extra field of output** and the projection one
  extra key. The alternative — asking the model a second question later — costs a
  whole second pass over the same span.
- **The suggestion is inert until someone reads it.** No ranking, no retrieval, no
  path-building consults it; it is a label on a node.
- **Open:** the analysis cursor's home (a checkpoint field, a cursor row per
  member, or a row per operation) is undecided — it lands with the review pass,
  which is also when "what a review pass produces" stops being hypothetical. And
  the count/decay question opened by ADR-0016 is untouched here.

## Related records

ADR-0008 §4 (the verdict and the one tail prompt), ADR-0011 (the decision log
that records the verdict), ADR-0016 (proposals — the other write side), gravity
ADR-0003 (skill execution shape), gravity ADR-0005 (a skill's body is code).