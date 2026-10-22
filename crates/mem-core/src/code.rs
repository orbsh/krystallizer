//! The code store (gravity ADR-0001/0003): the delivered script's home.
//!
//! One entity class = one ns; a code row is identity + what the code IS
//! (language, description, parameter definition) + its source. Identity
//! is a bare surrogate id — no name, no content hash: the caller
//! re-reads the row on every call and the source travels as a call
//! argument, so there is no cache to stale (gravity ADR-0003, "Code
//! identity").
//!
//! **A row is a reusable piece of code, not a skill.** A login script is
//! written once, and many skills carry a step pointing at it. That is why
//! the row carries a description and the parameter definition: whoever
//! links a skill to this code — or reads the graph afterwards — decides
//! from the row, instead of reading every script's source to find out
//! what it takes. Neither field is an LLM search surface: what a skill IS
//! lives in the graph (a structured node: preconditions / steps /
//! experience / verification).
//!
//! The language is DATA ON THE ROW — a script is data, and data has a
//! language. Nothing defaults it; the caller routes the call to the mount
//! serving that language, and a language with no mount is the caller's
//! error value, never a fallback here.
//!
//! gravity is the only client (ADR-0001: the node never talks to
//! krystallizer; the byte path is gravity → control path → node).

use okm_core::{Document, DocumentEncode, FjallStore, KeyEncode, ReduceCodec};

/// Code identity: a surrogate id, nothing else (no name, no hash —
/// gravity ADR-0003).
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[ok_ns(35)]
pub struct CodeKey {
    pub code_id: u64,
}

/// One piece of deliverable code: what it is, and its source.
///
/// `parameters` is the parameter definition, carried as a JSON document
/// in TEXT — the store does not parse it (storage stores, the reader
/// interprets), and the caller that delivered the code owns its shape.
/// `description` is the reference a skill-to-code link is made from.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(CodeKey)]
#[ok_ns(35)]
#[ok_name("code")]
// The surrogate counter is a WATERMARK (ADR-0023/0025, PLAN 2.9d): the highest
// `code_id` ever written, maintained on every write, unfold a NO-OP. That is
// exactly what an id allocator wants — a deleted row must not free its id for
// reuse — and it is one point read at open instead of a scan of the last key.
#[ok_reduce(HighWater(code_id))]
pub struct CodeRow {
    pub language: String,
    pub description: String,
    pub parameters: String,
    pub source: String,
}

/// Assembly point: the code table on a store handle.
///
/// This group lives in `crate::KEYSPACE_MEMORY` — the SAME keyspace as
/// flat memory (and, later, the graph and the skills): different entity
/// classes, different ns, one tree.
pub struct CodeStore {
    table: okm_core::Collection<FjallStore, CodeKey, CodeRow>,
    next_id: u64,
}

impl CodeStore {
    /// Open (or create) the group's keyspace at `path` and attach the
    /// code table — the standalone form (tests, single-table tools). A
    /// process serving several groups uses `crate::service::open_service`
    /// (one database, one keyspace per group), or attaches both groups to
    /// a handle it opened with [`CodeStore::wrap`].
    pub fn open(path: &std::path::Path) -> Result<Self, fjall::Error> {
        Ok(Self::wrap(FjallStore::open(path, crate::KEYSPACE_MEMORY)?))
    }

    /// Attach the code table to an already-open store handle.
    pub fn wrap(store: FjallStore) -> Self {
        // Surrogate ids are assigned here, so a fresh process must not hand out
        // an id that collides with (and silently overwrites) a stored row. The
        // watermark answers that in one point read: it is the highest `code_id`
        // EVER written, so a withdrawn row does not free its id either. No
        // watermark yet (a fresh table) → the counter starts at 1.
        let water = okm_core::reduce::scan_reduces::<_, __OkmReduce_CodeRow_0>(
            &store,
            <CodeRow as Document>::NS_PREFIX,
        )
        .into_iter()
        .next()
        .map(|(_, acc)| acc);
        let table = <CodeRow as Document>::collection(store);
        let next_id = water.map_or(1, |w| w.saturating_add(1));
        Self { table, next_id }
    }

    /// Store one piece of code; returns the assigned id. Every field is
    /// stored verbatim — the store does not police them (storage stores;
    /// the caller routes, parses and decides).
    pub fn put(
        &mut self,
        language: &str,
        description: &str,
        parameters: &str,
        source: &str,
    ) -> u64 {
        let code_id = self.next_id;
        self.next_id += 1;
        self.table.put(
            &CodeKey { code_id },
            &CodeRow {
                language: language.to_string(),
                description: description.to_string(),
                parameters: parameters.to_string(),
                source: source.to_string(),
            },
        );
        code_id
    }

    /// One piece of code by id — the read gravity makes at call time.
    /// Unknown id = absent, not an error (the caller's error value, its
    /// choice).
    pub fn get(&self, code_id: u64) -> Option<CodeRow> {
        self.table.get(&CodeKey { code_id })
    }

    /// This store's table, `(name, ns)` — the name from the row declaration.
    pub fn tables(&self) -> Vec<(&'static str, u16)> {
        vec![(
            <CodeRow as Document>::TABLE_NAME,
            crate::export::ns(CodeRow::NS_PREFIX),
        )]
    }

    /// Export the table by NAME as Parquet (ADR-0007's backup boundary).
    /// `None` = not this store's table.
    pub fn export(&mut self, name: &str, path: &std::path::Path) -> Option<Result<(), String>> {
        (name == <CodeRow as Document>::TABLE_NAME)
            .then(|| crate::export::table(&mut self.table, path))
    }

    /// Restore one table by NAME from a Parquet file — the mirror of `export`.
    /// The code table lands through the normal write contract, the reduce table
    /// pushes the id watermark, and the in-process issuer is then recomputed
    /// from the store: a restore that skipped it would hand out an id a
    /// restored row already holds.
    pub fn import(&mut self, name: &str, path: &std::path::Path) -> Option<Result<usize, String>> {
        if name == <CodeRow as Document>::TABLE_NAME {
            let out = crate::export::import_table(&mut self.table, path);
            Some(crate::export::finish_import(out, || self.reseed()))
        } else if name == crate::export::CODE_REDUCES {
            let out =
                okm_core::bridge::tooling::parquet_io::import_reduces::<_, __OkmReduce_CodeRow_0>(
                    self.table.store(),
                    <CodeRow as Document>::NS_PREFIX,
                    <CodeRow as Document>::TABLE_NAME,
                    path,
                )
                .map_err(|e| format!("reduce import: {e}"));
            Some(crate::export::finish_import(out, || self.reseed()))
        } else {
            None
        }
    }

    /// Recompute the id issuer from the store — the open path, reused after a
    /// restore wrote rows and a watermark underneath.
    pub fn reseed(&mut self) {
        let water = okm_core::reduce::scan_reduces::<_, __OkmReduce_CodeRow_0>(
            self.table.store(),
            <CodeRow as Document>::NS_PREFIX,
        )
        .into_iter()
        .next()
        .map(|(_, acc)| acc);
        self.next_id = water.map_or(1, |w| w.saturating_add(1));
    }

    /// The id allocator's watermark as the derived reduce table (ADR-0034 §5):
    /// without it a restored table would hand out an id it already used.
    pub fn export_reduces(&self, path: &std::path::Path) -> Result<(), String> {
        crate::export::reduces::<__OkmReduce_CodeRow_0>(
            self.table.store(),
            crate::export::ns(CodeRow::NS_PREFIX),
            <CodeRow as Document>::TABLE_NAME,
            path,
        )
    }
}
