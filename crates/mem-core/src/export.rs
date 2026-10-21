//! Parquet export — the backup boundary (ADR-0007): a bulk read over the same
//! HTTP door, not an admin CLI or a second storage format.
//!
//! The format is not k10r's: okm's eager bridge turns a collection into Arrow
//! `RecordBatch`es from the row DECLARATION (typed columns, vocabulary labels as
//! text, nullable where an id has no name), and `parquet_io` writes that batch to
//! a file. k10r only decides WHICH table and WHERE the file goes — so a backup is
//! readable by anything that reads Parquet, and the schema cannot drift from the
//! rows because both come from the same declaration.

use okm_core::bridge::tooling::parquet_io::export_parquet;
use okm_core::{Collection, Document, FjallStore, KeyEncode};
use std::path::Path;

/// The ns a table declares, read off its prefix — so a dispatcher keyed by ns
/// (the export door) never restates a number the row already owns.
pub const fn ns(prefix: &[u8]) -> u16 {
    u16::from_be_bytes([prefix[0], prefix[1]])
}

/// One table, one file. The collection carries its own ns (declared on the row),
/// so nothing here restates it.
pub fn table<K, R>(collection: &Collection<FjallStore, K, R>, path: &Path) -> Result<(), String>
where
    K: KeyEncode,
    R: Document<Key = K>,
{
    export_parquet(collection, path).map_err(|e| format!("parquet export: {e}"))
}
