//! Landing a lake back into a store (ADR-0026).
//!
//! The ruling this module implements is narrow on purpose: **a restore only goes
//! into an empty namespace.** There is no merge, no overwrite, no "skip what is
//! there" — a half-restored table is a table whose rows come from two different
//! moments, and no reader can tell which is which.
//!
//! Two shapes, and only two:
//!
//! - **full restore** — no mapping: every table the lake holds lands in the
//!   namespace its declaration names.
//! - **mapped restore** — a mapping of lake table → ns: the named tables land in
//!   the namespaces the caller chose, which is how an application imports part of
//!   a lake into a namespace it just created (and how a corpus moves to a new
//!   home without a second declaration per destination).
//!
//! Either way the emptiness check runs first and covers every namespace the plan
//! touches: one non-empty target aborts the whole restore before anything is
//! written. Reduce tables (a log's `seq` watermark, a code sheet's id watermark)
//! travel with the table they belong to — restoring rows without them hands out
//! ids that already exist.

use std::collections::BTreeMap;

use okm_lake::store::LakeStore;

use crate::export;

/// One table as it landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landed {
    pub table: String,
    pub ns: u16,
    pub rows: usize,
}

/// What a restore is about to do, decided and checked before it writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub table: String,
    pub ns: u16,
    /// The lake files the table's current version names.
    pub files: Vec<String>,
}

/// Refuse a plan whose targets are not all empty: `(table, ns)` per offender.
///
/// Emptiness is asked of the KEY HEADER the table would write — the collection
/// rooted at `ns` — so it sees rows, index entries, faces and reduce entries
/// alike, not just the rows a document scan would find.
pub fn land(
    stores: &mut crate::service::ServiceStores,
    lake: &dyn LakeStore,
    mapping: Option<&BTreeMap<String, u16>>,
) -> Result<Vec<Landed>, String> {
    let plan = plan(&*stores, lake, mapping)?;
    // An edge's endpoints are `[ns 2B]` refs, so the landing needs the DESTINATION
    // ns of every table it can point at — not the declaring one.
    let ns_by_table: BTreeMap<String, u16> = plan
        .iter()
        .map(|entry| (entry.table.clone(), entry.ns))
        .collect();
    let ns_of = |table: &str| -> Option<u16> { ns_by_table.get(table).copied() };
    let mut landed = Vec::with_capacity(plan.len());
    for entry in plan {
        let batches = read_table(lake, &entry)?;
        let rows = stores
            .land(&entry.table, entry.ns, &batches, &ns_of)
            .ok_or_else(|| format!("no store serves the table `{}`", entry.table))??;
        landed.push(Landed {
            table: entry.table,
            ns: entry.ns,
            rows,
        });
    }
    Ok(landed)
}

/// The planned entries, refuse-first: the lake must hold every table the plan
/// names, and every target namespace must be empty.
pub fn plan(
    stores: &crate::service::ServiceStores,
    lake: &dyn LakeStore,
    mapping: Option<&BTreeMap<String, u16>>,
) -> Result<Vec<Entry>, String> {
    let mut plan: Vec<Entry> = Vec::new();
    for &(table, declared_ns) in export::CATALOG {
        if let Some(map) = mapping {
            if !map.contains_key(table) {
                continue;
            }
        }
        let ns = mapping
            .and_then(|map| map.get(table).copied())
            .unwrap_or(declared_ns);
        // The edge set is published as one table per endpoint pair (ADR-0027), so
        // a restore reads them all and lands them into the one graph.
        let mut files: Vec<String> = Vec::new();
        if table == <crate::graph::Facts as okm_core::Document>::TABLE_NAME {
            for (src, dst) in export::EDGE_PAIRS {
                let pair = export::edge_table_name(src, dst);
                for file in okm_lake::delta::files_in_order(lake, &pair)? {
                    files.push(format!("{pair}/{file}"));
                }
            }
        } else {
            for file in okm_lake::delta::files_in_order(lake, table)? {
                files.push(format!("{table}/{file}"));
            }
        }
        if files.is_empty() {
            return Err(format!("the lake holds no `{table}` to restore"));
        }
        plan.push(Entry {
            table: table.to_string(),
            ns,
            files,
        });
    }
    // The reduce tables are not addressable on their own: a watermark belongs to
    // the table it counts, so it lands where that table landed, and it is skipped
    // when the table is.
    for (reduce, parent) in [(export::LOG_REDUCES, "log"), (export::CODE_REDUCES, "code")] {
        let Some(parent_ns) = plan
            .iter()
            .find(|entry| entry.table == parent)
            .map(|entry| entry.ns)
        else {
            continue;
        };
        let files: Vec<String> = okm_lake::delta::files_in_order(lake, reduce)?
            .into_iter()
            .map(|file| format!("{reduce}/{file}"))
            .collect();
        if files.is_empty() {
            continue;
        }
        plan.push(Entry {
            table: reduce.to_string(),
            ns: parent_ns,
            files,
        });
    }

    // Refuse-first: every planned namespace must be empty.
    let mut taken: Vec<(String, u16)> = Vec::new();
    for entry in &plan {
        let empty = stores
            .ns_is_empty(&entry.table, entry.ns)
            .ok_or_else(|| format!("no store serves the table `{}`", entry.table))??;
        if !empty {
            taken.push((entry.table.clone(), entry.ns));
        }
    }
    if !taken.is_empty() {
        let list = taken
            .iter()
            .map(|(table, ns)| format!("{table} in ns {ns}"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "a restore only goes into empty namespaces; these are not empty: {list}"
        ));
    }
    Ok(plan)
}

/// A table's current-state rows, read off the lake.
fn read_table(
    lake: &dyn LakeStore,
    entry: &Entry,
) -> Result<Vec<arrow::array::RecordBatch>, String> {
    let mut batches = Vec::new();
    for file in &entry.files {
        // A file is already a lake key (`facts__entities__members/data/…`): an
        // edge set's files come from four tables, so the key cannot be rebuilt
        // from the entry's base name.
        let key = file.clone();
        let bytes = lake.get(&key).map_err(|e| format!("{key}: {e}"))?;
        batches.extend(
            okm_lake::snapshot::parquet_io::read_batches(bytes::Bytes::from(bytes))
                .map_err(|e| format!("{key}: {e}"))?,
        );
    }
    Ok(batches)
}
