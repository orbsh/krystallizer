//! Store assembly: one Fjall database per k10r process (one directory,
//! one lock file), holding the service's two keyspaces.
//!
//! Fjall takes an exclusive lock file in the database directory, so a
//! process cannot open a second database over the same directory — a
//! second `FjallStore::open` there fails on the lock. `FjallStore::from_db`
//! attaches a keyspace to a database k10r already owns, which is how both
//! keyspaces share one directory and one lock.
//!
//! Why two keyspaces and not one: the channel log is queue-like (the
//! highest write rate in the store), so it gets its own LSM tree and its
//! compaction never drags the other tables along. Everything else lives in
//! `memory` and is discriminated by ns.
//!
//! This is how a service process arranges its tables; it changes nothing
//! about what k10r IS (ADR-0007: a standalone service with its own
//! storage, reached only over HTTP).

use crate::channel::ChannelStore;
use crate::code::CodeStore;
use crate::graph::GraphStore;
use crate::log::LogStore;
use crate::{KEYSPACE_CHANNEL, KEYSPACE_MEMORY};
use okm_core::FjallStore;

/// Every table group a k10r process serves, on one database.
pub struct ServiceStores {
    /// The channel group (ns 33 messages, ns 34 checkpoints) — its own
    /// keyspace.
    pub channels: ChannelStore,
    /// The structured log (ns 41) — the `channel` keyspace, because its
    /// write shape is the log's (append-only, high rate), not the memory
    /// side's (ADR-0002, ADR-0011).
    pub log: LogStore,
    /// The code group (ns 35) — the `memory` keyspace.
    pub code: CodeStore,
    /// The fact graph (ns 36 entities, ns 37 facts) — the `memory`
    /// keyspace, like every table whose write shape is not the log's.
    pub graph: GraphStore,
}

/// Open (or create) the service database at `path` and attach every table
/// group to it. A second call on the same directory fails on Fjall's lock
/// (one writer per directory, by construction).
pub fn open_service(path: &std::path::Path) -> Result<ServiceStores, String> {
    let db = fjall::Database::create_or_recover(fjall::Config::new(path))
        .map_err(|e| e.to_string())?;
    let channel_store =
        FjallStore::from_db(db.clone(), KEYSPACE_CHANNEL).map_err(|e| e.to_string())?;
    let memory_store = FjallStore::from_db(db, KEYSPACE_MEMORY).map_err(|e| e.to_string())?;
    Ok(ServiceStores {
        channels: ChannelStore::wrap(channel_store.clone())?,
        // The graph holds the identity collection's handle too (ADR-0014): a
        // `user` endpoint is validated and rendered by a point read across
        // keyspaces, never by a batch that would span them.
        graph: GraphStore::wrap(memory_store.clone())?.with_channel(channel_store.clone()),
        log: LogStore::wrap(channel_store)?,
        code: CodeStore::wrap(memory_store)?,
    })
}
