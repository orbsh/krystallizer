//! Phase 0 smoke: one flat-memory table over Fjall (ADR-0002), with a
//! brute-force substring search (ADR-0003 bring-up stance; BM25/arroy land
//! in Phase 1). Proves the OKM RowEncode -> Table -> FjallStore channel
//! end to end.

use okm_core::{FjallStore, KeyEncode, RowEncode};

/// Flat memory identity: user-scoped surrogate id.
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[kv_ns(32)]
pub struct MemoryKey {
    pub user_id: u64,
    pub id: u64,
}

/// Flat memory row: free text plus a monotonic insert counter.
#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(MemoryKey)]
pub struct MemoryRow {
    pub text: String,
    pub created_at: u64,
}

/// Assembly point: one Fjall keyspace holding the memory table.
pub struct MemoryStore {
    // Ownership of the underlying Database lives with the FjallStore
    // instance bound into the table (single store = atomicity boundary).
    table: okm_core::Table<FjallStore, MemoryKey, MemoryRow>,
    next_id: u64,
}

impl MemoryStore {
    /// Open (or create) the memory keyspace at `path`.
    pub fn open(path: &std::path::Path) -> Result<Self, fjall::Error> {
        let store = FjallStore::open(path, "memories")?;
        Ok(Self {
            // Table::new takes the store by value; ns matches #[kv_ns(32)].
            table: okm_core::Table::new(store, 32),
            next_id: 1,
        })
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

    /// Brute-force search: scan the user's memories, keep rows whose text
    /// contains the query. Order = key order (insert order by id).
    pub fn search(&self, user_id: u64, query: &str) -> Vec<(u64, String)> {
        if query.is_empty() {
            return Vec::new();
        }
        self.table
            .scan_keys()
            .into_iter()
            .filter(|k| k.user_id == user_id)
            .filter_map(|k| {
                let row = self.table.get(&k)?;
                row.text
                    .contains(query)
                    .then(|| (k.id, row.text.clone()))
            })
            .collect()
    }

    /// Flush to disk (fjall Buffer mode by default).
    pub fn persist(&self) -> Result<(), fjall::Error> {
        self.table.store().persist()
    }
}
