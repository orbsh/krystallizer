# ADR-0028: Weight's two terms — a capped base from totals, a live term from the rings

**Status**: Draft (2026-11-12). Builds on ADR-0005's un-landed amendment (two access rings at payload v4, time at the query); does not replace it.

## Context

ADR-0005 fixes the shape this ADR reasons about: the counters and the two rings ride the edge row, the stored `weight` is a monotone function of the counters, and the recency information is used only at query time to re-rank what `recall` already fetched. Three problems fall out of that shape, and they are the ones a reader hits first.

**1. Pure recency loses the old and load-bearing fact.** A fact that accumulated many accesses years ago decays to nothing under a query-time recency term, even when it is exactly the fact a question needs. Decay answers "how live is this", not "how established is this"; the total counters answer the second question and are currently thrown away at query time.

**2. Quadrant thresholds drift with age.** The four quadrants are cut by absolute counters (`RARITY_READ_FLOOR`, `RARITY_WRITE_CEIL`). Counters only grow, so after enough time nearly every fact sits on the "high read" side and the matrix degenerates into a line. The quadrants were meant to separate behaviours (read-heavy vs write-heavy), which is a rate, not a total.

**3. `write_count` mixes three different events.** An independent re-assertion (another path re-derived or re-observed the fact) is evidence. A model rewrite (reflection, merge, normalisation) and a duplicate import are not — they change or repeat the representation. All three currently increment the same counter and shift the same ring, so "reprocessed" looks like "confirmed".

## Decision

**1. Query-time weight is a sum of two terms, not one decayed total.** The stored value stays exactly what ADR-0005 defines (monotone, index-friendly, never expires between writes); the query computes:

```text
effective = base + live                                        (milli-units, integers)

base = min( ln(1 + read_count + write_count + tool_count) × K_BASE , CAP )
live = ln(1 + reads_in_ring) × 1.0
     + ln(1 + writes_in_ring) × 0.5
     + ln(1 + tools_in_ring) × 1.5

K_BASE = 0.25    CAP = 3.0 formula units (= 3000 milli)
```

`base` is the residue of everything that ever happened; `live` is what is happening now. The cap is the whole point: an established-but-stale fact never falls to zero, and it can never outrank a fact that is being read now. The constants are business constants (how much history is worth, how much the present is worth), not score calibration — that distinction is why they are acceptable where a "confidence score" is not.

**2. Quadrants are cut on rates, not totals.** `RARITY_READ_FLOOR` and `RARITY_WRITE_CEIL` are compared against the ring counts within the ring's own span (how many accesses fall in the most recent window the ring covers), not against the cumulative counters. A fact's quadrant can then change in both directions, which is what a behaviour classification has to do.

**3. Only an independent re-assertion counts as a write.** The call site knows which of the three it is; the counter and the ring move only for re-assertion. Rewrites and re-imports touch the fact's content or its provenance and leave the weight alone. This is a write-path rule, not a new column — no schema change, no third counter.

**4. History beyond the rings is not recoverable, and is not faked.** The rings hold the last eight accesses per direction; `base` is the only surviving trace of anything older. A question that needs the state of belief at a past date therefore cannot be answered by re-ranking. It is also not answered by a time-scoped query, because this model does not have one: a knowledge base stands in the present. What history is kept for is as material for the present — a lesson, an origin, a reason to doubt — not as a world to travel back into. Recording this as a boundary is the decision; approximating it with a decay curve would be a lie.

**5. No offline audit, and conflict is adjudicated where it is used.** There is no background pass that sweeps for stale facts and no review queue: a fact that is no longer touched simply stops accumulating `live`, which is the only signal staleness needs. New knowledge arriving from use either merges with what is there (it is compatible) and shares the heat, or it does not, and the heat decides which one surfaces first. When both are recalled into one window, both arrive with their scores and the reader judges. Two obligations follow for the read path: a conflict group — same subject and relation, different objects — must not be split by a top-k cut, or the older side never reaches the reader and there is nothing to judge; and a score must arrive as its components (establishedness, live heat, recency) rather than one number, because a single number invites "higher means truer", which no counter measures.

## Consequences

- `recall`'s re-rank becomes `base + live` instead of a decayed total, so an old well-established fact stays retrievable while a hot fact still wins.
- The quadrant read gains a window definition (the ring span); a row written before the rings existed has an empty ring and falls back to totals — same as ADR-0005's decoder rule.
- The write-path gains one rule with a name: re-assertion counts, reprocessing does not. Any consumer that reports writes must classify its own write.
- No storage change, no new index, no full-table job: everything is computed from fields the row already carries.

## Rejected

- **Pure recency, no base.** Loses exactly the facts that took years to establish; the metaverse-kind case (high totals, low recent traffic) becomes unretrievable.
- **Pure totals, no live term.** The current problem: age cannot lose, and superseded facts keep winning.
- **Storing the computed `effective`.** It would expire between writes, which ADR-0005 rejected for the stored value for the same reason.
- **A third counter for re-assertions.** The call site already knows what it is doing; a column would only move the same judgement into the schema and make every old row ambiguous.
- **An as-of query (`what did we believe at T`).** It would require content history, not access history: the rings and counters record how often a fact was touched, never what the store held. Even with unlimited timestamps, replaying them answers "what was hot in 2022", not "what was believed then". A knowledge base is read in the present tense; the past is carried as evidence attached to the live fact (a supersession note, a first-seen date, a source) rather than as a parallel world that can be entered.
- **A confidence score supplied by the model.** It is uncalibrated, unstable across models, and it does not decompose into anything a reader can audit. Counters + rings + a write rule are all observable.