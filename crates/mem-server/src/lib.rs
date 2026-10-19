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
use mem_core::channel::{ChannelStore, MessageKey, MessageRow, SessionView};
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
}

/// Open the database at `path` and build the router WITHOUT an embedding
/// engine — nodes then carry no vector. This is the shape the
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
        .route("/facts/{scope}", post(ingest_fact))
        .route("/facts/{scope}/search", get(search_facts))
        .route("/facts/{scope}/recall", get(recall_facts))
        .route("/facts/{scope}/edges/{edge_id}/touch", post(touch_fact))
        .route("/facts/{scope}/communities", get(list_communities))
        .route(
            "/facts/{scope}/communities/{community_id}",
            get(fetch_community).put(complete_community),
        )
        .route(
            "/facts/{scope}/communities/{community_id}/packet",
            get(fetch_community_packet),
        )
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

/// One landing seed (`GET /facts/{scope}/search`): the node the query landed
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
}

fn default_seed_count() -> usize {
    10
}

/// One recall (`GET /facts/{scope}/recall`): the landing seeds, the capsules
/// of their communities (ADR-0010 §5's layer-3 context), and the facts one
/// hop out of them, in one answer.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct RecallDto {
    pub seeds: Vec<SeedDto>,
    pub communities: Vec<CommunityDto>,
    pub facts: Vec<FactViewDto>,
}

/// `GET /facts/{scope}/search?q=&k=` — vector landing over one scope's
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

/// `GET /facts/{scope}/recall?q=&k=&ts_ms=` — the read shape ADR-0003
/// describes: vector landing plus ONE hop, ordered by weight (ADR-0005),
/// returned together so the caller does not stitch two calls. An empty
/// landing returns no seeds and no facts.
async fn recall_facts(
    State(state): State<Arc<MemState>>,
    Path(scope): Path<u64>,
    Query(query): Query<RecallQuery>,
) -> Result<Json<RecallDto>, FactError> {
    let mut graph = state.graph.lock().await;
    graph
        .recall(scope, &query.q, query.k, query.ts_ms)
        .map(|recall| {
            Json(RecallDto {
                seeds: recall
                    .seeds
                    .into_iter()
                    .map(|s| SeedDto {
                        node_id: s.node_id,
                        score: s.score,
                        name: s.name,
                        kind: s.kind,
                    })
                    .collect(),
                communities: recall.communities.into_iter().map(CommunityDto::from).collect(),
                facts: recall.facts.into_iter().map(FactViewDto::from).collect(),
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

/// `POST /facts/{scope}/edges/{edge_id}/touch` — count one TOOL consult of
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

// ---- community capsules (ADR-0010 steps 3–4) ------------------------
//
// The write side's output, pulled by the caller: k10r holds no LLM and never
// calls out, so consolidation is a PULL — the caller lists the communities
// that are stale, reads one community's packet (canonical 8-slot tuples),
// summarizes it elsewhere, and PUTs the summary back. Every read here is a
// declared access method (the capsule row, the by_community face).

/// One community's capsule (ADR-0010 §4) as a wire shape.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct CommunityDto {
    pub community_id: u64,
    /// `0` = pending (the summary is stale), `1` = done.
    pub state: u8,
    pub updated_at: u64,
    /// Assertions landed since the last summary.
    pub uncompressed: u32,
    pub title: String,
    pub summary: String,
}

impl From<(u64, mem_core::graph::CommunityRow)> for CommunityDto {
    fn from((community_id, row): (u64, mem_core::graph::CommunityRow)) -> Self {
        Self {
            community_id,
            state: row.state,
            updated_at: row.updated_at,
            uncompressed: row.uncompressed,
            title: row.title,
            summary: row.summary,
        }
    }
}

impl From<mem_core::Capsule> for CommunityDto {
    fn from(capsule: mem_core::Capsule) -> Self {
        Self {
            community_id: capsule.community_id,
            state: capsule.state,
            updated_at: capsule.updated_at,
            uncompressed: capsule.uncompressed,
            title: capsule.title,
            summary: capsule.summary,
        }
    }
}

/// One entry of the listing: the capsule plus the size the caller judges the
/// price of summarizing by.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct CommunityEntryDto {
    #[serde(flatten)]
    pub community: CommunityDto,
    pub member_count: u32,
}

/// One community read whole: the capsule, its member nodes, and the facts
/// INSIDE it (both endpoints members). This is ADR-0010 §5's bucket read —
/// one point get plus one face scan, no graph algorithm.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct CommunityBucketDto {
    pub community: CommunityDto,
    pub members: Vec<EntityDto>,
    pub facts: Vec<FactViewDto>,
}

/// One consolidation packet (ADR-0010 §3–§4): the community's internal facts
/// as canonical 8-slot tuples, one JSON array per line — the summarizer
/// prompt's input, verbatim.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct PacketDto {
    pub community_id: u64,
    pub state: u8,
    pub uncompressed: u32,
    pub member_count: u32,
    pub updated_at: u64,
    pub title: String,
    pub summary: String,
    pub facts: Vec<String>,
}

/// The listing's filter: `?state=pending|done` (absent = every community).
#[derive(Deserialize)]
pub struct CommunitiesQuery {
    pub state: Option<String>,
}

/// The write-back body: the summary the caller's model produced, and the
/// caller's clock (ADR-0008 §3).
#[derive(Deserialize, Serialize)]
pub struct CompleteCommunity {
    pub title: String,
    pub summary: String,
    pub ts_ms: u64,
}

fn bad_request(message: String) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": message })),
    )
        .into_response()
}

/// `GET /facts/{scope}/communities?state=` — every capsule of one scope, with
/// its member count. A filter the caller does not recognize is a 400, never a
/// silent "everything".
async fn list_communities(
    State(state): State<Arc<MemState>>,
    Path(scope): Path<u64>,
    Query(query): Query<CommunitiesQuery>,
) -> axum::response::Response {
    let wanted = match query.state.as_deref() {
        None => None,
        Some("pending") => Some(mem_core::STATE_PENDING),
        Some("done") => Some(mem_core::STATE_DONE),
        Some(other) => {
            return bad_request(format!(
                "unknown state filter `{other}` (expected `pending` or `done`)"
            ));
        }
    };
    let mut graph = state.graph.lock().await;
    let rows = graph.communities(scope);
    let mut out = Vec::with_capacity(rows.len());
    for (community_id, row) in rows {
        if let Some(state) = wanted {
            if row.state != state {
                continue;
            }
        }
        let member_count = graph.community_members(scope, community_id).len() as u32;
        out.push(CommunityEntryDto {
            community: CommunityDto::from((community_id, row)),
            member_count,
        });
    }
    Json(out).into_response()
}

/// `GET /facts/{scope}/communities/{id}` — the bucket read.
async fn fetch_community(
    State(state): State<Arc<MemState>>,
    Path((scope, community_id)): Path<(u64, u64)>,
) -> axum::response::Response {
    let mut graph = state.graph.lock().await;
    let Some(row) = graph.capsule(scope, community_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut members = Vec::new();
    for (node_id, node) in graph.community_members(scope, community_id) {
        let kind = graph.entity_kind(node.kind_id).unwrap_or_default();
        members.push(EntityDto {
            node_id,
            kind,
            literal_num: node.literal_num,
            name: node.name,
        });
    }
    let facts = graph
        .community_facts(scope, community_id)
        .into_iter()
        .map(FactViewDto::from)
        .collect();
    Json(CommunityBucketDto {
        community: CommunityDto::from((community_id, row)),
        members,
        facts,
    })
    .into_response()
}

/// `GET /facts/{scope}/communities/{id}/packet` — the consolidation pull.
async fn fetch_community_packet(
    State(state): State<Arc<MemState>>,
    Path((scope, community_id)): Path<(u64, u64)>,
) -> axum::response::Response {
    let mut graph = state.graph.lock().await;
    match graph.community_packet(scope, community_id) {
        Some(packet) => Json(PacketDto {
            community_id: packet.community_id,
            state: packet.state,
            uncompressed: packet.uncompressed,
            member_count: packet.member_count,
            updated_at: packet.updated_at,
            title: packet.title,
            summary: packet.summary,
            facts: packet.facts,
        })
        .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `PUT /facts/{scope}/communities/{id}` — the summary lands on the capsule
/// row (a point overwrite), the state becomes `done`, and the uncompressed
/// counter resets. An unknown community is a 404: a summary of something
/// that never existed does not create it.
async fn complete_community(
    State(state): State<Arc<MemState>>,
    Path((scope, community_id)): Path<(u64, u64)>,
    Json(body): Json<CompleteCommunity>,
) -> axum::response::Response {
    let mut graph = state.graph.lock().await;
    match graph.complete_community(
        scope,
        community_id,
        &body.title,
        &body.summary,
        body.ts_ms,
    ) {
        Some(row) => Json(CommunityDto::from((community_id, row))).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---- the graph's bulk read face (a renderer's door) ------------------
//
// One scope's graph in the shape a force-graph client consumes (nodes +
// links), so `docs/kg-demo-3d.html` reads the real graph instead of
// hand-written packets (PLAN's Deferred gate: a bulk read over the same
// HTTP boundary, read-only, no new mechanism). Each node carries its
// community id (ADR-0010 §6's color cluster); there is no paging yet — the
// gate's whole-table-vs-paged question stays open until a corpus needs it.

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
