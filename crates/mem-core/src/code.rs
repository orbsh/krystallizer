//! The code rows (gravity ADR-0001/0003, k10r ADR-0012 as amended by ADR-0023):
//! a delivered script is a VERTEX of the fact graph plus a sheet beside it.
//!
//! Why a vertex and not a table of its own — the reason this file changed shape:
//! a task is bound to the code row that finishes it (`task --finishes_on-->
//! code`, gravity ADR-0003 §3). Facts reach entities, so the moment anything has
//! to POINT at a piece of code, that piece of code has to be a node. The
//! alternative — a second endpoint vocabulary, or a binding kept outside the
//! graph — buys nothing and costs a parallel address space.
//!
//! What that means concretely:
//!
//! - The vertex is `kind = "code"` in ns 36, named `code-{id}` where `{id}` is
//!   the code id. The name exists because the graph addresses nodes by name; it
//!   carries no meaning beyond the id, and [`code_id_of_name`] is the only
//!   reader that cares. `code_id` itself is unchanged: a skill's `steps[].code_id`
//!   still holds it, and gravity still addresses code by it.
//! - The SHEET is ns 35, keyed `[scope][code_id]`: language, description,
//!   parameters and source. Presence is the meaning (a node with no sheet is not
//!   code), and the payload stays out of the vertex's props — source text does
//!   not belong in a dynamic segment every reader decodes.
//! - The id allocator is a WATERMARK over the sheet, exactly as it was: the
//!   highest `code_id` ever written, so a deleted row does not free its id. A
//!   fresh table starts at 1.
//!
//! A row is a reusable piece of code, not a skill: a login script is written
//! once and many skills carry a step pointing at it. The row therefore carries a
//! description and the parameter definition, so whoever links a skill to it — or
//! reads the graph afterwards — decides from the row instead of reading every
//! script's source.
//!
//! The language is DATA ON THE ROW — a script is data, and data has a language.
//! Nothing defaults it; the caller routes the call to the mount serving that
//! language, and a language with no mount is the caller's error value, never a
//! fallback here.
//!
//! gravity is the only client (ADR-0001: the node never talks to krystallizer;
//! the byte path is gravity → control path → node).

use okm_core::{Collection, Document, DocumentEncode, FjallStore, KeyEncode, ReduceCodec};

use crate::graph::{GraphStore, IngestError, Term};

/// The kind a code vertex carries. A piece of code IS this kind; the sheet
/// beside it holds the payload (language, description, parameters, source).
pub const CODE_KIND: &str = "code";

/// The vertex name a code id is addressed by. It carries the id and nothing
/// else — the graph needs a name, the caller needs a number.
pub fn code_name(code_id: u64) -> String {
    format!("code-{code_id}")
}

/// The code id a vertex name carries, `None` for any other kind of name.
pub fn code_id_of_name(name: &str) -> Option<u64> {
    name.strip_prefix("code-")?.parse().ok()
}

/// Code row key: the SHEET's address. `scope` first, so a table scan stays
/// inside one corpus — the ordering every other k10r table uses.
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[ok_ns(35)]
pub struct CodeKey {
    pub scope: u64,
    pub code_id: u64,
}

/// One piece of deliverable code: what it is, and its source.
///
/// `parameters` is the parameter definition, carried as a JSON document in TEXT
/// — the store does not parse it (storage stores, the reader interprets), and
/// the caller that delivered the code owns its shape. `description` is the
/// reference a skill-to-code link is made from.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(CodeKey)]
#[ok_ns(35)]
#[ok_name("code")]
// The surrogate counter is a WATERMARK (ADR-0023/0025, PLAN 2.9d): the highest
// `code_id` ever written, maintained on every write, unfold a NO-OP — a deleted
// row must not free its id for reuse.
#[ok_reduce(HighWater(code_id))]
pub struct CodeRow {
    pub language: String,
    pub description: String,
    pub parameters: String,
    pub source: String,
}

/// The sheet collection on a store handle — what the graph holds beside its
/// vertices, the way it holds task sheets.
pub(crate) fn code_collection(store: FjallStore) -> Collection<FjallStore, CodeKey, CodeRow> {
    <CodeRow as Document>::collection(store)
}

impl GraphStore {
    /// Store one piece of code; returns the assigned id. Every field is stored
    /// verbatim — the store does not police them (storage stores; the caller
    /// routes, parses and decides).
    ///
    /// Two writes, in this order: the vertex, then the sheet. The other order
    /// would leave a sheet nobody can reach; this order can leave a vertex with
    /// no sheet, which is an inert node (`code()` answers absent), never a false
    /// answer.
    pub fn put_code(
        &mut self,
        scope: u64,
        language: &str,
        description: &str,
        parameters: &str,
        source: &str,
    ) -> Result<u64, IngestError> {
        let code_id = self.take_code_id();
        let term = Term {
            value: code_name(code_id),
            type_label: CODE_KIND.to_string(),
            props: std::collections::BTreeMap::new(),
        };
        self.put_entity(scope, &term)?;
        self.put_code_sheet(
            scope,
            code_id,
            &CodeRow {
                language: language.to_string(),
                description: description.to_string(),
                parameters: parameters.to_string(),
                source: source.to_string(),
            },
        )?;
        Ok(code_id)
    }

    /// One piece of code by id — the read gravity makes at call time. Unknown id
    /// = absent, not an error (the caller's error value, its choice).
    pub fn code(&mut self, scope: u64, code_id: u64) -> Result<Option<CodeRow>, IngestError> {
        self.code_sheet(scope, code_id)
    }

    /// The code id behind one of this scope's vertices, `None` when the vertex is
    /// not code. This is how a `finishes_on` edge turns back into the number a
    /// caller matches on: the edge lands on a node, the number is the node's name.
    pub fn code_id_of(&mut self, scope: u64, node_id: u64) -> Result<Option<u64>, IngestError> {
        let Some(row) = self.entity(scope, node_id)? else {
            return Ok(None);
        };
        if row.kind.as_str() != Some(CODE_KIND) {
            return Ok(None);
        }
        Ok(code_id_of_name(&row.name))
    }

    /// The vertex a code id lives on, `None` when the id was never written.
    pub fn code_vertex(&mut self, scope: u64, code_id: u64) -> Result<Option<u64>, IngestError> {
        let name = code_name(code_id);
        Ok(self.entity_by_name(scope, &name)?.map(|(node_id, _)| node_id))
    }
}

/// The export boundary (ADR-0007's backup): the code sheet is a table like any
/// other, and a restore must not hand out an id a restored row already holds.
impl GraphStore {
    /// This store's code table name — the key the export catalogue uses.
    pub fn code_table_name(&self) -> &'static str {
        <CodeRow as Document>::TABLE_NAME
    }

    /// Export the code sheet by NAME as Parquet. `None` = not this table.
    pub fn export_code(&mut self, name: &str, path: &std::path::Path) -> Option<Result<(), String>> {
        (name == <CodeRow as Document>::TABLE_NAME)
            .then(|| crate::export::table(self.code_table_mut(), path))
    }

    /// The code watermark as a derived reduce table — without it a restored
    /// sheet would hand out ids it already used.
    pub fn export_code_reduces(&self, path: &std::path::Path) -> Result<(), String> {
        crate::export::reduces::<__OkmReduce_CodeRow_0>(
            self.code_table().store(),
            crate::export::ns(<CodeRow as Document>::NS_PREFIX),
            <CodeRow as Document>::TABLE_NAME,
            path,
        )
    }

    /// Restore the code sheet (or its reduce) by NAME, then reseed the id
    /// watermark from what landed.
    pub fn import_code(
        &mut self,
        name: &str,
        path: &std::path::Path,
    ) -> Option<Result<usize, String>> {
        if name == <CodeRow as Document>::TABLE_NAME {
            let out = crate::export::import_table(self.code_table_mut(), path);
            Some(crate::export::finish_import(out, || {
                self.reseed_code().map_err(|e| e.to_string())
            }))
        } else if name == crate::export::CODE_REDUCES {
            let out =
                okm_core::bridge::tooling::parquet_io::import_reduces::<_, __OkmReduce_CodeRow_0>(
                    self.code_table().store(),
                    <CodeRow as Document>::NS_PREFIX,
                    <CodeRow as Document>::TABLE_NAME,
                    path,
                )
                .map_err(|e| format!("reduce import: {e}"));
            Some(crate::export::finish_import(out, || {
                self.reseed_code().map_err(|e| e.to_string())
            }))
        } else {
            None
        }
    }

    fn code_table_mut(&mut self) -> &mut Collection<FjallStore, CodeKey, CodeRow> {
        &mut self.codes
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_code_name_round_trips() {
        assert_eq!(code_name(12), "code-12");
        assert_eq!(code_id_of_name("code-12"), Some(12));
        assert_eq!(code_id_of_name("task-12"), None);
        assert_eq!(code_id_of_name("code-"), None, "a prefix alone is not an id");
    }
}