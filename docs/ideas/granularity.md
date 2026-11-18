# Granularity: letting the graph be re-cut

Status: `GraphStore::merge` and `GraphStore::split` landed in mem-core on top of okm
`subgraph` / `cut` / `repoint` / `replace` / `materialize` (2026-11-17). What is
still an idea: the gate (who decides to cut), and the render-side work below.

## The problem

Granularity is not a schema constant. A fixed one is wrong in both directions: too coarse and one entity swallows two ideas (the fact set stops being about one thing), too fine and one idea scatters into synonyms that never meet again. Which is right is a property of the DATA — how many facts touch a node, whether its facts cluster into two groups with few cross edges, whether two names are spelled differently but carry the same external id.

So the graph has to be re-cuttable at run time, and a model is the only thing that can propose where to cut.

## The mechanism is cutting a selection and re-attaching it, not a merger

okm gives three pieces and no fourth:

- `subgraph(seed, hops, predicates)` — names a selection nothing had written: breadth-first, answering the cut directly. `hops` counts the rings INSIDE the selection, so the boundary is what leaves them.
- `cut(selection)` — asked of a selection you already hold, so it is not a read of "what the subgraph is": it answers one half-edge per edge crossing the boundary (the member end, which is the end re-attached, plus the far end) and the edges that die inside.
- `materialize(selection, kind, payload)` — the one step that stores a computed selection as a node set, for when the partition has to be asked about again.
- `Fragment::repoint(from, to)` — an assertion moved to a new head WITH its payload. An edge's head is its identity, so this is the only way to re-attach: delete-plus-declare would land the type's default payload.
- `Graph::replace(fragment)` — one batch that both retracts and declares. Atomic: no reader sees the gap. A replacement that deletes an entity must state every edge incident to it (repoint it or delete it) or the batch is refused.

Every case is that one verb:

| shape | what it is |
|---|---|
| merge two nodes | delete one node, repoint its crossings to the survivor, delete the edge that becomes a self-loop, delete the loser's row |
| split one node | declare the second node, repoint the crossings each one keeps, declare the edges that move, no node deletion |
| replace edges between nodes that stay | repoint or delete and declare, no node deletion at all — the completeness rule never fires |
| replace a whole selection | the general case: interior edges go, the boundary half-edges are re-attached to whatever replaces them |

Two degenerate shapes are ordinary, not special cases. A selection at the END OF A CHAIN has exactly one half-edge on its boundary; it is re-attached like any other. A DETACHED selection has none; there is nothing to re-attach, and a merge inside it is pure internal work (the edges between the merged pair collapse to self-loops and must be deleted explicitly, since they are incident to the retired node).

The one thing the primitive will not decide is which half of a split a single boundary half-edge belongs to. That is judgement, and it stays with the caller.

## No ledger: the current shape is the only truth

A merge is expensive and irreversible; a split is cheap and reversible. That asymmetry decides the policy:

- **Splits are gated on evidence about shape** — the node's edges fall into two groups with few edges between them. Cheap to do, cheap to undo.
- **Merges are gated on evidence about identity** — same external id, a name that differs only in spelling, attributes that do not contradict. "These look similar" is not evidence: a model's similarity judgement is not calibrated, and the cost of a wrong merge is a payload merged by a conflict hook into a row nobody can un-merge.

The model proposes; the application commits.

Two cases the implementation had to decide, and neither is in the caller's hands:

- An edge between the two merged entities becomes a self-loop, so it is DELETED. Keeping it would state that the survivor has a relation with itself, which is not what the merge said.
- Two edges landing on one head after the fold (the survivor already stated one, or two of the loser's do) collapse to the incumbent's payload. An edge carries one payload, so "keep both" is not available — silently picking the last one would be the worst of the three. Proposal is free and reversible, commitment is not, so the gate belongs where the irreversible act is.

**Nothing records that a shape was changed.** Not the nodes, not an event node, not a table. Three reasons, in the order they decided it:

- A ledger drifts away from the data it describes. The moment a node's edges change for any other reason, the record's "this is why" is stale, and nothing re-checks it — a second copy of the truth needs a maintenance pass that will not exist.
- Nothing consumes it. No algorithm reads "these two were merged in March" and decides better afterwards; the granularity pass judges the current edges either way.
- The premise is wrong on its own terms. A shape that was adjusted was unreasonable BEFORE the adjustment, and the nodes never carried a record of being unreasonable. Re-reading why is re-litigating a state that no longer exists.

What that leaves is one rule, and everything else follows from it: **the latest version is the truth**. When the graph changes and something else needs to know, it reads again — shared memory exists to overcome staleness, not to preserve it.

Anti-oscillation therefore cannot be a record either. It is the criterion plus the cadence: a merge or split fires only when the evidence is true of the data AS IT IS NOW. The stable failure mode (merge, split, merge, split) is answered by making the criterion harder to satisfy on the live edges, not by remembering that a pair was merged once. A split immediately after a merge means the merge criterion was wrong — and that shows up in the shape, not in a log.

## Render ids

The text render already carries `n<id>` per node (`render.rs`, from `NodeId::label`), which is what makes an instruction like "merge n42 into n37" cheap to write and unambiguous. Two consequences follow.

There is **no id→id alias face, deliberately**. A redirect would assert "this number is now that entity", and after a split, a partial re-merge and a later absorption that sentence is false — a silent rewrite of history, worse than a missing row, because a missing row at least reads as missing. okm stores what is currently true; the identity history is the application's record.

So a stale id resolves to nothing, and the read path FILTERS it out rather than explain it. There is no ledger to explain it with — nothing knows where the entity went, because the answer that matters is that it is not there now — and a row saying "n42 (gone)" still puts a dead name in front of the model, which is exactly the invitation to invent. What must not happen is a hollow `n42  []` row sitting in a prompt for the model to name.

An action that arrives against an id no longer live is refused and answered with "no such node, re-read", so the model re-reads instead of guessing.

## Open questions

- How hard should the criterion be, given that nothing remembers the last decision? Without a ledger the brake is entirely "the evidence must be true of the live edges", so the threshold is the only thing standing between a settled graph and a ping-pong.
- Is a per-scope entity budget the right brake, or does it just coarsen the wrong regions?
