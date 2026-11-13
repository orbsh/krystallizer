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

/// Land batches into an already-rooted collection: the restore's write seam
/// (ADR-0026). The caller hands the collection it means — `collection.rebased(ns)`
/// for a mapped table — so this function never names a key or row type.
pub fn land_into<S, K, R>(
    collection: &mut Collection<S, K, R>,
    batches: &[arrow::array::RecordBatch],
) -> Result<usize, String>
where
    S: okm_core::engine::storage::VirtualStorage,
    K: KeyEncode,
    R: Document<Key = K>,
{
    let mut rows = 0;
    for batch in batches {
        rows += parquet_io::import_parquet_batch(collection, batch)
            .map_err(|e| format!("restoring the table: {e}"))?;
    }
    Ok(rows)
}

/// Whether the namespace this handle would write into is untouched: one key
/// under its header is enough to refuse a restore (ADR-0026: a restore only goes
/// into an empty namespace). Faces, index entries and reduce rows share the
/// header, so this sees them too, not just the rows a scan of documents finds.
pub fn ns_is_empty_into<S, K, R>(collection: &Collection<S, K, R>) -> Result<bool, String>
where
    S: okm_core::engine::storage::VirtualStorage,
    K: KeyEncode,
    R: Document<Key = K>,
{
    let prefix = collection.header();
    let end = okm_core::engine::storage::prefix_end(&prefix);
    // One key under the header is enough, so the FIRST row is the whole answer — the
    // separate streaming entry (`scan_range_iter`) is gone from okm's storage trait and
    // `scan_range` is the one shape every backend answers.
    let rows = collection
        .store()
        .scan_range(&prefix, end.as_deref())
        .map_err(|e| e.to_string())?;
    Ok(rows.is_empty())
}

/// Land a reduce table's entries at `ns`: each row is `(reduce slot, group,
/// value)` and the key is `[ns 2B][slot][group]` — the same shape the reducer
/// writes, so a restored watermark counts what the restored rows already hold.
pub fn land_reduce<S>(
    store: &S,
    ns: u16,
    batches: &[arrow::array::RecordBatch],
) -> Result<usize, String>
where
    S: okm_core::engine::storage::VirtualStorage,
{
    let mut rows = 0usize;
    for batch in batches {
        let slot_col = batch
            .column_by_name("reduce")
            .and_then(|c| c.as_any().downcast_ref::<arrow::array::UInt16Array>())
            .ok_or_else(|| "a reduce table needs a `reduce` column".to_string())?;
        let group_col = batch
            .column_by_name("group")
            .and_then(|c| c.as_any().downcast_ref::<arrow::array::BinaryArray>())
            .ok_or_else(|| "a reduce table needs a `group` column".to_string())?;
        let value_col = batch
            .column_by_name("value")
            .and_then(|c| c.as_any().downcast_ref::<arrow::array::BinaryArray>())
            .ok_or_else(|| "a reduce table needs a `value` column".to_string())?;
        for row in 0..batch.num_rows() {
            let mut key = ns.to_be_bytes().to_vec();
            okm_core::model::index::write_slot(&mut key, slot_col.value(row));
            key.extend_from_slice(group_col.value(row));
            store
                .put(key, value_col.value(row).to_vec())
                .map_err(|e| format!("landing a reduce entry: {e}"))?;
            rows += 1;
        }
    }
    Ok(rows)
}

// ---- the edge set, one table per endpoint pair (ADR-0027) -----------------

/// The vertex tables an edge can name. Each is a table a property-graph
/// declaration roots at, and the columns below are the key it is rootable by.
pub const VERTEX_TABLES: &[&str] = &["entities", "members"];

/// The endpoint pairs an edge table is split by. A property graph's edge table
/// references ONE vertex table at each end (`SOURCE KEY (…) REFERENCES v (…)`),
/// so an edge set that spans vertex tables has to be one table per pair.
///
/// The list is the vertex tables crossed with themselves, written out: a pair
/// that holds no edge is published as an empty table, which is what makes the
/// published set a function of the declaration rather than of today's rows.
pub const EDGE_PAIRS: &[(&str, &str)] = &[
    ("entities", "entities"),
    ("entities", "members"),
    ("members", "entities"),
    ("members", "members"),
];

/// The lake table name for one endpoint pair: `facts__entities__members`. The
/// base table's name stays the prefix, so a reader (and the restore) can find
/// the edge set without a second catalog.
pub fn edge_table_name(src: &str, dst: &str) -> String {
    format!("{}__{src}__{dst}", <Facts as Document>::TABLE_NAME)
}

/// The pair a lake table name stands for, if it is an edge table.
pub fn edge_pair_of(name: &str) -> Option<(&str, &str)> {
    let rest = name
        .strip_prefix(<Facts as Document>::TABLE_NAME)?
        .strip_prefix("__")?;
    rest.split_once("__")
}

/// The lake table name's base table: `facts__entities__members` is `facts`.
pub fn base_table(name: &str) -> &str {
    name.split("__").next().unwrap_or(name)
}

/// One endpoint pair's slice of the edge set, as a table a property graph can
/// reference: the canonical edge batch's rows for that pair, with the pair's
/// vertex key DECODED into typed columns (`src_scope`, `src_node_id`, …).
///
/// The raw `src_pkey` columns stay. A restore needs the exact bytes, and keeping
/// them means no reader has to learn the key codec to join an edge to a node.
pub fn edge_batch(
    batch: &arrow::array::RecordBatch,
    src: &str,
    dst: &str,
) -> Result<arrow::array::RecordBatch, String> {
    use arrow::array::{Array, ArrayRef, BinaryArray, StringArray, UInt32Array};
    use arrow::datatypes::{Field, Schema};

    let names = |col: &str| -> Result<&StringArray, String> {
        batch
            .column_by_name(col)
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
            .ok_or_else(|| format!("the edge batch has no `{col}` column"))
    };
    let (srcs, dsts) = (names("src_table")?, names("dst_table")?);
    let keep: Vec<u32> = (0..batch.num_rows())
        .filter(|row| {
            srcs.is_valid(*row)
                && dsts.is_valid(*row)
                && srcs.value(*row) == src
                && dsts.value(*row) == dst
        })
        .map(|row| row as u32)
        .collect();
    let idx = UInt32Array::from(keep);
    let mut cols: Vec<ArrayRef> = batch
        .columns()
        .iter()
        .map(|c| arrow::compute::take(c.as_ref(), &idx, None).map_err(|e| e.to_string()))
        .collect::<Result<_, _>>()?;

    let index_of = |col: &str| -> Result<usize, String> {
        batch
            .schema()
            .index_of(col)
            .map_err(|_| format!("the edge batch has no `{col}` column"))
    };
    let (src_keys, dst_keys) = (
        cols[index_of("src_pkey")?]
            .as_any()
            .downcast_ref::<BinaryArray>()
            .ok_or("`src_pkey` is not a raw-key column")?,
        cols[index_of("dst_pkey")?]
            .as_any()
            .downcast_ref::<BinaryArray>()
            .ok_or("`dst_pkey` is not a raw-key column")?,
    );
    let mut typed: Vec<(Field, ArrayRef)> = Vec::new();
    endpoint_columns(src, src_keys, "src", &mut typed)?;
    endpoint_columns(dst, dst_keys, "dst", &mut typed)?;

    // The typed columns go before the variant (the last column): the endpoint
    // columns stay a block, and nothing depends on the tail's position.
    let mut fields: Vec<Field> = batch
        .schema()
        .fields()
        .iter()
        .map(|f| f.as_ref().clone())
        .collect();
    let mut arrays = cols.split_off(0);
    let tail = arrays
        .pop()
        .ok_or("an edge batch has at least one column")?;
    let variant_field = fields.pop().ok_or("an edge batch has at least one field")?;
    for (field, array) in typed {
        fields.push(field);
        arrays.push(array);
    }
    fields.push(variant_field);
    arrays.push(tail);
    let schema = std::sync::Arc::new(Schema::new(fields));
    arrow::array::RecordBatch::try_new(schema, arrays).map_err(|e| e.to_string())
}

/// One endpoint's key, decoded into the columns its vertex table declares.
fn endpoint_columns(
    vertex: &str,
    keys: &arrow::array::BinaryArray,
    prefix: &str,
    out: &mut Vec<(
        arrow::datatypes::Field,
        std::sync::Arc<dyn arrow::array::Array>,
    )>,
) -> Result<(), String> {
    use arrow::array::{Array, StringBuilder, UInt64Builder};
    use arrow::datatypes::{DataType, Field};

    match vertex {
        "entities" => {
            let (mut scope, mut node) = (UInt64Builder::new(), UInt64Builder::new());
            for row in 0..keys.len() {
                let key = <crate::graph::EntityKey as KeyEncode>::decode(keys.value(row));
                scope.append_value(key.scope);
                node.append_value(key.node_id);
            }
            out.push((
                Field::new(format!("{prefix}_scope"), DataType::UInt64, false),
                std::sync::Arc::new(scope.finish()),
            ));
            out.push((
                Field::new(format!("{prefix}_node_id"), DataType::UInt64, false),
                std::sync::Arc::new(node.finish()),
            ));
        }
        "members" => {
            let mut member = StringBuilder::new();
            for row in 0..keys.len() {
                let key = <crate::channel::MemberKey as KeyEncode>::decode(keys.value(row));
                // The vertex table writes a uuid as hyphenated text, and an edge
                // column that spells it differently joins nothing.
                member.append_value(key.member_id.to_string());
            }
            out.push((
                Field::new(format!("{prefix}_member_id"), DataType::Utf8, false),
                std::sync::Arc::new(member.finish()),
            ));
        }
        other => return Err(format!("no vertex table `{other}` in this service")),
    }
    Ok(())
}

/// The lake tables this service publishes: the catalog, with the edge set
/// expanded into one table per endpoint pair (ADR-0027). The base `facts` name
/// is not a lake table — it is the edge set's name, not one of its files.
pub fn lake_table_names() -> Vec<String> {
    let mut names = Vec::new();
    for &(name, _) in CATALOG {
        if name == <Facts as Document>::TABLE_NAME {
            for (src, dst) in EDGE_PAIRS {
                names.push(edge_table_name(src, dst));
            }
        } else {
            names.push(name.to_string());
        }
    }
    names
}
