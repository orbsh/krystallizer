//! mem-core: agent memory core.
//!
//! Storage + retrieval over Fjall with OKM-declared schemas. Three
//! interface surfaces: session control (branch/tail/summarize), memory
//! (passive/short-horizon context and active/long-horizon facts —
//! initialization memory included), and tasks. Zero LLM dependency,
//! zero transport dependency. Design: docs/adr/.

pub mod channel;
pub mod code;
pub mod config;
pub mod embedding;
pub mod graph;
pub mod service;
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
    kind, role, ChannelStore, CheckpointKey, CheckpointRow, MessageKey, MessageRow, SessionView,
    DEFAULT_MEMBER,
};
pub use code::{CodeKey, CodeRow, CodeStore};
pub use config::{ConfigError, EmbeddingConfig, KrystallizerConfig};
pub use embedding::{EmbedError, Embedder};
pub use graph::{
    CommunitySeqKey, CommunitySeqRow, CommunitySlice, EntityKey, EntityRow, FactBody, FactEdge,
    FactTuple, GraphStore, IngestError, IngestOutcome, Recall, Relation, Seed, Term, Touch,
    COMMUNITY_NS, ENTITY_NS, FACT_NS, FUNCTIONAL_PROP, LANDING_FLOOR, LITERAL_KIND, LPA_RADIUS,
    LPA_ROUNDS, WEIGHT_SCALE,
};
pub use service::{open_service, ServiceStores};
pub use view::{
    assemble, render_memory, role_name, Projection, DEFAULT_SCOPE, DEFAULT_USER_NODE,
};
