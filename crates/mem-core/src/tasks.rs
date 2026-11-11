//! The task node (ADR-0021): an ordinary vertex in the fact graph, plus the
//! sheet that holds what changes about it.
//!
//! A task is NOT a new kind of store. It is an entity of kind `task` in ns 36 —
//! the same collection as every other node — so a dependency, a parent, a tag or
//! a reference is an ordinary edge written by the ordinary verbs, and a fact
//! about a task is an ordinary fact. What a task owns beyond a name is state and
//! dates, and those live in a side table (ns 44) keyed EXACTLY like the vertex,
//! the way `NodeVector` is (ns 43): the key payload is byte-identical, so no
//! lookup table joins them.
//!
//! Why the sheet is separate rather than props on the vertex:
//!
//! - **Presence is the meaning.** A sheet exists iff the node is a task, and the
//!   vertex is the same four fields for every kind. Putting task fields on the
//!   entity row would give every node a task's columns (and bump the layout for
//!   every store), which is the same mistake as keeping the vector in the row.
//! - **The state is indexed.** "Which tasks are open" is an exact enumeration,
//!   not a similarity question. A declared field can carry a face; a dynamic
//!   segment cannot (okm has no index over props), so a state kept in props
//!   would force a scan of every task and a filter in memory.
//! - **The lifecycles differ.** The vertex lives as long as facts mention it;
//!   the sheet lives as long as the work does. One can be retired without the
//!   other, and neither write touches the other's row.
//!
//! What the store does NOT do: parse `next` (it is the producer's text), order
//! tasks, or decide what a state transition means. It answers "here is the row".

use std::collections::BTreeMap;

use okm_core::model::index::scan_index_exact;
use okm_core::Document;

use crate::code::code_name;
use crate::graph::{EntityKey, FactTuple, GraphStore, IngestError, Relation, Term};

use __OkmIndex_TaskSheet_by_state as ByState;

/// The kind a task vertex carries. A task IS this kind and nothing else marks
/// it: `by_kind` is how the graph already asks for "the tasks of this scope".
pub const TASK_KIND: &str = "task";

/// The states, closed on purpose. A typo in a state name would make a task
/// vanish from every listing while looking present in the graph, so the write
/// path refuses anything not in this list.
pub const TASK_STATES: &[&str] = &["open", "doing", "blocked", "done"];

/// The state a task is created in, and the state of a sheet nobody has touched.
pub const OPEN_STATE: &str = "open";

/// The edge that says which CODE a task is finished by (`task --finishes_on-->
/// code`). An ordinary fact, written by the ordinary verbs like a dependency is
/// (ADR-0021 §1) — this store never writes it, and the sheet deliberately has no
/// field for it: a binding is a relation, and relations are edges.
///
/// Why it is read here at all: gravity completes a task at the CALL SITE
/// (gravity ADR-0003 §3 — completion is never a model judgement), so the one
/// question it must answer before dispatching is "which open tasks does this
/// code row finish". That answer has to ride the listing the turn already
/// fetched, or it becomes a round trip per call.
pub const FINISHES_ON: &str = "finishes_on";

/// One task's working sheet. Same key as the vertex it belongs to.
#[derive(okm_derive::DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(EntityKey)]
#[ok_ns(44)]
#[ok_name("task_sheets")]
#[ok_index(by_state { fields(state) })]
pub struct TaskSheet {
    /// `open` / `doing` / `blocked` / `done` (see [`TASK_STATES`]). A cold field
    /// rather than a `Vocab<u16>`: the face is probed with the label's own
    /// bytes, the set is four words that never grows, and a reader that decodes
    /// the row can compare strings without resolving a vocabulary.
    pub state: String,
    /// The next action, in the producer's words. Empty means "nothing said".
    pub next: String,
    /// The caller's clock when the task was opened. `0` = opened by a fact, so
    /// nobody has stated a date (never "opened at the epoch").
    pub created_at: u64,
    /// The caller's clock at the last state or `next` change.
    pub updated_at: u64,
}

impl TaskSheet {
    /// A sheet for a task nobody has touched: open, no next action, no dates.
    pub fn opened() -> Self {
        Self {
            state: OPEN_STATE.to_string(),
            next: String::new(),
            created_at: 0,
            updated_at: 0,
        }
    }
}

/// What a caller states when opening a task. `name` is the identity facts use
/// (`["task-12","task",{},"depends_on",…]` reaches this node by name).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TaskDecl {
    pub name: String,
    pub next: Option<String>,
    pub state: Option<String>,
    /// Code ids this task is finished by (`finishes_on` edges, see
    /// [`FINISHES_ON`]). Stated at open because that is when the caller knows
    /// which step the work belongs to; a binding is ADDED, never rewritten — an
    /// edge that should go away is withdrawn, and no door pretends otherwise.
    pub finishes_on: Vec<u64>,
}

/// The answer to "open this task": the id it landed on, and whether this call
/// is what created the vertex.
#[derive(Clone, Debug, PartialEq)]
pub struct TaskLanded {
    pub node_id: u64,
    pub created: bool,
}

/// One task as the door reports it: the vertex's identity and the sheet's state,
/// plus the code rows it is bound to finish on (see [`FINISHES_ON`]).
#[derive(Clone, Debug, PartialEq)]
pub struct TaskView {
    pub node_id: u64,
    pub name: String,
    pub state: String,
    pub next: String,
    pub created_at: u64,
    pub updated_at: u64,
    /// Entity ids of this task's `finishes_on` targets, in edge order. Empty for
    /// a task nothing is bound to — which is every task that was not opened as
    /// part of a scripted step.
    pub finishes_on: Vec<u64>,
}

/// Reading a task's sheet: three outcomes, because the door reports them as
/// three (absent → 404, another kind → 400, found → the sheet).
#[derive(Clone, Debug, PartialEq)]
pub enum TaskRead {
    Found(Box<TaskView>),
    Absent,
    NotATask { kind: String },
}

fn checked_state(raw: &str) -> Result<String, IngestError> {
    let state = raw.trim();
    if TASK_STATES.contains(&state) {
        Ok(state.to_string())
    } else {
        Err(IngestError::Refused(format!(
            "`{state}` is not a task state — one of {}",
            TASK_STATES.join(", ")
        )))
    }
}

impl GraphStore {
    /// Open a task: create (or find) the vertex, then write its sheet. Opening
    /// the same name twice does not open a second task — the name is the
    /// identity, exactly as a skill's name is.
    pub fn open_task(
        &mut self,
        scope: u64,
        decl: &TaskDecl,
        now: u64,
    ) -> Result<TaskLanded, IngestError> {
        if decl.name.trim().is_empty() {
            return Err(IngestError::Refused(
                "a task needs a name: it is the identity edges and facts reach it by".to_string(),
            ));
        }
        let state = match decl.state.as_deref() {
            Some(raw) => checked_state(raw)?,
            None => OPEN_STATE.to_string(),
        };
        let term = Term {
            value: decl.name.clone(),
            type_label: TASK_KIND.to_string(),
            props: BTreeMap::new(),
        };
        let (node_id, created) = self.put_entity(scope, &term)?;
        if !created {
            // An existing node keeps its kind, so a name that is a person or a
            // skill must not become a task by declaration (ADR-0015's rule).
            let kind = self
                .entity(scope, node_id)?
                .map(|row| row.kind.as_str().unwrap_or_default().to_string())
                .unwrap_or_default();
            if kind != TASK_KIND {
                return Err(IngestError::Refused(format!(
                    "`{}` is a {kind}, not a task: a declaration does not re-label a node",
                    decl.name
                )));
            }
            // Opening a task that is already open RESTATES it and does not move it.
            // Writing the sheet here would be the quiet bug: a retried or duplicated
            // tool call would drag a task out of `doing` back into `open` and reset
            // its clock, so the one call whose entire meaning is "this work exists"
            // would be the call that loses the work's progress. A caller that wants
            // the task changed says so at `/move`.
            //
            // The BINDING is the exception, and it is safe to restate: an edge
            // carries no clock and no state, so re-stating it is idempotent (the
            // ingest answers `Bumped` for one that already stands). That is what
            // lets a turn bind a task it opened in an earlier turn.
            self.bind_finishes_on(scope, &decl.name, &decl.finishes_on)?;
            return Ok(TaskLanded { node_id, created });
        }
        let sheet = TaskSheet {
            state,
            next: decl.next.clone().unwrap_or_default(),
            created_at: now,
            updated_at: now,
        };
        self.put_task_sheet(scope, node_id, &sheet)?;
        self.bind_finishes_on(scope, &decl.name, &decl.finishes_on)?;
        Ok(TaskLanded { node_id, created })
    }

    /// Move a task: a state and/or a next action. `None` leaves that part as it
    /// was — "undeclare the next action" is expressed by an empty string, not by
    /// silence, so a caller cannot blank a field by forgetting it.
    pub fn set_task_state(
        &mut self,
        scope: u64,
        node_id: u64,
        state: Option<&str>,
        next: Option<&str>,
        now: u64,
    ) -> Result<TaskRead, IngestError> {
        let Some(row) = self.entity(scope, node_id)? else {
            return Ok(TaskRead::Absent);
        };
        let kind = row.kind.as_str().unwrap_or_default().to_string();
        if kind != TASK_KIND {
            return Ok(TaskRead::NotATask { kind });
        }
        let old = self
            .task_sheet(scope, node_id)?
            .unwrap_or_else(TaskSheet::opened);
        let sheet = TaskSheet {
            state: match state {
                Some(raw) => checked_state(raw)?,
                None => old.state,
            },
            next: match next {
                Some(text) => text.to_string(),
                None => old.next,
            },
            created_at: old.created_at,
            updated_at: now,
        };
        self.put_task_sheet(scope, node_id, &sheet)?;
        let finishes_on = self.bound_codes(scope, node_id)?;
        let view = TaskView {
            node_id,
            name: row.name,
            state: sheet.state,
            next: sheet.next,
            created_at: sheet.created_at,
            updated_at: sheet.updated_at,
            finishes_on,
        };
        Ok(TaskRead::Found(Box::new(view)))
    }

    /// The code ids a task is bound to, resolved from the `finishes_on` edges:
    /// the edge lands on a code VERTEX, and the caller wants the number a skill's
    /// `steps[].code_id` carries. A target that is not code is dropped — the
    /// predicate is this store's own, and an edge with an alien target is a
    /// caller bug, never a value worth passing on.
    fn bound_codes(&mut self, scope: u64, node_id: u64) -> Result<Vec<u64>, IngestError> {
        let mut out = Vec::new();
        for target in self.targets_of(scope, node_id, FINISHES_ON)? {
            if let Some(code_id) = self.code_id_of(scope, target)? {
                out.push(code_id);
            }
        }
        Ok(out)
    }

    /// Bind a task to the code rows that finish it: one ordinary fact per code
    /// row, written the same way any other edge is. Refused (not silently
    /// skipped) when a named code id was never written — a binding to nothing is
    /// a caller bug, and a phantom `code-99` vertex would be worse than an error.
    fn bind_finishes_on(
        &mut self,
        scope: u64,
        task_name: &str,
        code_ids: &[u64],
    ) -> Result<(), IngestError> {
        for code_id in code_ids {
            if self.code_vertex(scope, *code_id)?.is_none() {
                return Err(IngestError::Refused(format!(
                    "code row {code_id} does not exist in scope {scope}: a task binds to code \
                     that was delivered, never to an id somebody hoped for"
                )));
            }
            let props = BTreeMap::new();
            let fact = FactTuple {
                head: Term {
                    value: task_name.to_string(),
                    type_label: TASK_KIND.to_string(),
                    props: props.clone(),
                },
                rel: Relation {
                    name: FINISHES_ON.to_string(),
                    props: props.clone(),
                },
                tail: Term {
                    value: code_name(*code_id),
                    type_label: crate::code::CODE_KIND.to_string(),
                    props,
                },
            };
            self.ingest(scope, &fact)?;
        }
        Ok(())
    }

    /// One task, whether or not its sheet has been written.
    pub fn task(&mut self, scope: u64, node_id: u64) -> Result<TaskRead, IngestError> {
        let Some(row) = self.entity(scope, node_id)? else {
            return Ok(TaskRead::Absent);
        };
        let kind = row.kind.as_str().unwrap_or_default().to_string();
        if kind != TASK_KIND {
            return Ok(TaskRead::NotATask { kind });
        }
        let sheet = self
            .task_sheet(scope, node_id)?
            .unwrap_or_else(TaskSheet::opened);
        let finishes_on = self.bound_codes(scope, node_id)?;
        Ok(TaskRead::Found(Box::new(TaskView {
            node_id,
            name: row.name,
            state: sheet.state,
            next: sheet.next,
            created_at: sheet.created_at,
            updated_at: sheet.updated_at,
            finishes_on,
        })))
    }

    /// The tasks of a scope, optionally only those in one state.
    ///
    /// With a state named this is the sheet's face — an exact probe, no row read
    /// per candidate beyond the one that answers — and the scope filter is the
    /// `EntityKey` the face decodes to. Without one it is the kind scan the
    /// skill resolver also falls back to: the corpus is small by construction.
    pub fn list_tasks(
        &mut self,
        scope: u64,
        state: Option<&str>,
    ) -> Result<Vec<TaskView>, IngestError> {
        let mut out = Vec::new();
        match state {
            Some(raw) => {
                let wanted = checked_state(raw)?;
                let ids = {
                    let sheets = self.sheet_collection();
                    let entries = scan_index_exact::<_, ByState>(
                        sheets.store(),
                        TaskSheet::NS_PREFIX,
                        wanted.as_bytes(),
                    )
                    .map_err(|e| IngestError::Storage(e.to_string()))?;
                    entries
                        .into_iter()
                        .filter(|(_, key)| key.decoded.scope == scope)
                        .map(|(_, key)| key.decoded.node_id)
                        .collect::<Vec<u64>>()
                };
                for node_id in ids {
                    if let TaskRead::Found(view) = self.task(scope, node_id)? {
                        // The probe is an exact segment match, so a mismatch here
                        // is a face entry whose row moved — absence, never a
                        // guess.
                        if view.state == wanted {
                            out.push(*view);
                        }
                    }
                }
            }
            None => {
                for (node_id, row) in self.scope_entities(scope)? {
                    if row.kind.as_str() != Some(TASK_KIND) {
                        continue;
                    }
                    if let TaskRead::Found(view) = self.task(scope, node_id)? {
                        out.push(*view);
                    }
                }
            }
        }
        out.sort_by_key(|task| task.node_id);
        Ok(out)
    }
}
