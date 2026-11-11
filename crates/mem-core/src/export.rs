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

use okm_core::{Collection, Document, FjallStore, KeyEncode};
use okm_lake::snapshot::parquet_io;

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

/// One table written as Parquet bytes, with the columns that identify a row.
///
/// A lake may be a bucket, so the export stops at bytes: whoever holds the lake decides
/// where they land (ADR-0014). The open half of every row travels as the `variant`
/// column — a plain declaration export silently drops it.
pub struct Export {
    pub bytes: Vec<u8>,
    pub keys: Vec<String>,
}

/// The key columns a row declaration is identified by, in declaration order. Read
/// off the declaration itself, so the Delta metadata and the Parquet file beside it
/// cannot disagree about what a key is.
pub fn keys_of<R: Document>() -> Vec<String> {
    okm_core::schema::CollectionSchema::of::<R::Key, R>()
        .key_fields
        .iter()
        .map(|f| f.name.clone())
        .collect()
}

/// One table as Parquet bytes.
pub fn table_bytes<K, R>(collection: &mut Collection<FjallStore, K, R>) -> Result<Export, String>
where
    K: KeyEncode,
    R: Document<Key = K>,
{
    let bytes = parquet_io::export_parquet_with_variant_bytes(collection)
        .map_err(|e| format!("parquet export: {e}"))?;
    Ok(Export {
        bytes,
        keys: keys_of::<R>(),
    })
}

/// An already-built batch (the graph's edge table) as Parquet bytes. An edge's identity
/// is the triple the edge set is keyed by, which this layer does not restate: the Delta
/// metadata names no key column, and a reader that wants to fold it must be told.
pub fn batch_bytes(batch: &arrow::array::RecordBatch) -> Result<Export, String> {
    let bytes = parquet_io::batch_bytes(batch).map_err(|e| format!("parquet export: {e}"))?;
    Ok(Export {
        bytes,
        keys: Vec::new(),
    })
}

/// A reduce declaration's entries as the derived table
/// `(table, reduce, group, value)`, as Parquet bytes.
pub fn reduces_bytes<A: okm_core::model::reduce::Reduce>(
    store: &FjallStore,
    table_ns: u16,
    table: &str,
) -> Result<Export, String> {
    let bytes = parquet_io::export_reduces_bytes::<_, A>(store, &table_ns.to_be_bytes(), table)
        .map_err(|e| format!("reduce export: {e}"))?;
    Ok(Export {
        bytes,
        keys: Vec::new(),
    })
}
