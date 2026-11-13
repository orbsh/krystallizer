# ADR-0029: Identity is its own layer; merging never rewrites a fact

**Status**: Draft (2026-11-12). Follows ADR-0028 on how a fact's weight ages; this one is about what a fact *is* when new knowledge arrives.

## Context

Every write raises the same question: is this the same thing we already have, or is it new? Both answers have a failure mode that compounds. Answer "same" too eagerly and distinct facts collapse into one, unrecoverably. Answer "new" too eagerly and the store fills with near-duplicates that split the heat between them and make a later merge expensive, because merging then means rewriting facts.

The existing plan is a coarse filter plus ids: fetch candidate nodes, hand them to the LLM, let it pick an existing node or create one. That is the right shape for the ambiguous cases and the wrong shape for the common ones: if every write asks, the cost is unbounded and the answers are unstable across models and across days.

## Decision

**1. Facts are immutable and identity is a separate layer.** A fact is `(subject_id, relation, object_id)` plus payload, appended and never rewritten. "These two entities are the same" is recorded as an alias edge, `surface_form --alias_of--> canonical_id`, not as an edit to any fact. Merging therefore costs one append and is reversible; a wrong merge invalidates an alias, and no fact is damaged.

**2. Three tiers, and the LLM only decides in the middle one.**

```text
tier 1  key collision: (canonical_subject, relation, canonical_object) equal
        → merge silently: counters move, the ring shifts, no judgement
tier 2  same (subject, relation), different object, high similarity
        → the LLM decides: same meaning (add an alias) or a distinct fact
tier 3  everything else
        → new fact, no judgement
```

  Tier 1 must absorb the bulk of writes, which is only true if candidates are *canonical ids* rather than raw strings — the coarse filter's job is normalisation, not selection.

**3. Granularity: one edge per assertion, with two tests.** A fact is the size of one arrow on a mind map: a node is a concept (a noun you would draw a box around), an edge is an assertion (a verb or preposition you would draw as an arrow), and a modifier is a field on the edge — never a node of its own. An edge is the right size when both hold:

- it can be refuted on its own (you can imagine this arrow being false while its neighbours stay true);
- it can be queried on its own (some question would target it alone).

An assertion that fails the first test is a paragraph and must be split. One that passes the first and fails the second is a property, and belongs in the payload.

**4. The predicate vocabulary is closed and small.** Tens of relations (`is_a`, `part_of`, `causes`, `requires`, `contradicts`, `prefers`, `said_by`, `observed_at`, `supersedes`, ...), not an open set. A new shade of meaning is an existing predicate plus payload; minting a predicate per observation is what makes a fine-grained store unqueryable, because each arrow's verb then appears exactly once and nothing composes. Surface-form variation is handled by the alias layer, not by new relations.

## Consequences

- Redundancy is checked locally: candidates for a write are the facts in the same `(subject, relation)` bucket, reached by a prefix scan. No global dedup pass exists or is needed.
- Merging entities is cheap enough to be wrong sometimes, which is the point: the alias layer is where the system is allowed to be uncertain.
- Two costs are accepted: near-duplicates can coexist until the middle tier catches them, and an alias that is never revisited stays wrong. Both are visible in the data, unlike a rewrite that lost the original.
- Three numbers say whether granularity has drifted: the distribution of facts per `(subject, relation)` bucket (huge buckets: too fine or too generic a predicate; buckets of one: too coarse or entities not normalised), the share of writes that choose an existing node over a new one, and the count and growth of alias edges.

## Rejected

- **Ask the LLM on every write.** Unbounded cost, unstable answers, and it puts the merge decision where it cannot be revisited.
- **A global deduplication job.** Offline, expensive, and it decides on facts rather than on identity, so its output is a rewrite.
- **Merging by rewriting facts.** Destroys the record of what was believed, which ADR-0028's whole aging scheme depends on keeping.
- **An open predicate vocabulary.** Convenient per write, useless per query: composition requires that the same relation be reused.
