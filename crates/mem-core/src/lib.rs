//! mem-core: agent memory core.
//!
//! Storage + retrieval over Fjall with OKM-declared schemas. Three
//! interface surfaces: session control (branch/tail/summarize), memory
//! (passive/short-horizon context and active/long-horizon facts —
//! initialization memory included), and tasks. Zero LLM dependency,
//! zero transport dependency. Design: docs/adr/.

// Unit tests are allowed the unwrap/panic vocabulary the production targets
// are denied: a panicking test is a failing test with a stack trace. Set once
// here — `cfg(test)` is only on for this crate's own test build, never for
// the `tests/` crates (those carry the same line at their own root).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unwrap_in_result, clippy::panic_in_result_fn))]

pub mod bundle;
pub mod channel;
pub mod code;
pub mod compress;
pub mod config;
pub mod embedding;
pub mod export;
pub mod graph;
pub mod log;
pub mod proposals;
pub mod render;
pub mod service;
pub mod skills;
pub mod view;

/// The service's two keyspaces — one Fjall database (one directory, one
/// lock file) holds both.
///
/// `channel` is its own tree because the message log is queue-like: it
/// takes the highest write rate in the store, so its compaction never
/// drags the other tables along.
///
/// `memory` holds everything else — code, the fact graph, and later the
/// skills. Those tables are discriminated by ns, per okm's rule (one
/// entity class = one ns; different table types sharing a keyspace do not
/// conflict).
pub const KEYSPACE_CHANNEL: &str = "channel";
pub const KEYSPACE_MEMORY: &str = "memory";

pub use channel::{
    kind, member_node, member_role, role, ChannelStore, CheckpointKey, CheckpointRow, CursorKey,
    CursorRow, MemberKey, MemberRow, MessageKey, MessageRow, SessionView, Span, DEFAULT_MEMBER,
};
pub use code::{CodeKey, CodeRow, CodeStore};
pub use compress::{
    evaluate as evaluate_compression, Params as CompressionParams, TailOp, Trigger as CompressionTrigger,
    Verdict as CompressionVerdict, COMPRESS_OP, TAIL_OPS, TAIL_PROMPT,
};
pub use config::{
    CompressionConfig, ConfigError, EmbeddingConfig, EmbeddingEngine, KrystallizerConfig,
    LocalEmbedding, LogsConfig, RemoteEmbedding, DEFAULT_TIMEOUT_MS, MODE_LOCAL, MODE_NODE,
    MODE_REMOTE,
};
pub use embedding::{l2_normalize, EmbedError, Embedder, LocalEmbedder};
pub use graph::{
    Anchored, ChainHop, CommunitySeqKey, CommunitySeqRow, CommunitySlice, EntityKey, EntityRow,
    FactBody, FactTuple, Facts, GraphStore, IngestError, IngestOutcome, NodeId, Recall, Relation,
    Seed, Term, Touch, COMMUNITY_NS, CONTENT_FIELD, ENTITY_NS, FACT_NS, FUNCTIONAL_PROP,
    LANDING_FLOOR, LITERAL_KIND, LPA_RADIUS, LPA_ROUNDS, MEMBER_NS, REF_FIELD, SOURCE_FIELD,
    TITLE_FIELD, USER_KIND, WEIGHT_SCALE,
};
pub use log::{
    event as log_event, field as log_field, level as log_level, LogEntry, LogKey, LogRow, LogStore,
    DEFAULT_MAX_ROWS,
};
/// The props bag's value type, re-exported so a door can carry node attributes
/// without taking its own dependency on the storage library.
pub use okm_core::obj_dynamic::DynamicValue;
pub use render::{render_head, render_memory, RenderFormat};
pub use service::{open_service, ServiceStores};
pub use view::{assemble, member_memory, role_name, Projection, DEFAULT_SCOPE};
