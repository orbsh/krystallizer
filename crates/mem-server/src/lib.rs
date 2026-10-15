//! axum service shell over mem-core — the single cross-process form
//! (ADR-0007 as revised 2026-10-10: HTTP is the ONLY remote surface;
//! in-process mem-core linking is a local deployment detail, not a
//! second access mode).
//!
//! Endpoints mirror `ChannelStore`'s primitives (channel log) and
//! `CodeStore`'s (the code store) exactly; wire types restate the
//! stores' rows for serde (the core stays wire-free).
//! Ordering discipline carries over: the CALLER owns the clock —
//! `ts_ms` and `sub` ride the request (ADR-0008 §3, gravity's clock is
//! the order authority; the server never assigns).

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use mem_core::channel::{ChannelStore, MessageKey, MessageRow, SessionView};
use mem_core::code::{CodeRow, CodeStore};
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
    code: tokio::sync::Mutex<CodeStore>,
}

/// Open the database at `path` and build the router.
///
/// One database per process (one directory, one lock file): mem-core's
/// service assembly attaches every table group to it, so mem-server holds
/// one handle and one lock per group.
pub fn open(path: PathBuf) -> Result<Router, fjall::Error> {
    let stores = mem_core::open_service(&path)?;
    let state = Arc::new(MemState {
        store: tokio::sync::Mutex::new(stores.channels),
        code: tokio::sync::Mutex::new(stores.code),
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
