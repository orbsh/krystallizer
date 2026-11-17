# Granularity: letting the graph be re-cut

Status: idea, not planned. Depends on okm `replace` / `repoint` / `subgraph` (landed 2026-11-17).

## The problem

Granularity is not a schema constant. A fixed one is wrong in both directions: too coarse and one entity swallows two ideas (the fact set stops being about one thing), too fine and one idea scatters into synonyms that never meet again. Which is right is a property of the DATA — how many facts touch a node, whether its facts cluster into two groups with few cross edges, whether two names are spelled differently but carry the same external id.

So the graph has to be re-cuttable at run time, and a model is the only thing that can propose where to cut.

## The mechanism is subgraph replacement, not a merger

okm gives three pieces and no fourth:

- `subgraph(NodeSet)` — one selection read, three answers: the members, the edges with both ends inside, and the crossings (one end in, one end out, with the end a rewrite replaces).
- `Fragment::repoint(from, to)` — an assertion moved to a new head WITH its payload. An edge's head is its identity, so this is the only way to re-attach: delete-plus-declare would land the type's default payload.
- `Graph::replace(fragment)` — one batch that both retracts and declares. Atomic: no reader sees the gap. A replacement that deletes an entity must state every edge incident to it (repoint it or delete it) or the batch is refused.

Every case is that one verb:

| shape | what it is |
|---|---|
| merge two nodes | delete one node, repoint its crossings to the survivor, delete the edge that becomes a self-loop, delete the loser's row |
| split one node | declare the second node, repoint the crossings each one keeps, declare the edges that move, no node deletion |
| replace edges between nodes that stay | repoint or delete and declare, no node deletion at all — the completeness rule never fires |
| replace a whole subgraph | the general case: internal edges go, crossings are re-attached to whatever replaces them |

Two degenerate shapes are ordinary, not special cases. A selection at the END OF A CHAIN has exactly one crossing; it is re-attached like any other. A DETACHED selection has none; there is nothing to re-attach, and a merge inside it is pure internal work (the edges between the merged pair collapse to self-loops and must be deleted explicitly, since they are incident to the retired node).

The one thing the primitive will not decide is which half of a split a single crossing belongs to. That is judgement, and it stays with the caller.

## What the record has to keep

A merge is expensive and irreversible; a split is cheap and reversible. That asymmetry drives the whole policy:

- **Splits are gated on evidence about shape** — the node's edges fall into two groups with few edges between them. Cheap to do, cheap to undo.
- **Merges are gated on evidence about identity** — same external id, a name that differs only in spelling, attributes that do not contradict. "These look similar" is not evidence: a model's similarity judgement is not calibrated, and the cost of a wrong merge is a payload merged by a conflict hook into a row nobody can un-merge.
- **Both leave a record**: which nodes, which edges were the evidence, when. A year later "why is PG the same node as PostgreSQL" has to be answerable, and a split's criterion is read back out of the merge record's edge groups.
- **Anti-oscillation is by record, not by prompt**: the same pair may be merged once; merging again requires new evidence. Merge/split ping-pong is the stable failure mode of this kind of system, and asking a model to be careful does not stop it.

The model proposes; the application commits. Proposal is free and reversible, commitment is not, so the gate belongs where the irreversible act is.

## Render ids

The text render already carries `n<id>` per node (`render.rs`, from `NodeId::label`), which is what makes an instruction like "merge n42 into n37" cheap to write and unambiguous. Two consequences follow.

There is **no id→id alias face, deliberately**. A redirect would assert "this number is now that entity", and after a split, a partial re-merge and a later absorption that sentence is false — a silent rewrite of history, worse than a missing row, because a missing row at least reads as missing. okm stores what is currently true; the identity history is the application's record.

So a stale id resolves to nothing, and the read path must say so rather than hide it: an id whose entity is gone renders as a tombstone naming where it went (from the merge record), or is filtered out. What must not happen is a hollow `n42  []` row sitting in a prompt for the model to name — that invites an invention.

An action that arrives against an id no longer live is refused and answered with what happened to it, so the model re-reads instead of guessing.

## Open questions

- When is a merge "settled"? A split immediately after a merge of the same pair suggests the criterion was wrong, and the record should say which edge made it move.
- Does the merge record live in the graph (an event node) or in the application's own tables? An event node is queryable with the same traversal as everything else; a table is cheaper to write and easier to keep out of recall.
- Is a per-scope entity budget the right anti-oscillation brake, or does it just coarsen the wrong regions?
