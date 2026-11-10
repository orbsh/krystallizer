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
//! - The SHEET is ns 35, keyed `[scope][code_id][rev]`: language, description,
//!   parameters and source. Presence is the meaning (a node with no sheet is not
//!   code), and the payload stays out of the vertex's props — source text does
//!   not belong in a dynamic segment every reader decodes.
//! - A REVISION is immutable. Writing a correction appends `rev + 1`; nothing
//!   ever overwrites a version that may already have run. The current version is
//!   the LAST key of that script's range — ordering answers it, so there is no
//!   head pointer to fall out of step with the rows (see [`CodeKey`]).
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
//! The vertex also carries the USE counters — `invoke_calls`, `invoke_ok`,
//! `last_invoked`, the same three declared fields a skill node carries
//! (ADR-0018). They count the SCRIPT, not the caller: one script is reached from
//! several places (many skills can each name it as a step, and the tool that
//! dispatches can differ), so a count kept on the caller's side would have to be
//! split back out per script, and a count kept here has to be split per caller —
//! neither derives the other. Both are recorded, each where it belongs.
//!
//! gravity is the only client (ADR-0001: the node never talks to krystallizer;
//! the byte path is gravity → control path → node).

use okm_core::obj_dynamic::DynamicValue;
use okm_core::{Collection, Document, DocumentEncode, FjallStore, KeyEncode, ReduceCodec};
use std::collections::BTreeMap;

use crate::graph::{GraphStore, IngestError, Term};
// One invocation vocabulary for both rows: an invocation is attempt-then-success
// whether the counted row is a skill (the procedure) or a script (what runs).
use crate::skills::{TouchPhase, CALLS_FIELD, LAST_INVOKED_FIELD, OK_FIELD};

/// The kind a code vertex carries. A piece of code IS this kind; the sheet
/// beside it holds the payload (language, description, parameters, source).
pub const CODE_KIND: &str = "code";

/// The counters after one record of an invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeTouch {
    pub code_id: u64,
    pub calls: u64,
    pub ok: u64,
}

/// Recording an invocation: three outcomes, because the door reports them as
/// three (absent → 404, another kind → 400, counted → the counters).
#[derive(Clone, Debug, PartialEq)]
pub enum CodeTouchOutcome {
    Counted(CodeTouch),
    Absent,
    NotACode { kind: String },
}

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
///
/// `rev` is the version axis: 1-based, monotone per script, and never reused.
/// The rows of one script are therefore CONTIGUOUS in key order, which is what
/// makes "the current version" the last key of the range instead of a pointer
/// somebody has to keep true.
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
#[ok_ns(35)]
pub struct CodeKey {
    pub scope: u64,
    pub code_id: u64,
    pub rev: u64,
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

/// The payload of one version, built from what the caller delivered. The store
/// carries these verbatim; it does not police them (storage stores).
fn row_of(language: &str, description: &str, parameters: &str, source: &str) -> CodeRow {
    CodeRow {
        language: language.to_string(),
        description: description.to_string(),
        parameters: parameters.to_string(),
        source: source.to_string(),
    }
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
            1,
            &row_of(language, description, parameters, source),
        )?;
        Ok(code_id)
    }

    /// Store a new VERSION of a script that already exists; returns the revision.
    ///
    /// Nothing is overwritten (a version that may already have run must stay
    /// readable) and nothing about the script's address moves: the vertex, the
    /// skills that name its `code_id` and the tasks bound to it keep pointing at
    /// the same script. An id that was never written is REFUSED rather than
    /// invented — a revision of nothing is a caller bug, and a phantom script
    /// would answer calls with the wrong body.
    ///
    /// A vertex with no sheet (a write that failed halfway) is not refused: it is
    /// an inert node, and its first revision is `1`.
    #[allow(clippy::too_many_arguments)]
    pub fn put_code_revision(
        &mut self,
        scope: u64,
        code_id: u64,
        language: &str,
        description: &str,
        parameters: &str,
        source: &str,
    ) -> Result<u64, IngestError> {
        if self.code_vertex(scope, code_id)?.is_none() {
            return Err(IngestError::Refused(format!(
                "code {code_id} does not exist in scope {scope}: a revision needs a script"
            )));
        }
        let rev = self
            .latest_code(scope, code_id)?
            .map_or(1, |(rev, _)| rev.saturating_add(1));
        self.put_code_sheet(
            scope,
            code_id,
            rev,
            &row_of(language, description, parameters, source),
        )?;
        Ok(rev)
    }

    /// The CURRENT version of a piece of code, with its revision number — the
    /// read gravity makes at call time. "Current" is the last key of the
    /// script's range, so it costs one bounded seek, never a scan of the history.
    /// Unknown id = absent, not an error (the caller's error value, its choice).
    pub fn code(
        &mut self,
        scope: u64,
        code_id: u64,
    ) -> Result<Option<(u64, CodeRow)>, IngestError> {
        self.latest_code(scope, code_id)
    }

    /// One EXACT version — what an auditor reads to see what a run was given.
    pub fn code_at(
        &mut self,
        scope: u64,
        code_id: u64,
        rev: u64,
    ) -> Result<Option<CodeRow>, IngestError> {
        self.code_sheet_at(scope, code_id, rev)
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
        Ok(self
            .entity_by_name(scope, &name)?
            .map(|(node_id, _)| node_id))
    }

    /// Count one record of a script's invocation: `Attempt` when the caller
    /// dispatches it, `Success` when the answer came back (ADR-0018's phases,
    /// applied to the code vertex). A failed run therefore leaves `calls > ok`,
    /// which is exactly the state a reader is looking for.
    ///
    /// The count is on the SCRIPT because the script is what ran: the same
    /// `generate-pdf` row can be a step in three skills and reachable from more
    /// than one tool, and the row is where those paths meet. Who called it is
    /// still visible — on the caller's own row — and the two counts are not
    /// convertible, which is why both exist.
    pub fn touch_code(
        &mut self,
        scope: u64,
        code_id: u64,
        phase: TouchPhase,
        now: u64,
    ) -> Result<CodeTouchOutcome, IngestError> {
        let name = code_name(code_id);
        let Some((node_id, row)) = self.entity_by_name(scope, &name)? else {
            return Ok(CodeTouchOutcome::Absent);
        };
        let kind = row.kind.as_str().unwrap_or_default().to_string();
        if kind != CODE_KIND {
            return Ok(CodeTouchOutcome::NotACode { kind });
        }
        let calls = row.invoke_calls + u32::from(phase == TouchPhase::Attempt);
        let ok = row.invoke_ok + u32::from(phase == TouchPhase::Success);
        let mut write: BTreeMap<String, DynamicValue> = BTreeMap::new();
        write.insert(
            CALLS_FIELD.to_string(),
            DynamicValue::U64(u64::from(calls)),
        );
        write.insert(OK_FIELD.to_string(), DynamicValue::U64(u64::from(ok)));
        write.insert(LAST_INVOKED_FIELD.to_string(), DynamicValue::U64(now));
        self.merge_props(scope, node_id, &write)?;
        Ok(CodeTouchOutcome::Counted(CodeTouch {
            code_id,
            calls: u64::from(calls),
            ok: u64::from(ok),
        }))
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
    pub fn export_code(
        &mut self,
        name: &str,
        path: &std::path::Path,
    ) -> Option<Result<(), String>> {
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
                okm_parquet::snapshot::parquet_io::import_reduces::<_, __OkmReduce_CodeRow_0>(
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
        assert_eq!(
            code_id_of_name("code-"),
            None,
            "a prefix alone is not an id"
        );
    }
}
