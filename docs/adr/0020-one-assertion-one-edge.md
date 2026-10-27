# 0020 — One Assertion, One Edge

> **Languages:** [English](0020-one-assertion-one-edge.md) (primary) · [中文](0020-one-assertion-one-edge.zh-CN.md)

**Status:** Accepted and LANDED (2026-10-27). Depends on okm ADR-0041 (the faces carry
both endpoints).
**Date:** 2026-10-27

## Context

`ingest` had three outcomes: `Linked`, `Rewritten`, `Dropped`. Only a `functional`
relation was compared against anything — the incumbent edge of the same kind out of
the same source. A multi-valued relation had no identity at all, so the SAME
`(head, kind, tail)` ingested twice produced two edges:

```text
POST /memories/37  [["acme","repository",{},"OWNS",{},"issues-api","repository",{}]]
  1st → {"edge_id":1,"outcome":"linked"}
  2nd → {"edge_id":2,"outcome":"linked"}      # the same fact, a second row
```

That was survivable while every ingestion was a user act. It stopped being
survivable when a REVIEW pass started writing facts (ADR-0017 §7): the review reads
from the cut compression suggested, which can fall behind the coverage point, so the
same material passes under it twice by design. Every pass would re-add its facts —
doubling them in `recall`'s budget, in community sizes, and in the head.

## Decision

**`(head, kind, tail)` is one edge.** The identity is checked before anything is
written, and okm ADR-0041 makes it one seek: the pair leads the index key, so the
check does not depend on how many edges the source already has.

A re-assertion resolves three ways:

- **Same `since`, same `confidence`** → `Bumped`: the fact already stands, nothing is
  written, the incumbent's `edge_id` comes back. This is not a refusal — the fact IS
  in the graph, which is what the caller asked for.
- **Strictly newer `since`, or higher `confidence`** → `Rewritten`: the row is
  rewritten in place under the same `edge_id`, and the write is counted. A newer
  statement about the same fact is news; the weight system (ADR-0005) wants to see it.
- **Functional relations** keep their existing rule on top: a DIFFERENT tail out of
  the same source still adjudicates against the incumbent (`since >` or
  `confidence >=` wins, otherwise `Dropped`).

The two rules are ordered, not merged: identity says "this is the same fact", and
functionality says "this fact may replace that one".

**Why not "always bump the evidence".** A background pass re-reading the same span is
not independent evidence — it is the same evidence read twice, and counting it would
inflate a weight every time a pass runs. Counting only strict improvements keeps
`write_count` meaning something a caller stated.

**Why the props of a `Bumped` assertion are ignored.** The incumbent's open props are
the incumbent's. Identity is the triple; letting a re-assertion with different props
rewrite silently would make the props part of the identity without saying so, and the
first review pass to mention a fact would own the attributes of every later reader's
copy.

## Consequences

- **A re-reading pass is harmless.** The review writes what it finds, and finds land on
  the edges that already stand.
- **The outcome vocabulary grew to four** — `linked / rewritten / dropped / bumped` —
  in the door's answer, in `commit_memory`'s description (the model reads that string),
  and in `MODELING`.
- **`write_count` now means "distinct improvements"**, not "times mentioned".
- **A store written before okm ADR-0041 reports `linked` twice for one triple** until
  its faces are rebuilt; the two rows stay two rows, and this record does not merge
  them. `reindex` rewrites the indexes, not the corpus.

## Related records

ADR-0004 (the fact contract), ADR-0005 (the weight system), ADR-0017 §7 (the review
pass, the caller that made this urgent), okm ADR-0041 (both endpoints in the faces).