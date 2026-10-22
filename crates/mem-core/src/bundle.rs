//! The backup BUNDLE (ADR-0013): one tar.zst whose content is a small
//! warehouse — `<root>/<table>/data/<table>.parquet` beside each table's
//! Iceberg metadata, plus the catalog. One file to move, many Parquet files to
//! query once it is unpacked, and no k10r in the loop to read it.
//!
//! The split with okm is deliberate: okm writes Parquet and the Iceberg
//! metadata BESIDE the files it was given (it knows declarations, not
//! archives), and this module only arranges directories and compresses them.
//! So a compression or container change never reaches the schema layer, and a
//! schema change never reaches the archive layer.

use std::path::{Path, PathBuf};

/// The one directory inside the archive: unpacking it into a warehouse root
/// gives `krystallizer/<table>/...`, which is what a reader is pointed at.
pub const BUNDLE_DIR: &str = "krystallizer";

/// The catalog lives at the root of that directory (`table`, `ns`), beside the
/// tables it names.
pub const CATALOG_FILE: &str = "catalog.parquet";

/// Where one table's Parquet file goes inside the bundle.
pub fn table_file(root: &Path, table: &str) -> PathBuf {
    root.join(table)
        .join("data")
        .join(format!("{table}.parquet"))
}

/// Where one table's Iceberg metadata goes (okm writes into it).
pub fn table_dir(root: &Path, table: &str) -> PathBuf {
    root.join(table)
}

/// The order a restore walks: every catalogued table in DECLARATION order
/// (messages, checkpoints, cursors, members, code, entities, facts, log,
/// community_seq), then the derived reduce tables. Nodes and people land before
/// the edges that name them, and a watermark lands after the rows it counts —
/// so a caller never has to know the dependency order.
pub fn import_order() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = crate::export::CATALOG.iter().map(|(n, _)| *n).collect();
    names.push(crate::export::LOG_REDUCES);
    names.push(crate::export::CODE_REDUCES);
    names
}

/// Seal a directory as one `tar.zst`. The archive carries `root`'s ENTRIES
/// under [`BUNDLE_DIR`], so the file is self-describing when unpacked.
pub fn seal(root: &Path, archive: &Path) -> Result<(), String> {
    let file = std::fs::File::create(archive)
        .map_err(|e| format!("creating {}: {e}", archive.display()))?;
    // Level 3: Parquet is already compressed, so the archive is mostly
    // metadata; a higher level buys nothing but latency.
    let mut encoder = zstd::Encoder::new(file, 3).map_err(|e| format!("zstd: {e}"))?;
    {
        let mut tar = tar::Builder::new(&mut encoder);
        tar.append_dir_all(BUNDLE_DIR, root)
            .map_err(|e| format!("packing {}: {e}", root.display()))?;
        tar.finish()
            .map_err(|e| format!("packing {}: {e}", root.display()))?;
    }
    encoder.finish().map_err(|e| format!("zstd: {e}"))?;
    Ok(())
}

/// Unpack a bundle into `dest` and answer the warehouse root inside it.
///
/// Entries that would land outside `dest` (an absolute path, a `..` component)
/// are refused by the archive layer (`unpack_in` reports them, never follows
/// them) and counted as an error here: a backup is k10r's own file, so a
/// traversal-shaped entry is a corrupt or hostile archive, not a corner case.
pub fn unpack(archive: &Path, dest: &Path) -> Result<PathBuf, String> {
    let file =
        std::fs::File::open(archive).map_err(|e| format!("opening {}: {e}", archive.display()))?;
    let decoder = zstd::Decoder::new(file).map_err(|e| format!("zstd: {e}"))?;
    let mut tar = tar::Archive::new(decoder);
    let entries = tar.entries().map_err(|e| format!("tar: {e}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("tar: {e}"))?;
        let path = entry.path().map_err(|e| format!("tar: {e}"))?.into_owned();
        let inside = entry
            .unpack_in(dest)
            .map_err(|e| format!("unpacking {}: {e}", path.display()))?;
        if !inside {
            return Err(format!(
                "archive entry `{}` would land outside the restore directory — refused",
                path.display()
            ));
        }
    }
    let root = dest.join(BUNDLE_DIR);
    if !root.is_dir() {
        return Err(format!(
            "the archive has no `{BUNDLE_DIR}/` directory — this is not a krystallizer bundle"
        ));
    }
    Ok(root)
}
