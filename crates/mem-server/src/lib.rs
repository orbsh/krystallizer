//! axum service shell over mem-core — the single cross-process form
//! (ADR-0007 as revised 2026-10-10: HTTP is the ONLY remote surface;
//! in-process mem-core linking is a local deployment detail, not a
//! second access mode).
//!
//! Endpoints mirror `ChannelStore`'s primitives (channel log),
//! `CodeStore`'s (the code store) and `MemoryStore`'s (flat memory)
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
use mem_core::channel::{ChannelStore, MessageKey, MessageRow, SessionView};
use mem_core::code::{CodeRow, CodeStore};
use mem_core::graph::{FactTuple, GraphStore, IngestError, IngestOutcome};
use mem_core::memory::MemoryStore;
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
    /// The code group — the `memory` keyspace, shared with flat memory
    /// (different ns).
    code: tokio::sync::Mutex<CodeStore>,
    /// The flat-memory group — the `memory` keyspace, shared with the
    /// code group (different ns). This is the memory family's tool face:
    /// `commit_memory` stores here, `recall_memory` searches here.
    memory: tokio::sync::Mutex<MemoryStore>,
    /// The fact graph (ns 36 entities / 37 facts) — the `memory`
    /// keyspace. The active memory add lands here (ADR-0008: the facts
    /// come from the model's own add, the caller routes the tuple to
    /// `ingest(fact)`).
    graph: tokio::sync::Mutex<GraphStore>,
    /// The built-in embedding engine (ADR-0003), loaded once per process
    /// and shared: the memory door embeds a row's text in-process before
    /// it lands, so the client never sends a vector. `None` = no engine
    /// attached, and rows then land with an empty vector frame (a legal
    /// row, which is what the storage tests take); a serving process
    /// attaches one through `open_with`.
    embedder: Option<Arc<mem_core::Embedder>>,
}

/// Open the database at `path` and build the router WITHOUT an embedding
/// engine — memory rows then carry no vector. This is the shape the
/// storage-level tests use; a serving deployment wants `open_with`.
pub fn open(path: PathBuf) -> Result<Router, fjall::Error> {
    open_with(path, None)
}

/// Open the database at `path` and build the router, optionally with an
/// embedding engine attached.
///
/// One database per process (one directory, one lock file): mem-core's
/// service assembly attaches every table group to it, so mem-server holds
/// one handle and one lock per keyspace.
pub fn open_with(
    path: PathBuf,
    embedder: Option<Arc<mem_core::Embedder>>,
) -> Result<Router, fjall::Error> {
    let stores = mem_core::open_service(&path)?;
    let state = Arc::new(MemState {
        store: tokio::sync::Mutex::new(stores.channels),
        code: tokio::sync::Mutex::new(stores.code),
        memory: tokio::sync::Mutex::new(stores.memory),
        graph: tokio::sync::Mutex::new(stores.graph),
        embedder,
    });
    Ok(Router::new()
        .route("/channels/{channel_id}/messages", post(append_message))
        .route(
            "/channels/{channel_id}/members/{member_id}/session",
            get(fetch_session),
        )
        .route(
            "/channels/{channel_id}/members/{member_id}/checkpoint",
            post(append_checkpoint),
        )
        .route("/code", post(put_code).get(list_code))
        .route("/code/{code_id}", get(fetch_code))
        .route("/memory", post(store_memory))
        .route("/memory/{user_id}/search", get(search_memory))
        .route("/facts/{scope}", post(ingest_fact))
        .route("/facts/{scope}/entities/{name}", get(fetch_entity))
        .route(
            "/facts/{scope}/entities/{node_id}/edges",
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

/// One increment entry: the message and the key it lives under.
#[derive(Serialize)]
pub struct IncrementEntry {
    pub key: MessageKeyDto,
    pub row: MessageRowDto,
}

/// Wire restatement of `MessageRow` (the core's row has no serde).
#[derive(Serialize)]
pub struct MessageRowDto {
    pub role: u8,
    pub kind: u8,
    pub content: String,
}

impl From<MessageRow> for MessageRowDto {
    fn from(r: MessageRow) -> Self {
        Self {
            role: r.role,
            kind: r.kind,
            content: r.content,
        }
    }
}

/// Wire shape of `SessionView` (checkpoint + increment).
#[derive(Serialize)]
pub struct SessionDto {
    pub coverage_ts: u64,
    pub summary: Option<String>,
    pub increment: Vec<IncrementEntry>,
}

impl From<SessionView> for SessionDto {
    fn from(v: SessionView) -> Self {
        Self {
            coverage_ts: v.coverage_ts,
            summary: v.summary,
            increment: v
                .increment
                .into_iter()
                .map(|(key, row)| IncrementEntry {
                    key: key.into(),
                    row: row.into(),
                })
                .collect(),
        }
    }
}

/// `GET /channels/{id}/members/{member}/session` — the projection read:
/// latest checkpoint + the log increment strictly after its coverage
/// point (the store's `fetch_full`, unchanged).
async fn fetch_session(
    State(state): State<Arc<MemState>>,
    Path((channel_id, member_id)): Path<(u64, u64)>,
) -> Json<SessionDto> {
    let store = state.store.lock().await;
    Json(store.fetch_full(channel_id, member_id).into())
}

/// Request body of `POST .../checkpoint` — `append_checkpoint`'s
/// parameters minus path-carried ids.
#[derive(Deserialize, Serialize)]
pub struct AppendCheckpoint {
    pub coverage_ts: u64,
    pub summary: String,
}

/// `POST /channels/{id}/members/{member}/checkpoint` — record (or
/// advance) the member's checkpoint (same-key overwrite, ADR-0008).
async fn append_checkpoint(
    State(state): State<Arc<MemState>>,
    Path((channel_id, member_id)): Path<(u64, u64)>,
    Json(body): Json<AppendCheckpoint>,
) -> StatusCode {
    let mut store = state.store.lock().await;
    store.append_checkpoint(channel_id, member_id, body.coverage_ts, &body.summary);
    StatusCode::NO_CONTENT
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

/// One entry of the pool read: identity, language and description — the
/// shape a linker or resolver decides from. NO source and no parameter
/// definition (a pool read must not carry KB-scale payloads).
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct CodeCandidateDto {
    pub code_id: u64,
    pub language: String,
    pub description: String,
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

/// `GET /code` — what the store holds, in id order.
async fn list_code(State(state): State<Arc<MemState>>) -> Json<Vec<CodeCandidateDto>> {
    let code = state.code.lock().await;
    Json(
        code.list()
            .into_iter()
            .map(|(code_id, language, description)| CodeCandidateDto {
                code_id,
                language,
                description,
            })
            .collect(),
    )
}

// ---- flat memory (ns 32) --------------------------------------------
//
// The memory family's tool face (ADR-0001: the standing
// capability, offered as tools — `commit_memory` / `recall_memory`). One
// endpoint per core primitive, nothing added: the store assigns the id,
// and the CALLER owns the clock — `created_at` rides the request, the
// same rule the channel log carries (ADR-0008 §3); the server never
// stamps a time.

/// Request body of `POST /memory` — `MemoryStore::store`'s parameters.
#[derive(Deserialize, Serialize)]
pub struct StoreMemory {
    pub user_id: u64,
    pub text: String,
    /// The caller's clock, never the server's (ADR-0008 §3).
    pub created_at: u64,
}

/// The assigned identity, returned by `POST /memory` (the caller's record).
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct MemoryIdDto {
    pub memory_id: u64,
}

/// One search hit: identity plus the stored text.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct MemoryHitDto {
    pub memory_id: u64,
    pub text: String,
}

/// Query string of the search door: `?q=`.
#[derive(Deserialize)]
pub struct MemorySearchQuery {
    pub q: String,
}

/// `POST /memory` — store one flat memory; the store assigns the id.
///
/// The row's vector is computed HERE, in-process, by the attached engine
/// (ADR-0003: the client states nothing about embedding). No engine
/// attached = an empty vector frame, which is a legal row rather than a
/// zero vector.
async fn store_memory(
    State(state): State<Arc<MemState>>,
    Json(body): Json<StoreMemory>,
) -> Result<Json<MemoryIdDto>, EngineError> {
    let vector = match &state.embedder {
        Some(engine) => engine
            .embed(&body.text)
            .map_err(|e| EngineError(e.to_string()))?,
        None => Vec::new(),
    };
    let mut memory = state.memory.lock().await;
    Ok(Json(MemoryIdDto {
        memory_id: memory.store(body.user_id, &body.text, body.created_at, &vector),
    }))
}

/// `GET /memory/{user_id}/search?q=` — the user's rows containing `q`, in
/// key order. One user's range is scanned, never the whole table. An empty
/// query and "no match" are both an empty list, which is a legal answer
/// (the caller injects nothing) rather than an error.
async fn search_memory(
    State(state): State<Arc<MemState>>,
    Path(user_id): Path<u64>,
    Query(query): Query<MemorySearchQuery>,
) -> Json<Vec<MemoryHitDto>> {
    let memory = state.memory.lock().await;
    Json(
        memory
            .search(user_id, &query.q)
            .into_iter()
            .map(|(memory_id, text)| MemoryHitDto { memory_id, text })
            .collect(),
    )
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

/// `POST /facts/{scope}` — land one fact; the body IS the 8-slot tuple.
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
    pub weight: u32,
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
        }
    }
}

/// `GET /facts/{scope}/entities/{name}` — the exact name/alias entrance
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

/// `GET /facts/{scope}/entities/{node_id}/edges` — the node's outgoing
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

// ---- the graph's bulk read face (a renderer's door) ------------------
//
// One scope's graph in the shape a force-graph client consumes (nodes +
// links), so `docs/kg-demo-3d.html` reads the real graph instead of
// hand-written packets (PLAN's Deferred gate: a bulk read over the same
// HTTP boundary, read-only, no new mechanism). No community id rides it —
// that machinery (ADR-0010's steps 3–4) is not landed, and the demo colors
// by node kind; and there is no paging yet: the gate's whole-table-vs-paged
// question stays open until a corpus needs it.

/// One node of a rendered graph.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct GraphNodeDto {
    pub id: u64,
    pub name: String,
    pub kind: String,
    pub literal_num: i64,
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

/// Errors the shell itself can produce (the stores' primitives
/// are infallible; only engine-level failures bubble). Message passed
/// through verbatim — the original error text never stops here.
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
