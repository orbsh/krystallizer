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
use mem_core::proposals::ProposalDecl;
use mem_core::skills::{
    SkillDecl, SkillRead, SkillTouchOutcome, TouchPhase, DEFAULT_LIMIT,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

pub mod embedding;

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
/// storage-level tests use; a serving deployment wants `open_from_config`.
pub fn open(path: PathBuf) -> Result<Router, String> {
    build(
        path,
        None,
        mem_core::CompressionParams::default(),
        mem_core::DEFAULT_MAX_ROWS,
    )
}

/// Open the database at `path` and build the router, optionally with an
/// embedding engine attached. The compression numbers are the built-in
/// defaults (`CompressionParams::default()`); a serving deployment takes
/// every number from its config file through `open_from_config`.
///
/// One database per process (one directory, one lock file): mem-core's
/// service assembly attaches every table group to it, so mem-server holds
/// one handle and one lock per keyspace.
pub fn open_with(
    path: PathBuf,
    embedder: Option<Arc<dyn mem_core::Embedder>>,
) -> Result<Router, String> {
    build(
        path,
        embedder,
        mem_core::CompressionParams::default(),
        mem_core::DEFAULT_MAX_ROWS,
    )
}

/// `open_with`, with the compression policy's numbers stated (ADR-0008
/// §4: every threshold is a parameter, so the serving process hands the
/// engine the ones its config file carries).
pub fn open_with_compression(
    path: PathBuf,
    embedder: Option<Arc<dyn mem_core::Embedder>>,
    compression: mem_core::CompressionParams,
) -> Result<Router, String> {
    build(path, embedder, compression, mem_core::DEFAULT_MAX_ROWS)
}

/// Everything opening the service can fail on before a door exists: the
/// config's own resolution (`$HOME`, the artifact directory), the embedding
/// artifact itself, and the store's lock.
#[derive(Debug)]
pub enum OpenError {
    Config(mem_core::ConfigError),
    Embedding(mem_core::EmbedError),
    Storage(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The message travels verbatim: a boot failure has one reader, and it
        // needs the file or directory that failed, not a re-worded summary.
        match self {
            Self::Config(e) => write!(f, "{e}"),
            Self::Embedding(e) => write!(f, "{e}"),
            Self::Storage(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for OpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(e) => Some(e),
            Self::Embedding(e) => Some(e),
            // A store's refusal is the engine's own text, not a typed error to
            // walk down from.
            Self::Storage(_) => None,
        }
    }
}

impl From<mem_core::ConfigError> for OpenError {
    fn from(e: mem_core::ConfigError) -> Self {
        Self::Config(e)
    }
}

impl From<mem_core::EmbedError> for OpenError {
    fn from(e: mem_core::EmbedError) -> Self {
        Self::Embedding(e)
    }
}

impl From<fjall::Error> for OpenError {
    fn from(e: fjall::Error) -> Self {
        Self::Storage(e.to_string())
    }
}

/// Open the service the way a DEPLOYMENT does: everything from the one config
/// file. The database's directory is `storage.data-dir` (`$HOME` expanded), the
/// engine comes from the `embedding` node — the built-in model in-process, or a
/// service over HTTP (ADR-0003), and BOTH are proven at boot, because a deployment
/// that cannot embed cannot serve retrieval — and the compression numbers and the
/// log's ceiling come from their own nodes.
pub fn open_from_config(cfg: &mem_core::KrystallizerConfig) -> Result<Router, OpenError> {
    let dir = cfg.storage.data_dir()?;
    let engine = engine_from(cfg)?;
    build(
        dir,
        Some(engine),
        cfg.compression.params(),
        cfg.logs.max_rows,
    )
    .map_err(OpenError::Storage)
}

/// Build the engine the file names. Local = the artifact is loaded (a missing one is
/// a boot failure); remote = the client connects AND answers one probe (an
/// unreachable service or a wrong width is a boot failure). Neither mode is allowed
/// to open the doors onto a store that cannot embed.
fn engine_from(
    cfg: &mem_core::KrystallizerConfig,
) -> Result<Arc<dyn mem_core::Embedder>, OpenError> {
    match cfg.embedding.engine()? {
        mem_core::EmbeddingEngine::Local { model_dir } => {
            let engine: Arc<dyn mem_core::Embedder> =
                Arc::new(mem_core::LocalEmbedder::load(&model_dir)?);
            Ok(engine)
        }
        mem_core::EmbeddingEngine::Remote {
            base_url,
            model,
            api_key_env,
            timeout_ms,
        } => {
            let engine: Arc<dyn mem_core::Embedder> = Arc::new(embedding::RemoteEmbedder::connect(
                &base_url,
                &model,
                api_key_env.as_deref(),
                timeout_ms,
                cfg.embedding.dimensions() as usize,
            )?);
            Ok(engine)
        }
    }
}

/// The one assembly path: every public constructor above is this function with
/// different numbers, so a deployment and a test differ in their PARAMETERS and
/// never in the shape of what they open.
fn build(
    path: PathBuf,
    embedder: Option<Arc<dyn mem_core::Embedder>>,
    compression: mem_core::CompressionParams,
    log_max_rows: usize,
) -> Result<Router, String> {
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
        log: tokio::sync::Mutex::new(stores.log.with_max_rows(log_max_rows)),
        compression,
    });
    Ok(Router::new()
        .route(
            "/channels/{channel_id}/messages",
            post(append_message).get(read_channel),
        )
        .route(
            "/channels/{channel_id}/members/{member_id}/session",
            get(fetch_session),
        )
        .route(
            "/channels/{channel_id}/members/{member_id}/checkpoint",
            post(append_checkpoint),
        )
        .route("/code", post(put_code))
        .route("/export", get(export_bundle))
        .route("/export/{table}", get(export_table))
        // A bundle is the whole store: the body limit that protects the small
        // JSON doors does not apply to it.
        .route(
            "/import",
            post(import_bundle).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/code/{code_id}", get(fetch_code))
        // The skill node's three doors (ADR-0015): declare, resolve (the list
        // `resolve_skills` answers), read one (what a run walks).
        .route("/skills/{scope}", post(put_skill))
        .route("/skills/{scope}/resolve", post(resolve_skills))
        .route("/skills/{scope}/propose", post(propose_skill))
        .route("/skills/{scope}/proposals", get(list_proposals))
        .route("/skills/{scope}/{node_id}/promote", post(promote_skill))
        .route("/skills/{scope}/{node_id}/touch", post(touch_skill))
        .route("/skills/{scope}/{node_id}", get(fetch_skill))
        .route("/memories/{scope}", post(ingest_facts))
        .route("/graph/{scope}/search", post(search_facts))
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
        .route("/memories/{scope}/recall", post(recall_facts))
        .route("/memories/{scope}/edges/{edge_id}/touch", post(touch_fact))
        .route("/memories/{scope}/entities/{name}", get(fetch_entity))
        .route(
            "/graph/{scope}/entities/{node_id}/edges",
            get(list_entity_edges),
        )
        .route("/graph/{scope}", get(fetch_graph))
        // The graph view, served by the memory service itself: one origin, so
        // the page's fetch is same-origin and no CORS is opened on a service
        // that holds user memory (ADR-0007's ruling, ADR-0013 §8).
        .route("/viz", get(viz_page))
        .route("/viz/vendor/3d-force-graph.js", get(viz_vendor))
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
) -> Result<Json<MessageKeyDto>, FactError> {
    let mut store = state.store.lock().await;
    let key = store
        .append(
            channel_id,
            body.ts_ms,
            body.sub,
            body.role,
            body.kind,
            &body.content,
        )
        .map_err(FactError::internal)?;
    Ok(Json(key.into()))
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

/// One OPERATION to run as a branch beside the user's turn (ADR-0008 §4): the
/// prompt to append, and the span it applies to.
///
/// The list is what makes a second operation cheap: the store names an
/// operation and gives it a span, and a caller that knows the name does it. A
/// caller that does NOT know the name skips it — deployment skew is expected
/// here, because the store's list can grow before every guest is upgraded.
#[derive(Serialize)]
pub struct TailDto {
    /// `compress` today; the name is the dispatch key.
    pub name: &'static str,
    /// Appended after the span's records. The branch's answer is the product.
    pub prompt: &'static str,
    /// Span start, EXCLUSIVE: what the caller's summary already covers.
    pub from_ts: u64,
    pub from_sub: u16,
    /// Span end, INCLUSIVE: the record the operation's product must cover.
    pub to_ts: u64,
    pub to_sub: u16,
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
    /// The cut the last compression considered better than the mechanical one
    /// (`(0, 0)` = none reported). A LABEL: the increment is still cut at
    /// `coverage_*`. A later tail operation reads from here.
    pub suggested_ts: u64,
    pub suggested_sub: u16,
    /// The operations the policy asks for on this read (ADR-0008 §4). Empty =
    /// the caller builds its context from the records alone.
    pub tails: Vec<TailDto>,
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
            suggested_ts: p.view.suggested_ts,
            suggested_sub: p.view.suggested_sub,
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
            tails: match compress {
                // Every operation the store knows fires on the same verdict, and
                // each states the span IT needs: the compress pass covers the
                // record the verdict named, starting where the caller's summary
                // already ends. An operation with a narrower range (a review
                // pass reading from `suggested_*`) computes its own here.
                Some(v) => mem_core::TAIL_OPS
                    .iter()
                    .map(|op| TailDto {
                        name: op.name,
                        prompt: op.prompt,
                        from_ts: p.view.coverage_ts,
                        from_sub: p.view.coverage_sub,
                        to_ts: v.checkpoint_ts,
                        to_sub: v.checkpoint_sub,
                    })
                    .collect(),
                None => Vec::new(),
            },
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
    )?;
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
        channels
            .note_read(channel_id, member_id, clock)
            .map_err(FactError::internal)?;
    }
    // The head this read assembled is what the coming compression will have
    // been produced under, so the proposal freezes it NOW (ADR-0008 §2: the
    // checkpoint carries the persona, not a re-computation of it).
    let head_ids: Vec<u64> = projection.head.iter().map(|fact| fact.edge_id).collect();
    let verdict =
        mem_core::compress::evaluate(&projection.view, assembled, idle_ms, &state.compression);
    if let Some(v) = &verdict {
        channels
            .open_checkpoint(channel_id, (v.checkpoint_ts, v.checkpoint_sub), &head_ids)
            .map_err(FactError::internal)?;
    }
    // ADR-0011's first writer: the verdict is a decision, and decisions go to the
    // diagnostic log — naming the split it chose, the trigger that fired and the
    // numbers it read. The row's clock is K10R's own.
    if let Some(verdict) = &verdict {
        state
            .log
            .lock()
            .await
            .record_verdict(now_ms(), channel_id, verdict)
            .map_err(FactError::internal)?;
    }
    let turns: Vec<TurnDto> =
        mem_core::view::turns_of(&channels, channel_id, &projection.view.increment)?
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
) -> Result<Json<Vec<ChannelEntry>>, FactError> {
    let mut store = state.store.lock().await;
    let rows = store.history(channel_id).map_err(FactError::internal)?;
    let mut entries = Vec::with_capacity(rows.len());
    for (key, row) in rows {
        // An unregistered sender is absence, not a failure: the entry falls
        // back to the id-derived name.
        let sender_name = match store.member(channel_id, row.sender).map_err(FactError::internal)? {
            Some(member) => member.display_name,
            None => format!("member-{}", row.sender),
        };
        entries.push(ChannelEntry {
            key: MessageKeyDto::from(key),
            sender_name,
            sender: row.sender,
            role: mem_core::view::role_name(row.role).map(str::to_string),
            kind: row.kind,
            end_ts_ms: row.end_ts_ms,
            content: row.content,
        });
    }
    Ok(Json(entries))
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

/// Request body of `POST .../checkpoint` — the summary, and optionally the cut
/// the compression pass thought better than the mechanical one.
///
/// No coverage timestamp: ADR-0008 §4 has k10r derive it from the split it
/// served, so the caller never states where THIS node ends. The suggestion is
/// the opposite direction — a caller's observation about where the span WOULD
/// break better — so it is stated, and validated against the span being closed.
#[derive(Deserialize, Serialize)]
pub struct AppendCheckpoint {
    pub summary: String,
    /// The last record the summary should really have covered, as a pointer
    /// (`suggested_ts` + `suggested_sub`). Omitted = the mechanical cut was
    /// right, which is the common case.
    pub suggested_ts: Option<u64>,
    pub suggested_sub: Option<u16>,
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
    let open = store.open_span(channel_id).map_err(FactError::internal)?;
    let Some((start, end)) = open else {
        return Err(FactError {
            error: "no open compression node for this channel: the pointer comes from the \
                    verdict k10r served, so read the session first"
                .into(),
            status: StatusCode::BAD_REQUEST,
        });
    };
    // A suggestion is validated BEFORE it is stored: everything downstream reads
    // it as the start of a range that existed, so an out-of-span value would
    // make a later pass read a range that never did. Equal to the node's own
    // pointer = the mechanical cut restated, which is "no suggestion".
    let suggested = match (body.suggested_ts, body.suggested_sub) {
        (Some(ts), sub) if ts != 0 => {
            let point = (ts, sub.unwrap_or(0));
            if point > start && point <= end {
                Some(point).filter(|p| *p != end)
            } else {
                return Err(FactError {
                    error: format!(
                        "the suggested cut {ts}:{} is outside the span this checkpoint closes \
                         ({}:{} exclusive .. {}:{} inclusive)",
                        point.1, start.0, start.1, end.0, end.1
                    ),
                    status: StatusCode::BAD_REQUEST,
                });
            }
        }
        _ => None,
    };
    store
        .complete_checkpoint(channel_id, &body.summary, suggested)
        .map_err(FactError::internal)?
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
) -> Result<Json<CodeIdDto>, FactError> {
    let mut code = state.code.lock().await;
    let code_id = code
        .put(
            &body.language,
            &body.description,
            &body.parameters,
            &body.source,
        )
        .map_err(FactError::internal)?;
    Ok(Json(CodeIdDto { code_id }))
}

/// Request body of `POST /skills/{scope}` (ADR-0015). The three step-shaped
/// parts arrive as JSON documents in TEXT: the store carries them, it does not
/// parse them — the producer owns the shape, exactly as `parameters` does for a
/// code row (ADR-0012 §2).
#[derive(Deserialize, Serialize)]
struct PutSkill {
    name: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    preconditions: String,
    #[serde(default)]
    steps: String,
    #[serde(default)]
    verification: String,
}

/// The declaration's answer: the node it landed on, and whether this call is
/// what created it (a name that already exists IS the same skill).
#[derive(Deserialize, Serialize)]
struct SkillLandedDto {
    node_id: u64,
    created: bool,
}

/// One skill with all four parts — what an executor walks once the resolver's
/// candidate has been chosen.
#[derive(Deserialize, Serialize)]
struct SkillDto {
    node_id: u64,
    name: String,
    title: String,
    content: String,
    preconditions: String,
    steps: String,
    verification: String,
}

/// Request body of `POST /skills/{scope}/resolve`. `limit` absent means
/// `DEFAULT_LIMIT`; the core clamps whatever it is.
#[derive(Deserialize)]
struct ResolveSkills {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

/// One candidate: enough for a caller to decide, nothing more (the parts are
/// read once a skill is chosen).
#[derive(Deserialize, Serialize)]
struct SkillCandidateDto {
    node_id: u64,
    name: String,
    title: String,
    content: String,
    /// The steps, as declared (JSON TEXT): a model deciding whether to choose
    /// this skill has to see what it would make it do, and there is no second
    /// call that would tell it (ADR-0015 §4).
    steps: String,
    score: f32,
}

/// Request body of `POST /skills/{scope}/propose` (ADR-0016). `key` is the
/// composition as the WALKER spells it (the code ids in the order they ran):
/// the store carries `steps` as text and never parses it, so whoever walked the
/// run is who names it.
#[derive(Deserialize, Serialize)]
struct ProposeSkill {
    key: String,
    #[serde(default)]
    steps: String,
    ts_ms: u64,
}

/// The answer to "this composition ran": the row, its new count, and whether
/// this call created it.
#[derive(Deserialize, Serialize)]
struct ProposalLandedDto {
    node_id: u64,
    count: u64,
    created: bool,
}

/// One row of the review queue: what ran, how often, and (empty until a human
/// acts) what it became.
#[derive(Deserialize, Serialize)]
struct ProposalDto {
    node_id: u64,
    key: String,
    steps: String,
    count: u64,
    last_seen: u64,
    promoted_to: String,
}

/// Request body of `POST /skills/{scope}/{node_id}/touch` (ADR-0018): which
/// record of an invocation this is, and the caller's clock.
///
/// `attempt` is what a caller sends BEFORE the call — it does not know yet
/// whether it will succeed, and a call that never comes back must still be
/// visible. `success` is the second record, after the answer. The store derives
/// nothing: the counters are `calls` and `ok`, and their difference is failures.
#[derive(Deserialize, Serialize)]
pub struct TouchSkillBody {
    pub event: String,
    pub ts_ms: u64,
}

/// `POST /skills/{scope}/{node_id}/touch` — count one record of an invocation.
///
/// Best effort by CONTRACT, not by accident: the caller fires it beside the call
/// it is counting, and an answer here is a receipt, never a precondition. 404 for
/// an unknown id and 400 for a node that is not a skill, because a caller's wrong
/// id and a wrong kind are different mistakes.
async fn touch_skill(
    State(state): State<Arc<MemState>>,
    Path((scope, node_id)): Path<(u64, u64)>,
    Json(body): Json<TouchSkillBody>,
) -> Result<Json<SkillTouchDto>, FactError> {
    let Some(phase) = TouchPhase::parse(&body.event) else {
        return Err(FactError {
            error: format!(
                "unknown event `{}` — expected `attempt` (before the call) or `success` (after it)",
                body.event
            ),
            status: StatusCode::BAD_REQUEST,
        });
    };
    let mut store = state.graph.lock().await;
    match store
        .touch_skill(scope, node_id, phase, body.ts_ms)
        .map_err(FactError::from)?
    {
        SkillTouchOutcome::Counted(touched) => Ok(Json(SkillTouchDto {
            node_id: touched.node_id,
            calls: touched.calls,
            ok: touched.ok,
        })),
        SkillTouchOutcome::Absent => Err(FactError {
            error: format!("no node {node_id} in scope {scope}"),
            status: StatusCode::NOT_FOUND,
        }),
        SkillTouchOutcome::NotASkill { kind } => Err(FactError {
            error: format!("node {node_id} is a `{kind}`, not a skill"),
            status: StatusCode::BAD_REQUEST,
        }),
    }
}

/// What a touch answers: the counters as stored.
#[derive(Serialize, Deserialize)]
pub struct SkillTouchDto {
    pub node_id: u64,
    pub calls: u64,
    pub ok: u64,
}

/// Request body of `GET /skills/{scope}/proposals`. `min_count` absent means 1
/// (any composition a human has not looked at yet).
#[derive(Deserialize)]
struct ListProposals {
    #[serde(default)]
    min_count: Option<u64>,
}

/// Request body of `POST /skills/{scope}/{node_id}/promote`: the HUMAN's
/// declaration. The steps are copied from the proposal, never invented here.
#[derive(Deserialize, Serialize)]
struct PromoteSkill {
    name: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    content: String,
}

/// The promotion's answer: the skill that landed.
#[derive(Deserialize, Serialize)]
struct PromotedDto {
    skill_node_id: u64,
    name: String,
    created: bool,
}

/// `POST /skills/{scope}/propose` — count one walk of one composition
/// (ADR-0016). Idempotent on `key`: the same run again is the same row with a
/// higher count, never a second proposal.
async fn propose_skill(
    State(state): State<Arc<MemState>>,
    Path(scope): Path<u64>,
    Json(body): Json<ProposeSkill>,
) -> Result<Json<ProposalLandedDto>, FactError> {
    let mut graph = state.graph.lock().await;
    let landed = graph
        .propose(
            scope,
            &ProposalDecl {
                key: body.key,
                steps: body.steps,
                ts_ms: body.ts_ms,
            },
        )
        .map_err(FactError::from)?;
    Ok(Json(ProposalLandedDto {
        node_id: landed.node_id,
        count: landed.count,
        created: landed.created,
    }))
}

/// `GET /skills/{scope}/proposals` — the review queue (ADR-0016): most-walked
/// first, promoted rows filtered out. This is a HUMAN's door: `resolve` keeps
/// only the `skill` kind, so nothing here is visible to the model until it is
/// promoted.
async fn list_proposals(
    State(state): State<Arc<MemState>>,
    Path(scope): Path<u64>,
    Query(params): Query<ListProposals>,
) -> Result<Json<Vec<ProposalDto>>, FactError> {
    let mut graph = state.graph.lock().await;
    let proposals = graph
        .proposals(scope, params.min_count.unwrap_or(1))
        .map_err(FactError::from)?;
    Ok(Json(
        proposals
            .into_iter()
            .map(|proposal| ProposalDto {
                node_id: proposal.node_id,
                key: proposal.key,
                steps: proposal.steps,
                count: proposal.count,
                last_seen: proposal.last_seen,
                promoted_to: proposal.promoted_to,
            })
            .collect(),
    ))
}

/// `POST /skills/{scope}/{node_id}/promote` — a human declares the skill a
/// proposal suggested. Never a rename: the proposal keeps its row and records
/// what it became, which is also what stops it from being promoted twice.
async fn promote_skill(
    State(state): State<Arc<MemState>>,
    Path((scope, node_id)): Path<(u64, u64)>,
    Json(body): Json<PromoteSkill>,
) -> Result<Json<PromotedDto>, FactError> {
    let mut graph = state.graph.lock().await;
    let promoted = graph
        .promote(scope, node_id, &body.name, &body.title, &body.content)
        .map_err(FactError::from)?;
    Ok(Json(PromotedDto {
        skill_node_id: promoted.skill_node_id,
        name: promoted.name,
        created: promoted.created,
    }))
}

/// `POST /skills/{scope}` — declare a skill (idempotent on its name).
async fn put_skill(
    State(state): State<Arc<MemState>>,
    Path(scope): Path<u64>,
    Json(body): Json<PutSkill>,
) -> Result<Json<SkillLandedDto>, FactError> {
    let mut graph = state.graph.lock().await;
    let landed = graph
        .put_skill(
            scope,
            &SkillDecl {
                name: body.name,
                title: body.title,
                content: body.content,
                preconditions: body.preconditions,
                steps: body.steps,
                verification: body.verification,
            },
        )
        .map_err(FactError::from)?;
    Ok(Json(SkillLandedDto {
        node_id: landed.node_id,
        created: landed.created,
    }))
}

/// `GET /skills/{scope}/{node_id}` — one skill. Unknown id is 404; a node that
/// exists but is not a skill is 400, because the id is right and the request is
/// wrong about what lives there.
async fn fetch_skill(
    State(state): State<Arc<MemState>>,
    Path((scope, node_id)): Path<(u64, u64)>,
) -> axum::response::Response {
    let mut graph = state.graph.lock().await;
    match graph.skill(scope, node_id) {
        Ok(SkillRead::Found(skill)) => Json(SkillDto {
            node_id: skill.node_id,
            name: skill.name,
            title: skill.title,
            content: skill.content,
            preconditions: skill.preconditions,
            steps: skill.steps,
            verification: skill.verification,
        })
        .into_response(),
        Ok(SkillRead::Absent) => StatusCode::NOT_FOUND.into_response(),
        Ok(SkillRead::NotASkill { kind }) => FactError {
            error: format!("node {node_id} is a `{kind}`, not a skill"),
            status: StatusCode::BAD_REQUEST,
        }
        .into_response(),
        Err(e) => FactError::from(e).into_response(),
    }
}

/// `POST /skills/{scope}/resolve` — the selectable list (ADR-0015 §4).
///
/// Retrieval first, the kind-face scan as the fallback: a deployment with no
/// embedding engine still has skills, and "there are none" would be a wrong
/// answer. Nothing is counted — a candidate nobody chose is not a read.
async fn resolve_skills(
    State(state): State<Arc<MemState>>,
    Path(scope): Path<u64>,
    Json(body): Json<ResolveSkills>,
) -> Result<Json<Vec<SkillCandidateDto>>, FactError> {
    let mut graph = state.graph.lock().await;
    let limit = body.limit.unwrap_or(DEFAULT_LIMIT);
    Ok(Json(
        graph
            .resolve_skills(scope, &body.query, limit)
            .map_err(FactError::from)?
            .into_iter()
            .map(|candidate| SkillCandidateDto {
                node_id: candidate.node_id,
                name: candidate.name,
                title: candidate.title,
                content: candidate.content,
                steps: candidate.steps,
                score: candidate.score,
            })
            .collect(),
    ))
}

/// `GET /export` — the WHOLE store as one `tar.zst` bundle (ADR-0013).
///
/// Inside the archive: `<root>/<table>/data/<table>.parquet` beside that
/// table's Iceberg metadata, the catalog at the root, and nothing k10r-specific
/// beyond the directory names — unpack it and any Parquet/Iceberg reader can
/// open the tables without this service.
///
/// The tables are exported under the SAME lock each write takes, one at a time,
/// and the archive is built in a temp directory: a failure mid-way answers 500
/// and no half bundle is ever served.
async fn export_bundle(State(state): State<Arc<MemState>>) -> axum::response::Response {
    let dir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("no temp dir for the export: {e}"),
            )
                .into_response()
        }
    };
    let root = dir.path().join(mem_core::bundle::BUNDLE_DIR);
    if let Err(e) = std::fs::create_dir_all(&root) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("creating the bundle root: {e}"),
        )
            .into_response();
    }
    if let Err(e) = mem_core::export::catalog(&root.join(mem_core::bundle::CATALOG_FILE)) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    for &(name, _) in mem_core::export::CATALOG {
        if let Err(e) = export_one(&state, name, &root).await {
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    }
    for name in [
        mem_core::export::LOG_REDUCES,
        mem_core::export::CODE_REDUCES,
    ] {
        if let Err(e) = export_one(&state, name, &root).await {
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    }
    let archive = dir.path().join("backup.tar.zst");
    if let Err(e) = mem_core::bundle::seal(&root, &archive) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    let bytes = match std::fs::read(&archive) {
        Ok(bytes) => bytes,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("the bundle was written but not read back: {e}"),
            )
                .into_response()
        }
    };
    let filename = format!("krystallizer-backup-{}.tar.zst", mem_core::export::stamp());
    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/zstd".to_string(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        bytes,
    )
        .into_response()
}

/// One table into the bundle: the Parquet file, then its Iceberg metadata.
///
/// Each lock is taken as its OWN statement (and dropped before the next), so a
/// table that falls through every store cannot re-take a lock it still holds —
/// that is a self-deadlock, not a lookup miss.
async fn export_one(
    state: &Arc<MemState>,
    name: &str,
    root: &std::path::Path,
) -> Result<(), String> {
    let file = mem_core::bundle::table_file(root, name);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let mut found: Option<Result<(), String>> = None;
    if found.is_none() {
        found = state.store.lock().await.export(name, &file);
    }
    if found.is_none() {
        found = state.code.lock().await.export(name, &file);
    }
    if found.is_none() {
        found = state.log.lock().await.export(name, &file);
    }
    if found.is_none() {
        found = state.graph.lock().await.export(name, &file);
    }
    if found.is_none() && name == mem_core::export::LOG_REDUCES {
        found = Some(state.log.lock().await.export_reduces(&file));
    }
    if found.is_none() && name == mem_core::export::CODE_REDUCES {
        found = Some(state.code.lock().await.export_reduces(&file));
    }
    match found {
        Some(Ok(())) => {}
        Some(Err(e)) => return Err(e),
        // A catalogued table no store serves is a wiring bug, not a 404.
        None => return Err(format!("no store serves the table `{name}`")),
    }
    mem_core::export::write_metadata(&mem_core::bundle::table_dir(root, name), &file)
}

/// `GET /export/{table}` — ONE table as a Parquet file (inspection, not the
/// backup's form: the backup is `GET /export`'s bundle). Table names come from
/// the row declarations, so the door and the tables cannot drift; an unknown
/// name is a 404 pointing at the bundle.
async fn export_table(
    State(state): State<Arc<MemState>>,
    Path(name): Path<String>,
) -> axum::response::Response {
    let dir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("no temp dir for the export: {e}"),
            )
                .into_response()
        }
    };
    let file = dir.path().join(format!("{name}.parquet"));
    let mut found: Option<Result<(), String>> = None;
    if found.is_none() {
        found = state.store.lock().await.export(&name, &file);
    }
    if found.is_none() {
        found = state.code.lock().await.export(&name, &file);
    }
    if found.is_none() {
        found = state.log.lock().await.export(&name, &file);
    }
    if found.is_none() {
        found = state.graph.lock().await.export(&name, &file);
    }
    if found.is_none() && name == mem_core::export::LOG_REDUCES {
        found = Some(state.log.lock().await.export_reduces(&file));
    }
    if found.is_none() && name == mem_core::export::CODE_REDUCES {
        found = Some(state.code.lock().await.export_reduces(&file));
    }
    let Some(outcome) = found else {
        return (
            StatusCode::NOT_FOUND,
            format!(
                "no table named {name} (the backup is GET /export; the catalog travels inside it)"
            ),
        )
            .into_response();
    };
    if let Err(e) = outcome {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    read_parquet_response(&file, &format!("{name}.parquet"))
}

/// Answer with a file's bytes as an attachment of the given content type.
fn read_parquet_response(file: &std::path::Path, filename: &str) -> axum::response::Response {
    match std::fs::read(file) {
        Ok(bytes) => (
            [
                (
                    axum::http::header::CONTENT_TYPE,
                    "application/vnd.apache.parquet".to_string(),
                ),
                (
                    axum::http::header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{filename}\""),
                ),
            ],
            bytes,
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("the export was written but not read back: {e}"),
        )
            .into_response(),
    }
}

/// `POST /import` — restore a bundle (the mirror of `GET /export`).
///
/// ONE request restores every table, in the dependency order the catalog
/// declares; the operator never sorts files. Rows land through the normal write
/// contracts (merge by key: same key overwrites, new key inserts — the door
/// never deletes), then each store recomputes its in-process issuers from what
/// the store now holds, so a restored graph does not hand out an edge id a
/// restored row already carries.
///
/// Refusals are the caller's bug and nothing lands: a body that is not a
/// `.tar.zst`, an archive missing the bundle directory, a table member the
/// catalog names but the archive lacks.
async fn import_bundle(
    State(state): State<Arc<MemState>>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "an empty body is not a bundle").into_response();
    }
    let dir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("no temp dir for the restore: {e}"),
            )
                .into_response()
        }
    };
    let archive = dir.path().join("backup.tar.zst");
    if let Err(e) = std::fs::write(&archive, &body) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("writing the uploaded bundle: {e}"),
        )
            .into_response();
    }
    let root = match mem_core::bundle::unpack(&archive, dir.path()) {
        Ok(root) => root,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };

    let mut report: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    for name in mem_core::bundle::import_order() {
        let file = mem_core::bundle::table_file(&root, name);
        if !file.exists() {
            return (
                StatusCode::BAD_REQUEST,
                format!(
                    "the bundle has no member for `{name}` (expected {})",
                    file.display()
                ),
            )
                .into_response();
        }
        let mut found: Option<Result<usize, String>> = None;
        if found.is_none() {
            found = state.store.lock().await.import(name, &file);
        }
        if found.is_none() {
            found = state.code.lock().await.import(name, &file);
        }
        if found.is_none() {
            found = state.log.lock().await.import(name, &file);
        }
        if found.is_none() {
            found = state.graph.lock().await.import(name, &file);
        }
        match found {
            Some(Ok(rows)) => {
                report.insert(name, rows);
            }
            Some(Err(e)) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("restoring `{name}`: {e}"),
                )
                    .into_response()
            }
            None => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("no store serves the table `{name}`"),
                )
                    .into_response()
            }
        }
    }
    Json(report).into_response()
}

/// `GET /code/{code_id}` — the script gravity reads at call time.
/// Unknown id = 404 (the caller turns it into an error value for the
/// model; the store has no opinion).
async fn fetch_code(
    State(state): State<Arc<MemState>>,
    Path(code_id): Path<u64>,
) -> axum::response::Response {
    let code = state.code.lock().await;
    match code.get(code_id) {
        Ok(Some(row)) => Json(CodeRowDto::from(row)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => FactError::internal(e).into_response(),
    }
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

impl FactError {
    /// A store that could not answer: the engine's own text rides along and
    /// the door answers 500 (the same class as the graph's `Storage`).
    fn internal(error: String) -> Self {
        Self {
            error,
            status: StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
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

/// `POST /memories/{scope}` — land facts; the body is an ARRAY of 8-slot
/// tuples, and the answer is an array in the same order.
///
/// Plural because a caller (a model, above all) writes a PARAGRAPH of knowledge,
/// not one fact at a time: one round trip instead of N, and one place where a
/// partial failure is visible. A malformed element is a `refused` entry with
/// the core's error text and does NOT stop the rest — the caller fixes that one
/// and sends it again. Only a body that is not a JSON array is a 400, because
/// that is not a fact list at all.
async fn ingest_facts(
    State(state): State<Arc<MemState>>,
    Path(scope): Path<u64>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<Vec<serde_json::Value>>, FactError> {
    let Some(items) = body.as_array() else {
        return Err(FactError {
            error: "the body must be a JSON array of fact tuples".to_string(),
            status: StatusCode::BAD_REQUEST,
        });
    };
    let mut graph = state.graph.lock().await;
    let mut answers = Vec::with_capacity(items.len());
    for item in items {
        // `from_value` already answers the core's error, so there is nothing
        // to convert: the tuple's decode failure and the ingest refusal travel
        // to the caller through the same type.
        let landed = FactTuple::from_value(item).and_then(|fact| graph.ingest(scope, &fact));
        answers.push(match landed {
            Ok(outcome) => serde_json::json!(IngestOutcomeDto::from(outcome)),
            Err(e) => serde_json::json!({
                "outcome": "refused",
                "error": e.to_string(),
            }),
        });
    }
    Ok(Json(answers))
}

/// One node as the read face returns it.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct EntityDto {
    /// The corpus entity's id — present when the endpoint is an entity node
    /// (ns 36), absent for a user node (identity is not scoped to the corpus).
    pub node_id: Option<u64>,
    /// The user endpoint, `[channel, member]` (ADR-0014), when this endpoint is
    /// a person instead of a corpus entity.
    pub user: Option<(u64, u64)>,
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
    Json(queries): Json<Vec<String>>,
) -> Result<Json<Vec<Vec<SeedDto>>>, FactError> {
    let mut graph = state.graph.lock().await;
    let mut answers = Vec::with_capacity(queries.len());
    for q in &queries {
        let seeds = graph.retrieve(scope, q, query.k).map_err(FactError::from)?;
        answers.push(
            seeds
                .into_iter()
                .map(|s| SeedDto {
                    node_id: s.node_id,
                    score: s.score,
                    name: s.name,
                    kind: s.kind,
                })
                .collect(),
        );
    }
    Ok(Json(answers))
}

/// `GET /graph/{scope}/entities/{node_id}/anchored` — the entity-anchored
/// entrance (PLAN 4b): the facts that touch this node, then the rest of its
/// community, heaviest first. Identity instead of similarity — no query, no
/// embedding, no landing.
async fn anchored_entity(
    State(state): State<Arc<MemState>>,
    Path((scope, node_id)): Path<(u64, u64)>,
) -> axum::response::Response {
    let mut graph = state.graph.lock().await;
    match graph.anchored(scope, node_id) {
        Ok(Some(anchored)) => Json(AnchoredDto::from(anchored)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => FactError::from(e).into_response(),
    }
}

/// `GET /graph/{scope}/timeline?k=` — the temporal entrance: the scope's facts in
/// the order they were learned, newest first.
async fn timeline_facts(
    State(state): State<Arc<MemState>>,
    Path(scope): Path<u64>,
    Query(query): Query<TopQuery>,
) -> Result<Json<Vec<FactViewDto>>, FactError> {
    let mut graph = state.graph.lock().await;
    Ok(Json(
        graph
            .timeline(scope, query.k)
            .map_err(FactError::from)?
            .into_iter()
            .map(FactViewDto::from)
            .collect(),
    ))
}

/// `GET /graph/{scope}/chain?from=&kind=&hops=` — the causal entrance: follow ONE
/// predicate from a node, hop by hop. The answer's order IS the walk.
async fn chain_facts(
    State(state): State<Arc<MemState>>,
    Path(scope): Path<u64>,
    Query(query): Query<ChainQuery>,
) -> Result<Json<Vec<ChainHopDto>>, FactError> {
    let mut graph = state.graph.lock().await;
    Ok(Json(
        graph
            .chain(scope, query.from, &query.kind, query.hops)
            .map_err(FactError::from)?
            .into_iter()
            .map(ChainHopDto::from)
            .collect(),
    ))
}

/// One fact BY ITS ID — the deterministic read, the mirror of the write that
/// answered with an `edge_id`: no query, no embedding, no ranking. `404` when the
/// id names nothing in this scope, so a stale reference link is a clean miss.
async fn fetch_fact(
    State(state): State<Arc<MemState>>,
    Path((scope, edge_id)): Path<(u64, u64)>,
) -> axum::response::Response {
    let mut graph = state.graph.lock().await;
    match graph.fact_in_scope(scope, edge_id) {
        Ok(Some(fact)) => Json(FactViewDto::from(fact)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => FactError::from(e).into_response(),
    }
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
) -> Result<Json<Vec<FactViewDto>>, FactError> {
    let mut graph = state.graph.lock().await;
    Ok(Json(
        graph
            .top_by_weight(scope, query.k)
            .map_err(FactError::from)?
            .into_iter()
            .map(FactViewDto::from)
            .collect(),
    ))
}

/// `GET /memories/{scope}/recall?q=&k=&ts_ms=` — the read shape ADR-0003
/// describes: vector landing plus ONE hop, ordered by weight (ADR-0005),
/// returned together so the caller does not stitch two calls. An empty
/// landing returns no seeds and no facts.
async fn recall_facts(
    State(state): State<Arc<MemState>>,
    Path(scope): Path<u64>,
    Query(query): Query<RecallQuery>,
    Json(queries): Json<Vec<String>>,
) -> Result<Json<Vec<RecallDto>>, FactError> {
    let format = parse_format(query.format.as_deref())?;
    let structured = !format.renders_text();
    let mut graph = state.graph.lock().await;
    let mut answers = Vec::with_capacity(queries.len());
    for q in &queries {
        let recall = graph
            .recall(scope, q, query.k, query.ts_ms, query.budget, format)
            .map_err(FactError::from)?;
        answers.push(RecallDto {
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
            facts: structured.then(|| recall.facts.into_iter().map(FactViewDto::from).collect()),
            text: recall.text,
            shown: recall.shown,
            total: recall.total,
        });
    }
    Ok(Json(answers))
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
            node_id: n.id.entity_id(),
            user: n.id.user(),
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
) -> axum::response::Response {
    let mut graph = state.graph.lock().await;
    match graph.entity_by_name(scope, &name) {
        Ok(Some((node_id, row))) => Json(EntityDto {
            node_id: Some(node_id),
            user: None,
            kind: row.kind.as_str().unwrap_or_default().to_string(),
            literal_num: row.literal_num,
            name: row.name,
        })
        .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => FactError::from(e).into_response(),
    }
}

/// `GET /graph/{scope}/entities/{node_id}/edges` — the node's outgoing
/// facts, endpoints resolved: one hop of the traversal, in edge-id order.
/// An empty list is a legal answer (a node with no facts yet).
async fn list_entity_edges(
    State(state): State<Arc<MemState>>,
    Path((scope, node_id)): Path<(u64, u64)>,
) -> Result<Json<Vec<FactViewDto>>, FactError> {
    let mut graph = state.graph.lock().await;
    Ok(Json(
        graph
            .out_facts(scope, node_id)
            .map_err(FactError::from)?
            .into_iter()
            .map(FactViewDto::from)
            .collect(),
    ))
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
    /// The node's open attributes — `title` / `content` / `source` and whatever
    /// else the writer attached. A renderer needs them to caption a node without
    /// opening the document, which is the point of the corpus; the analysis face
    /// may carry them (ADR-0007 allows the shape to drift with the analysis
    /// need), and they are small: a few short strings per node.
    pub props: serde_json::Map<String, serde_json::Value>,
}

/// One link: a fact, with `source` / `target` as node ids (what the client
/// needs) plus the declared fields it may render or ignore.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct GraphLinkDto {
    pub edge_id: u64,
    /// The entity endpoint's id; absent when that endpoint is a user node
    /// (ADR-0014) — the link still exists, its end just is not a corpus node.
    pub source: Option<u64>,
    pub target: Option<u64>,
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
/// The live graph page, embedded in the binary: the deployment has one artifact,
/// so a page that must be served same-origin travels inside it.
const VIZ_PAGE: &str = include_str!("../static/viz.html");

/// The 3D force-graph library, embedded for the same reason (no CDN: this
/// machine may have no network, and a viewer for user memory must not phone
/// out). Vendored under `docs/vendor/`, the copy the demo page uses.
const VIZ_VENDOR: &[u8] = include_bytes!("../../../docs/vendor/3d-force-graph.js");

async fn viz_page() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        VIZ_PAGE,
    )
}

async fn viz_vendor() -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/javascript; charset=utf-8",
        )],
        VIZ_VENDOR,
    )
}

async fn fetch_graph(
    State(state): State<Arc<MemState>>,
    Path(scope): Path<u64>,
) -> Result<Json<GraphDto>, FactError> {
    let mut graph = state.graph.lock().await;
    let mut nodes: Vec<GraphNodeDto> = Vec::new();
    for (node_id, row) in graph.scope_entities(scope).map_err(FactError::from)? {
        // No open attributes on this node is absence; the bag is empty.
        let props = graph
            .entity_props(scope, node_id)
            .map_err(FactError::from)?
            .map(|found| {
                found
                    .into_iter()
                    .map(|(name, value)| (name, dynamic_to_json(value)))
                    .collect()
            })
            .unwrap_or_default();
        nodes.push(GraphNodeDto {
            id: node_id,
            kind: row.kind.as_str().unwrap_or_default().to_string(),
            literal_num: row.literal_num,
            name: row.name,
            community_id: row.community_id,
            props,
        });
    }
    let links: Vec<GraphLinkDto> = graph
        .scope_facts(scope)
        .map_err(FactError::from)?
        .into_iter()
        .map(|fact| GraphLinkDto {
            edge_id: fact.edge_id,
            source: fact.src.id.entity_id(),
            target: fact.dst.id.entity_id(),
            kind: fact.kind,
            since: fact.edge.since,
            confidence: fact.edge.confidence,
            weight: fact.edge.weight,
        })
        .collect();
    Ok(Json(GraphDto { nodes, links }))
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

/// A node attribute as wire JSON. The bag is storage's shape (`UInt` / `Int` /
/// `Str` / …); a renderer only ever needs JSON, and bytes are the one variant
/// with no JSON counterpart — they are shown as a length, never as a wall of
/// numbers that would bury the rest of the card.
fn dynamic_to_json(value: mem_core::DynamicValue) -> serde_json::Value {
    use mem_core::DynamicValue as D;
    match value {
        D::UInt(n) => serde_json::Value::from(n),
        D::Int(n) => serde_json::Value::from(n),
        D::F64(n) => serde_json::Number::from_f64(n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        D::Str(text) => serde_json::Value::String(text),
        D::Bool(flag) => serde_json::Value::Bool(flag),
        D::Null => serde_json::Value::Null,
        D::Bytes(bytes) => serde_json::Value::String(format!("<{} bytes>", bytes.len())),
        D::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(dynamic_to_json).collect())
        }
        D::Obj(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(name, value)| (name, dynamic_to_json(value)))
                .collect(),
        ),
    }
}
