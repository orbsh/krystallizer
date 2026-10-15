//! mem-core: agent memory core.
//!
//! Storage + retrieval over Fjall with OKM-declared schemas.
//! Two interface surfaces: session control (branch/tail/summarize) and
//! memory (full-session / assist). Zero LLM dependency, zero transport
//! dependency. Design: docs/adr/.

pub mod channel;
pub mod code;
pub mod config;
pub mod memory;
pub mod service;

pub use channel::{
    kind, role, ChannelStore, CheckpointKey, CheckpointRow, MessageKey, MessageRow, SessionView,
    DEFAULT_MEMBER,
};
pub use code::{CodeKey, CodeRow, CodeStore};
pub use config::{ConfigError, EmbeddingConfig, KrystallizerConfig};
pub use memory::{MemoryKey, MemoryRow, MemoryStats, MemoryStore};
pub use service::{open_service, ServiceStores};
