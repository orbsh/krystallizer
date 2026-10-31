# Idea: an extraction is a group, and the group gets its own record

**Status**: idea, recorded 2026-10-31. Not a decision — no ADR, no PLAN item on its
own. Companion to [hypergraph-population.md](hypergraph-population.md): that file is
about *finding* relations, this one is about *keeping the fact that they were found
together*. They are separable, and this half does not need the other.

## 1. The problem

Atomic facts are the storage ideal and the retrieval problem at once: one row per
claim is precise, deduplicable, independently weighted and independently forgettable,
but six facts pulled from one stretch of conversation arrive in the store as six rows
that do not know they belong together. The one thing they shared — what that stretch
of conversation was *about* — has nowhere to live.

Three fixes are usually reached for, and all three are wrong here:

1. **Extract coarser.** One fat fact instead of six. This trades precision for
   coherence on a single axis: the fat fact can no longer be reused in a different
   context, re-weighted by usage, or superseded piecewise. It is a symptom-level fix.
2. **A container** (a hyperedge, one row holding all six members). It keeps the
   grouping but freezes it: the group becomes a storage decision taken at write time
   that can never be recomputed, and every per-fact property the weight system depends
   on (usage counts, decay, confidence, invalidation) becomes a property of the group.
3. **A pointer to the source passage.** This one fails for a different reason: the
   source is *speech*. A turn of conversation is mostly filler, restatement and
   thinking aloud; citing it stores noise and calls it context. Worse, the passage is
   the thing most likely to be compressed away later, so the pointer would eventually
   reference something that no longer exists.

## 2. The claim

An extraction should emit **two objects, not one**: the facts, and a record of what
the group means — written by the extractor, in its own words.

```text
Group      one row: statement (one short sentence), scope, weight, timestamps
Facts      each carries `group: u64` (0 = ungrouped), plus its own weight/decay/etc.
```

The statement is the group's *shared context*, distilled at extraction time. It is not
a quote and not a summary of the text — it is the extractor's answer to "what was this
about", which is the only part of that stretch of conversation that carries knowledge.
If the extractor cannot say it in one sentence, that is a signal the extraction is
really two groups.

The group is a **node**, not a container: it can be referenced, indexed, embedded,
weighted, superseded and forgotten on its own schedule, exactly like any entity row in
this store. The facts point at it and stay independent.

## 3. Concretely, in this codebase

- `Facts` (mem-core) gains `group: u64` — one field, `0` meaning "no group", so this
  is additive and backward-compatible by the ordinary `#[ok_default]` rule.
- `#[ok_index(by_group)]` on that field. okm's declared indexes are counting-family
  faces, so "the facts of one group" is one prefix scan — the same machinery
  `by_scope_weight` already uses, no new mechanism.
- The group itself is an existing entity kind (a document row with a short text field,
  an optional `Vector<f32>` via `okm-vector`, and the ordinary graph edges to the
  entities it is about). Nothing new is needed in okm.
- Retrieval: resolve groups (by scope/weight/embedding as today), then read each
  group's statement as the context header and its facts as the body. Ranking *inside*
  a group keeps using ADR-0005's weights, which is the property a container destroys.
- Dreaming/sleep-time consolidation can **re-cut the grouping** offline: merging two
  groups or splitting one is a change to a single `group` field on the affected facts.
  A container cannot do this at all; that asymmetry is the main reason to prefer this
  shape.

## 4. Open questions

1. **Granularity of a group.** One per extraction call (the unit the model actually
   saw) is the obvious default; one per paragraph and one per tool call are both
   defensible. Consolidation can merge later, so the write-time choice only has to be
   fine enough, not exact.
2. **Who writes the statement, and at what cost.** One extra short sentence per
   extraction. A cheaper variant: the statement is written only when the group is
   first *retrieved* (lazily), which shifts the cost to the read side and risks a stale
   statement for a group that grew.
3. **A fact in two groups.** Parallel edges already exist in the graph model, so
   duplicating the fact is mechanically fine but doubles its weight bookkeeping.
   Alternative: the group is chosen by first sight and consolidation later moves it.
   Not decidable until there is data.
4. **The statement can be wrong.** It is generated text, so it must be marked as a
   summary rather than a fact — it gets a confidence like any other claim, and it is
   never the authority for what the facts say.
5. **Cascade on forget.** Forgetting the last fact of a group leaves an empty group.
   Keep it (it records that something was once believed) or drop it? Leaning keep, with
   a weight that decays to zero — deletion that erases history is the expensive kind.
6. **Does the group need its own embedding?** If yes it is just an entity row with a
   vector, which okm-vector already supports; the open part is whether group similarity
   should drive retrieval on its own or only re-rank the groups already chosen.

## 5. What this does not solve

Entity resolution and predicate canonicalization (see the companion file, §4–§5). A
group of facts is only as good as the entities inside it; a wrong resolution writes a
wrong group that looks exactly like a right one.