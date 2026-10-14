//! axum service shell over mem-core — the single cross-process form
//! (ADR-0007 as revised 2026-10-10: HTTP is the ONLY remote surface;
//! in-process mem-core linking is a local deployment detail, not a
//! second access mode).
//!
//! Endpoints mirror `ChannelStore`'s three primitives exactly; wire
//! types restate the store's rows for serde (the core stays wire-free).
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
}

/// Open the keyspace at `path` and build the router.
pub fn open(path: PathBuf) -> Result<Router, fjall::Error> {
    let store = ChannelStore::open(&path)?;
    let state = Arc::new(MemState {
        store: tokio::sync::Mutex::new(store),
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

/// Errors the shell itself can produce (the store's three primitives
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
