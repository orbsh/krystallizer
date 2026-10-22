# ADR-0010: Algorithm Improvement — Write-Time Resolution and Automatic Cognitive Binning

**Status**: Landed (steps 1–2 on 2026-10-17; steps 3–4 and the read side on
2026-10-19)
**Date**: 2026-10-16
**Chinese**: [0010-algorithm-improvement.zh-CN.md](0010-algorithm-improvement.zh-CN.md)

## Context

The 主动 (long-term) side (ADR-0001 has a table but no
algorithm. The facts arrive one at a time (each an 8-slot tuple) and land as scattered
nodes; nothing gives them order. Order can be paid for in exactly two
places: at read time (every search clustering the neighborhood it needs)
or at write time (a background pass in the typing gap, where the caller
already runs its own work, ADR-0007 — the facts themselves arrive from the
model's active memory add, in-turn).

This ADR takes the second. The write side absorbs the whole cost —
conflict resolution, binning, subgraph consolidation — inside that
write-time pass,
so the read side runs no graph algorithm at all. It merges the two drafts
written for the track: a loose `docs/LPA.md` (three-table schema,
lifecycle phases, visualization extension) and a write-time-resolution
variant (the adjudication rule, the density boundary metric, the capsule
prompt, the read-side sketch) — the first three of which this ADR then
retired (§3–§4) — and corrects one thing in both: they spell
the storage as string prefixes with JSON values, which okm's modeling
rules reject (variable text in a key segment, a hand-maintained prefix
standing in for an access method, a value as serialized text). §1 is the
same three collections expressed in okm's terms.

What constrains every step below:

- **mem-core holds no LLM, no transport, and is not an actor** (ADR-0007).
  Everything running inside it is deterministic and local; the facts arrive
  from the model's active memory add (the caller routes that call to
  `ingest(fact)`), and k10r never calls anyone back.
- **Retrieval is vector landing + exact name/alias lookup, then traversal
  ranked by weight** (ADR-0003. There is no lexical
  branch to reuse, and no third retrieval mechanism is to be added.
- **One WriteBatch = one transaction** (ADR-0002): every index change this
  algorithm makes rides the same commit as the data change.
- **ADR-0005's weight model supplies the vote's edge weights** — no second
  weight semantics is introduced.
- **Append-first, not append-only** (ADR-0002: a
  modification is allowed where the ban costs more than the update. This
  ADR is a case of that rule, not an exception carved out of an
  immutability dogma.

## Decision

### 0. Pipeline

```
[ step 1: a fact tuple ] ──► [ step 2: write-time resolution (upsert) ]
                                          │
                                    (Fjall write)
                                          ▼
[ step 5: read side ] ◄── [ step 4: the community's bucket ] ◄── [ step 3: incremental LPA ]
```

Steps 1–2 run as the fact lands through the caller's `ingest(fact)` call, and
steps 3–4 run in the SAME call, after that fact's own commit — no background
worker, no second pass over the store. Step 5 is the read path and runs no
algorithm at all.

### 1. Write-time normalization and resolution (the layout in okm's terms)

The model's active add produces structured facts (subject / predicate / object
triples, serialized as the 8-slot tuple), and the caller hands them to the
core through the deterministic `ingest(fact)` door; the core does not parse
prose and does not call a model. Three collections carry the track's state; the
okm-conformant layout is below (ns is allocated at landing: entities 36 and
facts 37 LANDED 2026-10-17, the community sequence 38 on 2026-10-19).

```text
Entities — node collection (ns 36), one per graph
  0x0 primary        [scope u64][node_id u64]
                     → body: kind_id (hot), community_id (hot — the LPA
                       result, read through its own face),
                       literal_num (hot, i64 — a literal node's integer
                       projection), name (cold TLV), props (dynamic segment)
  0x1 node-kind face [kind_id u16][scope u64][node_id u64]        declared index
  0x1 by_name face   [name][scope u64][node_id u64]               exact name/alias entrance
  0x1 by_community   [community_id u64][scope u64][node_id u64]   ← the drafts' index:community:, now a declared face
  0x4/0x5 predicate vocabulary [kind_id u16] → kind name ／ [kind name] → kind_id
                     (the pair is okm's; an entity's kind rides it like a graph's predicate)
                     node kinds, separate from the props' field names (0x02/0x03)

Update 2026-10-21: the node row's kind is `kind: Vocab<u16>` — a categorical
FIELD whose labels live in that field's own data vocabulary (okm ADR-0032), so
the node side no longer uses the 0x04/0x05 pair and needs no allocation code.
The 0x04/0x05 vocabulary stays the GRAPH's (predicates only), and the reader
takes the label from the row (`row.kind.as_str()`) instead of resolving an id.
The wire width did not change (two bytes); the layout is version 3.

Facts — edge collection (ns 37), one ns per graph (okm ADR-0017)
  0x0 primary        [edge_id u64]
                     → body: [src NodeRef][dst NodeRef][predicate_id][since][confidence][weight]
  0x1 dynamic segment [edge_id u64] → the relation's open props + the `functional` flag
  0x1 declared-field face per DECLARED #[ok_index] — two, each with a reader:
                     by_since (timeline) and by_scope_weight (top_by_weight, which
                     carries the row's denormalized `scope` first, PLAN 4a)
                     [field value][edge_id u64]
  0x4/0x5 predicate dict  [predicate name] → predicate_id  /  [predicate_id] → name
  0x6 predicate index [predicate_id][edge_id u64]
  0x7 out-edges      [src NodeRef][edge_id u64]
  0x8 in-edges       [dst NodeRef][edge_id u64]
  0x9 predicate+out  [predicate_id][src NodeRef][edge_id u64]
  0xA predicate+in   [predicate_id][dst NodeRef][edge_id u64]

  No SCOPE segment on this side: the edge collection is a `Graph<S, E>` and
  builds its own keys (okm's edge shape), so the ownership boundary rides the
  ENDPOINTS — every `NodeRef` carries its entity key, scope included. A
  traversal starts at a scoped node and stays in scope; only a whole-ns scan
  (0x6, "every edge of a predicate") crosses scopes, and it is the caller's to
  filter by the endpoints. A per-scope edge prefix would mean hand-rolling
  the collection against okm's Graph.

Community sequence (ns 38, one row per scope since 2026-10-19)
  0x0 primary        [scope u64]
                     → body: high_water (hot: the last community id issued;
                       monotonic, so an id is never reused)
```

`scope` leads every pkey because the corpus is per-user (ADR-0003) — the
ownership boundary, the same shape flat memory uses (`[user_id][id]`,
ns 32), not an optimization. `NodeRef` is `[ns 2B][len varint][pkey]`,
self-describing (okm ADR-0017 §2), so any node collection participates with
zero declaration.

A term's props — the tuple's head/tail props — land on the entity row's
dynamic segment: the entity's attributes, shared by every fact about
it. They MERGE, and the merge is the ruled reading: a same-named
attribute takes the newer value while the others survive. Wholesale
replacement would let one fact's props erase every other fact's
attributes on the same entity, which is why it was rejected.

Five points where this differs from the drafts, and why:

- **Names never enter a key, and nothing is hashed into one.** `KeyEncode`
  rejects `String` at compile time, and a hash into a fixed-width segment
  collides silently (it addresses another entity's rows) and cannot be
  rendered back — okm's own reasoning for proxy ids (MODELING, "Open
  vocabularies"). An entity name lives in the `by_name` face and in the
  row's cold segment, reached as `name → node_id`. The drafts'
  `community_id = hash(entity_name)` seed and `c_milkyway_2026`-style ids
  are gone: a community exists as the value on its member nodes, its id is a
  u64 issued by the scope's sequence row (monotonic, never reused), and its
  human-readable face is the rendered text's header (`community c7`).
- **Every query dimension is a declared access method, not a prefix buried
  in a key.** The community reverse index is the `by_community` face
  (`[community_id][scope][node_id]`), whose tail IS the primary-key
  encoding — decodable, per okm's rule that index tails are decodable and
  primary-key custom tails are not. Hand-written prefix schemes are what
  that rule exists to prevent.
- **No `[order]` segment anywhere here.** The primary reads are point and
  traversal reads, never plural ordered ranges, and a sort key earns its
  bytes only when the result is plural (okm MODELING, "Key naming").
  `since` / `updated_at` are payload fields, following ADR-0005's own
  precedent that mutable metadata never enters the key.
- **The kinds are runtime data; the three algorithm-queried edge fields are
  declared.** Predicates allocate through the GRAPH's own predicate
  vocabulary (fixed faces 0x04/0x05, name → `predicate_id`, okm ADR-0017
  §3); they are no longer the payload's field-name vocabulary (0x02/0x03).
  Node kinds ride the same rule in the entity collection — its own vocabulary
  pair at 0x04/0x05, with the props' names staying in 0x02/0x03.
  `since` / `confidence` /
  `weight` are declared fields of k10r's own schema, each earning one 0x1
  face (okm ADR-0017 §3) — the two are orthogonal in okm's design. The
  extracted, open-ended attributes ride the dynamic segment and are
  reachable through path indexes (okm ADR-0012 / ADR-0029) when a query
  needs one.
- **The fact tuple's relation is its name plus props, with no label of its
  own.** The name IS this `predicate_id`: a second, coarser kind would be a
  class-of-kinds with no face and no reader, and a classification the model
  wants kept visible rides the props (queryable through a path index). The
  tuple's three roles are therefore NOT symmetric — head and tail carry a name
  AND a kind (the node row's `kind_id` plus the `node-kind` face); the relation
  carries only a name.
- **Values are native segments, never a JSON blob** (okm ADR-0018): hot
  fixed-width fields contiguous, variable text as a cold TLV segment.

A newly landed fact is a scattered node carrying an independent seed
label — the seed is a fresh community id, not a hash.

**Resolution: look up the incumbent before the fact is written.**

```
                       new fact arrives
                             │
                             ▼
   one scan on the 0x9 face: which edges of this predicate leave this source?
                             │
          ┌──────────────────┴──────────────────┐
          ▼ (none)                              ▼ (incumbent body)
      link a new edge                  adjudicate:
      (parallel edges are              new.since > old.since
       separate facts)                 OR new.confidence >= old.confidence ?
                                            │
                          ┌─────────────────┴─────────────────┐
                          ▼ (yes)                             ▼ (no)
                    rewrite the body in place           drop the new fact
                    (same edge_id)                      (or lower its weight)
```

Two pieces of the drafts' bookkeeping disappear with okm's mechanism:

- **No manual index deletion, no tombstone.** The drafts deleted the old
  index entry by hand in the same batch; okm's declared faces are derived
  from the row, so rewriting the row regenerates its face entries (weight /
  since / confidence) and a stale face cannot survive.
- **Parallel edges are separate facts** (okm ADR-0017): the same
  (src, kind) with a different object is a different edge with its own
  `edge_id`, so the overwrite applies to functional relation kinds only —
  see the Owed list for how a kind declares its cardinality.

```rust
// draft sketch — mem-core, on a newly extracted triple (okm forms)
let predicate_id = kinds.id_of(predicate);   // the graph's predicate dict (0x04/0x05)
let src = NodeRef::new(&ENTITY_NS.to_be_bytes(), &subject_id.to_be_bytes());
let dst = NodeRef::new(&ENTITY_NS.to_be_bytes(), &object_id.to_be_bytes());

// 1. the edges of this predicate out of this source: one scan on the 0x9 face
let incumbent = edges.typed_out(predicate, &src).next();

let mut batch = edges.batch();
match incumbent {
    // 2. adjudicate against the incumbent body
    Some(edge_id) => {
        let old = edges.get(edge_id);
        if new_fact.since > old.since || new_fact.confidence >= old.confidence {
            // in place: same edge_id, the row's faces follow from the row
            batch.put(edge_id, new_fact.into_body(src, dst, kind));
        }
        // else: dropped, no write
    }
    // 3. no incumbent: link a new edge (0x6/0x7/0x8/0x9/0xA in the batch,
    //    plus 0x4/0x5 when the predicate is first seen)
    None => batch.link(EdgeFact { src, dst, kind, ..new_fact }),
}
batch.commit();   // one WriteBatch = one transaction (ADR-0002)
```

### 2. Incremental label propagation (LPA)

Append-first ingestion produces a pile, not order; the pile is given a
border in memory, per write, instead of by a global pass.

**Why local incremental LPA.** Its cost is near O(E) over the touched
neighborhood, it needs no full graph in memory, and it is exactly the
class of algorithm ADR-0003 says earns a dependency (community detection
is named there as the exception to "implement simple algorithms
directly"). `petgraph` is the carrier for the in-memory local graph.

Mechanics:

1. **Construct the local subgraph** — load the newly written node plus its
   directly connected 2-hop neighbors through the traversal faces (0x7/0x8,
   0x9/0x0A), assembled into a temporary in-memory graph. No global graph is
   loaded.
2. **Neighbor voting** — a node inspects its neighbors' `community_id` and
   votes by edge `confidence` weighted by accumulated edge weight:
   `Target_Community = arg max over C of Σ_{e ∈ Edges(C)} Weight(e) × Confidence(e)`
3. **Convergence and commit** — 3 rounds converge in the common case (the
   drafts allow 3–5). The winning `community_id` is written as a field on
   the member node rows. The `by_community` face is DERIVED from the row,
   but okm's document overwrite leaves a stale index entry dangling (the
   entry key encodes the indexed value, so the new value lands at a new
   key) — a re-bin therefore deletes the old face entry in the SAME batch.
   The earlier "no tombstone" claim was wrong; landing caught it. Nothing
   else is written — there is no summary state to mark. All of it rides one
   WriteBatch (ADR-0002), the sequence row included.

The pass runs inside `ingest`, after the fact's own commit, so a fact that
landed is binned before the call returns. A node landing for the first time
draws its id from the scope's SEQUENCE row (one per scope, monotonic): the
watermark never goes down, so an id is never reused even when a community
empties out. A vote never degenerates into an all-zero tie — a fresh assertion
already weighs `ln(2) × 0.5` — and ties go to the SMALLER community id, so the
outcome does not depend on iteration order. Every node that moved is rewritten
in one batch together with the sequence row.

### 3. The community is bounded at READ time, not at write time

Nothing summarizes a community on the write side. The density water line and
the uncompressed-fact watermark are retired: they existed to decide WHEN a
community was worth an LLM summary, and there is no summary any more.

What still needs bounding is the ANSWER. A community grows by accretion —
LPA merges nodes as facts connect them — so its size is not bounded by
construction. The read side therefore caps the answer at a fact budget
(20 lines by default, `?budget=` on the door, 0 = no cap), drops the
LOWEST-weight facts first (ADR-0005's order), and states the elision in the
header (`showing 8 of 15`). A line is never cut in half: an id pointing at
half a fact is worse than an id that is absent.

If elision ever dominates a corpus — the header says so on every read — the
fix is a write-side split rule, a follow-up that waits for that number
rather than a threshold guessed now.

### 4. No consolidation pass: the read renders the community

The earlier design put an LLM summary (a "capsule") on the community, pulled
by the caller and written back with `PUT`. It is retired, for two reasons:

- **It was a chore, not a surface.** The consumer would have had to list
  pending communities, pull a packet, run a model and write the answer back
  — a second workload behind an interface whose only job is to serve memory.
  The consumer already runs an LLM for its own work; making it also maintain
  k10r's index is the wrong trade, and it is exactly the shape the ruling
  against "internal indexing as a consumer chore" rejects.
- **The information was already there.** A community's understanding IS its
  nodes and its edges. A summary is a lossy projection of precisely that,
  and it goes stale on the next fact.

So there is no capsule table, no `pending` state, no packet, no write-back
and no summarizer prompt. The community is rendered when it is read (§5).

### 5. Read side: landing, then the landed community

The read path runs no graph algorithm and no model call:

1. **Land** — the query embeds and the seeds come back (ADR-0003), each
   carrying its node's `community_id`.
2. **Take the landed community** — the seeds' communities, deduplicated in
   seed order (the first landed community is what the query is most about).
3. **Read the bucket** — the community's members through the `by_community`
   face, its facts through the traversal faces with a membership filter
   (both endpoints inside the community), each edge once.
4. **Render** — one block per community: a header (size, plus the elision
   when the budget cut anything), one line per node (`n12 简洁 [style]`), one
   line per fact (`e34 n12 -[PREFERS]-> n15 · w=1.400 · c=0.90 · since=…`).
   Every line carries the id it names, because that id is the handle a caller
   points at — today to read one more line, later to edit that row precisely.

**Rendering is k10r's, never the caller's** — a consumer of memory need not
know how the graph is assembled, and a third-party agent has no idea what a
community slice is. So the answer carries ONE representation, chosen by
`format`:

- `plaintext` (the default) — the line form above; what a prompt wants;
- `markdown` — the same lines as headings and bullets;
- `mermaid` — `graph LR`, one `subgraph` per community, the relation as the
  edge label (a diagram's label carries the relation alone: the counters are
  read off a line, not off a picture);
- `struct` — the data alone (seeds, community ids, facts) and **no text at
  all**; the caller that assembles the graph itself asks for this one.

The three text forms state the same facts and differ only in syntax. An
unknown `format` value is a 400, never a silent fallback — a typo must not
quietly change the representation.

The answer is ONE door,
`GET /memories/{scope}/recall?q=&k=&budget=&ts_ms=&format=`, returning the seeds,
the community ids, the shown facts and the rendered text together. There is no
N-hop: a community IS the bounded slice, and a caller that wants a deeper walk
composes it from `out_edges`/`in_edges`.

The text is deterministic: the same corpus, the same budget and the same
format answer identically, and time decay only reorders as ADR-0005 says it
does.

### 6. Front-end extension (no new mechanism)

The persisted `community_id` can be handed to visualization at zero cost: a
deterministic tuple → renderer conversion (no LLM involved) feeds
**3d-force-graph** — the WebGL/three.js force-graph library with orbit-style
camera control, the carrier this repo already exercises in
`docs/kg-demo-3d.html`. That demo streams node/edge packets into the running
layout and reheats the simulation per batch instead of redrawing the tree,
which is exactly the producer shape here. Node color binds to
`community_id` (one color cluster per community) and node size to weight, so
the growth of order in long-term memory — the binning itself — is inspectable
straight from `GET /graph/{scope}`, which now carries `community_id` on every
node; the demo page still colours by node kind.

## Consequences

- **This is the live case for append-first, not append-only.** The edge body,
  the node row and the sequence row are overwritten in place because keeping a
  superseded version costs more: it would poison both retrieval and the
  binning that reads it. The log-shaped tables (ns 33/34) are untouched by
  this ADR's resolution step.
- **Every read in this design is a declared access method** (okm: "access
  methods are mandatory"): the bucket is the `by_community` face, the
  resolution lookup is the 0x9 face, the exact entrance is `by_name`. No raw
  prefix scan and no general full-scan surface is introduced.
- **Keys carry no text and no hashed values.** Entity names and community
  labels live in payload fields and index segments, so they can be rendered
  back and listed in human terms; identities are surrogates issued by a
  watermark, never a hash into a key segment.
- **No new retrieval mechanism.** The community is a projection over the
  fact graph and the answer is its rendering, never a scored branch; search
  still enters by landing or by exact name/alias lookup (ADR-0003).
- **The vote consumes ADR-0005's weights** (`Weight(e)`, `confidence`, the
  read-write quadrants). ADR-0005 declares no weight face: a `[weight][edge_id]`
  entry would address the whole store (edge keys carry no scope — the endpoints
  do), so a scope-wide Top-K is a projection table there, not a face.
- **Every number here is a policy parameter** and belongs in
  `config/krystallizer.kdl` (ADR-0008's rule): the iteration cap, the 2-hop
  radius, the read budget. They are named constants in `graph.rs` today
  (`LPA_ROUNDS`, `LPA_RADIUS`, and the door's `default_budget`); promoting
  them to config is a config-node change, not a schema change. A message-count watermark (the
  draft's 100 messages) is NOT k10r's — when to add belongs to whoever reasons
  (ADR-0007); k10r takes the facts it is given.
- **Re-partitioning is cheap by construction.** A node changing community
  rewrites one row and swaps one face entry in the SAME batch (okm's overwrite
  leaves the old entry behind); repeated binning never becomes a
  read-modify-write across a scan or a wait on a file lock.

### Owed / open (not settled by this document)

- **The tables:** the entities collection (ns 36) and the facts edge
  collection (ns 37) LANDED 2026-10-17; ns 38 holds the community SEQUENCE
  row (one per scope, the id issuer) since 2026-10-19 — the capsule table
  that ns once carried is retired (§4).
- **Functional vs multi-valued kinds — CLOSED 2026-10-17.** The caller
  declares it PER ASSERTION, in the relation's props: `{"functional": true}`
  means one value per source, so the arriving fact is adjudicated against the
  incumbent edge of the same kind out of the same source; absent (or false)
  means multi-valued and the fact links a parallel edge. That default is the
  safe one — a dropped fact is lost, an extra edge is recoverable — and the
  declaration rides the assertion because the model that emits the triple is
  the side that knows its predicate's cardinality. Storage does not police it:
  the same predicate declared inconsistently is a data-level call, made per
  arriving fact. The declaration is kept on the edge's dynamic segment (the
  assertion's own record).
- **The community's carrier — SETTLED, landed 2026-10-19: on the NODE.** The
  drafts disagreed — `community_id` on the fact row (their resolution/index code)
  vs on the entity node (their LPA and the density metric). LPA votes over nodes
  and `|V|` counts them, so the node row carries it with a `by_community` face,
  and the community's edges arrive through the traversal faces with a membership
  filter. An edge-side face would only be needed if the read had to serve
  edges WITHOUT their endpoint rows.
- **Literal objects — CLOSED 2026-10-17.** A literal becomes a NODE: kind
  `literal`, its exact text in `name` (the `by_name` entrance) and its integer
  projection in the declared hot `literal_num`. Uniformity is the reason —
  every fact is two nodes and one edge, so traversal, binning and the density
  metric need no special case — and the value's numeric form is a declared
  field, so an interval or nearest read stays an ordered key scan rather than
  a vector-distance guess. Owed within it: a canonical projection for dates
  and non-numeric strings (only integers project today).
- **The capsule call's carrier — RETIRED 2026-10-20.** There is no
  consolidation call and no door for one: the read renders the community
  (§4–§5). The four pull doors (pending listing, bucket, packet, `PUT`) were
  removed with the capsule table.
- **Community id issuance — a SEQUENCE row (2026-10-20).** One row per scope
  (`CommunitySeqKey { scope }` → `high_water`), written into the same batch as
  the rows carrying the ids and flushed on the single-row path too. It only
  grows, so an id is never reused even when a community's last member leaves —
  which is the case "the maximum of the communities that exist" gets wrong
  (the previous issuer, retired here).
- **Two vocabularies in one ns vs. okm's per-handle vocabulary cache** (found
  the hard way 2026-10-17; RETIRED the same day by an okm ruling): the edge ns
  used to hold BOTH relation kinds (allocated by okm's `Graph` through its
  long-lived `VocabCache`) and the relation props' names (k10r's). Two caches
  allocating in one vocabulary hand out the same id for different names — the
  frame then reads back under the WRONG name, silently. okm ADR-0017 §3 gave
  the predicate its OWN vocabulary (0x04/0x05; the payload's field names keep
  0x02/0x03), so the collision is structurally gone and k10r's two
  disciplines — "allocate through a FRESH cache", "build the edge handle PER
  CALL" — RETIRE with it. **The read side is paid too (LANDED 2026-10-19):**
  okm's `VocabCache::id_get` loads the vocabulary on probe, so a fresh handle
  answers typed reads with no priming — the `facts_primed` shim is deleted.
  The ENTITY side is symmetric (its kinds at 0x04/0x05, prop names at
  0x02/0x03), so the one-cache-per-call discipline retires there too.
- **The functional rewrite is ONE engine commit — PAID (2026-10-19).** okm's
  `Graph::unlink_into` puts the delete set into a caller-owned batch, so a
  rewrite is `unlink_into` + `link_into` in a single commit, which is what
  ADR-0002's rule wants. The edge's dynamic segment is k10r's own entry (not
  okm's), so the same batch deletes it first — a rewrite can never inherit the
  previous assertion's props. The gap is closed, not mitigated by re-ingest.
- **The edge's payload codec is the declaration's — PAID (2026-10-19).** The
  edge's payload is an obj (okm ADR-0017 §3) and the declaration is
  `GraphEncode`, so one declaration generates both codec directions and the
  face entries: `Facts::encode_payload` / `decode_payload`, with `since` /
  `confidence` / `weight` each declared as an explicit `#[ok_index]` face.
  k10r's hand-written `from_attrs` (13 bytes of carried widths) and its
  locking test are deleted; the adjudication input is
  `Facts::decode_payload(&body.payload)`.
- **k10r's prefix scans assumed the retired 4-byte head — FIXED (2026-10-19).**
  After okm ADR-0030 a fixed face is ONE byte, so a primary-key head is
  `[ns 2B][0x00]` (3 bytes, not 4). Four readers still built the old prefix or
  decoded at offset 4: the channel log (`messages_after` — every fetch came
  back empty), per-user memory reads (`search` / `list`), the entity
  scope scan, and the edge-id recovery on reopen (which would hand out
  duplicate `edge_id`s). All now go through `write_slot` + a 3-byte offset.
- **The drafts' prefix layouts are retired, not preserved**: the
  hand-maintained `node:` / `index:community:` prefixes are replaced above by
  okm's faces, so the drafts' "two key shapes for one field" contradiction is
  closed rather than carried.
- **The retired spelling is swept where it described TODAY'S layout**:
  ADR-0002's own line and MODELING's access-method inventory are rewritten
  in place (2026-10-17). ADR-0005's counter sketch is re-expressed too: the
  counters ride the edge row (2026-10-19), and its old `edge:{src}:{label}:
  {dst}/read/{ts}` delta keys survive only as the multi-writer escape's
  undecided key layout.
- **Relationship to PLAN — SETTLED.** Community detection sat in Phase 5+
  ("dreaming consolidation — petgraph community detection + human-reviewed op
  set, do not start early"). This ADR's incremental LOCAL form landed with
  Phase 3's write side (2026-10-19); Phase 5 keeps the global, human-reviewed
  pass, which is a different job.
- **Claims carried from the drafts, not measured here:** millisecond read
  latency, ≥90% provider context-cache hit rate, "the Rust core shrinks by
  half". They are the shape's rationale and targets, not results.

## History

**What the drafts had, and why it went.**

- **The binning's placement.** The pipeline put steps 3–4 in a background pass
  after the write, which would need a worker, a lag and a retry story for a
  derivation. They run inside the same `ingest` call, after the fact's own
  commit: one local pass, and a bad derivation is a caller retry.
- **The community id's issuer, twice.** This ADR first named okm's `HighWater`
  reduce. The first landing replaced it with "one past the scope's current
  maximum" read from the capsule table — one fewer mechanism, but the guarantee
  went with it: a departed community lowers that maximum, so an id could be
  handed out twice. The sequence row (§1) restores the guarantee in one row.
- **The capsule, and the doors that carried it.** The design had an LLM
  summary per community, pulled by the caller (four doors: pending listing,
  bucket, packet, `PUT`) — landed 2026-10-19, retired 2026-10-20. It made the
  consumer maintain k10r's index, and it re-derived information the nodes and
  edges already were. The read now renders the community (§4–§5).
- **"Re-binning needs no tombstone."** Wrong under okm's overwrite semantics,
  and corrected in §2: the old face entry survives the rewrite, so a re-bin
  deletes it explicitly in the same batch.
