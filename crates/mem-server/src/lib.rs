//! axum service shell over mem-core — the single cross-process form
//! (ADR-0007 as revised 2026-10-10: HTTP is the ONLY remote surface;
//! in-process mem-core linking is a local deployment detail, not a
//! second access mode).
//!
//! Endpoints mirror `ChannelStore`'s primitives (channel log),
//! `CodeStore`'s (the code store) and `GraphStore`'s (the fact graph)
//! exactly; wire types restate the stores' rows for serde (the core
//! stays wire-free).
//! Ordering discipline carries over: the CALLER owns the clock —
//! `ts_ms` and `sub` ride the request (ADR-0008 §3, gravity's clock is
//! the order authority; the server never assigns).

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use mem_core::channel::{ChannelStore, MessageKey};
use mem_core::code::{CodeRow, CodeStore};
use mem_core::graph::{FactTuple, GraphStore, IngestError, IngestOutcome};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// Shared server state: one keyspace, one writer domain. The store is
/// behind a tokio mutex — mem-core's append/checkpoint take `&mut self`
/// and the channel log is a single-writer table by design (gravity's
/// clock); serializing through one lock matches that discipline instead
/// of adding a sync layer to the core.
pub struct MemState {
    store: tokio::sync::Mutex<ChannelStore>,
    /// The code group — the `memory` keyspace, shared with the fact
    /// graph (different ns).
    code: tokio::sync::Mutex<CodeStore>,
    /// The fact graph (ns 36 entities / 37 facts) — the `memory`
    /// keyspace. The active memory add lands here (ADR-0008: the facts
    /// come from the model's own add, the caller routes the tuple to
    /// `ingest(fact)`). The built-in embedding engine (ADR-0003) is
    /// attached to the graph in `open_with`, so node vectors are computed
    /// in-process at ingest and the client never sends one; a server
    /// opened without an engine lands empty vector frames instead (a legal
    /// row, which is what the storage tests take).
    graph: tokio::sync::Mutex<GraphStore>,
    /// The compression policy's numbers (ADR-0008 §4). The engine is
    /// pure, so the parameters are the whole of its deployment state.
    compression: mem_core::CompressionParams,
    /// The structured log (ns 41, the `channel` keyspace): k10r's own
    /// diagnostics — the compression verdict's row is its first writer
    /// (ADR-0011). No HTTP door reads it.
    log: tokio::sync::Mutex<mem_core::LogStore>,
}

/// Open the database at `path` and build the router WITHOUT an embedding
/// engine — nodes then carry no vector. This is the shape the
/// storage-level tests use; a serving deployment wants `open_with`.
pub fn open(path: PathBuf) -> Result<Router, fjall::Error> {
    open_with(path, None)
}

/// Open the database at `path` and build the router, optionally with an
/// embedding engine attached. The compression numbers are the config's
/// defaults (`CompressionParams::default()`); a deployment that loaded
/// `config/krystallizer.kdl` passes them through `open_with_compression`.
///
/// One database per process (one directory, one lock file): mem-core's
/// service assembly attaches every table group to it, so mem-server holds
/// one handle and one lock per keyspace.
pub fn open_with(
    path: PathBuf,
    embedder: Option<Arc<mem_core::Embedder>>,
) -> Result<Router, fjall::Error> {
    open_with_compression(path, embedder, mem_core::CompressionParams::default())
}

/// `open_with`, with the compression policy's numbers stated (ADR-0008
/// §4: every threshold is a parameter, so the serving process hands the
/// engine the ones its config file carries).
pub fn open_with_compression(
    path: PathBuf,
    embedder: Option<Arc<mem_core::Embedder>>,
    compression: mem_core::CompressionParams,
) -> Result<Router, fjall::Error> {
    let stores = mem_core::open_service(&path)?;
    let graph = match &embedder {
        // The serving process attaches its engine to the graph too: node
        // vectors are computed in-process at ingest (ADR-0003), so the HTTP
        // client never sends one.
        Some(engine) => stores.graph.with_embedder(engine.clone()),
        None => stores.graph,
    };
    let state = Arc::new(MemState {
        store: tokio::sync::Mutex::new(stores.channels),
        code: tokio::sync::Mutex::new(stores.code),
        graph: tokio::sync::Mutex::new(graph),
        log: tokio::sync::Mutex::new(stores.log),
        compression,
    });
    Ok(Router::new()
        .route("/channels/{channel_id}/messages", post(append_message).get(read_channel))
        .route(
            "/channels/{channel_id}/members/{member_id}/session",
            get(fetch_session),
        )
        .route(
            "/channels/{channel_id}/members/{member_id}/checkpoint",
            post(append_checkpoint),
        )
        .route("/code", post(put_code))
        .route("/code/{code_id}", get(fetch_code))
        .route("/memories/{scope}", post(ingest_fact))
        .route("/graph/{scope}/search", get(search_facts))
        .route("/graph/{scope}/top", get(top_facts))
        .route(
            "/graph/{scope}/entities/{node_id}/anchored",
            get(anchored_entity),
        )
        .route("/graph/{scope}/timeline", get(timeline_facts))
        .route("/graph/{scope}/chain", get(chain_facts))
        .route(
            "/graph/{scope}/facts/{edge_id}",
            get(fetch_fact).delete(withdraw_fact),
        )
        .route("/memories/{scope}/recall", get(recall_facts))
        .route("/memories/{scope}/edges/{edge_id}/touch", post(touch_fact))
        .route("/memories/{scope}/entities/{name}", get(fetch_entity))
        .route(
            "/graph/{scope}/entities/{node_id}/edges",
            get(list_entity_edges),
        )
        .route("/graph/{scope}", get(fetch_graph))
        .with_state(state))
}

/// Request body of `POST /channels/{id}/messages` — the `append`
/// parameters minus `channel_id` (path).
#[derive(Deserialize, Serialize)]
pub struct AppendMessage {
    pub ts_ms: u64,
    pub sub: u16,
    pub role: u8,
    pub kind: u8,
    pub content: String,
}

/// `POST /channels/{id}/messages` — append one log entry; returns the
/// full key so the caller's delta record can replay idempotently.
async fn append_message(
    State(state): State<Arc<MemState>>,
    Path(channel_id): Path<u64>,
    Json(body): Json<AppendMessage>,
) -> Json<MessageKeyDto> {
    let mut store = state.store.lock().await;
    let key = store.append(
        channel_id,
        body.ts_ms,
        body.sub,
        body.role,
        body.kind,
        &body.content,
    );
    Json(key.into())
}

/// The key a message lives under, split into fields (the client needs
/// `ts_ms`/`sub` back for its delta record).
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct MessageKeyDto {
    pub channel_id: u64,
    pub ts_ms: u64,
    pub sub: u16,
}

impl From<MessageKey> for MessageKeyDto {
    fn from(k: MessageKey) -> Self {
        Self {
            channel_id: k.channel_id,
            ts_ms: k.ts_ms,
            sub: k.sub,
        }
    }
}

/// One increment entry: the message, the key it lives under, and the LLM
/// role name the view layer assigned it (ADR-0008 §2 — the mapping from
/// the stored code is k10r's, not the caller's).
#[derive(Serialize)]
pub struct IncrementEntry {
    pub key: MessageKeyDto,
    /// `user` / `assistant` / `tool` / `system`; null for a code the store
    /// never polices (a writer's bug, surfaced rather than guessed at).
    pub role: Option<String>,
    pub kind: u8,
    pub content: String,
}

/// One conversation turn (ADR-0009 §4): a run of user messages BETWEEN two
/// replies is one thing said, so the model gets one turn with per-line
/// attribution rather than a flat record list.
#[derive(Serialize)]
pub struct TurnDto {
    pub role: &'static str,
    pub text: String,
    pub start_ts: u64,
    /// When the turn ended: a reply keeps the clock it finished on, so a line
    /// typed mid-generation is visible inside its span.
    pub end_ts: u64,
    /// The member ids this turn speaks for, in line order.
    pub senders: Vec<u64>,
}

impl From<mem_core::view::Turn> for TurnDto {
    fn from(turn: mem_core::view::Turn) -> Self {
        Self {
            role: turn.role,
            text: turn.text,
            start_ts: turn.start_ts,
            end_ts: turn.end_ts,
            senders: turn.senders,
        }
    }
}

/// One raw record of the channel — the PEOPLE's view (ADR-0009 §4): every row
/// as written, attributed by name but otherwise untouched. No checkpoint, no
/// summary, no merging.
#[derive(Serialize)]
pub struct ChannelEntry {
    pub key: MessageKeyDto,
    /// The member id the row was written with (never absent).
    pub sender: u64,
    /// Its display name at read time; a rename re-labels history here.
    pub sender_name: String,
    pub role: Option<String>,
    pub kind: u8,
    /// The record's own clock (`0` = instant; a reply keeps its END here).
    pub end_ts_ms: u64,
    pub content: String,
}

/// The head's representation follows the requested format (ADR-0010 §5):
/// rendered text under every text format, the facts themselves under
/// `struct` — the consumer that assembles the answer asks for the data.
#[derive(Serialize)]
#[serde(untagged)]
pub enum HeadDto {
    Text(String),
    Facts(Vec<FactViewDto>),
}

/// The compression verdict as the caller receives it (ADR-0008 §4): the
/// record to summarize UP TO, and the tail prompt to append to that span.
/// It rides the projection as a CONTROL FIELD beside the records — never
/// as a marker written into a log row.
#[derive(Serialize)]
pub struct CompressDto {
    /// The span's last record. The summary that comes back covers
    /// everything at or before it: k10r derives the checkpoint's
    /// coverage point from that, so the caller never states one.
    pub checkpoint: u64,
    pub prompt: &'static str,
}

/// Wire shape of the assembled projection (ADR-0008 §2): head first, then
/// the compression point, then the increment. Structure, not a rendered
/// prompt — except the head, which k10r renders in the requested format.
#[derive(Serialize)]
pub struct SessionDto {
    /// Echoed: which representation this answer carries.
    pub format: &'static str,
    /// Initialization memory (ADR-0001): the facts touching the user
    /// node, weight-descending. Empty when the node has no facts yet.
    pub head: HeadDto,
    pub coverage_ts: u64,
    /// The pointer's same-millisecond tiebreaker (ADR-0008 §2): a coverage
    /// point names a RECORD, so `ts` alone is not the whole position. Additive
    /// on the wire — a decoder that only knows `coverage_ts` still works.
    pub coverage_sub: u16,
    pub summary: Option<String>,
    /// A newer node without a summary sits above the coverage point: a
    /// compression is in flight, and a second pass over the same span would be
    /// a wasted model call. The answer above is still complete — the read falls
    /// back to the newest node that HAS a summary.
    pub compressing: bool,
    pub increment: Vec<IncrementEntry>,
    /// The same records as TURNS (ADR-0009 §4) — what the model actually reads:
    /// user bursts merged, every line `[name(id=N)]`. The raw `increment` stays
    /// beside it for callers that want the records themselves.
    pub turns: Vec<TurnDto>,
    /// The policy's verdict when one fired (ADR-0008 §4). Absent = the
    /// caller builds its context from the records alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compress: Option<CompressDto>,
}

impl SessionDto {
    fn new(
        p: mem_core::view::Projection,
        format: mem_core::RenderFormat,
        compress: Option<mem_core::CompressionVerdict>,
        turns: Vec<TurnDto>,
    ) -> Self {
        let head = if format.renders_text() {
            HeadDto::Text(mem_core::render::render_head(&p.head, format))
        } else {
            HeadDto::Facts(p.head.into_iter().map(FactViewDto::from).collect())
        };
        Self {
            format: format.name(),
            head,
            coverage_ts: p.view.coverage_ts,
            coverage_sub: p.view.coverage_sub,
            summary: p.view.summary,
            compressing: p.view.compressing,
            turns,
            increment: p
                .view
                .increment
                .into_iter()
                .map(|(key, row)| IncrementEntry {
                    key: key.into(),
                    role: mem_core::view::role_name(row.role).map(str::to_string),
                    kind: row.kind,
                    content: row.content,
                })
                .collect(),
            compress: compress.map(|v| CompressDto {
                checkpoint: v.checkpoint_ts,
                prompt: mem_core::TAIL_PROMPT,
            }),
        }
    }
}

/// `GET /channels/{c}/members/{m}/session?format=&ts_ms=` — the answer's
/// representation (ADR-0010 §5; absent = `plaintext`) and, optionally,
/// the caller's clock for this read.
#[derive(Deserialize)]
pub struct SessionQuery {
    pub format: Option<String>,
    /// ADR-0008 §3's rule carried onto the read side: the CALLER owns the
    /// clock, so no clock means the read is served with nothing stamped —
    /// the read cursor does not move and the cache clock stays silent
    /// (the same discipline as recall's "no clock, no count").
    pub ts_ms: Option<u64>,
}

/// Parse the caller's `format` value. Absent is the default
/// (`plaintext`); an unknown value is the caller's bug (400), never a
/// silent fallback — a typo must not quietly change the representation.
fn parse_format(raw: Option<&str>) -> Result<mem_core::RenderFormat, FactError> {
    let Some(raw) = raw else {
        return Ok(mem_core::RenderFormat::default());
    };
    mem_core::RenderFormat::parse(raw).ok_or_else(|| FactError {
        error: format!(
            "unknown format `{raw}` — expected one of {}",
            mem_core::RenderFormat::NAMES.join(" / ")
        ),
        status: StatusCode::BAD_REQUEST,
    })
}

/// `GET /channels/{id}/members/{member}/session` — the projection read:
/// the head (initialization memory) + the latest checkpoint + the log
/// increment strictly after its coverage point, assembled in that order
/// by `mem_core::view::assemble`, with the compression policy's verdict
/// beside them (ADR-0008 §4).
///
/// The engine evaluates HERE, and the order inside this handler IS the
/// signal: measure the idle time against the cursor the member had
/// BEFORE this read, stamp the cursor with this read's clock, then
/// evaluate and record the verdict. Reading is the cache clock's only
/// observable, so stamping first would measure zero every time.
///
/// The graph and the channel locks are taken in ONE order (channel, then
/// graph) — both are needed to answer this door.
async fn fetch_session(
    State(state): State<Arc<MemState>>,
    Path((channel_id, member_id)): Path<(u64, u64)>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<SessionDto>, FactError> {
    let format = parse_format(query.format.as_deref())?;
    let mut channels = state.store.lock().await;
    let mut graph = state.graph.lock().await;
    let projection = mem_core::view::assemble(
        &mut channels,
        &mut graph,
        channel_id,
        member_id,
        mem_core::view::DEFAULT_SCOPE,
    );
    let assembled = assembled_bytes(&projection);
    // The clock is the caller's (`query.ts_ms`); the POSITION is k10r's. The
    // idle therefore measures how stale the record this member last saw has
    // become, not how long ago the HTTP call happened (ADR-0008 §2).
    let idle_ms = query
        .ts_ms
        .zip(projection.view.read_at)
        .map(|(now, read_at)| now.saturating_sub(read_at));
    // Only the clock is stamped: the model's read always reaches the log's end,
    // so a stored POSITION would be a constant (ADR-0008 §2).
    if let Some(clock) = query.ts_ms {
        channels.note_read(channel_id, member_id, clock);
    }
    // The head this read assembled is what the coming compression will have
    // been produced under, so the proposal freezes it NOW (ADR-0008 §2: the
    // checkpoint carries the persona, not a re-computation of it).
    let head_ids: Vec<u64> = projection.head.iter().map(|fact| fact.edge_id).collect();
    let verdict =
        mem_core::compress::evaluate(&projection.view, assembled, idle_ms, &state.compression);
    if let Some(v) = &verdict {
        channels.open_checkpoint(channel_id, (v.checkpoint_ts, v.checkpoint_sub), &head_ids);
    }
    // ADR-0011's first writer: the verdict is a decision, and decisions go to the
    // diagnostic log — naming the split it chose, the trigger that fired and the
    // numbers it read. The row's clock is K10R's own.
    if let Some(verdict) = &verdict {
        state
            .log
            .lock()
            .await
            .record_verdict(now_ms(), channel_id, verdict);
    }
    let turns: Vec<TurnDto> =
        mem_core::view::turns_of(&channels, channel_id, &projection.view.increment)
            .into_iter()
            .map(TurnDto::from)
            .collect();
    Ok(Json(SessionDto::new(projection, format, verdict, turns)))
}

/// k10r's own clock, milliseconds since the epoch — the log's key (ADR-0011: a
/// diagnostic row records when K10R observed the event, not when the caller
/// believes it happened).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// The PEOPLE's view (ADR-0009 §4): the channel as written — every record, in log
/// order, no checkpoint, no summary, no merging. Attribution is a name lookup, so
/// a rename re-labels history without touching a row.
async fn read_channel(
    State(state): State<Arc<MemState>>,
    Path(channel_id): Path<u64>,
) -> Json<Vec<ChannelEntry>> {
    let mut store = state.store.lock().await;
    let rows = store.history(channel_id);
    Json(
        rows.into_iter()
            .map(|(key, row)| ChannelEntry {
                key: MessageKeyDto::from(key),
                sender_name: store
                    .member(channel_id, row.sender)
                    .map(|member| member.display_name)
                    .unwrap_or_else(|| format!("member-{}", row.sender)),
                sender: row.sender,
                role: mem_core::view::role_name(row.role).map(str::to_string),
                kind: row.kind,
                end_ts_ms: row.end_ts_ms,
                content: row.content,
            })
            .collect(),
    )
}

/// The assembled context's size in bytes — what the budget trigger
/// measures. The head is counted in `plaintext` whatever `?format=`
/// asked for: the budget is about how much conversation the caller's
/// window holds, and a rendering's own markup is second-order against
/// the conversation it wraps.
fn assembled_bytes(p: &mem_core::view::Projection) -> usize {
    let head = mem_core::render::render_head(&p.head, mem_core::RenderFormat::Plaintext);
    head.len()
        + p.view.summary.as_deref().map_or(0, str::len)
        + p.view
            .increment
            .iter()
            .map(|(_, row)| row.content.len())
            .sum::<usize>()
}

/// Request body of `POST .../checkpoint` — the summary itself.
///
/// No coverage timestamp: ADR-0008 §4 has k10r derive it from the split it
/// served, so the caller never computes or states one. The store's
/// `append_checkpoint_from_verdict` is the only path that lands a summary.
#[derive(Deserialize, Serialize)]
pub struct AppendCheckpoint {
    pub summary: String,
}

/// `POST /channels/{id}/members/{member}/checkpoint` — land the summary for
/// the compression verdict k10r served: the checkpoint takes that verdict's
/// split as its coverage point and the decision closes in the same batch.
///
/// 400 when no verdict is open. A summary with nothing to land on is the
/// caller's bug (the same class as a malformed fact tuple), and inventing a
/// coverage point here would make k10r guess what it is supposed to own.
async fn append_checkpoint(
    State(state): State<Arc<MemState>>,
    Path((channel_id, _member_id)): Path<(u64, u64)>,
    Json(body): Json<AppendCheckpoint>,
) -> Result<StatusCode, FactError> {
    let mut store = state.store.lock().await;
    store
        .complete_checkpoint(channel_id, &body.summary)
        .map(|_pointer| StatusCode::NO_CONTENT)
        .ok_or_else(|| FactError {
            error: "no open compression node for this channel: the pointer comes from the \
                    verdict k10r served, so read the session first"
                .into(),
            status: StatusCode::BAD_REQUEST,
        })
}

// ---- the code store (gravity ADR-0001/0003) --------------------------
//
// The delivered script's home: gravity writes a script once (the
// authoring act) and re-reads it on every call, so the read goes to the
// row's identity — no name, no content hash (code identity = the id).

/// Request body of `POST /code` — one piece of code: the language it is
/// written in, what it is (description, parameter definition) and its
/// source. The language is required data, never defaulted; the
/// description and the parameter definition are what a skill-to-code
/// link is decided from without reading the source.
#[derive(Deserialize, Serialize)]
pub struct PutCode {
    pub language: String,
    pub description: String,
    /// Parameter definition as a JSON document in TEXT (the store does
    /// not parse it; the caller that delivered the code owns its shape).
    pub parameters: String,
    pub source: String,
}

/// The assigned identity, returned by `POST /code` (the caller's record).
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct CodeIdDto {
    pub code_id: u64,
}

/// Wire restatement of `CodeRow` (the core's row has no serde).
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct CodeRowDto {
    pub language: String,
    pub description: String,
    pub parameters: String,
    pub source: String,
}

impl From<CodeRow> for CodeRowDto {
    fn from(r: CodeRow) -> Self {
        Self {
            language: r.language,
            description: r.description,
            parameters: r.parameters,
            source: r.source,
        }
    }
}


/// `POST /code` — store one piece of code; the store assigns the id.
async fn put_code(
    State(state): State<Arc<MemState>>,
    Json(body): Json<PutCode>,
) -> Json<CodeIdDto> {
    let mut code = state.code.lock().await;
    Json(CodeIdDto {
        code_id: code.put(
            &body.language,
            &body.description,
            &body.parameters,
            &body.source,
        ),
    })
}

/// `GET /code/{code_id}` — the script gravity reads at call time.
/// Unknown id = 404 (the caller turns it into an error value for the
/// model; the store has no opinion).
async fn fetch_code(
    State(state): State<Arc<MemState>>,
    Path(code_id): Path<u64>,
) -> Result<Json<CodeRowDto>, StatusCode> {
    let code = state.code.lock().await;
    code.get(code_id)
        .map(|r| Json(r.into()))
        .ok_or(StatusCode::NOT_FOUND)
}


// ---- the fact graph (ns 36 entities / 37 facts) ----------------------
//
// The active memory add's door: the model emits a fact in-turn and the
// caller routes that tuple to `ingest(fact)` (ADR-0008). The tuple
// travels VERBATIM — the canonical 8-slot JSON list is the request body,
// so the caller pastes the form the wiki/PROMPTS state and nothing
// restates it. `scope` (the per-user corpus boundary) rides the path.
// Ids and resolution are the store's (ADR-0010 §1); the caller owns the
// clock inside the tuple's own props.

/// What one ingest did: the three-way outcome of ADR-0010 §1, so the
/// caller can tell a landed fact from an adjudicated loss.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct IngestOutcomeDto {
    /// `linked` (a new edge) | `rewritten` (the incumbent's row, same
    /// `edge_id`) | `dropped` (the incumbent won).
    pub outcome: String,
    pub edge_id: u64,
}

impl From<IngestOutcome> for IngestOutcomeDto {
    fn from(o: IngestOutcome) -> Self {
        let (outcome, edge_id) = match o {
            IngestOutcome::Linked { edge_id } => ("linked", edge_id),
            IngestOutcome::Rewritten { edge_id } => ("rewritten", edge_id),
            IngestOutcome::Dropped { edge_id } => ("dropped", edge_id),
        };
        Self {
            outcome: outcome.to_string(),
            edge_id,
        }
    }
}

/// A refused fact: the caller's structural bug (400 — the tuple's arity
/// or a slot's shape) or an engine failure (500). The core's error text
/// travels verbatim, never a reworded summary.
#[derive(Serialize)]
pub struct FactError {
    pub error: String,
    #[serde(skip)]
    status: StatusCode,
}

impl From<IngestError> for FactError {
    fn from(e: IngestError) -> Self {
        let status = match e {
            IngestError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_REQUEST,
        };
        Self {
            error: e.to_string(),
            status,
        }
    }
}

impl IntoResponse for FactError {
    fn into_response(self) -> axum::response::Response {
        (self.status, Json(self)).into_response()
    }
}

/// `POST /memories/{scope}` — land one fact; the body IS the 8-slot tuple.
async fn ingest_fact(
    State(state): State<Arc<MemState>>,
    Path(scope): Path<u64>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<IngestOutcomeDto>, FactError> {
    let fact = FactTuple::from_value(&body).map_err(FactError::from)?;
    let mut graph = state.graph.lock().await;
    graph
        .ingest(scope, &fact)
        .map(|outcome| Json(outcome.into()))
        .map_err(FactError::from)
}

/// One node as the read face returns it.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct EntityDto {
    pub node_id: u64,
    pub kind: String,
    pub literal_num: i64,
    pub name: String,
}

/// One landing seed (`GET /graph/{scope}/search`): the node the query landed
/// on and its similarity. The caller expands from here (ADR-0003: vector
/// landing + graph traversal); an EMPTY list is the legal "nothing landed".
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct SeedDto {
    pub node_id: u64,
    pub score: f32,
    pub name: String,
    pub kind: String,
}

/// The retrieval door's query: `?q=<text>&k=<n>` — `k` defaults to 10.
#[derive(Deserialize)]
pub struct FactSearchQuery {
    pub q: String,
    #[serde(default = "default_seed_count")]
    pub k: usize,
}

/// `GET /graph/{scope}/chain`'s query: where the chain starts, which predicate it
/// follows, and how many hops are allowed. No default for `kind` — a chain over
/// "whatever edges happen to point onward" is not a chain.
#[derive(Deserialize)]
pub struct ChainQuery {
    pub from: u64,
    pub kind: String,
    pub hops: usize,
}

/// `GET /graph/{scope}/entities/{node_id}/anchored` — one node's memory, with the
/// community it was binned into (`0` = not binned yet).
#[derive(Serialize)]
pub struct AnchoredDto {
    pub node_id: u64,
    pub community: u64,
    pub facts: Vec<FactViewDto>,
}

impl From<mem_core::graph::Anchored> for AnchoredDto {
    fn from(anchored: mem_core::graph::Anchored) -> Self {
        Self {
            node_id: anchored.node_id,
            community: anchored.community,
            facts: anchored.facts.into_iter().map(FactViewDto::from).collect(),
        }
    }
}

/// One hop of a causal chain: the depth it sits at, and the fact that takes it.
#[derive(Serialize)]
pub struct ChainHopDto {
    pub depth: usize,
    pub fact: FactViewDto,
}

impl From<mem_core::graph::ChainHop> for ChainHopDto {
    fn from(hop: mem_core::graph::ChainHop) -> Self {
        Self {
            depth: hop.depth,
            fact: FactViewDto::from(hop.fact),
        }
    }
}

/// `GET /graph/{scope}/top`'s query: how many, and nothing else. A ranking is not
/// a search — no `q`, no clock, no format: the answer is the same shape as
/// recall's facts.
#[derive(Deserialize)]
pub struct TopQuery {
    #[serde(default = "default_seed_count")]
    pub k: usize,
}

/// The recall door's query: the landing query, the seed count, and the
/// CALLER's clock (`ts_ms`). A clock that rides the call makes every served
/// fact count as one read and re-folds its weight; absent, the answer is
/// served but nothing is counted (ADR-0005 / ADR-0008 §3 — k10r never
/// stamps a time of its own).
#[derive(Deserialize)]
pub struct RecallQuery {
    pub q: String,
    #[serde(default = "default_seed_count")]
    pub k: usize,
    pub ts_ms: Option<u64>,
    /// MAX FACTS in the answer (ADR-0010 §5's read budget). The default
    /// keeps the rendered text at a few hundred tokens; `budget=0` lifts it.
    #[serde(default = "default_budget")]
    pub budget: usize,
    /// The answer's representation (ADR-0010 §5); absent = `plaintext`.
    pub format: Option<String>,
}

fn default_seed_count() -> usize {
    10
}

/// The read side's default budget: ~20 fact lines, the few-hundred-token
/// answer ADR-0010 §5 asks for (a fact line is ~15 tokens).
fn default_budget() -> usize {
    20
}

/// One recall (`GET /memories/{scope}/recall`): the landing seeds, the
/// communities they landed in, the facts the answer shows, and the rendered
/// memory text — id-bearing lines, one block per community (ADR-0010 §5).
///
/// ONE representation per answer: the text formats fill `text`, `struct`
/// fills the data fields (`seeds` / `facts`) and carries no text at all.
/// `communities` and the budget accounting ride both.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct RecallDto {
    /// Echoed: which representation this answer carries.
    pub format: &'static str,
    pub communities: Vec<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seeds: Option<Vec<SeedDto>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facts: Option<Vec<FactViewDto>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub shown: usize,
    pub total: usize,
}

/// `GET /graph/{scope}/search?q=&k=` — vector landing over one scope's
/// entities (ADR-0003). Below the landing floor the answer is an empty list,
/// never an error; no embedding engine attached is the same empty list.
async fn search_facts(
    State(state): State<Arc<MemState>>,
    Path(scope): Path<u64>,
    Query(query): Query<FactSearchQuery>,
) -> Result<Json<Vec<SeedDto>>, FactError> {
    let mut graph = state.graph.lock().await;
    graph
        .retrieve(scope, &query.q, query.k)
        .map(|seeds| {
            Json(
                seeds
                    .into_iter()
                    .map(|s| SeedDto {
                        node_id: s.node_id,
                        score: s.score,
                        name: s.name,
                        kind: s.kind,
                    })
                    .collect(),
            )
        })
        .map_err(FactError::from)
}

/// `GET /graph/{scope}/entities/{node_id}/anchored` — the entity-anchored
/// entrance (PLAN 4b): the facts that touch this node, then the rest of its
/// community, heaviest first. Identity instead of similarity — no query, no
/// embedding, no landing.
async fn anchored_entity(
    State(state): State<Arc<MemState>>,
    Path((scope, node_id)): Path<(u64, u64)>,
) -> Result<Json<AnchoredDto>, StatusCode> {
    let mut graph = state.graph.lock().await;
    graph
        .anchored(scope, node_id)
        .map(|anchored| Json(AnchoredDto::from(anchored)))
        .ok_or(StatusCode::NOT_FOUND)
}

/// `GET /graph/{scope}/timeline?k=` — the temporal entrance: the scope's facts in
/// the order they were learned, newest first.
async fn timeline_facts(
    State(state): State<Arc<MemState>>,
    Path(scope): Path<u64>,
    Query(query): Query<TopQuery>,
) -> Json<Vec<FactViewDto>> {
    let mut graph = state.graph.lock().await;
    Json(
        graph
            .timeline(scope, query.k)
            .into_iter()
            .map(FactViewDto::from)
            .collect(),
    )
}

/// `GET /graph/{scope}/chain?from=&kind=&hops=` — the causal entrance: follow ONE
/// predicate from a node, hop by hop. The answer's order IS the walk.
async fn chain_facts(
    State(state): State<Arc<MemState>>,
    Path(scope): Path<u64>,
    Query(query): Query<ChainQuery>,
) -> Json<Vec<ChainHopDto>> {
    let mut graph = state.graph.lock().await;
    Json(
        graph
            .chain(scope, query.from, &query.kind, query.hops)
            .into_iter()
            .map(ChainHopDto::from)
            .collect(),
    )
}

/// One fact BY ITS ID — the deterministic read, the mirror of the write that
/// answered with an `edge_id`: no query, no embedding, no ranking. `404` when the
/// id names nothing in this scope, so a stale reference link is a clean miss.
async fn fetch_fact(
    State(state): State<Arc<MemState>>,
    Path((scope, edge_id)): Path<(u64, u64)>,
) -> Result<Json<FactViewDto>, StatusCode> {
    let mut graph = state.graph.lock().await;
    graph
        .fact_in_scope(scope, edge_id)
        .map(|fact| Json(FactViewDto::from(fact)))
        .ok_or(StatusCode::NOT_FOUND)
}

/// Withdraw one fact — the write side's third verb, beside link and rewrite. The
/// row goes with every derived face entry in one batch (okm's `unlink`), so no
/// stale weight or community entry survives it. `204` on success, `404` when the
/// id names nothing here.
async fn withdraw_fact(
    State(state): State<Arc<MemState>>,
    Path((scope, edge_id)): Path<(u64, u64)>,
) -> Result<StatusCode, (StatusCode, String)> {
    let mut graph = state.graph.lock().await;
    match graph.withdraw(scope, edge_id) {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) => Err((StatusCode::NOT_FOUND, "no such fact".into())),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// `GET /graph/{scope}/top?k=` — Phase 4's progressive read: the heaviest facts
/// of a scope, weight-descending (ties by edge id), as data. No query, no
/// embedding: this is the ordering a caller walks when it wants "the important
/// part of the scope first" rather than the answer to a question.
async fn top_facts(
    State(state): State<Arc<MemState>>,
    Path(scope): Path<u64>,
    Query(query): Query<TopQuery>,
) -> Json<Vec<FactViewDto>> {
    let mut graph = state.graph.lock().await;
    Json(
        graph
            .top_by_weight(scope, query.k)
            .into_iter()
            .map(FactViewDto::from)
            .collect(),
    )
}

/// `GET /memories/{scope}/recall?q=&k=&ts_ms=` — the read shape ADR-0003
/// describes: vector landing plus ONE hop, ordered by weight (ADR-0005),
/// returned together so the caller does not stitch two calls. An empty
/// landing returns no seeds and no facts.
async fn recall_facts(
    State(state): State<Arc<MemState>>,
    Path(scope): Path<u64>,
    Query(query): Query<RecallQuery>,
) -> Result<Json<RecallDto>, FactError> {
    let format = parse_format(query.format.as_deref())?;
    let structured = !format.renders_text();
    let mut graph = state.graph.lock().await;
    graph
        .recall(scope, &query.q, query.k, query.ts_ms, query.budget, format)
        .map(|recall| {
            Json(RecallDto {
                format: format.name(),
                communities: recall.communities,
                seeds: structured.then(|| {
                    recall
                        .seeds
                        .into_iter()
                        .map(|s| SeedDto {
                            node_id: s.node_id,
                            score: s.score,
                            name: s.name,
                            kind: s.kind,
                        })
                        .collect()
                }),
                facts: structured
                    .then(|| recall.facts.into_iter().map(FactViewDto::from).collect()),
                text: recall.text,
                shown: recall.shown,
                total: recall.total,
            })
        })
        .map_err(FactError::from)
}

/// Body of the tool-touch door: the CALLER's clock (ms). k10r cannot see a
/// tool call from inside, so the consumer that used the fact reports it.
#[derive(Deserialize, Serialize)]
pub struct TouchFact {
    pub ts_ms: u64,
}

/// `POST /memories/{scope}/edges/{edge_id}/touch` — count one TOOL consult of
/// a fact (ADR-0005's `tool_invoke_count`, the index that outranks a plain
/// read) and answer the fact's new view. An unknown edge, or one whose
/// source lives in another scope, is a 404: edges carry no scope segment,
/// their endpoints do.
async fn touch_fact(
    State(state): State<Arc<MemState>>,
    Path((scope, edge_id)): Path<(u64, u64)>,
    Json(body): Json<TouchFact>,
) -> axum::response::Response {
    let mut graph = state.graph.lock().await;
    match graph.touch(scope, edge_id, mem_core::Touch::Tool, body.ts_ms) {
        Ok(Some(view)) => Json(FactViewDto::from(view)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => FactError::from(e).into_response(),
    }
}

/// One fact with both endpoints resolved (what a renderer or a recall
/// needs — an edge id alone is not renderable).
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct FactViewDto {
    pub edge_id: u64,
    pub kind: String,
    pub src: EntityDto,
    pub dst: EntityDto,
    pub since: u64,
    pub confidence: u8,
    /// The folded weight (ADR-0005, milli-units) and its inputs.
    pub weight: u32,
    pub read_count: u32,
    pub write_count: u32,
    pub tool_count: u32,
    pub last_read: u64,
    pub last_write: u64,
    /// The fact ids this fact points at (PLAN 4c). Each one is an address a
    /// caller may follow on its own — `GET /graph/{scope}/facts/{id}` — never a
    /// subtree this answer inlined.
    pub refs: Vec<u64>,
}

impl From<mem_core::graph::FactView> for FactViewDto {
    fn from(v: mem_core::graph::FactView) -> Self {
        let node = |n: mem_core::graph::ResolvedNode| EntityDto {
            node_id: n.node_id,
            kind: n.kind,
            literal_num: n.literal_num,
            name: n.name,
        };
        Self {
            edge_id: v.edge_id,
            kind: v.kind,
            src: node(v.src),
            dst: node(v.dst),
            since: v.edge.since,
            confidence: v.edge.confidence,
            weight: v.edge.weight,
            read_count: v.edge.read_count,
            write_count: v.edge.write_count,
            tool_count: v.edge.tool_count,
            last_read: v.edge.last_read,
            last_write: v.edge.last_write,
            refs: v.refs,
        }
    }
}

/// `GET /memories/{scope}/entities/{name}` — the exact name/alias entrance
/// (ADR-0003: the name is an attribute, and this face is how the human
/// identity reaches the store). An unknown name is a 404 — the caller
/// turns it into an error value; an empty RESULT would be a different
/// statement (nothing matched a search), which this door does not make.
async fn fetch_entity(
    State(state): State<Arc<MemState>>,
    Path((scope, name)): Path<(u64, String)>,
) -> Result<Json<EntityDto>, StatusCode> {
    let mut graph = state.graph.lock().await;
    let (node_id, row) = graph
        .entity_by_name(scope, &name)
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(EntityDto {
        node_id,
        kind: graph.entity_kind(row.kind_id).unwrap_or_default(),
        literal_num: row.literal_num,
        name: row.name,
    }))
}

/// `GET /graph/{scope}/entities/{node_id}/edges` — the node's outgoing
/// facts, endpoints resolved: one hop of the traversal, in edge-id order.
/// An empty list is a legal answer (a node with no facts yet).
async fn list_entity_edges(
    State(state): State<Arc<MemState>>,
    Path((scope, node_id)): Path<(u64, u64)>,
) -> Json<Vec<FactViewDto>> {
    let mut graph = state.graph.lock().await;
    Json(
        graph
            .out_facts(scope, node_id)
            .into_iter()
            .map(FactViewDto::from)
            .collect(),
    )
}

/// One node of a rendered graph.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct GraphNodeDto {
    pub id: u64,
    pub name: String,
    pub kind: String,
    pub literal_num: i64,
    /// The node's community (ADR-0010 §2) — the color cluster, and the
    /// reason the binning is inspectable at all.
    pub community_id: u64,
}

/// One link: a fact, with `source` / `target` as node ids (what the client
/// needs) plus the declared fields it may render or ignore.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct GraphLinkDto {
    pub edge_id: u64,
    pub source: u64,
    pub target: u64,
    pub kind: String,
    pub since: u64,
    pub confidence: u8,
    pub weight: u32,
}

/// A whole scope as nodes + links — one read, one scope.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct GraphDto {
    pub nodes: Vec<GraphNodeDto>,
    pub links: Vec<GraphLinkDto>,
}

/// `GET /graph/{scope}` — one scope's graph for a renderer. An empty
/// scope is a legal empty graph, never an error.
async fn fetch_graph(State(state): State<Arc<MemState>>, Path(scope): Path<u64>) -> Json<GraphDto> {
    let mut graph = state.graph.lock().await;
    let nodes: Vec<GraphNodeDto> = graph
        .scope_entities(scope)
        .into_iter()
        .map(|(node_id, row)| GraphNodeDto {
            id: node_id,
            kind: graph.entity_kind(row.kind_id).unwrap_or_default(),
            literal_num: row.literal_num,
            name: row.name,
            community_id: row.community_id,
        })
        .collect();
    let links: Vec<GraphLinkDto> = graph
        .scope_facts(scope)
        .into_iter()
        .map(|fact| GraphLinkDto {
            edge_id: fact.edge_id,
            source: fact.src.node_id,
            target: fact.dst.node_id,
            kind: fact.kind,
            since: fact.edge.since,
            confidence: fact.edge.confidence,
            weight: fact.edge.weight,
        })
        .collect();
    Json(GraphDto { nodes, links })
}

/// The engine's own failures on a door. `FactError` covers the graph's
/// structural (400) and storage (500) cases; this type is the general
/// shell error and is kept for doors that have nothing more specific to
/// say — message passed through verbatim, the original error text never
/// stops here.
#[derive(Serialize)]
pub struct EngineError(pub String);

impl From<fjall::Error> for EngineError {
    fn from(e: fjall::Error) -> Self {
        Self(e.to_string())
    }
}

impl IntoResponse for EngineError {
    fn into_response(self) -> axum::response::Response {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(self)).into_response()
    }
}
