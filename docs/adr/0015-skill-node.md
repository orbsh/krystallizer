# 0015 — The skill node: what the resolver lists and the executor walks

Status: Accepted (2026-10-26)

## Context

gravity's tool list is fixed and hardcoded (gravity ADR-0004), and one of its four
names is `resolve_skills` — the call a model makes when the prompt and the session
head are not enough: the call returns candidate skills, the model picks one, and
`invoke_skill` walks it. That name has no door today, and the reason is not effort.

It was withdrawn on 2026-10-20 with zero clients (`GET /code`, the bring-up pool
read), on the argument that "the list arrives with the skill node, not before".
The skill node was Phase 5+. Now the client exists, so the second group of the tool
list is blocked on a SCHEMA, not on a caller — and the schema is what this record
fixes. It deliberately takes the smallest slice that makes the two calls possible
and leaves the planner, the ordering and the consolidation where they were.

## Decision

### 1. A skill is an entity node of kind `skill`, in the fact graph

`EntityRow` at ns 36, in the same `scope` as the facts, with `kind = "skill"`.

The resolver's question is "which of these fits the task in front of me", which is
the graph's retrieval question — the vector is already computed over `title` +
`content` (ADR-0003 as amended 2026-10-23), and the landing machinery already
answers "what near this text is worth reading". A dedicated table would need a
second index, a second retrieval path and a join against the facts a skill's
experience hangs on; the graph has all three already.

It is not a code row (ns 35): ADR-0012 §Context already separates a skill from a
script — one skill may name several scripts and one script may serve several
skills, and the row exists precisely so the link can be a link.

### 2. Identity is the node's `name`, and declaring is idempotent

A skill's handle is its name (`by_name` face, an attribute like every other). A
declaration whose name already exists IS that skill: the props merge, and the door
reports the node id it landed on. There is no skill id space to allocate, no version
column — a skill is a row that says what it currently is, and history, when it is
wanted, is a second node plus a link saying one succeeded the other (the ADR-0012
argument, the same shape).

An entity's name is unique per scope by resolution order already; a skill inherits
that and needs nothing of its own.

### 3. The four parts, and where each one lives

- **`preconditions`** — TEXT JSON array of strings: what must hold before the first
  step. The planner checks them (with `recall_memory`); evaluating them is not the
  store's job.
- **`steps`** — TEXT JSON array of objects, execution order: `[{"code_id": 7,
  "note": "…"}]`. The executable identity is ns 35's row, so a skill never
  duplicates a script's language or parameter definition.
- **`verification`** — TEXT JSON array of strings: what to check after a step,
  before the next one.
- **`experience`** — NOT a prop. It is the skill node's INCIDENT FACTS: runs,
  outcomes, what failed and against what. The weight system (ADR-0005) already
  counts reads/writes/tools and decays them, so experience rides machinery that
  exists and is re-folded on every touch. A JSON blob would be a second, unweighted
  memory that nothing ever decays.

The three that are props are TEXT JSON for the reason ADR-0012 §2 gives for
`parameters`: the store does not parse them, the producer owns the shape. A prop the
store parses becomes a second schema to migrate, and the graph's contract
(`name` / `kind` / props) has no business knowing what a step is.

`title` and `content` stay the conventional props (MODELING §"three conventional
props"): `content` is the skill's "what this does and when to reach for it" text,
which is to say the retrieval surface — a skill declared without it is a row the
resolver can only find by exact name.

### 4. Three doors, scoped like the graph's

| door | body / answer |
| --- | --- |
| `POST /skills/{scope}` | `{name, title?, content?, preconditions?, steps?, verification?}` → `{node_id, created}` |
| `GET /skills/{scope}/{node_id}` | the skill with all four parts; 404 for an unknown id, 400 when the node is not a skill |
| `POST /skills/{scope}/resolve` | `{query, limit}` → `[{node_id, name, title, content, steps, score}]`, best first |

`resolve` reads through the graph's retrieval and keeps the `skill` kind. When
retrieval lands nothing — no embedding engine attached, or nothing inside the
landing distance — it falls back to the `by_kind` face in id order, cut at `limit`
(default 5, max 20). A resolver that answers "there are no skills" because the
deployment has no engine is a wrong answer, and the corpus is small by
construction, so the scan is cheap.

Each candidate carries its `steps`, because deciding needs them: the consumer is
a model with a candidate list in front of it, and "what would this make me do" is
part of choosing. `preconditions` and `verification` stay out — those are what the
walk reads after the choice. The alternative would be a second call ("read this
skill") or a fifth tool name on gravity's fixed list; a field on the row the
resolver already answers is cheaper than either.

`resolve` takes NO caller clock and counts nothing. It returns candidates; only the
one the model picks is a use, and the use is counted when the skill is actually
walked — a candidate nobody chose is not a read (ADR-0005's rule that an unstamped
read is not a read).

### 5. What this is not

No planner, no topological ordering, no retry policy, no weight write-back from a
run, no dreaming consolidation. This record fixes the ROW a resolver reads and an
executor walks. Deciding the order, retrying a failed step and folding outcomes back
into experience stay gravity's (ADR-0004 §3: one step per call, the model drives
the sequence) and the consolidation layer's (Phase 5) —
k10r has no business knowing what a step means.

## Why Not

- **A skill as a fact (an edge)**: a skill is not one assertion. Split into several
  edges it needs a shape nothing indexes, and "resolve a skill" becomes a traversal
  with no landing point.
- **A skill in the code row**: the two are many-to-many, and folding them would make
  every skill that shares a script a copy of it.
- **One JSON document in a single prop**: opaque to the vector (which reads `title`
  and `content` only) unless it is duplicated there, and the four parts have three
  different consumers — planner, executor, consolidation — who would each parse the
  same blob differently.
- **A reserved ns for skills**: within one scope the graph is already one corpus;
  `kind` discriminates, and a separate ns would only buy a second keyspace to keep
  in step.
- **A version column, or a skill id space**: the same argument ADR-0012 makes for
  code rows.
- **Sorting steps in k10r**: order is execution policy. The store answers "here is
  the row"; who walks it and in what order is the executor's judgement.

## Consequences

- A skill with no `steps` is legal and useless: the resolver may hand it to the
  model and the executor will find nothing to run. That failure surfaces at invoke
  time (gravity), not here — the store does not police a skill's usefulness.
- Skills ride the fact graph's weight and decay, so a skill nobody resolves decays
  like anything else in the corpus. If that turns out wrong, the fix is a `Touch` on
  the use path, not a separate store.
- `content` becomes load-bearing: a skill declared with only a name is findable by
  exact name and by nothing else. The declaration door does not require it (a legal
  node needs one field), and a deployment that skips it will see the resolver answer
  short and will have to look at what it declared.
- The `GET /code` pool read stays withdrawn. A skill names its scripts by id, and
  the caller reads the rows it was told about — the list it once offered is now the
  skill's `steps`, which says which script belongs to which skill, something the code
  table never knew.