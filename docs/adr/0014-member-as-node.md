# 0014 — The member is a node: identity rows join the graph

Status: Accepted (2026-10-21), LANDED the same day

## Context

ADR-0009 §1 split identity from profile: the members table (ns 42) holds the
deterministic layer (`member_id → display name + role`), and profile knowledge is
facts in the graph. The split holds. What does not hold is the BRIDGE between the
two layers. A member reaches the graph today only as an ENTITY row whose name is a
naming rule — `member-{channel}-{member}` (`channel::member_node`) — created
under the tuple's type slot like any extracted term. Three consequences, all
wrong in the same direction:

- **Identity is typed as extracted knowledge.** The member proxy sits in the
  entity collection, so it carries an `embedding` and is a landing seed: a query
  can land on a person. Retrieval's job is to land on knowledge; a person is not
  knowledge — they are the domain's owner, administered outside the extraction
  loop.
- **The proxy row and the identity layer are two truths.** `MemberRow` already
  holds `display_name` and `role`; the proxy holds neither, so any consumer that
  wants a name for a user node goes back to the channel side and parses a string.
- **The graph has no user endpoint.** "The initialization memory is a traversal
  from the user node" (PLAN Phase 3) starts, in practice, from whatever entity the
  ingest path happened to create — a node whose only claim to being the user is
  that its name follows a rule.

## Decision

### 1. The members collection IS the graph's user node set

ns 42 joins the fact graph's member set by being NAMED in its `#[ok_nodes]`
(`#[ok_nodes(EntityRow, MemberRow)]`, from which the derive generates
`FactsNode { EntityRow, MemberRow }` and its ns list — the ns comes off each
table's own `#[ok_ns]`). Nothing else is needed — an edge endpoint is a
self-describing `[ns][len][pkey]`, so the member row's `[channel_id][member_id]`
key becomes a node reference without a registry, a second table, or a name.

This is the CLOSED-type shape, deliberately: "user" is one type, declared once, so
the collection is the type marker. Entities keep the open-kind shape (kind as an
indexed field inside one collection, ADR-0010 §1) because their kinds arrive at
ingest time. A person does not arrive — they are registered.

### 2. `member-{channel}-{member}` becomes a REFERENCE SYNTAX, not a node name

The tuple's type vocabulary gains a second reserved token: `user` (the first is
`literal`, ADR-0004). A value typed `user` carries `member-{channel}-{member}`,
which `ingest` PARSES into `NodeRef { ns: 42, pkey: [channel_id][member_id] }`.

- The string is written by the caller and parsed by the core; it is never stored
  as a name, so it cannot drift from the key.
- **Nothing is created on a miss.** Identity rows are admin-time data (ADR-0009
  §1); an ingest that names an unregistered member fails that fact rather than
  inventing a person. This is the one place where the graph's "create when
  absent" behaviour is wrong — an entity may be discovered, a member is
  registered.
- The syntax stays stable across a rename, which is the property ADR-0009 §1
  bought with the naming rule in the first place. What changes is where it lives:
  the wire, not storage.

### 3. The entity collection stops holding member proxies

`entity_by_name` no longer sees `member-…` rows because none are written. A
profile fact is `user -[PREFERS]-> iced_americano`: its head endpoint at ns 42,
its tail at ns 36 — one graph, two node collections, which is the case the edge
carrier's open endpoints exist for.

### 4. What the read side gains, and what it deliberately does not

- A user node has **no vector**: `MemberRow` declares no `embedding`, and a
  different collection is not in the entity landing index at all. People are
  entered by traversal (the context head, profile reads), never by a query.
- **No new declared face.** A member row is read by point lookup (its key) and by
  prefix scan for one channel's members. Both are the primary key; a face would
  have no reader of its own.
- The weight system, the fact contract and the edge layout are untouched.

## Why Not

- **A new `users` table keyed `[scope][user_id]`.** It would duplicate
  `MemberRow`'s fields under a second identity, and two id spaces for people is
  the one duplication that cannot be reconciled later. ADR-0009 §1 already put the
  identity layer in a table; the missing half was the graph's participation, not
  another table.
- **A union node type (`kind = member`) inside the entity collection.** That is
  today's proxy with a field instead of a name: it keeps people in the landing
  index and keeps identity in the extracted collection — the two defects above,
  preserved.
- **A `person` layer above seats (cross-channel profiles).** ADR-0009 §1 states
  that profiles are valid across channels while a seat is per channel; the member
  node inherits that tension exactly as the naming rule did. Resolving it needs a
  person registry with its own allocation authority — a separate decision, not a
  free rider on this one. Until then, one profile across two channels is one
  member row registered in both.

## Consequences

- `channel::member_node()` retires as identity and survives as the reference
  syntax: same string, opposite direction — written by the caller, parsed by the
  core, never stored.
- mem-core's graph path reads the members collection through the CHANNEL
  keyspace's handle. The two keyspaces are separate (`channel`, `memory`) and okm's
  `commit_batch` writes into the handle it was called on, so a fact's batch stays
  inside `memory` and the member read is a point read across handles — never part
  of that batch.
- A member endpoint renders by reading its row (display name, role). A recall line
  therefore pays one extra point read per user endpoint; the alternative —
  denormalizing the display name onto the edge — would put admin-time data in an
  extracted row and turn a rename into a graph migration.
- Existing member-proxy entity rows and the edges pointing at them (ns 36, name
  `member-…`) are a migration question, not a schema one — the same stance the
  legacy flat rows got (ADR-0002).
- **Landed (2026-10-21).** `#[ok_nodes(EntityRow, MemberRow)]`, generating
  `FactsNode { EntityRow, MemberRow }` with `NS_VALUES = &[36, 42]`;
  `ingest` resolves the reserved `user` token through
  `parse_member_ref` and REFUSES (`IngestError::Refused`) a malformed or
  unregistered reference, creating nothing; the endpoints became a write-side
  `Endpoint` enum so a fact's batch still lands inside `memory` while the member
  row is a point read on the channel handle (`GraphStore::with_channel`, attached
  by `open_service`); `resolve_ref` answers a `NodeId`
  (`Entity { scope, node_id }` | `User { channel, member }`) and renders a person
  from the row's display name; the binning pass takes only entity endpoints, so a
  user node never enters a community and never reaches the landing index. The
  ref-taking traversal forms (`out_edges_of` / `in_edges_of` / `typed_out_of`,
  `user_ref`) are the user-anchored entrance. Gates: mem-core graph 33/0 (two new
  tests — `a_user_endpoint_is_a_node_of_the_identity_collection`,
  `an_unregistered_user_reference_is_refused_and_creates_nothing`), every other
  suite green, clippy 0. The kind field landed with it as `kind: Vocab<u16>`
  (okm ADR-0032), retiring the hand-rolled `kind_id` + `PREDICATE_VOCAB` path.