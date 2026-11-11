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
use mem_core::code::{CodeRow, CodeTouchOutcome};
use mem_core::graph::{FactTuple, GraphStore, IngestError, IngestOutcome};
use mem_core::proposals::ProposalDecl;
use mem_core::skills::{SkillDecl, SkillRead, SkillTouchOutcome, TouchPhase, DEFAULT_LIMIT};
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
    /// Where the service publishes itself (ADR-0014). `None` = no lake: the
    /// config has no `lake` node and nothing leaves this machine.
    lake: Option<Arc<dyn okm_lake::store::LakeStore>>,
}

/// Open the database at `path` with a lake, and publish into it every `interval_secs`.
///
/// The seam the `lake` node of the config ends in: the same store the binary builds from
/// a bucket URL, opened here without credentials so a test (or an in-process caller) can
/// point at a directory.
pub fn open_with_lake(path: PathBuf, target: &str, interval_secs: u64) -> Result<Router, String> {
    let store = okm_lake::store::open(target, None)?;
    build(
        path,
        None,
        mem_core::CompressionParams::default(),
        mem_core::DEFAULT_MAX_ROWS,
        Some((Arc::from(store), interval_secs)),
    )
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
        None,
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
        None,
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
    build(
        path,
        embedder,
        compression,
        mem_core::DEFAULT_MAX_ROWS,
        None,
    )
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
    // A lake node that cannot be built is a boot failure, like a missing embedding
    // artifact: a service that thinks it is publishing and is not is worse than one that
    // refuses to start.
    let lake = match &cfg.lake {
        Some(node) => {
            let store = node.open().map_err(|e| OpenError::Storage(e.to_string()))?;
            Some((Arc::from(store), node.interval_secs))
        }
        None => None,
    };
    build(
        dir,
        Some(engine),
        cfg.compression.params(),
        cfg.logs.max_rows,
        lake,
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
    lake: Option<(Arc<dyn okm_lake::store::LakeStore>, u64)>,
) -> Result<Router, String> {
    let stores = mem_core::open_service(&path)?;
    let graph = match &embedder {
        // The serving process attaches its engine to the graph too: node
        // vectors are computed in-process at ingest (ADR-0003), so the HTTP
        // client never sends one.
        Some(engine) => stores.graph.with_embedder(engine.clone()),
        None => stores.graph,
    };
    let (lake, interval) = match lake {
        Some((lake, secs)) => (Some(lake), Some(secs)),
        None => (None, None),
    };
    let state = Arc::new(MemState {
        store: tokio::sync::Mutex::new(stores.channels),
        graph: tokio::sync::Mutex::new(graph),
        log: tokio::sync::Mutex::new(stores.log.with_max_rows(log_max_rows)),
        compression,
        lake,
    });
    // The publisher runs beside the doors, not behind them: it takes each store's lock
    // only to COPY a table out, and does the network work once the locks are dropped.
    if let Some(secs) = interval {
        spawn_publisher(state.clone(), secs);
    }
    Ok(Router::new()
        // The turn axis (k10r ADR-0024): a turn is opened here, its rounds are
        // appended through the message door with the id it answers, and it is
        // closed with ONE record naming the rows that survive. The collapse of
        // the rest is the closing write's own business — no door for it.
        .route(
            "/channels/{channel_id}/turns/{turn_id}/close",
            post(close_turn),
        )
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
        // THE CHANNEL IS A ROW (ADR-0024's axis, and the many-to-many around it): a channel
        // carries the application's own scene annotation, membership reads from both sides, and
        // a member's lines are one prefix scan. A caller that only has an id can now ask what it
        // IS instead of inferring it from the log.
        .route("/channels", get(list_channels))
        .route(
            "/channels/{channel_id}",
            get(read_one_channel).post(open_channel),
        )
        .route(
            "/channels/{channel_id}/members",
            post(add_member).get(list_channel_members),
        )
        .route("/members/{member_id}/channels", get(list_member_channels))
        .route("/code/{scope}", post(put_code))
        // A bundle is the whole store: the body limit that protects the small
        // JSON doors does not apply to it.
        .route("/code/{scope}/{code_id}", get(fetch_code))
        // One EXACT version — what an auditor reads to see what a run was given.
        // The current version stays the default (`/code/{scope}/{code_id}`), so
        // the ordinary read never names a revision.
        .route("/code/{scope}/{code_id}/{rev}", get(fetch_code_at))
        // One record of a script's invocation (k10r ADR-0018's phases, applied to
        // the code vertex). The caller fires it beside the run it counts; the
        // answer is a receipt, never a precondition.
        .route("/code/{scope}/{code_id}/touch", post(touch_code))
        // The skill node's three doors (ADR-0015): declare, resolve (the list
        // `resolve_skills` answers), read one (what a run walks).
        .route("/skills/{scope}", post(put_skill))
        .route("/skills/{scope}/resolve", post(resolve_skills))
        .route("/skills/{scope}/propose", post(propose_skill))
        .route("/skills/{scope}/proposals", get(list_proposals))
        .route("/skills/{scope}/{node_id}/promote", post(promote_skill))
        .route("/skills/{scope}/{node_id}/touch", post(touch_skill))
        .route("/skills/{scope}/{node_id}", get(fetch_skill))
        .route("/tasks/{scope}", post(open_task).get(list_tasks))
        .route("/tasks/{scope}/{node_id}/move", post(move_task))
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
    /// The turn this record belongs to (ADR-0024). OMITTED, the row OPENS a new
    /// turn and the store mints the id as part of writing it — a turn exists
    /// because a user said something, so the first row and the mint are one call.
    /// Every later row of that turn carries the id the first one returned.
    #[serde(default)]
    pub turn_id: Option<u64>,
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
    let written = match body.turn_id {
        Some(turn_id) => store.append(
            channel_id,
            turn_id,
            body.ts_ms,
            body.sub,
            body.role,
            body.kind,
            &body.content,
        ),
        None => store.append_opening(
            channel_id,
            body.ts_ms,
            body.sub,
            body.role,
            body.kind,
            &body.content,
        ),
    };
    let key = written.map_err(|e| FactError {
        // An append into a turn that already closed is a caller that lost
        // the ordering, not a bad request: nothing about the body is
        // malformed.
        error: e,
        status: StatusCode::CONFLICT,
    })?;
    Ok(Json(key.into()))
}

/// One record a closing turn asks to keep — a pointer inside the turn.
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct KeptRecord {
    pub ts_ms: u64,
    pub sub: u16,
}

/// Request body of `POST /channels/{id}/turns/{turn}/close` (ADR-0024 §4): the
/// closing record's own key, its content, and the records to survive.
///
/// The keep list is data the caller states, not something k10r infers from the
/// roles and kinds: a rule over `role`/`kind` would be a second mapping to keep
/// in step with the first.
#[derive(Deserialize, Serialize)]
pub struct CloseTurn {
    pub ts_ms: u64,
    pub sub: u16,
    pub role: u8,
    pub content: String,
    pub keep: Vec<KeptRecord>,
}

/// `POST /channels/{id}/turns/{turn}/close` — write the turn's closing record,
/// keep the rows it names, purge the rest. 204 on success.
async fn close_turn(
    State(state): State<Arc<MemState>>,
    Path((channel_id, turn_id)): Path<(u64, u64)>,
    Json(body): Json<CloseTurn>,
) -> Result<StatusCode, FactError> {
    let mut store = state.store.lock().await;
    let keep: Vec<(u64, u16)> = body.keep.iter().map(|k| (k.ts_ms, k.sub)).collect();
    store
        .close_turn(
            channel_id,
            turn_id,
            body.ts_ms,
            body.sub,
            body.role,
            &body.content,
            &keep,
        )
        .map_err(|e| FactError {
            error: e,
            status: StatusCode::CONFLICT,
        })?;
    Ok(StatusCode::NO_CONTENT)
}

/// The key a message lives under, split into fields (the client needs
/// `ts_ms`/`sub` back for its delta record).
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct MessageKeyDto {
    pub channel_id: u64,
    /// The turn this record belongs to (ADR-0024). Additive on the wire: a
    /// decoder that predates it ignores it, a delta record replays with it.
    pub turn_id: u64,
    pub ts_ms: u64,
    pub sub: u16,
}

impl From<MessageKey> for MessageKeyDto {
    fn from(k: MessageKey) -> Self {
        Self {
            channel_id: k.channel_id,
            turn_id: k.turn_id,
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

/// One message as the model reads it (ADR-0009 §4): a run of user messages
/// BETWEEN two replies is one thing said, so the model gets one message with
/// per-line attribution rather than a flat record list.
///
/// Not the turn axis (ADR-0024): `key.turn_id` names the request that wrote a
/// record, while this merges records by speaker — a merge can span turns.
#[derive(Serialize)]
pub struct ModelMessageDto {
    pub role: &'static str,
    pub text: String,
    pub start_ts: u64,
    /// When the turn ended: a reply keeps the clock it finished on, so a line
    /// typed mid-generation is visible inside its span.
    pub end_ts: u64,
    /// The member ids this turn speaks for, in line order.
    pub senders: Vec<String>,
}

impl From<mem_core::view::ModelMessage> for ModelMessageDto {
    fn from(message: mem_core::view::ModelMessage) -> Self {
        Self {
            role: message.role,
            text: message.text,
            start_ts: message.start_ts,
            end_ts: message.end_ts,
            senders: message.senders.iter().map(ToString::to_string).collect(),
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
    pub sender: String,
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
    pub from_turn: u64,
    pub from_ts: u64,
    pub from_sub: u16,
    /// Span end, INCLUSIVE: the record the operation's product must cover.
    pub to_turn: u64,
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
    /// The turn the coverage point falls in (ADR-0024 §7). The pointer names a
    /// RECORD, and `(ts, sub)` stops being unique across a channel once two
    /// turns can be open at once, so a reader that resolves the pointer from the
    /// older two fields alone is guessing which turn it meant.
    pub coverage_turn: u64,
    pub coverage_ts: u64,
    /// The pointer's same-millisecond tiebreaker (ADR-0008 §2): a coverage
    /// point names a RECORD, so `ts` alone is not the whole position.
    pub coverage_sub: u16,
    pub summary: Option<String>,
    /// A newer node without a summary sits above the coverage point: a
    /// compression is in flight, and a second pass over the same span would be
    /// a wasted model call. The answer above is still complete — the read falls
    /// back to the newest node that HAS a summary.
    pub compressing: bool,
    pub increment: Vec<IncrementEntry>,
    /// The same records as the MESSAGES the model reads (ADR-0009 §4): user
    /// bursts merged, every line `[name(id=N)]`. The raw `increment` stays
    /// beside it for callers that want the records themselves. Merging is by
    /// speaker, not by turn (ADR-0024), so a message can span turns.
    pub messages: Vec<ModelMessageDto>,
    /// The cut the last compression considered better than the mechanical one
    /// (`(0, 0)` = none reported). A LABEL: the increment is still cut at
    /// `coverage_*`. A later tail operation reads from here.
    pub suggested_turn: u64,
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
        messages: Vec<ModelMessageDto>,
    ) -> Self {
        let head = if format.renders_text() {
            HeadDto::Text(mem_core::render::render_head(&p.head, format))
        } else {
            HeadDto::Facts(p.head.into_iter().map(FactViewDto::from).collect())
        };
        // Spans are computed BEFORE the literal: `increment` moves out of the
        // projection, and the operations read the view's cursors (ADR-0020).
        let tails: Vec<TailDto> = match compress {
            // Every operation the store knows fires on the same read, and each
            // states the span IT reads (ADR-0020): the compression covers the
            // record the verdict named, starting where the caller's summary
            // already ends; a review reads from the cut the last compression
            // suggested, to the newest record. An operation with nothing to
            // read is not asked for.
            Some(v) => mem_core::TAIL_OPS
                .iter()
                .filter_map(|op| {
                    let span = mem_core::tail_span(op, &p.view, &v)?;
                    Some(TailDto {
                        name: op.name,
                        prompt: op.prompt,
                        from_turn: span.start.turn_id,
                        from_ts: span.start.ts_ms,
                        from_sub: span.start.sub,
                        to_turn: span.end.turn_id,
                        to_ts: span.end.ts_ms,
                        to_sub: span.end.sub,
                    })
                })
                .collect(),
            None => Vec::new(),
        };
        Self {
            format: format.name(),
            head,
            coverage_turn: p.view.coverage.turn_id,
            coverage_ts: p.view.coverage.ts_ms,
            coverage_sub: p.view.coverage.sub,
            summary: p.view.summary,
            compressing: p.view.compressing,
            suggested_turn: p.view.suggested.turn_id,
            suggested_ts: p.view.suggested.ts_ms,
            suggested_sub: p.view.suggested.sub,
            messages,
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
            tails,
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
    Path((channel_id, member_raw)): Path<(u64, String)>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<SessionDto>, FactError> {
    let member_id = parse_member_id(&member_raw)?;
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
            .open_checkpoint(channel_id, v.checkpoint, &head_ids)
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
    let messages: Vec<ModelMessageDto> =
        mem_core::view::messages_of(&channels, channel_id, &projection.view.increment)?
            .into_iter()
            .map(ModelMessageDto::from)
            .collect();
    Ok(Json(SessionDto::new(projection, format, verdict, messages)))
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
        let sender_name = match store.member(row.sender).map_err(FactError::internal)? {
            Some(member) => member.display_name,
            None => format!("member-{}", row.sender),
        };
        entries.push(ChannelEntry {
            key: MessageKeyDto::from(key),
            sender_name,
            sender: row.sender.to_string(),
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
    /// (`suggested_turn` + `suggested_ts` + `suggested_sub`). Omitted = the
    /// mechanical cut was right, which is the common case.
    pub suggested_turn: Option<u64>,
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
    Path((channel_id, member_raw)): Path<(u64, String)>,
    Json(body): Json<AppendCheckpoint>,
) -> Result<StatusCode, FactError> {
    // The checkpoint write is channel-scoped, but the door still NAMES a member: a segment that is
    // not an id is a caller error here, not something to ignore.
    parse_member_id(&member_raw)?;
    let mut store = state.store.lock().await;
    let open = store.open_span(channel_id).map_err(FactError::internal)?;
    let Some(mem_core::Span { start, end }) = open else {
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
            let point = mem_core::channel::Pointer {
                turn_id: body.suggested_turn.unwrap_or(0),
                ts_ms: ts,
                sub: sub.unwrap_or(0),
            };
            if point > start && point <= end {
                Some(point).filter(|p| *p != end)
            } else {
                return Err(FactError {
                    error: format!(
                        "the suggested cut {}:{}:{} is outside the span this checkpoint closes \
                         ({}:{}:{} exclusive .. {}:{}:{} inclusive)",
                        point.turn_id,
                        point.ts_ms,
                        point.sub,
                        start.turn_id,
                        start.ts_ms,
                        start.sub,
                        end.turn_id,
                        end.ts_ms,
                        end.sub
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

/// Request body of `POST /code/{scope}` — one piece of code: the language it is
/// written in, what it is (description, parameter definition) and its
/// source. The language is required data, never defaulted; the
/// description and the parameter definition are what a skill-to-code
/// link is decided from without reading the source.
#[derive(Deserialize, Serialize)]
pub struct PutCode {
    /// The script this write REVISES. Absent = a new script (rev 1); present =
    /// append the next revision of that script, leaving every earlier one
    /// readable. The store refuses an id it never wrote, by id.
    #[serde(default)]
    pub code_id: Option<u64>,
    pub language: String,
    /// One line about what it does. Absent = an empty description: a script
    /// without one is harder to reuse, not invalid.
    #[serde(default)]
    pub description: String,
    /// Parameter definition as a JSON document in TEXT (the store does
    /// not parse it; the caller that delivered the code owns its shape).
    #[serde(default)]
    pub parameters: String,
    pub source: String,
}

/// The assigned identity, returned by `POST /code` (the caller's record).
/// `rev` is the version that write produced: 1 for a new script, N+1 for a
/// revision of one that already exists.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct CodeIdDto {
    pub code_id: u64,
    pub rev: u64,
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

/// A version and its payload. The revision lives in the KEY, never in the row:
/// the answer restates it so a caller can name what it ran without decoding a
/// key it never sees.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct CodeVersionDto {
    pub rev: u64,
    #[serde(flatten)]
    pub row: CodeRowDto,
}

/// `POST /code/{scope}` — store one piece of code; the store assigns the id.
///
/// Scoped since k10r ADR-0023: a code row is a VERTEX of that scope's fact graph
/// (plus a sheet), because tasks bind to the code that finishes them and a fact
/// reaches entities. The id is unchanged — skills still name it in
/// `steps[].code_id`, and gravity still addresses it by number.
async fn put_code(
    State(state): State<Arc<MemState>>,
    Path(scope): Path<u64>,
    Json(body): Json<PutCode>,
) -> Result<Json<CodeIdDto>, FactError> {
    let mut graph = state.graph.lock().await;
    let (code_id, rev) = match body.code_id {
        Some(code_id) => {
            let rev = graph
                .put_code_revision(
                    scope,
                    code_id,
                    &body.language,
                    &body.description,
                    &body.parameters,
                    &body.source,
                )
                .map_err(FactError::from)?;
            (code_id, rev)
        }
        None => {
            let code_id = graph
                .put_code(
                    scope,
                    &body.language,
                    &body.description,
                    &body.parameters,
                    &body.source,
                )
                .map_err(FactError::from)?;
            (code_id, 1)
        }
    };
    Ok(Json(CodeIdDto { code_id, rev }))
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

/// Request body of the touch doors (`/skills/{scope}/{node_id}/touch`,
/// `/code/{scope}/{code_id}/touch`) — ADR-0018: which record of an invocation
/// this is, and the caller's clock.
///
/// `attempt` is what a caller sends BEFORE the call — it does not know yet
/// whether it will succeed, and a call that never comes back must still be
/// visible. `success` is the second record, after the answer. The store derives
/// nothing: the counters are `calls` and `ok`, and their difference is failures.
///
/// ONE body for both rows: an invocation is attempt-then-success whether the
/// counted row is a skill (the procedure) or a script (what runs).
#[derive(Deserialize, Serialize)]
pub struct TouchBody {
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
    Json(body): Json<TouchBody>,
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

// ---- tasks (k10r ADR-0021) ------------------------------------------------
//
// A task is an ordinary vertex of the fact graph with a sheet beside it, so these
// three doors move ONLY the parts a vertex cannot: whether it is open, what the
// next action is, when it was stated. Dependencies, parents, tags and references
// are ordinary edges and ride `POST /memories/{scope}` like any other fact — a
// door per relation would be a second vocabulary for one graph.

/// Request body of `POST /tasks/{scope}`. `ts_ms` is the caller's clock, the same
/// discipline the checkpoint and touch doors follow: k10r has no clock of its own.
#[derive(Debug, Deserialize)]
pub struct OpenTaskBody {
    pub name: String,
    #[serde(default)]
    pub next: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    /// The code rows that finish this task (`task --finishes_on--> code`).
    /// Stated here because opening is when the caller knows which step the work
    /// belongs to; the edges it writes are the truth, and gravity closes the task
    /// at the call site (gravity ADR-0003 §3).
    #[serde(default)]
    pub finishes_on: Vec<u64>,
    #[serde(default)]
    pub ts_ms: Option<u64>,
}

/// Request body of `PATCH /tasks/{scope}/{node_id}`. Absent fields are LEFT
/// ALONE — an empty string is how a caller clears the next action, so silence can
/// never blank a field somebody else filled.
#[derive(Debug, Deserialize)]
pub struct MoveTaskBody {
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub next: Option<String>,
    #[serde(default)]
    pub ts_ms: Option<u64>,
}

/// What a task's sheet reports. No `status` string of its own: the sheet IS the
/// state, and a second vocabulary would drift from it.
#[derive(Debug, Serialize)]
struct TaskDto {
    node_id: u64,
    name: String,
    state: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    next: String,
    created_at: u64,
    updated_at: u64,
    /// The code rows this task is finished by (`finishes_on` edges, ADR-0021).
    /// Absent rather than `[]`: a caller builds "which calls close what" from the
    /// bindings that EXIST, and an empty array is noise on every other task.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    finishes_on: Vec<u64>,
}

#[derive(Debug, Serialize)]
struct TaskLandedDto {
    node_id: u64,
    created: bool,
}

fn task_dto(task: mem_core::TaskView) -> TaskDto {
    TaskDto {
        node_id: task.node_id,
        name: task.name,
        state: task.state,
        next: task.next,
        created_at: task.created_at,
        updated_at: task.updated_at,
        finishes_on: task.finishes_on,
    }
}

/// `POST /tasks/{scope}` — open a task. Idempotent on the name: opening twice is
/// opening once (the name is what facts reach it by), and `created` says which
/// happened.
async fn open_task(
    State(state): State<Arc<MemState>>,
    Path(scope): Path<u64>,
    Json(body): Json<OpenTaskBody>,
) -> Result<Json<TaskLandedDto>, FactError> {
    let now = body.ts_ms.unwrap_or_else(crate::now_ms);
    let mut graph = state.graph.lock().await;
    let landed = graph
        .open_task(
            scope,
            &mem_core::TaskDecl {
                name: body.name,
                next: body.next,
                state: body.state,
                finishes_on: body.finishes_on,
            },
            now,
        )
        .map_err(FactError::from)?;
    Ok(Json(TaskLandedDto {
        node_id: landed.node_id,
        created: landed.created,
    }))
}

/// `POST /tasks/{scope}/{node_id}/move` — move a task: a state, a next action, or
/// both. 404 for an unknown id and 400 for a node that is not a task, because a
/// wrong id and a wrong expectation are different mistakes.
///
/// A verb and not `PATCH /tasks/{scope}/{node_id}`, matching the store's other
/// actions (`/touch`, `/promote`): one door shape for "do something to this node",
/// so a caller never has to know which HTTP verb a particular action took.
async fn move_task(
    State(state): State<Arc<MemState>>,
    Path((scope, node_id)): Path<(u64, u64)>,
    Json(body): Json<MoveTaskBody>,
) -> axum::response::Response {
    let now = body.ts_ms.unwrap_or_else(crate::now_ms);
    if body.state.is_none() && body.next.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "a move names a state, a next action, or both".to_string(),
        )
            .into_response();
    }
    let mut graph = state.graph.lock().await;
    match graph.set_task_state(
        scope,
        node_id,
        body.state.as_deref(),
        body.next.as_deref(),
        now,
    ) {
        Ok(mem_core::TaskRead::Found(task)) => Json(task_dto(*task)).into_response(),
        Ok(mem_core::TaskRead::Absent) => StatusCode::NOT_FOUND.into_response(),
        Ok(mem_core::TaskRead::NotATask { kind }) => (
            StatusCode::BAD_REQUEST,
            format!("node {node_id} is a `{kind}`, not a task"),
        )
            .into_response(),
        Err(e) => FactError::from(e).into_response(),
    }
}

/// Query of `GET /tasks/{scope}` — `?state=open` narrows to one state.
#[derive(Debug, Deserialize)]
pub struct TaskListQuery {
    #[serde(default)]
    pub state: Option<String>,
}

/// `GET /tasks/{scope}` — the tasks of a scope, oldest id first. With a state
/// named the answer comes off the sheet's face (an exact probe); without one it
/// is the kind scan, which is what "everything" costs.
async fn list_tasks(
    State(state): State<Arc<MemState>>,
    Path(scope): Path<u64>,
    Query(query): Query<TaskListQuery>,
) -> Result<Json<Vec<TaskDto>>, FactError> {
    let mut graph = state.graph.lock().await;
    let tasks = graph
        .list_tasks(scope, query.state.as_deref())
        .map_err(FactError::from)?;
    Ok(Json(tasks.into_iter().map(task_dto).collect()))
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

/// Publish the service into its lake: once when the doors open, then every interval.
///
/// `build` is called by tests as well as by the binary, and a test may hold no runtime;
/// a router built without one simply has no publisher — nothing else changes.
fn spawn_publisher(state: Arc<MemState>, interval_secs: u64) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn(async move {
        let period = std::time::Duration::from_secs(interval_secs.max(1));
        loop {
            if let Err(e) = publish_all(&state).await {
                // A lake that is down is not a reason to stop serving: the next interval
                // tries again, and the store has not changed in the meantime.
                eprintln!("lake: {e}");
            }
            tokio::time::sleep(period).await;
        }
    });
}

/// One snapshot: copy every table out under its own lock, then write them to the lake.
///
/// The two phases are separate on purpose. The locks are held only while a table is copied
/// into memory, so a slow bucket cannot stall a request; the publishes then run on a
/// blocking thread (the S3 client is synchronous by design) one table at a time.
async fn publish_all(state: &Arc<MemState>) -> Result<usize, String> {
    let Some(lake) = state.lake.clone() else {
        return Ok(0);
    };
    let mut exports: Vec<(String, mem_core::export::Export)> = Vec::new();
    for name in mem_core::export::lake_table_names() {
        let name = name.as_str();
        let mut got = { state.store.lock().await.export(name) };
        if got.is_none() {
            got = state.log.lock().await.export(name);
        }
        if got.is_none() {
            got = state.graph.lock().await.export(name);
        }
        match got {
            Some(Ok(export)) => exports.push((name.to_string(), export)),
            Some(Err(e)) => return Err(format!("{name}: {e}")),
            // A catalogued table no store serves is a wiring bug, not a missing row.
            None => return Err(format!("no store serves the table `{name}`")),
        }
    }
    for (name, got) in [
        (
            mem_core::export::LOG_REDUCES,
            state.log.lock().await.export_reduces(),
        ),
        (
            mem_core::export::CODE_REDUCES,
            state.graph.lock().await.export_code_reduces(),
        ),
    ] {
        exports.push((name.to_string(), got.map_err(|e| format!("{name}: {e}"))?));
    }

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let count = exports.len();
    let lake = lake.clone();
    tokio::task::spawn_blocking(move || -> Result<usize, String> {
        for (name, export) in exports {
            let file = format!("{name}-{stamp}.parquet");
            okm_lake::publish::replace_table(&*lake, &name, &file, &export.bytes, &export.keys)
                .map_err(|e| format!("publishing `{name}`: {e}"))?;
        }
        Ok(count)
    })
    .await
    .map_err(|e| format!("the publisher did not finish: {e}"))??;
    Ok(count)
}

/// `GET /code/{code_id}` — the script gravity reads at call time.
/// Unknown id = 404 (the caller turns it into an error value for the
/// model; the store has no opinion).
async fn fetch_code(
    State(state): State<Arc<MemState>>,
    Path((scope, code_id)): Path<(u64, u64)>,
) -> axum::response::Response {
    let mut graph = state.graph.lock().await;
    match graph.code(scope, code_id) {
        Ok(Some((rev, row))) => Json(CodeVersionDto {
            rev,
            row: CodeRowDto::from(row),
        })
        .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => FactError::internal(e.to_string()).into_response(),
    }
}

/// `GET /code/{scope}/{code_id}/{rev} — one exact version (the audit read).
async fn fetch_code_at(
    State(state): State<Arc<MemState>>,
    Path((scope, code_id, rev)): Path<(u64, u64, u64)>,
) -> axum::response::Response {
    let mut graph = state.graph.lock().await;
    match graph.code_at(scope, code_id, rev) {
        Ok(Some(row)) => Json(CodeVersionDto {
            rev,
            row: CodeRowDto::from(row),
        })
        .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => FactError::internal(e.to_string()).into_response(),
    }
}

/// `POST /code/{scope}/{code_id}/touch` — count one record of a script's
/// invocation on its vertex.
///
/// The count lives on the SCRIPT, not on the caller: one `generate-pdf` row can
/// be a step in any number of skills and reached from more than one tool, and the
/// row is where those paths meet. The caller's own counters (a skill's
/// `invoke_calls`) stay where they were — a count kept on one side cannot be
/// split back into the other.
async fn touch_code(
    State(state): State<Arc<MemState>>,
    Path((scope, code_id)): Path<(u64, u64)>,
    Json(body): Json<TouchBody>,
) -> Result<Json<CodeTouchDto>, FactError> {
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
        .touch_code(scope, code_id, phase, body.ts_ms)
        .map_err(FactError::from)?
    {
        CodeTouchOutcome::Counted(touched) => Ok(Json(CodeTouchDto {
            code_id: touched.code_id,
            calls: touched.calls,
            ok: touched.ok,
        })),
        CodeTouchOutcome::Absent => Err(FactError {
            error: format!("no code {code_id} in scope {scope}"),
            status: StatusCode::NOT_FOUND,
        }),
        CodeTouchOutcome::NotACode { kind } => Err(FactError {
            error: format!("node {code_id} is a `{kind}`, not code"),
            status: StatusCode::BAD_REQUEST,
        }),
    }
}

/// What a code touch answers: the counters as stored.
#[derive(Serialize, Deserialize)]
pub struct CodeTouchDto {
    pub code_id: u64,
    pub calls: u64,
    pub ok: u64,
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
    /// `edge_id`) | `dropped` (the incumbent won) | `bumped` (the assertion
    /// already stood: same head, kind and tail — nothing was written).
    pub outcome: String,
    pub edge_id: u64,
}

impl From<IngestOutcome> for IngestOutcomeDto {
    fn from(o: IngestOutcome) -> Self {
        let (outcome, edge_id) = match o {
            IngestOutcome::Linked { edge_id } => ("linked", edge_id),
            IngestOutcome::Rewritten { edge_id } => ("rewritten", edge_id),
            IngestOutcome::Dropped { edge_id } => ("dropped", edge_id),
            IngestOutcome::Bumped { edge_id } => ("bumped", edge_id),
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
    /// The user endpoint — the member id as its uuid text, when this endpoint is a person
    /// instead of a corpus entity. No channel: an identity is global (ADR-0051), and which
    /// channel the fact was written on is the fact's own key.
    pub user: Option<String>,
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
            user: n.id.user().map(|m| m.to_string()),
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
    /// The node's usage telemetry (ADR-0018, a declared field set since the
    /// 2026-10-27 amendment): how many times it was invoked and when. A graph
    /// view reads them to say "alive" or "abandoned" without opening the node.
    pub invoke_calls: u32,
    pub invoke_ok: u32,
    pub last_invoked: u64,
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
            invoke_calls: row.invoke_calls,
            invoke_ok: row.invoke_ok,
            last_invoked: row.last_invoked,
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

/// A node attribute as wire JSON. The bag is storage's shape (`U64` / `I64` /
/// `Str` / …); a renderer only ever needs JSON, and bytes are the one variant
/// with no JSON counterpart — they are shown as a length, never as a wall of
/// numbers that would bury the rest of the card.
fn dynamic_to_json(value: mem_core::DynamicValue) -> serde_json::Value {
    use mem_core::DynamicValue as D;
    match value {
        // Widths are a Rust-side convenience: the wire is widthless, so every
        // unsigned variant is one number here.
        D::U8(n) => serde_json::Value::from(n),
        D::U16(n) => serde_json::Value::from(n),
        D::U32(n) => serde_json::Value::from(n),
        D::U64(n) => serde_json::Value::from(n),
        D::I64(n) => serde_json::Value::from(n),
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

/// `POST /channels/{id}` — open or re-annotate a channel. The scene is the APPLICATION's word
/// ("private", "group:ops", anything); the storage layer has no vocabulary of its own to impose.
#[derive(Deserialize, Serialize)]
pub struct OpenChannel {
    /// The OIDC `client_id` of the owning application, as the uuid it is (a text uuid on the
    /// wire, like a member id): a filter key the deployment writes, never relabelled — see
    /// `ChannelRow::app_id`.
    pub app_id: String,
    pub scene: String,
    pub created_ms: u64,
}

/// An application id is an OIDC `client_id`: a uuid, and on the wire its text, for the same
/// reason a member id is text — `Uuid7` carries no serde impl, and a JSON number would be the
/// surrogate ADR-0025 §4 rejects.
fn parse_app_id(raw: &str) -> Result<mem_core::Uuid7, FactError> {
    raw.parse::<mem_core::Uuid7>().map_err(|_| FactError {
        error: format!("`{raw}` is not an app id (a uuid)"),
        status: StatusCode::BAD_REQUEST,
    })
}

/// A member id on the wire is the uuid's TEXT: `Uuid7` carries no serde impl, and a JSON number
/// would be the surrogate ADR-0025 §4 rejects. A path segment that is not a uuid is a caller
/// error, not a store error — 400, with the value named.
fn parse_member_id(raw: &str) -> Result<mem_core::Uuid7, FactError> {
    raw.parse::<mem_core::Uuid7>().map_err(|_| FactError {
        error: format!("`{raw}` is not a member id (a uuid)"),
        status: StatusCode::BAD_REQUEST,
    })
}

/// One channel row on the wire.
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct ChannelDto {
    pub channel_id: u64,
    pub app_id: String,
    pub scene: String,
    pub created_ms: u64,
}

impl From<(u64, mem_core::channel::ChannelRow)> for ChannelDto {
    fn from((channel_id, row): (u64, mem_core::channel::ChannelRow)) -> Self {
        Self {
            channel_id,
            app_id: row.app_id.to_string(),
            scene: row.scene,
            created_ms: row.created_ms,
        }
    }
}

/// `POST /channels/{id}` — write the row that makes the id a thing. 204: the row is the answer,
/// and a caller that renames a scene is doing exactly what it asked for.
async fn open_channel(
    State(state): State<Arc<MemState>>,
    Path(channel_id): Path<u64>,
    Json(body): Json<OpenChannel>,
) -> Result<StatusCode, FactError> {
    let store = state.store.lock().await;
    store
        .put_channel(
            channel_id,
            parse_app_id(&body.app_id)?,
            &body.scene,
            body.created_ms,
        )
        .map_err(FactError::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /channels/{id}` — what that channel IS. Absent = nobody opened it, which is a 404 and
/// not an empty channel.
async fn read_one_channel(
    State(state): State<Arc<MemState>>,
    Path(channel_id): Path<u64>,
) -> axum::response::Response {
    let store = state.store.lock().await;
    match store.channel(channel_id) {
        Ok(Some(row)) => Json(ChannelDto::from((channel_id, row))).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => FactError::internal(e).into_response(),
    }
}

/// `GET /channels` — every channel row, id order. The set is one per line, so this is a small
/// answer by nature and the caller does the filtering by scene.
#[derive(Deserialize)]
pub struct AppFilter {
    /// `?app=gravity` — the owning application. Absent means every app's channels.
    pub app: Option<String>,
}

async fn list_channels(
    State(state): State<Arc<MemState>>,
    Query(filter): Query<AppFilter>,
) -> axum::response::Response {
    let mut store = state.store.lock().await;
    let rows = match filter.app.as_deref() {
        Some(raw) => match raw.parse::<mem_core::Uuid7>() {
            Ok(app_id) => store.channels_of_app(app_id),
            Err(_) => Err(format!("`{raw}` is not an app id (a uuid)")),
        },
        None => store.channels(),
    };
    match rows {
        Ok(rows) => Json(
            rows.into_iter()
                .map(|(key, row)| ChannelDto::from((key.channel_id, row)))
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => FactError::internal(e).into_response(),
    }
}

/// `POST /channels/{id}/members` — register a member AND their membership in one commit (the
/// double write is the store's, so a caller cannot land one half).
#[derive(Deserialize, Serialize)]
pub struct AddMember {
    pub member_id: String,
    pub display_name: String,
    pub role: u8,
    pub since_ms: u64,
}

async fn add_member(
    State(state): State<Arc<MemState>>,
    Path(channel_id): Path<u64>,
    Json(body): Json<AddMember>,
) -> Result<StatusCode, FactError> {
    let store = state.store.lock().await;
    let member_id = parse_member_id(&body.member_id)?;
    store
        .join(
            channel_id,
            member_id,
            &body.display_name,
            body.role,
            body.since_ms,
        )
        .map_err(FactError::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// One member as the channel sees them — the identity layer's row (ADR-0009 §1).
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct MemberDto {
    pub member_id: String,
    pub display_name: String,
    pub role: u8,
}

/// `GET /channels/{id}/members` — who is in this channel, member-id order.
async fn list_channel_members(
    State(state): State<Arc<MemState>>,
    Path(channel_id): Path<u64>,
) -> axum::response::Response {
    let mut store = state.store.lock().await;
    match store.members(channel_id) {
        Ok(rows) => Json(
            rows.into_iter()
                .map(|(key, row)| MemberDto {
                    member_id: key.member_id.to_string(),
                    display_name: row.display_name,
                    role: row.role,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => FactError::internal(e).into_response(),
    }
}

/// One membership as the MEMBER sees it: which channel, and since when.
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct MembershipDto {
    pub channel_id: u64,
    pub since_ms: u64,
}

/// `GET /members/{id}/channels` — the mirror direction: this member's lines, channel-id order.
async fn list_member_channels(
    State(state): State<Arc<MemState>>,
    Path(raw): Path<String>,
    Query(filter): Query<AppFilter>,
) -> axum::response::Response {
    let member_id = match parse_member_id(&raw) {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    let mut store = state.store.lock().await;
    match store.channels_of(member_id) {
        Ok(rows) => {
            // The filter is applied to the channel ROW, not the membership: "this member's
            // channels, the ones my app owns" — a membership carries no app of its own.
            let mut out = Vec::with_capacity(rows.len());
            for (channel_id, row) in rows {
                if let Some(raw) = filter.app.as_deref() {
                    let owned = raw.parse::<mem_core::Uuid7>().ok().is_some_and(|app_id| {
                        store
                            .channel(channel_id)
                            .ok()
                            .flatten()
                            .is_some_and(|c| c.app_id == app_id)
                    });
                    if !owned {
                        continue;
                    }
                }
                out.push(MembershipDto {
                    channel_id,
                    since_ms: row.since_ms,
                });
            }
            Json(out).into_response()
        }
        Err(e) => FactError::internal(e).into_response(),
    }
}
