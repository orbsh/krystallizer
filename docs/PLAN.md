# PLAN

Phases reference ADRs; design content lives in the ADRs, never here.

- [x] Phase 0 — Skeleton: mem-core crate + Fjall engine + PyO3 binding channel smoke test (one store / one search). (ADR-0002)
- [ ] Phase 1 — Flat memory parity: store / search / forget / list / stats semantics on Fjall; retrieval = built-in local embedding model (in-process, no API, model locked with the data) landing into the landed community (rendered as id-bearing text), plus an exact name/alias point lookup as the second entrance. PG exit gate: end-to-end recall comparison on a fixed query set. (ADR-0002, ADR-0003, ADR-0007)
  - [x] 1a. Semantics: store / search (user-prefix scan) / forget / list / stats; config loading (`config/krystallizer.kdl`, knus, env-resolved secrets)
  - [ ] 1b. Built-in embedder: local small model loaded in-process (no endpoint, no key), embeddings written as `Vector<f32>` frames; name/alias → node id point lookup
    - Engine half LANDED 2026-10-17: `mem-core/src/embedding.rs` (`Embedder`: candle BERT, WordPiece tokenize → BERT → CLS pooling → L2 normalize), config `embedding { model-dir … }` + `KRYSTALLIZER_MODEL_DIR` override, model = BAAI/bge-small-zh-v1.5 (512-dim, artifact under `~/.local/share/krystallizer/models/`, not in the repo). Gates: mem-core 8/0 (embedding 3 — a missing artifact names the file it could not read; a Chinese sentence embeds to 512 unit-length floats; related text beats unrelated — the two inference tests skip when the machine has no artifact, so the default gate stays runnable anywhere), clippy 0, fmt clean.
    - Frame half LANDED 2026-10-17: `MemoryRow.embedding: Vector<f32>` (cold frame, ANY length — no `#[ok_len]` contract, because a model swap changes the data rather than the wire); `MemoryStore::store(…, embedding: &[f32])` states the vector and `get()` reads a row back; the SHELL embeds at the door — a `mem_core::Embedder` rides `mem-server`'s state (`open_with(path, Some(engine))`), while `open()` = no engine and rows then land with an empty frame. Gates: mem-core 8/0 lib + 6/0 smoke (a row's vector round-trips through a reopen, including a 512-wide one; an empty frame is not a zero vector), mem-server 5/0 memory_surface (an engine-attached door still round-trips), clippy 0, fmt clean. The mem-ffi binding passes an empty slice — it attaches no engine.
    - Owed: the per-user in-memory vector table + brute-force Top-K (1c), reading rows through `get` / a bulk vector read. Note: no door returns a row's VECTOR (2 KB per hit is what the code store's own pool-read rule refuses), so the vector's contents are asserted in mem-core and the door's test asserts that embedding does not change the surface.
  - [x] 1c. In-memory vector search LANDED (2026-10-19, on the GRAPH's nodes — the graph is the successor of flat memory): the entity row carries `embedding: Vector<f32>` (empty is legal → skipped), `GraphStore::retrieve(scope, q, k)` loads the scope's rows from the KV on first use into `okm-vector`'s `ExactIndex` (the scan is okm's; k10r owns only when to rebuild and the memory budget), drops everything under `LANDING_FLOOR` (0.35 cosine, i.e. `dist2 <= 2 - 2*cos`) and returns top-k seeds; the table is derived state (dropped on every write) and byte-budgeted with LRU eviction. Door: `GET /graph/{scope}/search?q=&k=` (empty landing = 200 + `[]`). Gates: mem-core 15/0 for the graph suite (3 retrieval — exact-name lands at score 1.0, the table follows writes, no engine lands nothing), mem-server facts 9/0. Followed the same day by the ONE-HOP expansion: `GraphStore::recall` + `GET /memories/{scope}/recall` (seeds + every fact touching them in either direction, deduped by edge id; an empty landing returns both empty). Gates: mem-core graph 16/0, mem-server facts 10/0. Paid inside this item: the weight-ordered ranking (ADR-0005 — 2026-10-19, see 3b). Not this repo's item by design: deeper-than-one-hop walks (the caller composes them); ANN stays re-opened only on a measured threshold (ADR-0003).
  - [x] 1d. Recall gate LANDED (2026-10-19) as a MEASURED BASELINE, not the PG comparison: `crates/mem-core/tests/recall_gate.rs` builds a 14-node corpus (k10r's own vocabulary) + a fixed 14-query set, each query naming the node it must reach, and prints/asserts the numbers — **precision@1 = 13/14 (0.929), precision@3 = 14/14 (1.000), lowest true-hit score 0.637 against the 0.35 floor, and the off-corpus query ("今天北京天气怎么样，要不要带伞") lands nothing**. The one p@1 miss is "记忆图谱" → `用户记忆` (0.680) over `事实图谱` (0.661) — two memory concepts, not a failure of landing. The gate asserts p@1 ≥ 0.85, p@3 ≥ 0.95, every true hit above the floor, and the empty landing; it skips when the machine has no model artifact. Open (a follow-up, not a debt — that stack is not runnable here): the comparison against the legacy pgvector + pdb.bm25 + RRF stack; the fixture above is what a future comparison is measured against.
  - [x] 1e. HTTP face for the 主动面 LANDED (2026-10-16): `POST /memory` (`MemoryStore::store` — the store assigns the id, the caller's `created_at` rides the request) and `GET /memory/{user_id}/search?q=` (`MemoryStore::search` — one user's key range; an empty result is legal). `forget` / `list` / `stats` still have no route. This is the tool face gravity and other agents read and write through (ADR-0001; the graph's door (`ingest(fact)`) is Phase 3. Gates: mem-server 12/0 (4 in `tests/memory_surface.rs` — round trip, user-scoped scan, legal empty result, flat memory + code sharing one keyspace without leaking into each other), mem-core 22/0 untouched, clippy 0, fmt clean. Transitional: this door is withdrawn together with the table when the fact graph lands (Phase 3).
  - [ ] 1f. ANN — when a MEASURED corpus crosses ADR-0003's threshold, vector search switches to **okm-vector** directly (the okm workspace crate built for exactly this): `VectorIndex` = fnv-1a sharded in-memory HNSW over quantized codes (`Codec::I8` cosine or `Codec::Pq`), snapshot one blob per shard under okm's fixed face 0x0B — derived state, one batch, droppable; candidates come back at code distance and are re-scored against the stored f32 vector. Not a hand-rolled graph, not arroy (LMDB = a second storage engine, rejected in ADR-0003), no schema change here: the corpus stays the entity rows' `embedding` frames. Trigger is measured scope size/latency, never a date; below it `retrieve` still scans exactly, through `ExactIndex` in the same crate.
- [ ] Phase 2 — Channel log & agent projection: messages / members /
  cursors / checkpoints (per member) tables with gravity-assigned
  timestamp ordering; session-control primitives (tail / summarize-as-
  append) over the projection; thread = parented sub-channel; topic tag
  index. The compression point is its OWN table (ns 34) pointing into the
  log by `coverage_ts` — latest-wins inside that table — while the log
  itself stays append-only (ADR-0002 / ADR-0008).
  (ADR-0001, ADR-0002, ADR-0008)
  - [x] Minimal slice LANDED (2026-10-09): ns 33 messages + ns 34
    checkpoints, `ChannelStore::append`/`fetch_full`/`append_checkpoint`.
  - [x] HTTP surface LANDED (2026-10-10): mem-server carries the three
    channel-log endpoints over axum (`POST /channels/{id}/messages`,
    `GET /channels/{id}/members/{m}/session`,
    `POST /channels/{id}/members/{m}/checkpoint`); caller owns the
    clock (ts_ms/sub ride the request); 4 integration tests against a
    real server (key echo, checkpoint/increment semantics, channel +
    member isolation).
- [ ] Phase 2.5 — View-layer serialization: projection read path
  (checkpoint + increment assembly) with tool-call pruning in the view
  (storage always full; read-side only), enabling the stateless-agent
  integration contract (append → read projection → run). The assembled
  projection is a structured object — the records plus the control
  field(s) the caller must act on out of band (2.6's compression
  directive) — never markers written into records. Assembly is
  ORDERED, head first: the global layer (persona / style) → the
  compression point → the messages after it, and it stays a k10r internal
  detail (ADR-0008. (ADR-0006, ADR-0008)
- [ ] Phase 2.6 — Compression policy engine: parameterized triggers
  (cache-clock, budget percentage, max-gap split — all from
  `krystallizer.kdl`, per-user override layer reserved), structured
  decision logs; regression tuning deferred until feedback labels exist.
  The engine owns its inputs (the log's timestamps, the assembled
  context's size, the member's read cursor) and evaluates at the
  projection read; its verdict rides the projection as a control field
  (`compress: { checkpoint, prompt }`) — there is NO agent-side
  compression interface. The caller runs the extra branch turn and
  returns the summary; k10r derives coverage from the compressed span.
  Owed with it: the decision-log table + its ns, and the tail prompt's
  text in PROMPTS. Lands with/after 2.5 — its budget signal measures
  the assembled view. (ADR-0008)
- [ ] Phase 2.7 — Multi-party participation: member display-name/role
  fields + per-message attribution in serialization; profile knowledge as
  graph facts (a member node plus its edges — the `kind=profile` flat category
  is withdrawn, ADR-0009; interrupt
  trigger (seconds-scale passive window) with silent outcomes in the
  decision log. (ADR-0009)
- [ ] Phase 2.8 — Code store: the delivered script's home, and the one
  table an executor's call reads (gravity ADR-0001/0003 — the script
  travels as a call argument, so identity is a bare surrogate id: no
  name, no content hash). A row is a reusable piece of code, NOT a
  skill: it carries the language, a description and the parameter
  definition, so a skill-to-code link is decided from the row instead of
  reading every source — while what a skill IS stays with the graph (a
  structured node: preconditions / steps / experience / verification).
  Nothing defaults the language. gravity is the only client (the node
  never talks to krystallizer).
  LANDED 2026-10-15 (bring-up slice): ns 35 `CodeKey`/`CodeRow`
  (`language`, `description`, `parameters`, `source`) + `CodeStore`
  (`put` / `get` / `list`); mem-server `POST /code` (the store assigns
  the id), `GET /code/{code_id}` (404 on an unknown id), `GET /code`
  (identity + language + description, no source); one Fjall database per
  process with TWO keyspaces — `channel` (queue-like, its own LSM tree so
  its compaction never drags the other tables) and `memory` (flat memory
  + code, ns-discriminated; the graph and the skills join it later):
  `service::open_service` opens the database and attaches each group with
  `FjallStore::from_db` — Fjall locks the directory, so a second `open` on
  it fails — leaving the server one handle and one lock per keyspace.
  k10r stays what ADR-0007 made it: a standalone service with its own
  storage, reached only over HTTP.
  Gates: mem-core 22/0 (code 4 — layout lock, four-field round trip, pool
  shape, id recovery across reopen; service 3 — the assembly serves every
  group, the two memory-keyspace tables declare different ns, a second
  open of the directory is refused), mem-server 8/0 (round trip, 404, pool
  shape, code + channel coexisting in one database), clippy 0, fmt clean.
  Owed: this repo's own ADR for the store, and `list()` is the declared
  bring-up posture for a resolver's pool read — the graph's retrieval
  replaces it.
- [ ] Phase 3 — Atomic-fact graph: fact node/edge tables, ingestion of the 8-slot fact tuple (the facts arrive by the model's ACTIVE add — the caller routes that call to `ingest(fact)`; no compression pass and no trailing extraction worker, ADR-0008, write-time fusion (in-domain id / out-of-domain embedding convergence + small-candidate LLM decision — the LLM decision is gravity's, per ADR-0007's boundary), weight system (the counters on the edge row, read-write quadrants, tool_invoke_count); initialization memory (人设/风格) — a traversal from the user node, delivered at the context head; the task graph (task vertices + `task_links` dependency edges) shares that same graph — tasks are the third interface surface, their instance state structured rather than approximate. Write-time resolution (upsert on a new fact) and cognitive binning ride this side per ADR-0010 (fact node / community reverse index / the community sequence; ns 36/37 for the graph, 38 for the sequence). Scope note: append-first covers THIS side (node/edge rows, weights, derived entries) — the log does not (ADR-0002. Flat memory (ns 32) and its HTTP door are withdrawn in this landing: the graph is its successor (ADR-0001's carrier line, "flat memory, then the graph") — flat was the pre-graph form of the same extracted knowledge, and a text row carries no per-fact identity. What is owed with it: the fate of legacy flat rows (ADR-0002's PG migration imports them as-is) is a migration question, not a schema one. (ADR-0001, ADR-0002, ADR-0004, ADR-0005, ADR-0007, ADR-0010)
  - [x] 3a. The write side's steps 1–2 LANDED (2026-10-17): the entities
    collection (ns 36) + the facts edge collection (ns 37) and the
    deterministic `ingest(fact)` path — `GraphStore` in
    `crates/mem-core/src/graph.rs`: the two entity faces
    (`by_kind` / `by_name`; `community_id` and `by_community` wait for the
    read path that scans them — ADR-0010's steps 3–4 are deferred), the
    edge's declared
    `since` / `confidence` / `weight`, the functional-vs-multi-valued
    resolution the caller declares per assertion, literal nodes, and the
    relation props on the edge's dynamic segment (same batch as the
    link). Gates: mem-core 34/0 (graph 12 — layout locks, scope
    isolation, the polygons of the resolution, the literal node, props
    merge, id recovery), clippy 0, fmt clean. The write door
    `POST /memories/{scope}` landed with it (the request body IS the 8-slot
    tuple; the outcome comes back as linked / rewritten / dropped; a
    structural bug is a 400 carrying the core's own reason). The read
    face landed with it: `GET /memories/{scope}/entities/{name}` (the exact
    name/alias entrance, 404 when absent) and
    `GET /graph/{scope}/entities/{node_id}/edges` (one hop with both
    endpoints resolved — the ANALYSIS class: inspection, not the answer).
    Gates: mem-server 19/0 (facts 7 — the write
    round trip incl. the incumbent rewrite and scope isolation, the
    refusal, the keyspace leak test, the entrance, the one-hop read),
    mem-core 34/0, clippy 0, fmt clean. Owed: the graph's retrieval
    — the LANDING part is done (2026-10-19: node vectors, `retrieve`, the
    search door; see 1c), what remains is the neighbourhood expansion and
    the weight-ordered ranking (ADR-0005) — and
    ADR-0010's steps 3–4 (LPA, binning). The okm-side items of
    ADR-0010's owed list are all PAID (2026-10-19): the read-side load on
    probe, the generated payload decode, and `unlink_into` (a functional
    rewrite is now ONE batch).

  - [x] 3b. Weight system LANDED 2026-10-19 (ADR-0005): the counters ride the
    edge row (`read_count` / `write_count` / `tool_count` / `last_read` /
    `last_write`, payload layout v2) and `weight` is the folded formula in
    milli-units. Writers: `ingest` bumps `Write` (link or rewrite, never a
    drop); `recall`/`expand` bump `Read` only when the caller's `ts_ms` rides
    the call (no clock = served but not counted, and no invented decay); and
    `POST /memories/{scope}/edges/{id}/touch` bumps `Tool` (404 for an unknown
    edge or another scope's). Decay is charged at the fold against the
    PREVIOUS access, then the same fold re-bases the clock. `recall` returns
    the served facts ordered by weight (stable; seeds keep similarity order).
    The three declared faces (`by_since` / `by_confidence` / `by_weight`) were
    REMOVED: nothing scans them, and every touch would maintain them for no
    reader — ADR-0005 records the verdict (a per-scope top-K would
    be a `[scope][weight][edge_id]` projection, not a face). Gates: mem-core
    graph 25/0 (5 weight tests), mem-server facts 12/0, clippy 0. Follow-ups
    (not debts): the delta-append table (multi-writer only — a Deferred gate)
    and a scope-wide Top-K (Phase 4's read; it needs that projection table
    written on the same touch, ADR-0005).
  - [x] 3c. Steps 3–4 LANDED 2026-10-19, RESHAPED 2026-10-20 (ADR-0010):
    `EntityRow.community_id` + the `by_community` face; incremental LPA inside
    `ingest` (2-hop subgraph, 3 rounds, vote = `weight × confidence`, ties to
    the smaller community, one batch for every moved node). The id issuer is
    the scope's SEQUENCE row (ns 38: `[scope]` → `high_water`, monotonic, so
    an id is never reused). The read side renders instead of summarizing:
    `recall` lands → takes the landed COMMUNITY's bucket (members through
    `by_community`, facts whose endpoints are both members, each edge once) →
    ranks by weight → renders id-bearing text under a fact budget (20 by
    default, `?budget=`), in the representation `?format=` names:
    `plaintext` (default) / `markdown` / `mermaid`, or `struct` for the data
    alone (no text at all). ONE representation per answer: the text formats
    fill `text`, `struct` fills `seeds` + `facts`; `{format, communities,
    shown, total}` ride both. The session door's head follows the same knob
    (k10r renders the initialization memory; gravity injects the text and
    refuses `struct`, which its config validates). `GET /graph/{scope}` nodes
    carry `community_id`.
    RETIRED in the same reshape: the capsule table (title/summary/state/
    uncompressed), the density water line + its `DENSITY_MIN_VERTICES` guard,
    the uncompressed watermark, the orphan-capsule deletion, and the four
    consolidation doors (`?state=`, bucket, packet, `PUT`) — the design turned
    the consumer into k10r's index maintainer for a lossy projection of what
    the nodes and edges already were. Gates: mem-core 51/0 across its suites
    (view 4, graph 25 incl. the budget and the id-monotonicity tests),
    mem-server 21/0, clippy 0. Follow-up: the per-scope top-K read (Phase 4,
    with the projection table 3b names).
  - [x] 3d. Flat memory (ns 32) and its HTTP door WITHDRAWN 2026-10-19: the
    table and `MemoryStore` are deleted, `POST /memory` and
    `GET /memory/{user_id}/search` are gone, and `mem-ffi` now binds
    `GraphStore` (ingest / retrieve / recall / entity) instead of the removed
    store. The keyspace-sharing test moved to graph + code (different ns, same
    tree, both take id 1 without colliding). Docs swept: ADR-0001's carrier
    line, MODELING's flat section (kept as the historical schema, marked
    withdrawn), SEAM's endpoint table, README. Legacy flat rows stay a
    migration question (ADR-0002's PG import), not a schema one. Gates:
    mem-core 47/0 across its suites, mem-server 20/0, clippy 0.
- [ ] Phase 4 — Clustered retrieval: entity-anchored / temporal / causal-chain views; progressive loading (a scope-wide Top-K by weight — a `[scope][weight][edge_id]` projection table maintained on each touch, not a declared face; ADR-0005). Reads the write-time structure ADR-0010 lands (the node's community id + the `by_community` face + the rendered community) instead of running a graph algorithm on the read path. (ADR-0005, ADR-0010)
- [ ] Phase 5+ — Deferred (do not start early): dreaming consolidation (petgraph community detection + human-reviewed op set), SKILL topological four-part nodes, Aura Actor nodes. (mem-server left this list 2026-10-10 — it is the single cross-process form, ADR-0007 as revised.) ADR-0010 takes the incremental LOCAL form of community detection onto the write side and leaves OPEN whether that promotes a slice of Phase 5 ahead of Phase 3/4 — an open ruling, not settled.

Deferred gates:

- Multi-consumer service shell: mem-server is the single cross-process
  form (ADR-0007 as revised 2026-10-10); it graduates from deferred to
  deliverable with the channel-log endpoints (append / fetch_full /
  append_checkpoint) first, grown as consumers need; core is
  transport-free so the server is additive.
- Delta-append weight counts: single-process serial start uses direct RMW;
  key layout stays delta-compatible so multi-agent switchover changes no schema.
- Backup export endpoint: one HTTP endpoint that exports the okm data
  as Parquet for direct download (backup = bulk read over the same
  HTTP boundary, no admin CLI). Deferred until the flat-memory tables
  tables settle (Phase 3); format/open questions — table granularity
  (per-ns vs per-collection), whether to reuse okm's export surface if
  one lands — recorded here so the endpoint grows with consumers, not
  ahead of them.
- Graph visualization endpoint: LANDED 2026-10-17 as
  `GET /graph/{scope}` (`GraphStore::scope_entities` + `scope_facts`) —
  one read-only bulk read over the same HTTP boundary (ADR-0007), no new
  mechanism. Nodes carry id / name / kind / literal_num / community_id; links carry
  source / target / kind / since / confidence / weight — the shape
  3d-force-graph consumes, and the scope prefix bounds the scan, so the
  demo page reads the real graph instead of hand-written packets. Still
  open: (a) whole-table vs paged — one scope is read whole, paging waits
  for a corpus that needs it; (b) RULED 2026-10-17: the page is served
  FROM mem-server, same origin — no CORS is opened on the memory service
  (it holds user memory; letting `file://` sandboxes read it is the wrong
  trade) and the page's fetch stays same-origin; (c) cluster coloring — the
  node now carries `community_id` (ADR-0010 §6), so the colour is available;
  `docs/viz-style.js` still colours by kind, which is the demo's own open
  slice rather than a missing field.
