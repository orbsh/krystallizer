//! mem-core: agent memory core.
//!
//! Storage + retrieval over Fjall with OKM-declared schemas.
//! Two interface surfaces: session control (branch/tail/summarize) and
//! memory (full-session / assist). Zero LLM dependency, zero transport
//! dependency. Design: docs/adr/.

pub mod config;
pub mod memory;

pub use config::{ConfigError, EmbeddingConfig, KrystallizerConfig};
pub use memory::{MemoryKey, MemoryRow, MemoryStats, MemoryStore};
