//! Parquet export — the backup boundary (ADR-0007): a bulk read over the same
//! HTTP door, not an admin CLI or a second storage format.
//!
//! The format is not k10r's. okm's eager bridge turns a collection into Arrow
//! `RecordBatch`es from the row DECLARATION (typed columns, vocabulary labels as
//! text, nullable where an id has no name), the graph hands out its edge set as a
//! table of its own, and `parquet_io` writes those to a file. k10r decides WHICH
//! table and WHERE the file goes; the shape stays the declaration's.
//!
//! Two consequences worth stating, because they are the reason this module is
//! small: a backup is readable without k10r in the loop (Parquet + labels), and
//! the schema cannot drift from the rows (one declaration generates both).

use okm_core::bridge::tooling::parquet_io;
use okm_core::{Collection, Document, FjallStore, KeyEncode};
use std::path::Path;

use crate::channel::{CheckpointRow, CursorRow, MemberRow, MessageRow};
use crate::code::CodeRow;
use crate::graph::{EntityRow, Facts};
use crate::log::LogRow;

/// The ns a table declares, read off its prefix — so a dispatcher keyed by name
/// never restates a number the row already owns.
pub const fn ns(prefix: &[u8]) -> u16 {
    u16::from_be_bytes([prefix[0], prefix[1]])
}

/// Every table this service serves, `(name, ns)`, each taken from the row that
/// declares it. This IS the catalog the export ships: one row per table, no
/// second vocabulary to keep in step.
pub const CATALOG: &[(&str, u16)] = &[
    (<MessageRow as Document>::TABLE_NAME, ns(MessageRow::NS_PREFIX)),
    (
        <CheckpointRow as Document>::TABLE_NAME,
        ns(CheckpointRow::NS_PREFIX),
    ),
    (<CursorRow as Document>::TABLE_NAME, ns(CursorRow::NS_PREFIX)),
    (<MemberRow as Document>::TABLE_NAME, ns(MemberRow::NS_PREFIX)),
    (<CodeRow as Document>::TABLE_NAME, ns(CodeRow::NS_PREFIX)),
    (<EntityRow as Document>::TABLE_NAME, ns(EntityRow::NS_PREFIX)),
    (<Facts as Document>::TABLE_NAME, ns(Facts::NS_PREFIX)),
    (<LogRow as Document>::TABLE_NAME, ns(LogRow::NS_PREFIX)),
];

/// The derived reduce tables (ADR-0034 §5). A reduce is not a row, so it is not
/// a table's file; it is named beside the table it belongs to.
pub const LOG_REDUCES: &str = "log__reduces";
pub const CODE_REDUCES: &str = "code__reduces";

/// The table's name for an ns, from the catalog — what the graph's edge export
/// needs to write `(table, key)` endpoints.
pub fn name_of(ns: u16) -> Option<String> {
    CATALOG
        .iter()
        .find(|(_, n)| *n == ns)
        .map(|(name, _)| (*name).to_string())
}

/// Write the catalog: `(table, ns)` per row.
pub fn catalog(path: &Path) -> Result<(), String> {
    parquet_io::catalog_parquet(CATALOG, path).map_err(|e| format!("catalog export: {e}"))
}

/// One table, one file, WITH the dynamic segment as a `variant` column — the
/// open half of every row, which a plain declaration export silently drops.
pub fn table<K, R>(
    collection: &mut Collection<FjallStore, K, R>,
    path: &Path,
) -> Result<(), String>
where
    K: KeyEncode,
    R: Document<Key = K>,
{
    parquet_io::export_parquet_with_variant(collection, path)
        .map_err(|e| format!("parquet export: {e}"))
}

/// An already-built batch (the graph's edge table) to a file.
pub fn batch(batch: &okm_core::RecordBatch, path: &Path) -> Result<(), String> {
    parquet_io::write_batch(batch, path).map_err(|e| format!("parquet export: {e}"))
}

/// A reduce declaration's entries as the derived table
/// `(table, reduce, group, value)`.
pub fn reduces<A: okm_core::model::reduce::Reduce>(
    store: &FjallStore,
    table_ns: u16,
    table: &str,
    path: &Path,
) -> Result<(), String> {
    parquet_io::export_reduces::<_, A>(store, &table_ns.to_be_bytes(), table, path)
        .map_err(|e| format!("reduce export: {e}"))
}
