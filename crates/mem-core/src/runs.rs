//! A run's snapshot (gravity ADR-0011 §7, k10r's side of the door).
//!
//! The engine serialises its state whole at a suspension point, this table keeps
//! the bytes, and a restart reads them back. k10r does NOT parse the document:
//! the run's shape belongs to the engine that wrote it, and a memory core that
//! learned it would have to learn every future engine's shape too.
//!
//! Why a table of its own and not a vertex: run state is APPLICATION state, not
//! corpus knowledge. A node row would be binned into a community, embedded,
//! ranked by recall and exported with the memories — four readers that have no
//! business with a step counter. The sheet's key is the pair the engine speaks
//! in, and nothing else reaches it.

/// One run's address: the scope it was started in, and the id the engine issued.
#[derive(okm_derive::KeyEncode, Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub struct RunKey {
    pub scope: u64,
    pub run_id: u64,
}

/// One run's document, opaque here.
///
/// `revision` is the write's own counter, monotonic per run: a reader that
/// fetched revision 3 can tell whether what it is holding is still the newest
/// answer, without decoding the document to find out.
#[derive(okm_derive::DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(RunKey)]
#[ok_ns(47)]
#[ok_name("runs")]
pub struct RunRow {
    pub doc: String,
    /// Bumped on every write; `0` never lands (a write is revision 1).
    #[ok_default(0)]
    pub revision: u32,
    /// The caller's clock at the write — the engine owns time, k10r records it.
    #[ok_default(0)]
    pub updated_at: u64,
}
use crate::graph::{GraphStore, IngestError};

impl GraphStore {
    /// Write one run's snapshot (gravity ADR-0011 §7).
    ///
    /// k10r does not read the document — it stores the engine's own bytes and
    /// bumps a revision, so a reader knows whether what it holds is current. The
    /// whole document is replaced: a run's state is a VALUE, so a crash leaves
    /// the previous value or the next one, never a mixture.
    pub fn put_run(
        &mut self,
        scope: u64,
        run_id: u64,
        doc: String,
        now: u64,
    ) -> Result<u32, IngestError> {
        self.put_run_if(scope, run_id, doc, now, None)
    }

    /// The same write, conditional on the revision the caller last read
    /// (`None` = unconditional, for the first write).
    ///
    /// A run is advanced by reading it, appending one completion, and writing it
    /// back. Without the condition two writers each read revision N and the second
    /// write silently erases the first completion — and a run's log is the whole
    /// record of what happened, so a lost entry is a lost fact. The condition
    /// turns that race into a refusal the caller can retry.
    pub fn put_run_if(
        &mut self,
        scope: u64,
        run_id: u64,
        doc: String,
        now: u64,
        expect: Option<u32>,
    ) -> Result<u32, IngestError> {
        let key = RunKey { scope, run_id };
        let stored = self
            .runs
            .get_row(&key)
            .map_err(|e| IngestError::Storage(e.to_string()))?
            .map_or(0, |row| row.revision);
        if let Some(expected) = expect {
            if expected != stored {
                return Err(IngestError::Conflict {
                    expected,
                    stored,
                });
            }
        }
        let previous = stored;
        let row = RunRow {
            doc,
            revision: previous.saturating_add(1),
            updated_at: now,
        };
        self.runs
            .put(&key, &row)
            .map_err(|e| IngestError::Storage(e.to_string()))?;
        Ok(row.revision)
    }

    /// Every run in one scope, oldest id first.
    ///
    /// A run is addressed by (scope, id) and that is enough to CONTINUE one — but not
    /// enough to SHOW what is waiting: nobody holds the ids of runs started on another
    /// device, and a page that lists work in progress must ask "what is here", not
    /// "what is at this address". The scan is over this table's own prefix; the scope
    /// rides the key's first eight bytes.
    /// Every run in one scope, oldest id first.
    ///
    /// A run is addressed by (scope, id) and that is enough to CONTINUE one — but not
    /// enough to SHOW what is waiting: nobody holds the ids of runs started on another
    /// device, and a page that lists work in progress asks "what is here", not "what is
    /// at this address". The ids come from this table's own key prefix (the scope rides
    /// its first eight bytes); each row is then read the ordinary way, which keeps the
    /// decoding in one place.
    pub fn runs_of(&mut self, scope: u64) -> Result<Vec<(u64, RunRow)>, IngestError> {
        let prefix = scope.to_be_bytes();
        let raw = self
            .runs
            .scan_documents_raw()
            .map_err(|e| IngestError::Storage(e.to_string()))?;
        let mut ids = Vec::new();
        for (suffix, payload) in raw {
            if !suffix.starts_with(&prefix) || payload.is_none() {
                continue;
            }
            let Some(id) = suffix.get(8..16) else {
                continue;
            };
            let Ok(id) = <[u8; 8]>::try_from(id) else {
                continue;
            };
            ids.push(u64::from_be_bytes(id));
        }
        ids.sort_unstable();
        let mut out = Vec::with_capacity(ids.len());
        for run_id in ids {
            if let Some(row) = self.run(scope, run_id)? {
                out.push((run_id, row));
            }
        }
        Ok(out)
    }

    /// One run's snapshot, or `None` when no such run was ever written.
    pub fn run(&mut self, scope: u64, run_id: u64) -> Result<Option<RunRow>, IngestError> {
        self.runs
            .get_row(&RunKey { scope, run_id })
            .map_err(|e| IngestError::Storage(e.to_string()))
    }
}
