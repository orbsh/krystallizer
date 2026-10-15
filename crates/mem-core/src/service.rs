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
use crate::memory::MemoryStore;
use crate::{KEYSPACE_CHANNEL, KEYSPACE_MEMORY};
use okm_core::FjallStore;

/// Every table group a k10r process serves, on one database.
pub struct ServiceStores {
    /// The channel group (ns 33 messages, ns 34 checkpoints) — its own
    /// keyspace.
    pub channels: ChannelStore,
    /// The flat-memory group (ns 32) — the `memory` keyspace.
    pub memory: MemoryStore,
    /// The code group (ns 35) — the SAME `memory` keyspace as flat
    /// memory: different entity classes, different ns, one tree.
    pub code: CodeStore,
}

/// Open (or create) the service database at `path` and attach every table
/// group to it. A second call on the same directory fails on Fjall's lock
/// (one writer per directory, by construction).
pub fn open_service(path: &std::path::Path) -> Result<ServiceStores, fjall::Error> {
    let db = fjall::Database::create_or_recover(fjall::Config::new(path))?;
    let channel_store = FjallStore::from_db(db.clone(), KEYSPACE_CHANNEL)?;
    let memory_store = FjallStore::from_db(db, KEYSPACE_MEMORY)?;
    Ok(ServiceStores {
        channels: ChannelStore::wrap(channel_store),
        memory: MemoryStore::wrap(memory_store.clone()),
        code: CodeStore::wrap(memory_store),
    })
}
