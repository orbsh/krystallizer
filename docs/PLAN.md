# PLAN

Phases reference ADRs; design content lives in the ADRs, never here.

- [x] Phase 0 — Skeleton: mem-core crate + Fjall engine + PyO3 binding channel smoke test (one store / one search). (ADR-0002)
- [x] Phase 1 — Flat memory parity: SUPERSEDED 2026-10-20, not abandoned unfinished — every goal here was re-answered on the graph by Phase 3 and the doors that came with it (the flat table itself was withdrawn in 3d), so the phase closes as "its successor did it" rather than staying open with an empty box. The original text stays for the record: store / search / forget / list / stats semantics on Fjall; retrieval = built-in local embedding model (in-process, no API, model locked with the data) landing into the landed community (rendered as id-bearing text), plus an exact name/alias point lookup as the second entrance. PG exit gate: end-to-end recall comparison on a fixed query set. (ADR-0002, ADR-0003, ADR-0007)
  - [x] 1a. Semantics: store / search (user-prefix scan) / forget / list / stats; config loading (`config/krystallizer.kdl`, knus, env-resolved secrets)
  - [x] 1b. Built-in embedder — CLOSED 2026-10-20, its two halves landing elsewhere: the in-process model (no endpoint, no key) landed as `mem-core/src/embedding.rs` and writes `Vector<f32>` frames, but on the ENTITY row rather than a flat-memory row, and the vector table + Top-K it wanted landed as 1c on the graph; the name/alias → node point lookup landed as 3a's `GET /memories/{scope}/entities/{name}`. Nothing is owed here that another item did not pay.
    - Engine half LANDED 2026-10-17: `mem-core/src/embedding.rs` (`Embedder`: candle BERT, WordPiece tokenize → BERT → CLS pooling → L2 normalize), config `embedding { model-dir … }` + `KRYSTALLIZER_MODEL_DIR` override, model = BAAI/bge-small-zh-v1.5 (512-dim, artifact under `~/.local/share/krystallizer/models/`, not in the repo). Gates: mem-core 8/0 (embedding 3 — a missing artifact names the file it could not read; a Chinese sentence embeds to 512 unit-length floats; related text beats unrelated — the two inference tests skip when the machine has no artifact, so the default gate stays runnable anywhere), clippy 0, fmt clean.
    - Frame half LANDED 2026-10-17: `MemoryRow.embedding: Vector<f32>` (cold frame, ANY length — no `#[ok_len]` contract, because a model swap changes the data rather than the wire); `MemoryStore::store(…, embedding: &[f32])` states the vector and `get()` reads a row back; the SHELL embeds at the door — a `mem_core::Embedder` rides `mem-server`'s state (`open_with(path, Some(engine))`), while `open()` = no engine and rows then land with an empty frame. Gates: mem-core 8/0 lib + 6/0 smoke (a row's vector round-trips through a reopen, including a 512-wide one; an empty frame is not a zero vector), mem-server 5/0 memory_surface (an engine-attached door still round-trips), clippy 0, fmt clean. (mem-ffi passed an empty slice there; that crate is withdrawn as of 2026-10-21 — see below.)
    - Owed: the per-user in-memory vector table + brute-force Top-K (1c), reading rows through `get` / a bulk vector read. Note: no door returns a row's VECTOR (2 KB per hit is what the code store's own pool-read rule refuses), so the vector's contents are asserted in mem-core and the door's test asserts that embedding does not change the surface.
  - [x] 1c. In-memory vector search LANDED (2026-10-19, on the GRAPH's nodes — the graph is the successor of flat memory): the entity row carries `embedding: Vector<f32>` (empty is legal → skipped), `GraphStore::retrieve(scope, q, k)` loads the scope's rows from the KV on first use into `okm-vector`'s `ExactIndex` (the scan is okm's; k10r owns only when to rebuild and the memory budget), drops everything under `LANDING_FLOOR` (0.35 cosine, i.e. `dist2 <= 2 - 2*cos`) and returns top-k seeds; the table is derived state (dropped on every write) and byte-budgeted with LRU eviction. Door: `GET /graph/{scope}/search?q=&k=` (empty landing = 200 + `[]`). Gates: mem-core 15/0 for the graph suite (3 retrieval — exact-name lands at score 1.0, the table follows writes, no engine lands nothing), mem-server facts 9/0. Followed the same day by the ONE-HOP expansion: `GraphStore::recall` + `GET /memories/{scope}/recall` (seeds + every fact touching them in either direction, deduped by edge id; an empty landing returns both empty). Gates: mem-core graph 16/0, mem-server facts 10/0. Paid inside this item: the weight-ordered ranking (ADR-0005 — 2026-10-19, see 3b). Not this repo's items by design: deeper-than-one-hop walks (the caller composes them); the index's own form — exact or approximate — is `okm-vector`'s call, adopted from that crate, never a phase or a threshold parameter here (ADR-0003).
  - [x] 1d. Recall gate LANDED (2026-10-19) as a MEASURED BASELINE, not the PG comparison: `crates/mem-core/tests/recall_gate.rs` builds a 14-node corpus (k10r's own vocabulary) + a fixed 14-query set, each query naming the node it must reach, and prints/asserts the numbers — **precision@1 = 13/14 (0.929), precision@3 = 14/14 (1.000), lowest true-hit score 0.637 against the 0.35 floor, and the off-corpus query ("今天北京天气怎么样，要不要带伞") lands nothing**. The one p@1 miss is "记忆图谱" → `用户记忆` (0.680) over `事实图谱` (0.661) — two memory concepts, not a failure of landing. The gate asserts p@1 ≥ 0.85, p@3 ≥ 0.95, every true hit above the floor, and the empty landing; it skips when the machine has no model artifact. Open (a follow-up, not a debt — that stack is not runnable here): the comparison against the legacy pgvector + pdb.bm25 + RRF stack; the fixture above is what a future comparison is measured against.
  - [x] 1e. HTTP face for the 主动面 LANDED (2026-10-16): `POST /memory` (`MemoryStore::store` — the store assigns the id, the caller's `created_at` rides the request) and `GET /memory/{user_id}/search?q=` (`MemoryStore::search` — one user's key range; an empty result is legal). `forget` / `list` / `stats` still have no route. This is the tool face gravity and other agents read and write through (ADR-0001; the graph's door (`ingest(fact)`) is Phase 3. Gates: mem-server 12/0 (4 in `tests/memory_surface.rs` — round trip, user-scoped scan, legal empty result, flat memory + code sharing one keyspace without leaking into each other), mem-core 22/0 untouched, clippy 0, fmt clean. Transitional: this door is withdrawn together with the table when the fact graph lands (Phase 3).
- [x] Phase 2 — Channel log & agent projection: messages / cursors /
  checkpoints tables with gravity-assigned
  timestamp ordering; session-control primitives (tail / summarize-as-
  append) over the projection; thread = parented sub-channel; topic tag
  index. The compression point is its OWN table (ns 34), a `(ts_ms, sub)`
  pointer at ONE log record — latest-wins inside that table — while the log
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
- [x] Phase 2.5 — View-layer serialization: projection read path
  (checkpoint + increment assembly) with tool-call pruning in the view
  (storage always full; read-side only), enabling the stateless-agent
  integration contract (append → read projection → run). The assembled
  projection is a structured object — the records plus the control
  field(s) the caller must act on out of band (2.6's compression
  directive) — never markers written into records. Assembly is
  ORDERED, head first: the global layer (persona / style) → the
  compression point → the messages after it, and it stays a k10r internal
  detail (ADR-0008. (ADR-0006, ADR-0008)
- [x] Phase 2.6 — Compression policy engine: parameterized triggers
  (cache-clock, budget percentage, max-gap split — all from
  `krystallizer.kdl`, per-user override layer reserved); NO decision log
  (2.6d: the checkpoint row itself answers which span was compressed, and
  tuning needs labels that do not exist yet).
  The engine owns its inputs (the log's timestamps, the assembled
  context's size, the channel's read position) and evaluates at the
  projection read; its verdict rides the projection as a control field
  (`compress: { checkpoint, prompt }`) — there is NO agent-side
  compression interface. The caller runs the extra branch turn and
  returns the summary; k10r fills it into the checkpoint row it opened.
  Owed with it: the tail prompt's text in PROMPTS (§2, landed). Lands
  with/after 2.5 — its budget signal measures the assembled view.
  (ADR-0008)
  - [x] 2.6a. The STATE side LANDED 2026-10-20: `CursorRow` (ns 39,
    `[channel][member] → last-read ts`, latest-wins and never rewinding) and
    `DecisionRow` (ns 40, keyed by the proposed SPLIT so re-reads refresh one
    row), both in the CHANNEL keyspace because a summary landing and its
    decision closing commit in ONE batch (`append_checkpoint_from_verdict`).
    The decision log was WITHDRAWN the same day (2.6d): the checkpoint row
    carries that state itself.
  - [x] 2.6b. The ENGINE LANDED 2026-10-20: `crates/mem-core/src/compress.rs`
    — pure functions, the three heuristic signals (budget mid-band, cache
    clock, max-gap split), no LLM and no timer; the numbers are the
    `compression { }` node, and a test asserts the node equals
    `CompressionParams::default()`. Door: `GET …/session?ts_ms=` measures the
    idle time against the PREVIOUS cursor, stamps, evaluates, records, and
    returns `compress: { checkpoint, prompt }`. Gates: mem-core 72/0
    (compress 9, channel 10), mem-server 25/0 (compression_surface 4).
  - [x] 2.6c. The checkpoint door takes the SUMMARY alone LANDED 2026-10-20:
    the body is `{ summary }`, `append_checkpoint_from_verdict` lands it on the
    split k10r served (checkpoint + decision closed in one batch), and a summary
    with no open verdict is a 400. The consumer's half moved with it — gravity's
    `SessionProjection.compress` carries `{checkpoint, prompt}` through both
    surfaces — so the seam is closed at both ends; firing the branch turn is
    gravity Phase 3 (its PLAN records the seam half). Gates: mem-server 25/0
    (the summary→coverage loop is `compression_surface`'s 4th test),
    gravity-core 17/0 after the seam change.
  - [x] 2.6d. The POINTER + STATE rulings (2026-10-20, ADR-0008 §2/§4):
    (a) the checkpoint is the CHANNEL's pointer — key `[channel][ts][sub]`, the
    key IS the pointer at one log record, so the increment reads rows strictly
    after it in `(ts, sub)` order and same-millisecond messages are not
    swallowed by a millisecond-granular cut; no member segment in ANY TABLE (one
    channel, one voice; an agent's boundary moves with its long-term memory).
    The DOORS keep `{m}`: the read is an act by that member (its cursor is
    stamped, its projection is the answer), so `GET
    /channels/{c}/members/{m}/session` and `POST …/members/{m}/checkpoint` stay
    as they are — and gravity's `MemorySurface`, prism's dial face and the
    booth's turn guest are NOT touched by this landing.
    (b) The compression state lives in the CHECKPOINT TABLE ALONE: rows
    accumulate (duplicates are idempotent, an earlier pointer's late summary is
    free history), an EMPTY summary is the "compressing" marker the engine
    skips, and the initial prompt rides each row as `prompt_snapshot` — stored,
    never recomputed, so the context prefix stays byte-stable for the prompt
    cache. The decision log (ns 40) is WITHDRAWN: it served "tune later" with no
    labels and no reader, while the checkpoint row already answers which span was
    compressed. The cursor stays `[channel][member] → (ts, sub)` and only in the
    multi-party shape (single user + agent: the log's newest record IS the read
    position, so no row and no cache-clock storage).
  - [x] 2.6d-i. The POINTER LANDED 2026-10-20: coverage is a RECORD, not a
    millisecond. `CheckpointRow.coverage_sub` (payload v2) beside
    `coverage_ts`, `SessionView.coverage_sub`, `Verdict.checkpoint_sub` (the
    split names `pair[0]`'s full key), `DecisionRow.checkpoint_sub` (the
    decision key carries the ts, so the second segment rides the row), the
    increment filter compares `(ts, sub)` lexicographically, and
    `append_checkpoint_from_verdict` derives the full pointer. The session DTO
    KEEPS `coverage_ts` and ADDS `coverage_sub` — additive, so gravity's decode
    and the door paths stay untouched. Gates: mem-core 80/0 (two new channel
    tests: a same-millisecond record after the pointer stays raw, and the
    derived coverage carries the split's `sub`), mem-server 25/0, clippy 0.
  - [x] 2.6d-ii. THE SHAPE LANDED 2026-10-20: `CheckpointKey` IS the pointer
    (`[channel][ts_ms][sub]`), `CheckpointRow` holds only the summary (the head
    rides the dynamic `head` array), nodes accumulate, and
    `open_checkpoint(pointer, head)` / `complete_checkpoint(summary)` replace
    note_verdict / pending_verdict / append_checkpoint_from_verdict. The
    decision log (ns 40) is WITHDRAWN — the open node carries what it carried
    (pointer, frozen head, in-flight state) with no second table. The read is
    one rule: newest node WITH a summary decides summary + head + cut point;
    an empty-summary node above it only sets `compressing` (now on the session
    DTO). The read door opens the node, `POST …/checkpoint` completes it. Gates:
    mem-core 80/0, mem-server 26/0.
  - [x] 2.6d-iii. THE CURSOR IS A CLOCK (landed 2026-10-20, narrowed same day):
    `CursorRow` = `read_at` alone — the caller's clock at that read. The model's
    read always answers up to the newest record, so a stored POSITION would be a
    constant, and the checkpoint is what cuts the increment; the earlier
    `(ts_ms, sub)` position was withdrawn on that ground.
    `note_read(channel, member, read_at)`; `SessionView.read_at`; the cache clock
    measures `query.ts_ms − read_at`. Gates: mem-core 84/0, mem-server 26/0.
    Docs are synced in this pass (ADR-0008 pair, ADR-0001/0009 pairs, MODELING
    pair, SEAM pair); the CODE still carries `DEFAULT_MEMBER` and the standalone
    decision log (the ts-only pointer is already gone — 2.6d-i). Touches
    `channel.rs` (three keys, `open`/`complete` checkpoint writes),
    `mem-server` (the checkpoint door fills the open row; the session DTO KEEPS
    `coverage_ts` and ADDS `coverage_sub` + the persona snapshot, so gravity's
    decode stays valid), `view::assemble` + `view.rs`'s placeholder constants
    (the head becomes the row's snapshot). No cross-repo edit: with the door
    paths intact, gravity / prism / booth see no change.
- [x] Phase 2.7 — Multi-party participation: member display-name/role
  fields + per-message attribution in serialization; profile knowledge as
  graph facts (a member node plus its edges — the `kind=profile` flat category
  is withdrawn, ADR-0009; interrupt
  trigger (seconds-scale passive window); interrupt outcomes are not recorded.
  (ADR-0009)
  - [x] 2.7a. THE IDENTITY LAYER LANDED 2026-10-20: ns 42 `MemberKey`/
    `MemberRow` (`display_name`, `role` = HUMAN/AGENT — the one axis
    serialization needs), `put_member` / `member` / `members`, and
    `MessageRow.sender` (payload v2, ALWAYS set — a single-party channel seeds
    its user as member 1) written through `append_from`. A rename re-attributes
    history without rewriting one row: the log stores the id.
  - [x] 2.7b. THE TWO VIEWS LANDED 2026-10-20: `MessageRow.end_ts_ms` (a reply
    keeps the clock it FINISHED on while its key is its first token),
    `append_from(… sender, end_ts_ms …)`, member 1 seeded on a channel's first
    touch (`DEFAULT_MEMBER_NAME`), `ChannelStore::history` (the people's view:
    raw, no checkpoint, no merging) and `view::conversation` (the model's view:
    user bursts merge into one turn, every line `[name(id=N)]`, order = key
    order, tool rows skipped). Tests: the merge + the mid-generation ordering,
    and the seed. Wire: `SessionDto.turns` carries them beside the raw
    `increment` (the handler fills both from one read).
  - [x] 2.7c. THE MEMBER → NODE MAPPING LANDED 2026-10-20:
    `channel::member_node(channel, member)` = `member-{channel}-{member}` (a
    NAMING RULE, not a table — a rename moves no fact), `view::member_memory`
    (the head is EVERY member's incident facts, both directions, deduped by edge
    id, weight-descending), and `assemble` loses its `user_node` parameter —
    `DEFAULT_USER_NODE` is gone with the placeholder it was. Tests: the head
    union over two members; the HTTP head test now ingests on `member-7-1`.
    Gates: 111/0, clippy 0. The profile EXTRACTION (who writes those facts) is
    the reasoning layer's, not this repo's.
- [x] Phase 2.8 — Code store: the delivered script's home, and the one
  table an executor's call reads (gravity ADR-0001/0003 — the script
  travels as a call argument, so identity is a bare surrogate id: no
  name, no content hash). A row is a reusable piece of code, NOT a
  skill: it carries the language, a description and the parameter
  definition, so a skill-to-code link is decided from the row instead of
  reading every source — while what a skill IS stays with the graph (a
  structured node: preconditions / steps / experience / verification).
  Nothing defaults the language. gravity is the only client (the node
  never talks to krystallizer).
  LANDED 2026-10-15 (bring-up slice; ADR-0012 was written 2026-10-20): ns 35 `CodeKey`/`CodeRow`
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
  CLOSED 2026-10-20: `list()` and its `GET /code` door were WITHDRAWN — gravity
  reads one row by id at call time and never issued the pool read (verified: no
  `/code` call in gravity or prism), so the "the graph's retrieval replaces it"
  posture resolved to "nothing replaces it; the list arrives with the skill node
  (Phase 5+)". Gates: 123/0.
- [x] Phase 2.9 — Structured log (ADR-0011): one append-only table at ns 41,
  keyed `[ts_ms][seq]` (k10r's own clock; `seq` seeded from the last key at
  open), declared `level` + `event` from vocabulary constants and NO faces yet,
  locator (`channel` / `scope` / `entity` / `edge` / `msg`) + payload in the
  dynamic segment, `logs { max-rows }` trimming oldest-first, trimming never
  logged, no HTTP door. First writer: the compression verdict with its
  parameter/signal snapshot.
  - [x] 2.9a. The TABLE LANDED 2026-10-20: `crates/mem-core/src/log.rs`
    (`LogKey`/`LogRow`, ns 41, in the CHANNEL keyspace) with `LogStore::open` /
    `wrap` / `append` / `get` / `entries`, the `level` / `event` / `field`
    vocabularies, and the ceiling (`logs { max-rows }` in the kdl file, default
    10_000, drift test) trimming the smallest key on every append that crosses
    it. The trim index is kept in KEY order, not arrival order — a clock step
    backwards must not make "oldest" mean "written first". Gates: mem-core 79/0
    (log 7 — hex lock, dynamic round trip, a bare event line, key-order reads
    incl. same-millisecond, `seq` across a reopen, the ceiling, the config
    drift), mem-server 25/0, clippy 0.
  - [x] 2.9b. THE FIRST WRITER LANDED 2026-10-20: `LogStore::record_verdict(now_ms,
    channel, &verdict)` writes the decision as a row — `channel`, `split_ts`,
    `split_sub`, `trigger` (as a WORD: `budget` / `cache_clock` / `both`), plus
    `assembled_bytes` and whichever signals fired (`idle_ms`, `max_gap_ms`; an
    absent signal writes no field, so absence never reads as zero). The door
    writes it on every read whose policy fired, keyed by K10R's own clock
    (`now_ms()` in mem-server). Windowed read: `LogStore::since(ts_ms)` — a lower
    bound in key order, bounded by `max_rows`. Covered at the row level (mem-core
    log 8/0); the door's single call is compiled but not asserted end to end —
    that needs either a read door (ADR-0011 says none) or a debug endpoint.
  - [x] 2.9c. THE LOG'S FIELD SHAPE LANDED 2026-10-20: `event` and
    `source` become `Vocab<u16>` (a closed, small, endlessly repeated set — two
    bytes, no spellings), `note Str` is the ONE free-text field, `level u8` and
    `ts_ms u64` stay, everything else rides the dynamic segment (ADR-0011 §8). The
    change is the row's FIELD TYPES and nothing else: interning is the type's own
    business (okm's `Collection::put` clones the document and runs `__okm_intern`;
    `get` runs `__okm_deref_vocab`), so `append` keeps taking `&str` — it gains a
    `source` argument and builds `Vocab::of(..)` internally, and no call site
    changes. The one cost is a document clone per write on a vocab-declaring table.
    Layout break is clean (pre-production, ADR-0018 — no migration). It waited on
    one thing only: krystallizer's pinned okm rev predated Vocab document fields
    (`DocumentEncode: unsupported type Vocab<u16>`), so the lock moved e0842a7 →
    49c7406 — the vocabulary commits — and the row compiled as ruled. `LogStore::
    row(key)` now returns the declared fields (the map read gives the dynamic
    segment), which is what the test asserts. Gates: log 9/0, 116/0 total,
    clippy 0.
  - [x] 2.9d. INTERNAL COUNTERS LANDED ON `ok_reduce`'s HighWater 2026-10-20:
    `LogStore::seq` and `CodeStore::next_id` currently recover their high-water by
    reading the table's last key at open. `#[ok_reduce(HighWater(field))]` with no
    `group(…)` is ONE whole-table group maintained on every write — unfold is a
    NO-OP, a watermark must not fall when the newest row is deleted — and read in
    O(1) via `scan_reduces` (one entry: no group segment). The non-resetting
    semantics is what an internal id wants: deleting the highest id must not make
    the next one reuse it. Landed on both: `LogRow` declares `HighWater(seq)`
    (`seq` is a KEY field — okm's two-source rule resolves it) and `CodeRow`
    declares `HighWater(code_id)`; each store reads the watermark once at open
    instead of scanning keys. The log test proves the point the scan got wrong:
    trim the table back to one row, reopen, and the counter continues past the
    number the trim deleted. Gates: log 10/0, 117/0 total, clippy 0.
- [x] Phase 3 — Atomic-fact graph: fact node/edge tables, ingestion of the 8-slot fact tuple (the facts arrive by the model's ACTIVE add — the caller routes that call to `ingest(fact)`; no compression pass and no trailing extraction worker, ADR-0008, write-time fusion (in-domain id / out-of-domain embedding convergence + small-candidate LLM decision — the LLM decision is gravity's, per ADR-0007's boundary), weight system (the counters on the edge row, read-write quadrants, tool_invoke_count); initialization memory (人设/风格) — a traversal from the user node, delivered at the context head; the task graph (task vertices + `task_links` dependency edges) shares that same graph — tasks are the third interface surface, their instance state structured rather than approximate. Write-time resolution (upsert on a new fact) and cognitive binning ride this side per ADR-0010 (fact node / community reverse index / the community sequence; ns 36/37 for the graph, 38 for the sequence). Scope note: append-first covers THIS side (node/edge rows, weights, derived entries) — the log does not (ADR-0002. Flat memory (ns 32) and its HTTP door are withdrawn in this landing: the graph is its successor (ADR-0001's carrier line, "flat memory, then the graph") — flat was the pre-graph form of the same extracted knowledge, and a text row carries no per-fact identity. What is owed with it: the fate of legacy flat rows (ADR-0002's PG migration imports them as-is) is a migration question, not a schema one. (ADR-0001, ADR-0002, ADR-0004, ADR-0005, ADR-0007, ADR-0010)
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
    reader — ADR-0005 records the verdict. (A face is not a one-time ruling: on
    2026-10-20 `by_since` came back for `timeline` and `by_scope_weight` replaced
    the weight-only face for `top_by_weight`, which turned out to be a DECLARED
    face after all — the prediction that it needed a projection table was wrong.
    `by_confidence` and the bare `by_weight` stayed out: still no reader.) Gates: mem-core
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
    `GET /memory/{user_id}/search` are gone (mem-ffi was pointed at `GraphStore`
    at the time; the crate itself is withdrawn — see the next item). The keyspace-sharing test moved to graph + code (different ns, same
    tree, both take id 1 without colliding). Docs swept: ADR-0001's carrier
    line, MODELING's flat section (kept as the historical schema, marked
    withdrawn), SEAM's endpoint table, README. Legacy flat rows stay a
    migration question (ADR-0002's PG import), not a schema one. Gates:
    mem-core 47/0 across its suites, mem-server 20/0, clippy 0.
  - [x] 3e. THE DETERMINISTIC READ AND THE THIRD WRITE VERB LANDED 2026-10-20:
    `GraphStore::fact_in_scope(scope, edge_id)` — the point read that answers with
    the same id the write returned, no query and no embedding, the scope checked
    against the edge's own endpoint (another scope's id is a miss, never a leak) —
    and `GraphStore::withdraw(scope, edge_id)` (okm's `unlink`: the row and every
    derived face entry in ONE batch). Doors: `GET /graph/{scope}/facts/{edge_id}`
    (200 / 404) and `DELETE …/facts/{edge_id}` (204 / 404; a second delete is a
    miss, not an error storm). UPDATE is deliberately NOT a new verb:
    re-ingesting the same tuple IS the update for a functional relation (same
    `edge_id`, row rewritten in place), and a field-level `PATCH` would owe a
    ruling on who may change what and what happens to the weight. Gates:
    http_surface 8/0, 115/0 total, clippy 0.
- [x] Phase 4 — Clustered retrieval: LANDED 2026-10-20 (4a / 4b / 4c below). entity-anchored / temporal / causal-chain views; progressive loading (a scope-wide Top-K by weight — the `by_scope_weight` declared face, `[scope][weight][edge_id]`, maintained as an index entry rather than a projection table; ADR-0005). Reads the write-time structure ADR-0010 lands (the node's community id + the `by_community` face + the rendered community) instead of running a graph algorithm on the read path. (ADR-0005, ADR-0010)
  - [x] 4a. THE WEIGHT RANKING LANDED 2026-10-20: `GraphStore::top_by_weight(scope, k)`
    — the scope's heaviest facts, weight-descending, ties by edge id — and
    `GET /graph/{scope}/top?k=`. This is the progressive-loading face: a caller
    walks "the important part of the scope first" instead of asking a question.
    THE BOUNDED READ LANDED 2026-10-20, and it needed no change in okm: the row
    declares `scope` (a denormalized copy of its source endpoint's scope, written
    once) and a `by_scope_weight { fields(scope, weight) }` face, and the read
    probes that face through okm's public `scan_index` (with `Graph::store()`
    handing out the engine) — so it takes this scope's keys in weight order, stops
    at k, and decodes only those k rows: O(k rows) + O(scope keys), not O(scope
    rows). Reversal puts ties on edge id DESCENDING, so the k rows are re-sorted
    into the documented (weight desc, edge id asc) order — k rows, never the scope.
    Still open, and genuinely an okm addition: a LAZY walk
    (`Collection::scan_range_iter`'s DoubleEnded behaviour forwarded by the graph
    wrapper) — it would save the key scan itself, which is the cheap half. Gates: graph 26/0 (the ranking + scope
    isolation + `k=0`), http_surface 7/0 (the touched fact ranks first, `k` bounds
    the answer, an empty scope answers `[]`), 114/0 total, clippy 0.
  - [x] 4b. THE THREE ENTRANCES LANDED 2026-10-20 — each reads the write-time
    structure instead of asking a question, which is the gap `recall` left: a
    memory answer was only reachable by a vector landing.
    - entity-anchored: `GraphStore::anchored(scope, node_id)` — the facts that
      touch the node (both directions, from the node rows' ref lists, so the cost
      follows its DEGREE) first, then the rest of its `community_id` slice from
      the `by_community` face, heaviest first. A node with `community_id == 0`
      answers its neighbourhood only: 0 means "not binned yet", not "community
      zero". Dorr: `GET /graph/{scope}/entities/{node_id}/anchored` (404 for an
      unknown node).
    - temporal: `GraphStore::timeline(scope, limit)` — the scope's facts in the
      order they were LEARNED (`since`, newest first), walked over the existing
      `by_since` face (reverse order, rows decoded only for the keys it keeps).
      The face is scope-BLIND, so the walk skips other scopes' keys — a row read
      per skipped key until `limit` of this scope are found. A `by_scope_since`
      face would remove the skip; it is the same declaration shape as
      `by_scope_weight` and is worth declaring only when a scope carries enough
      facts for the skip to matter. Door:
      `GET /graph/{scope}/timeline?k=`.
    - causal: `GraphStore::chain(scope, from, kind, max_hops)` — follow ONE named
      predicate hop by hop, breadth-first, cycle-safe by node id, bounded by
      `max_hops`. The answer's order IS the walk (`depth`), which is what makes
      "A leads to B leads to C" readable as a sequence. Door:
      `GET /graph/{scope}/chain?from=&kind=&hops=` (no default `kind`: a chain over
      whatever edges point onward is not a chain).
    Gates: graph 28/0 (anchor's facts lead, a cycle closes without re-expanding,
    hop bound, one-predicate-only, `since` order, `k=0`), http_surface 10/0,
    122/0 total, clippy 0.
  - [x] 4c. REFERENCE LINKS LANDED 2026-10-20: a fact names another fact's id in
    its props under `ref` — ONE id (`{"ref": 41}`) or a list (`{"ref": [41,57]}`) —
    and `FactView.refs` carries the result, so every answer that shows a fact also
    shows what it points at. The renderers print the pointer as an ADDRESS
    (`… · refs=#41,#57`) rather than inlining the target: an id is a place the
    reader may go (`GET /graph/{scope}/facts/{id}`), never a subtree the writer
    forces in. The head keeps its names-not-ids rule for IDENTITY and still shows
    the pointer, because an address without the id would be useless.
    Deliberately unvalidated: the target may be missing (a dangling link is the
    reader's 404, not a write error) and a value that is not an id is not a
    reference at all (`refs_of` drops it) — a writer's mistake must not become a
    phantom target. Cost, stated rather than hidden: one extra point read per shown
    fact (the props read), which is what makes the pointer visible in the answer.
    Gates: graph 27/0 (one id, a list kept in order with a dangling entry, no
    `ref`, a non-id ref), render 8/0 (the tail appears only when there are refs),
    http_surface 9/0 (`refs` rides the DTO and the address resolves), 120/0 total,
    clippy 0.
- [x] Phase 4.5 — The user node (ADR-0014): LANDED 2026-10-21. The members table (ns 42) is in the fact graph's member set — one type name in `#[ok_nodes(EntityRow, MemberRow)]`, the enum and its ns list generated by the derive (`FactNodes { EntityRow, MemberRow }`, `&[36, 42]`) and `member-{channel}-{member}` is the WIRE reference a `user`-typed term carries: `ingest` parses it into a `NodeRef` at ns 42 and REFUSES a malformed or unregistered one (`IngestError::Refused`) — nothing is created, because a person is registered, never discovered. The write side carries an `Endpoint` enum (`Entity` | `User`), so a fact's batch stays inside the `memory` keyspace while the member row is one point read on the channel handle (`GraphStore::with_channel`, attached by `open_service`); `resolve_ref` answers a `NodeId` (`Entity { scope, node_id }` | `User { channel, member }`) and a person renders from the row's display name; the binning pass takes entity endpoints only, so a user never enters a community and never reaches the landing index. User-anchored traversal: `user_ref` + `out_edges_of` / `in_edges_of` / `typed_out_of`. Gates: graph 33/0 (2 new), every suite green, clippy 0. Owed: the fate of any pre-existing `member-…` entity rows (a migration question — none exist in a fresh store).
- [x] Phase 4.6 — The kind field's vocabulary (okm ADR-0032): LANDED 2026-10-21. `EntityRow` declares `kind: Vocab<u16>` — `put` interns the label, `get` resolves it back through that field's OWN data vocabulary, `by_kind { fields(kind) }` indexes it — so the hand-rolled `kind_id: u16` interned through the collection's `PREDICATE_VOCAB` retires with `dict_id` / `dict_name` / `entity_kind`. The field is named `kind` because the value IS the label; the wire stayed two bytes and the layout is version 3. Callers read `row.kind.as_str()` (mem-server's `EntityDto` and the landing table carry the label, never an id). Gates: graph 33/0, render 8/0, every suite green, clippy 0.
- [ ] Phase 5+ — Deferred (do not start early): dreaming consolidation (petgraph community detection + human-reviewed op set), SKILL topological four-part nodes, Aura Actor nodes. (mem-server left this list 2026-10-10 — it is the single cross-process form, ADR-0007 as revised.) ADR-0010 takes the incremental LOCAL form of community detection onto the write side and leaves OPEN whether that promotes a slice of Phase 5 ahead of Phase 3/4 — an open ruling, not settled.

Deferred gates:

- Multi-consumer service shell: mem-server is the single cross-process
  form (ADR-0007 as revised 2026-10-10); it graduates from deferred to
  deliverable with the channel-log endpoints (append / fetch_full /
  append_checkpoint) first, grown as consumers need; core is
  transport-free so the server is additive.
- Delta-append weight counts: single-process serial start uses direct RMW;
  key layout stays delta-compatible so multi-agent switchover changes no schema.
- Backup export: LANDED (ADR-0013). **2026-10-21 revision** — the door is addressed
  by TABLE NAME (`GET /export/{table}`), because an ns is the store's own addressing
  and means nothing to whoever opens the file. Names are declared where the ns is
  declared: okm `#[ok_name("...")]` → `Document::TABLE_NAME` (okm ADR-0034 §1), so
  k10r keeps no second list. `GET /export` is the catalog, `(table, ns)`, generated
  from the declarations at request time — analysis does not read it, an import does.
  Format is still okm's eager bridge reading the ROW DECLARATION (typed columns,
  `Vocab` as labels, `parquet` feature on): one table, one file, `Content-Disposition:
  <table>.parquet`; an empty table is a valid empty file; an unknown name is a 404
  pointing at the catalog. Two things the first cut missed and now carry:
  the dynamic segment as a `variant` column (`export_parquet_with_variant`), and the
  FACT table — 501 no longer, it exports as okm's edge table
  (`Graph::to_record_batch_table`): `(src_table, src_pkey, dst_table, dst_pkey,
  predicate)` + the declaration's fields + a props variant, endpoint keys raw.
  `log__reduces` / `code__reduces` are the derived reduce tables
  (`(table, reduce, group, value)`), because a restore must replay the watermark
  before it allows another write. Owed: restore itself (`import_parquet` is a
  separate door with its own rulings — target table, replace vs merge, an unknown
  `Vocab` label, pushing the watermark). Gates: 129/0 (export_surface 6 — catalog
  lists eight names, a fact exports with endpoints and props, the reduce table
  carries the watermark, a real Parquet reader opens every body).
- mem-ffi WITHDRAWN 2026-10-21 (`crates/mem-ffi/`, PyO3 bindings, `pyo3` +
  `maturin` out of the workspace). ADR-0007 §3 kept it on two claims — "it has
  existing bindings" and "it remains the Phase 0 binding smoke test" — and neither
  held: no repo consumed it, the crate had no tests, and gravity's Phase 0 links
  `mem-core` directly. Adding tests could not have rescued it: the binding attaches
  no embedding engine, so `retrieve` / `recall` answered EMPTY on that channel, and
  a per-language binding multiplies by host language while HTTP does not. A second
  shape of the same core that silently degrades is worse than no second shape.
  Cross-process consumption is HTTP (`krystallizer --path …`), same-process
  consumption is linking `mem-core`. Two crates remain: mem-core, mem-server.
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
- Boot from the config file LANDED 2026-10-21: the service's parameters are ONE
  file, `krystallizer.kdl`, read from the directory of the BINARY (canonicalized, so
  a supervisor's symlink still finds it) unless `--config` names another; `--path` is
  withdrawn — the database's directory is `storage.data-dir` in the file. Path values
  may carry `$HOME` (the ONLY placeholder; `$HOMELESS` and a non-leading `$HOME` stay
  literal, and an unset `HOME` is a boot error rather than a directory named
  `$HOME`), which also took the shipped `embedding.model-dir` off this machine's
  absolute path. `mem_server::open_from_config` is the deployment's entry and the one
  assembly path (`build`): it resolves the data dir, ATTACHES the embedding engine
  (no artifact = no boot, never rows with empty vectors), and hands the compression
  numbers and `logs.max-rows` to the stores. Boot line prints the bound address plus
  the config and data paths. Gates: 134/0 (mem-core config 8 + boot 3 — the config
  beside the binary is the one read, `--config` wins and `$HOME` expands against the
  CHILD's environment, a missing config fails the boot naming the file), clippy 0.
  Noted, not built at the time: a REMOTE embedding mode (an external compute node)
  was a reserved direction. **It LANDED 2026-10-21** as `mode "remote"` in the same
  node: an OpenAI-shaped service (`POST {base-url}/embeddings`), `base-url` /
  `model` / `api-key-env` (a VARIABLE name) / `timeout-ms`. The seam is a trait
  (`mem_core::Embedder`) with `LocalEmbedder` as one implementation; the client
  (`mem_server::embedding::RemoteEmbedder`) lives in the shell, so mem-core still
  owns no transport. Both modes are proven at boot (missing artifact / unreachable
  service / width ≠ `dimensions` = start failure), the remote answer is L2-normalized
  locally, requests are serialized on one client thread (the store's single-writer
  discipline), and a width mismatch is refused rather than truncated. The node is
  TAGGED (`mode` child): the decode picks the variant and that variant's struct IS the
  field set, so a field of the other mode is an unknown child at that line, and the
  hand-kept list of stray names is gone. The client
  lives on its own thread because `reqwest::blocking` builds a runtime that panics
  when dropped inside tokio's. Gates: 142/0 (remote 6 — the OpenAI shape on the wire,
  no header without a key, wrong width and non-2xx as boot failures, the key variable
  named when unset, and a fact ingested through the door retrievable by landing),
  clippy 0. ADR-0003 records the mode and ADR-0007 the one outbound call the SHELL
  may now make.
