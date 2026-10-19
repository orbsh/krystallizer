# ADR-0010: Algorithm Improvement — Write-Time Resolution and Automatic Cognitive Binning

**Status**: Partially landed (2026-10-17: the write side's steps 1–2 — the
entities collection (ns 36) and the facts edge collection (ns 37) plus the
deterministic `ingest(fact)` write-time resolution path, in
`crates/mem-core/src/graph.rs`; pending: steps 3–4 (incremental LPA,
community binning, the capsule collection) and the read side)
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
prompt, the read-side sketch), and corrects one thing in both: they spell
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
[ step 5: read side ] ◄── [ step 4: subgraph export + capsule ] ◄── [ step 3: incremental LPA ]
```

Steps 1–2 run as the fact lands through the caller's `ingest(fact)` call;
steps 3–4 run in the same background pass after the write; step 5 is the read path and runs no
algorithm at all.

### 1. Write-time normalization and resolution (the layout in okm's terms)

The model's active add produces structured facts (subject / predicate / object
triples, serialized as the 8-slot tuple), and the caller hands them to the
core through the deterministic `ingest(fact)` door; the core does not parse
prose and does not call a model. Three collections carry the track's state; the
okm-conformant layout is below (ns is allocated at landing: entities 36 and
facts 37 are LANDED, 2026-10-17; the capsule's 38 is still a proposal).

```text
Entities — node collection (ns 36), one per graph
  0x0 primary        [scope u64][node_id u64]
                     → body: kind_id (hot), community_id (hot — steps 3–4:
                       NOT declared until something reads it),
                       literal_num (hot, i64 — a literal node's integer
                       projection), name (cold TLV), props (dynamic segment)
  0x1 node-kind face [kind_id u16][scope u64][node_id u64]        declared index
  0x1 by_name face   [name][scope u64][node_id u64]               exact name/alias entrance
  0x1 by_community   [community_id u64][scope u64][node_id u64]   ← the drafts' index:community: (steps 3–4)
  0x4/0x5 kind vocabulary [kind_id u16] → kind name ／ [kind name] → kind_id
                     node kinds, separate from the props' field names (0x02/0x03)

Facts — edge collection (ns 37), one ns per graph (okm ADR-0017)
  0x0 primary        [edge_id u64]
                     → body: [src NodeRef][dst NodeRef][predicate_id][since][confidence][weight]
  0x1 dynamic segment [edge_id u64] → the relation's open props + the `functional` flag
  0x1 declared-field face per DECLARED #[ok_index] (since / confidence / weight)
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

Communities — capsule collection (ns 38 proposed)
  0x0 primary        [scope u64][community_id u64]
                     → body: title (cold TLV), summary (cold TLV),
                       updated_at (hot), state (hot: pending / done)
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
  are gone: a community is a row in the capsule collection, its id is a u64
  issued by a `HighWater` reduce (single writer, never reused), and the
  human label is the capsule's `title`.
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
  dictionary (fixed faces 0x04/0x05, name → `predicate_id`, okm ADR-0017
  §3); they are no longer the payload's field-name dictionary (0x02/0x03).
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
   the member node rows; the `by_community` face follows from the row, so
   re-binning a node needs no tombstone and no second index write. The
   capsule row's `state` is set to `pending`. All of it rides one
   WriteBatch (ADR-0002).

### 3. Subgraph boundary judgment

LPA has drawn the boxes; the next question is when a box is worth
summarizing, so that a single new fact does not trigger an API call.

**Metric — density water line:**

`Density = 2 × |E| / (|V| × (|V| − 1))`

where `|E|` is the number of edges inside one community and `|V|` the
number of entity nodes in it.

**Trigger.** A community is exported for consolidation when either (a) its
density crosses the configured threshold as a new fact joins, or (b) the
count of its uncompressed new facts crosses the watermark (15 facts in the
draft).

**Export.** One scan on the `by_community` face pulls the community's
members; their edges arrive through the traversal faces with an in-memory
membership filter — the community's subgraph, sliced into a plain-text packet
of 8-slot tuples, which is the input the next step consumes.

### 4. Capsule consolidation (layer 3)

This is the bridge from layer 4 (fine-grained graph facts) to layer 3
(macro semantic understanding).

**The call is the caller's, not the core's.** k10r holds no LLM and no
transport and never calls out (ADR-0007), so its own part is the packet
plus the `pending` mark on the community; the inference itself is drawn by
whichever consumer produced these facts (gravity for
chat consumers; a memory-primary consumer's single-pass worker otherwise).
The drafts phrase it as "handed outward through the non-blocking channel
gravity provides" — that names the carrier the caller uses, and it must not
be read as an outbound call from mem-core.

**Prompt (carried verbatim from the draft; the packet is 8-slot tuples):**

```
你是一个图谱语义提炼器。以下输入是某个特定知识社区内的全部原子事实片段（定长 8 元组，每行一条）：
------------------------
${FACT_TUPLES}
------------------------
请不要发散，直接执行两项任务：
1. Title: 为这个知识社区拟定一个 10 字以内的精准概念主题。
2. Summary: 将上述零碎的三元组关系融会贯通，高度压缩提炼成 2 句话的"语义胶囊摘要"，捕获核心架构、因果决策或用户偏好。
```

**Write-back is a point overwrite of the capsule row** (one row per
community, keyed by its own id):

```text
0x0 primary  [scope u64][community_id u64]
             → { title, summary, updated_at, state = done }
```

The overwrite is the point: a capsule is a cache — the latest summary is
the only one worth reading, and the history is what the facts themselves
already are. One row per community also keeps the key count bounded by the
number of communities instead of growing once per re-summarization.

### 5. Read side (what the write-side cost buys)

By the time a question arrives, conflicts were resolved at write time, the
`by_community` face was maintained, and the macro summary is already
materialized. So the read path is: land, read the capsule, scan the bucket,
assemble — and every step is a named access method.

```rust
// draft sketch — the read path runs no graph algorithm
pub fn assist_search(query: &str, top_k: usize) -> AssistantContext {
    // 1. in-process local embedding lands the query (top-k seeds; the by_name
    //    face is the exact entrance for identifiers — ADR-0003)
    let landed = local_embedding_engine.seeds(query, top_k);
    let cid = landed.community_id;                 // a declared field on the node row

    // 2. the pre-computed layer-3 capsule: one point read (zero LLM cost)
    let capsule = communities.get(CommunityKey { scope, community_id: cid });

    // 3. the community bucket: one by_community scan for the members,
    //    their edges through the traversal faces (0x7/0x9)
    let members = entities.scan(by_community(cid));
    let facts = edges.within(&members);

    // 4. assemble as the auxiliary context injected at the caller's prompt head
    AssistantContext::new(capsule, members, facts)
}
```

The path is a landing plus named reads (one point get, one face scan); the
graph topology work that would otherwise sit in front of every question is
gone. The injected packet is stable between consolidations — same facts,
same capsule — which is what the drafts count on for provider-side
context-cache reuse: the community's facts and capsule are unchanged until
the next rewrite.

### 6. Front-end extension (no new mechanism)

The persisted `community_id` can be handed to visualization at zero cost: a
deterministic tuple → renderer conversion (no LLM involved) feeds
**3d-force-graph** — the WebGL/three.js force-graph library with orbit-style
camera control, the carrier this repo already exercises in
`docs/kg-demo-3d.html`. That demo streams node/edge packets into the running
layout and reheats the simulation per batch instead of redrawing the tree,
which is exactly the producer shape here. Node color binds to
`community_id` (one color cluster per community) and node size to weight, so
the growth of order in long-term memory — the binning itself — is
inspectable from a dashboard reading `get_stats`.

## Consequences

- **This is the live case for append-first, not append-only.** The edge body
  and the capsule row are overwritten because keeping a superseded fact as a
  second row costs more: it would poison both retrieval and the binning that
  reads it. The log-shaped tables (ns 33/34) are untouched by this ADR's
  resolution step.
- **Every read in this design is a declared access method** (okm: "access
  methods are mandatory"): the bucket is the `by_community` face, the
  resolution lookup is the 0x9 face, the exact entrance is `by_name`. No raw
  prefix scan and no general full-scan surface is introduced.
- **Keys carry no text and no hashed values.** Entity names and community
  labels live in payload fields and index segments, so they can be rendered
  back and listed in human terms; identities are surrogates issued by a
  watermark, never a hash into a key segment.
- **No new retrieval mechanism.** The community index is a projection over
  the fact graph and the capsule is a cached summary read, never a scored
  branch; search still enters by landing or by exact name/alias lookup
  (ADR-0003).
- **The vote consumes ADR-0005's weights** (`Weight(e)`, `confidence`, the
  read-write quadrants). ADR-0005's weight index, spelled there as
  `W:{weight}:{edge_id}`, is that same rule one layer down: a declared-field
  face of the edge collection (`[weight][edge_id]` — the edge keys carry no
  scope; the endpoints do).
- **Every number here is a policy parameter** and belongs in
  `config/krystallizer.kdl` (ADR-0008's rule): the density threshold, the
  uncompressed-fact watermark, the iteration cap, the 2-hop radius. A
  message-count watermark (the draft's 100 messages) is NOT k10r's — when to add
  belongs to whoever reasons (ADR-0007); k10r takes the facts it is given.
- **Re-partitioning is cheap by construction.** A node changing community
  rewrites one row field and its face entry follows; repeated binning never
  becomes a read-modify-write across a scan or a wait on a file lock.

### Owed / open (not settled by this document)

- **The tables:** the entities collection (ns 36) and the facts edge
  collection (ns 37) LANDED 2026-10-17. The capsule collection's ns 38 is
  still a proposal, allocated through the dictionary when it lands.
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
- **The community's carrier.** The drafts disagree — `community_id` on the
  fact row (their resolution/index code) vs on the entity node (their LPA and
  the density metric). This ADR puts it on the NODE (LPA votes over nodes and
  `|V|` counts them) and pulls the community's edges through the traversal
  faces; an edge-side `by_community` face is the alternative if the injected
  packet must be edges directly.
- **Literal objects — CLOSED 2026-10-17.** A literal becomes a NODE: kind
  `literal`, its exact text in `name` (the `by_name` entrance) and its integer
  projection in the declared hot `literal_num`. Uniformity is the reason —
  every fact is two nodes and one edge, so traversal, binning and the density
  metric need no special case — and the value's numeric form is a declared
  field, so an interval or nearest read stays an ordered key scan rather than
  a vector-distance guess. Owed within it: a canonical projection for dates
  and non-numeric strings (only integers project today).
- **The capsule call's carrier.** Since k10r cannot call out, the
  consolidation request has to be pulled by the caller or marked for it.
  Which door carries it (a pending-consolidation read on the memory surface
  vs a push the caller owns) is undecided.
- **Community id issuance** rides okm's issuer discipline (`HighWater`
  reduce, single writer); the mechanism lands with steps 3–4.
- **Two vocabularies in one ns vs. okm's per-handle dictionary cache** (found
  the hard way 2026-10-17; RETIRED the same day by an okm ruling): the edge ns
  used to hold BOTH relation kinds (allocated by okm's `Graph` through its
  long-lived `DictCache`) and the relation props' names (k10r's). Two caches
  allocating in one dictionary hand out the same id for different names — the
  frame then reads back under the WRONG name, silently. okm ADR-0017 §3 gave
  the predicate its OWN dictionary (0x04/0x05; the payload's field names keep
  0x02/0x03), so the collision is structurally gone and k10r's two
  disciplines — "allocate through a FRESH cache", "build the edge handle PER
  CALL" — RETIRE with it. **The read side is paid too (LANDED 2026-10-19):**
  okm's `DictCache::id_get` loads the vocabulary on probe, so a fresh handle
  answers typed reads with no priming — the `facts_primed` shim is deleted.
  The ENTITY side is symmetric (its kinds at 0x04/0x05, prop names at
  0x02/0x03), so the one-cache-per-call discipline retires there too.
- **`unlink` has no into-batch form** (owed to okm): a functional rewrite is
  `unlink` + `link`, i.e. TWO engine commits, where ADR-0002's rule wants
  one. The window is recovered by re-ingesting, and the edge id is
  preserved.
- **The edge's payload codec is the declaration's — PAID (2026-10-19).** The
  edge's payload is an obj (okm ADR-0017 §3) and the declaration is
  `GraphEncode`, so one declaration generates both codec directions and the
  face entries: `FactEdge::encode_payload` / `decode_payload`, with `since` /
  `confidence` / `weight` each declared as an explicit `#[ok_index]` face.
  k10r's hand-written `from_attrs` (13 bytes of carried widths) and its
  locking test are deleted; the adjudication input is
  `FactEdge::decode_payload(&body.payload)`.
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
  in place (2026-10-17). Still owed: ADR-0005's counter sketch spells its
  delta keys `edge:{src}:{label}:{dst}/read/{ts}` — the retired LAYER, to be
  re-expressed when the weight system lands (Phase 5), together with the
  exact okm form of its delta table.
- **Relationship to PLAN.** Community detection sits in Phase 5+ ("dreaming
  consolidation — petgraph community detection + human-reviewed op set, do
  not start early"). This ADR describes the incremental LOCAL form of it,
  moved into the write-time pass. Whether that promotes a slice of Phase 5 ahead
  of Phase 3/4 is a ruling, not a consequence of this document.
- **Claims carried from the drafts, not measured here:** millisecond read
  latency, ≥90% provider context-cache hit rate, "the Rust core shrinks by
  half". They are the shape's rationale and targets, not results.