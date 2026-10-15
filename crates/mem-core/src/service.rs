//! Store assembly: one Fjall database per k10r process, one keyspace per
//! table group.
//!
//! Fjall takes an exclusive lock file in the database directory, so a
//! process cannot open a second database over the same directory — a
//! second `FjallStore::open` there fails on the lock. `FjallStore::from_db`
//! attaches another keyspace to a database k10r already owns, which is how
//! every table group shares one directory and one lock.
//!
//! This is how a service process arranges its tables; it changes nothing
//! about what k10r IS (ADR-0007: a standalone service with its own
//! storage, reached only over HTTP).

use crate::channel::{ChannelStore, KEYSPACE as CHANNEL_KEYSPACE};
use crate::code::{CodeStore, KEYSPACE as CODE_KEYSPACE};
use okm_core::FjallStore;

/// Every table group a k10r process serves, on one database.
pub struct ServiceStores {
    pub channels: ChannelStore,
    pub code: CodeStore,
}

/// Open (or create) the service database at `path` and attach every table
/// group to it.
pub fn open_service(path: &std::path::Path) -> Result<ServiceStores, fjall::Error> {
    let db = fjall::Database::create_or_recover(fjall::Config::new(path))?;
    Ok(ServiceStores {
        channels: ChannelStore::wrap(FjallStore::from_db(db.clone(), CHANNEL_KEYSPACE)?),
        code: CodeStore::wrap(FjallStore::from_db(db, CODE_KEYSPACE)?),
    })
}
