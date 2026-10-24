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
use crate::graph::{CommunitySeqRow, EntityRow, Facts};
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
    (
        <MessageRow as Document>::TABLE_NAME,
        ns(MessageRow::NS_PREFIX),
    ),
    (
        <CheckpointRow as Document>::TABLE_NAME,
        ns(CheckpointRow::NS_PREFIX),
    ),
    (
        <CursorRow as Document>::TABLE_NAME,
        ns(CursorRow::NS_PREFIX),
    ),
    (
        <MemberRow as Document>::TABLE_NAME,
        ns(MemberRow::NS_PREFIX),
    ),
    (<CodeRow as Document>::TABLE_NAME, ns(CodeRow::NS_PREFIX)),
    (
        <EntityRow as Document>::TABLE_NAME,
        ns(EntityRow::NS_PREFIX),
    ),
    (<Facts as Document>::TABLE_NAME, ns(Facts::NS_PREFIX)),
    (<LogRow as Document>::TABLE_NAME, ns(LogRow::NS_PREFIX)),
    (
        <CommunitySeqRow as Document>::TABLE_NAME,
        ns(CommunitySeqRow::NS_PREFIX),
    ),
];

/// The derived reduce tables (ADR-0034 §5). A reduce is not a row, so it is not
/// a table's file; it is named beside the table it belongs to.
pub const LOG_REDUCES: &str = "log__reduces";
pub const CODE_REDUCES: &str = "code__reduces";

/// The ns for a table's NAME — the import's direction of `name_of`, read off
/// the same catalog, so a restore cannot address a table the export does not
/// name.
pub fn ns_of(name: &str) -> Option<u16> {
    CATALOG.iter().find(|(n, _)| *n == name).map(|(_, ns)| *ns)
}

/// Restore one table from a Parquet file through okm's import (the normal write
/// contract: primary key + index entries + reduces). Returns the rows written.
pub fn import_table<K, R>(
    collection: &mut okm_core::Collection<FjallStore, K, R>,
    path: &std::path::Path,
) -> Result<usize, String>
where
    K: KeyEncode,
    R: Document<Key = K>,
{
    okm_core::bridge::tooling::parquet_io::import_parquet(collection, path)
        .map_err(|e| format!("parquet import: {e}"))
}

/// Run the restore's counter reseed only when the table landed: a failed import
/// leaves the store where it was, and the issuers must not move either.
pub(crate) fn finish_import(
    out: Result<usize, String>,
    reseed: impl FnOnce() -> Result<(), String>,
) -> Result<usize, String> {
    let rows = out?;
    // The reseed reads the store back (watermark + key scan): a failure there
    // means the imported rows are in place but the issuer did not learn where
    // they stop, which the caller has to hear about.
    reseed()?;
    Ok(rows)
}

/// The table's name for an ns, from the catalog — what the graph's edge export
/// needs to write `(table, key)` endpoints.
pub fn name_of(ns: u16) -> Option<String> {
    CATALOG
        .iter()
        .find(|(_, n)| *n == ns)
        .map(|(name, _)| (*name).to_string())
}

/// A filename-safe UTC stamp (`20261022T143012Z`) for the bundle's name: the
/// file says WHEN it was taken without anyone opening it.
pub fn stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = crate::export::civil(secs);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

/// `(year, month, day, hour, minute, second)` in UTC, from a Unix second count —
/// days-to-civil, so no date crate rides along for a filename.
fn civil(secs: u64) -> (i64, u64, u64, u64, u64, u64) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u64;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u64;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// One exported table's Iceberg metadata beside its data file — the call the
/// bundle makes after each table's Parquet lands. okm reads the file's own
/// columns and row count; nothing here restates a schema.
pub fn write_metadata(table_dir: &std::path::Path, file: &std::path::Path) -> Result<(), String> {
    okm_core::bridge::iceberg::write_table_metadata(table_dir, &[file.to_path_buf()])
        .map(|_| ())
        .map_err(|e| format!("iceberg metadata for {}: {e}", file.display()))
}

/// Write the catalog: `(table, ns)` per row.
pub fn catalog(path: &Path) -> Result<(), String> {
    parquet_io::catalog_parquet(CATALOG, path).map_err(|e| format!("catalog export: {e}"))
}

/// One table, one file, WITH the dynamic segment as a `variant` column — the
/// open half of every row, which a plain declaration export silently drops.
pub fn table<K, R>(collection: &mut Collection<FjallStore, K, R>, path: &Path) -> Result<(), String>
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
