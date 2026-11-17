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
        let key = RunKey { scope, run_id };
        let previous = self
            .runs
            .get_row(&key)
            .map_err(|e| IngestError::Storage(e.to_string()))?
            .map_or(0, |row| row.revision);
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

    /// One run's snapshot, or `None` when no such run was ever written.
    pub fn run(&mut self, scope: u64, run_id: u64) -> Result<Option<RunRow>, IngestError> {
        self.runs
            .get_row(&RunKey { scope, run_id })
            .map_err(|e| IngestError::Storage(e.to_string()))
    }
}
