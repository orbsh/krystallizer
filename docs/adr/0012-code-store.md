# 0012 — The code store: the delivered script's home

Status: Accepted (2026-10-20)

## Context

gravity executes code that a model wrote: a script is delivered, then run through a
node's mount, with the source travelling as a call argument (gravity ADR-0001/0003).
Something has to keep that script between the delivery and the next call, and the
keeper cannot be the reasoner: k10r is the process that owns durable data, and gravity
is the only client allowed to reach it (ADR-0007's consumption boundary — the node
never talks to k10r).

Two neighbouring things are easy to confuse with this one, and the store's shape
depends on keeping them apart:

- a **skill** — what the agent knows how to do, structured in the graph
  (preconditions / steps / experience / verification);
- a **fact** — one atomic assertion, weighted and decayed.

A script is neither. It is an artifact: bytes that a specific runtime can execute, plus
the little a caller must know before executing them.

## Decision

### 1. One row per reusable piece of code, keyed by a bare surrogate id

`CodeKey { code_id: u64 }` at ns 35. No name, no content hash.

- **No name** because a name is a namespace, and this store has no authority to
  allocate one: two authors both write `login.nu`, and resolving that is a governance
  question, not a storage one. The id is assigned on write (`code_id == 0` → take the
  next), so a caller never invents identity.
- **No content hash** because deduplication was never a requirement. The row is
  re-read on every call and the source travels inside the call, so there is no cache to
  invalidate and nothing to gain from "is this byte-identical to something already
  stored". Two identical scripts delivered twice are two deliverables with two
  parameter definitions.

The allocator is a watermark, not a scan: the row declares `HighWater(code_id)`, so
the highest id ever written is one point read at open and the unfold is a NO-OP —
withdrawing a row never frees its id for the next delivery. A counter recovered from
"the last key" would do exactly that, silently, the first time a table was emptied.

### 2. The row carries what a caller needs BEFORE reading the source

`CodeRow { language, description, parameters, source }`.

`description` is what a skill-to-code link is decided from; `parameters` is the
parameter definition, carried as a JSON document in TEXT — **the store does not parse
it**, because storage stores and the reader interprets, and the caller that delivered
the code owns the shape. Neither field is an LLM search surface: what a skill IS lives
in the graph.

The store still owns the ONE thing a producer needs told: when `parameters` means to
constrain anything, its shape is a JSON Schema subset — `required`, `properties.*.type`
(a name or a list), `additionalProperties: false` — because that is what the reader on
the other side checks (gravity's `invoke_skill`, gravity ADR-0004: the arguments are
validated before a node runs). A document that declares none of those keys, or is not
JSON at all, is prose: it constrains nothing and the reader checks nothing, which is an
honest "no contract", not a silent pass of one. Today every row in the tree is `{}`, so
the check has no producer to exercise it yet — that is the gap, not the design.

### 3. The language is data on the row, and it never defaults

A script is data, and data has a language. Nothing here assumes one, picks a default,
or falls back when the field is empty — the caller routes the call to the mount serving
that language, and a language with no mount is the caller's error value. A default here
would turn the caller's routing bug into a silent misinterpretation of someone's
source.

### 4. A row is code, not a skill

One login script is written once; many skills carry a step pointing at it. That
direction — many skills to one code row — is why the row exists separately at all. If
code were stored INSIDE a skill node, that script would be duplicated into every skill
that uses it.

### 5. `list()` is a bring-up posture, not the interface

The bring-up pool read (`GET /code`, identity + language + description and **no
source**) was WITHDRAWN 2026-10-20: no consumer ever issued it — gravity reads one
row by id at call time, and the resolver's "which step points at which code" question
is the graph's (Phase 5+). A door nobody calls is a surface to keep honest for
nothing. It was described as
while skills do not yet link to code; when the graph's retrieval can answer "which code
does a step of this skill point at", that query replaces the list.

### 6. Storage placement

The table lives in the `memory` keyspace — one tree with the graph and (later) the
skills, discriminated by ns, so a code row and a fact do not need two database handles.
The API is `put` / `get` / `list`.

## Why Not

- **Source as facts in the graph**: a 600-line script is not an atomic assertion. Stored
  as one fact it is an opaque blob wearing an edge's clothes; split into pieces it needs
  reassembly rules nothing else in the graph has. Weight and decay, the graph's whole
  point, mean nothing for source.
- **A `name` field as the lookup key**: inviting a global namespace the store cannot
  police (collisions, renames, case folding), and a rename would either break links or
  require rewriting them.
- **A content hash as identity**: dedupe by value. It also silently merges two rows that
  differ only in `parameters` — the field that actually decides how the code is called.
- **A version column**: gravity re-reads the row on every call, so the row's source is
  whatever it says now. History, when it is wanted, is a second row plus a link saying
  one succeeded the other — not a version integer the store would have to interpret.
- **A search door over source**: this is not a second memory surface. Retrieval for code
  belongs to the graph's link from a skill, not to a text search here.

## Consequences

- Code rows are inert data: no cascade, no delete path a door exposes, no lifecycle.
  A row that nothing links to is simply unreferenced.
- A caller that stores code without a matching mount on the executing node finds out at
  call time, loudly. That asymmetry is deliberate: the store cannot know what a node
  can run.
- The `list()` shape is owed a replacement by the graph's retrieval (PLAN 2.8), and this
  ADR records that the list is a bring-up posture so nobody mistakes it for the design.