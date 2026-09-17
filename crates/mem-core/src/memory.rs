//! Phase 0 smoke: one flat-memory table over Fjall (ADR-0002), with a
//! brute-force substring search (ADR-0003 bring-up stance; BM25/arroy land
//! in Phase 1). Proves the OKM DocumentEncode -> Table -> FjallStore channel
//! end to end.

use okm_core::{FjallStore, KeyEncode, Document, DocumentEncode, VirtualStorage};

/// Flat memory identity: user-scoped surrogate id.
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[ok_ns(32)]
pub struct MemoryKey {
    pub user_id: u64,
    pub id: u64,
}

/// Flat memory row: free text plus a monotonic insert counter.
///
/// The ns is declared here (#[ok_ns]) — the row is the table's
/// declaration point; Table::new no longer takes an ns argument.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(MemoryKey)]
#[ok_ns(32)]
pub struct MemoryRow {
    pub text: String,
    pub created_at: u64,
}

/// Per-user counts plus the table-wide total.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct MemoryStats {
    pub total: u64,
    pub per_user: std::collections::HashMap<u64, u64>,
}

/// Assembly point: one Fjall keyspace holding the memory table.
pub struct MemoryStore {
    // Ownership of the underlying Database lives with the FjallStore
    // instance bound into the table (single store = atomicity boundary).
    table: okm_core::Collection<FjallStore, MemoryKey, MemoryRow>,
    next_id: u64,
}

impl MemoryStore {
    /// Open (or create) the memory keyspace at `path`.
    ///
    /// `next_id` is recovered by scanning existing keys: ids are
    /// per-user surrogate keys, so a fresh process must not hand out
    /// an id that collides with (and silently overwrites) a stored
    /// row. Empty table → counter starts at 1.
    pub fn open(path: &std::path::Path) -> Result<Self, fjall::Error> {
        let store = FjallStore::open(path, "memories")?;
        // ns comes from the row's #[ok_ns(32)] — not a constructor arg.
        let table = <MemoryRow as okm_core::Document>::table(store);
        // Key layout is [user_id 8B BE][id 8B BE], so the last key in
        // scan order carries the highest id seen (across any user).
        let next_id = table
            .scan_keys()
            .last()
            .map_or(1, |k| k.id.saturating_add(1));
        Ok(Self { table, next_id })
    }

    /// Store one memory for `user_id`; returns the assigned id.
    pub fn store(&mut self, user_id: u64, text: &str, created_at: u64) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let key = MemoryKey { user_id, id };
        let row = MemoryRow {
            text: text.to_string(),
            created_at,
        };
        self.table.put(&key, &row);
        id
    }

    /// Brute-force substring search: scan the user's memories, keep rows
    /// whose text contains the query. Order = key order (insert order
    /// by id). The scan is user-prefixed: only the user's key range is
    /// read, never the whole table.
    pub fn search(&self, user_id: u64, query: &str) -> Vec<(u64, String)> {
        if query.is_empty() {
            return Vec::new();
        }
        let (prefix, prefix_len) = self.user_prefix(user_id);
        self.table
            .store()
            .scan_suffix(&prefix)
            .iter()
            .filter_map(|sfx| {
                // suffix = `[id 8B]` (after `[ns 2B][slot][user_id 8B]`);
                // decode via full-key reconstruction.
                let mut full = prefix.clone();
                full.extend_from_slice(sfx);
                let key = MemoryKey::decode(&full[3..]);
                let row = self.table.get(&key)?;
                row.text
                    .contains(query)
                    .then(|| (key.id, row.text.clone()))
            })
            .collect()
    }

    /// Remove one memory by id. Returns whether a row was deleted.
    pub fn forget(&mut self, user_id: u64, id: u64) -> bool {
        let key = MemoryKey { user_id, id };
        self.table.delete_by_pkey(&key)
    }

    /// All of a user's memories, key order (insert order by id).
    pub fn list(&self, user_id: u64) -> Vec<(u64, String)> {
        let (prefix, prefix_len) = self.user_prefix(user_id);
        self.table
            .store()
            .scan_suffix(&prefix)
            .iter()
            .filter_map(|sfx| {
                let mut full = prefix.clone();
                full.extend_from_slice(sfx);
                let key = MemoryKey::decode(&full[3..]);
                let row = self.table.get(&key)?;
                Some((key.id, row.text.clone()))
            })
            .collect()
    }

    /// Per-user counts plus the table-wide total (dashboards/ops; not
    /// an agent-facing query path).
    pub fn stats(&self) -> MemoryStats {
        let mut stats = MemoryStats::default();
        for key in self.table.scan_keys() {
            stats.total += 1;
            *stats.per_user.entry(key.user_id).or_insert(0) += 1;
        }
        stats
    }

    /// Flush to disk (fjall Buffer mode by default).
    pub fn persist(&self) -> Result<(), fjall::Error> {
        self.table.store().persist()
    }

    /// Test/probe access to the underlying engine (scan-byte debugging).
    pub fn store_ref(&self) -> &okm_core::FjallStore {
        self.table.store()
    }

    /// Scan prefix for one user's primary entries:
    /// `[ns 2B][slot 0][user_id 8B]`. Returns the prefix bytes and the
    /// header length (3) — the key payload starts right after it, so a
    /// suffix byte range reconstructs to `MemoryKey::decode(&full[3..])`.
    fn user_prefix(&self, user_id: u64) -> (Vec<u8>, usize) {
        let mut prefix = Vec::with_capacity(3 + 8);
        prefix.extend_from_slice(MemoryRow::NS_PREFIX);
        prefix.push(okm_core::index::PRIMARY_SLOT);
        let probe = MemoryKey { user_id, id: 0 };
        probe.encode_prefix_named(&mut prefix, &["user_id"]);
        let payload_len = prefix.len() - 3;
        (prefix, payload_len)
    }
}
