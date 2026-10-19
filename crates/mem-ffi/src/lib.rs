//! mem-ffi: PyO3 bindings for mem-core (thin shell).
//!
//! Every method forwards 1:1 to `GraphStore`; no logic lives here. The
//! fact graph is the carrier (flat memory was withdrawn with the graph's
//! landing — ADR-0001's "flat memory, then the graph").
//!
//! This binding attaches NO embedding engine, so a node lands with an
//! empty vector frame and `retrieve`/`recall` land nothing: ADR-0003's
//! engine is k10r's own, and a binding shell does not grow one. The
//! graph's exact entrance (`entity`) and its writes work regardless.

use mem_core::{FactTuple, GraphStore};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

#[pyclass]
struct PyGraphStore {
    inner: GraphStore,
}

#[pymethods]
impl PyGraphStore {
    #[new]
    fn new(path: &str) -> PyResult<Self> {
        GraphStore::open(std::path::Path::new(path))
            .map(|inner| Self { inner })
            .map_err(|e| PyRuntimeError::new_err(format!("failed to open graph: {e}")))
    }

    /// Land one fact from its canonical 8-slot JSON tuple; returns
    /// `(outcome, edge_id)` where outcome is `linked` / `rewritten` /
    /// `dropped` (ADR-0010 §1).
    fn ingest(&mut self, scope: u64, tuple_json: &str) -> PyResult<(String, u64)> {
        let fact = FactTuple::from_json(tuple_json)
            .map_err(|e| PyRuntimeError::new_err(format!("bad fact tuple: {e}")))?;
        let outcome = self
            .inner
            .ingest(scope, &fact)
            .map_err(|e| PyRuntimeError::new_err(format!("ingest failed: {e}")))?;
        Ok(match outcome {
            mem_core::IngestOutcome::Linked { edge_id } => ("linked".to_string(), edge_id),
            mem_core::IngestOutcome::Rewritten { edge_id } => ("rewritten".to_string(), edge_id),
            mem_core::IngestOutcome::Dropped { edge_id } => ("dropped".to_string(), edge_id),
        })
    }

    /// Land a query on the scope's nodes: `(node_id, score, name, kind)`
    /// rows, best first. An empty list is the legal "nothing landed".
    fn retrieve(
        &mut self,
        scope: u64,
        query: &str,
        k: usize,
    ) -> PyResult<Vec<(u64, f32, String, String)>> {
        self.inner
            .retrieve(scope, query, k)
            .map(|seeds| {
                seeds
                    .into_iter()
                    .map(|s| (s.node_id, s.score, s.name, s.kind))
                    .collect()
            })
            .map_err(|e| PyRuntimeError::new_err(format!("retrieve failed: {e}")))
    }

    /// Landing plus one hop: `(seeds, facts)` where a fact row is
    /// `(edge_id, kind, src_name, dst_name, weight)`.
    fn recall(
        &mut self,
        scope: u64,
        query: &str,
        k: usize,
    ) -> PyResult<(
        Vec<(u64, f32, String, String)>,
        Vec<(u64, String, String, String, u32)>,
    )> {
        self.inner
            .recall(scope, query, k, None)
            .map(|recall| {
                (
                    recall
                        .seeds
                        .into_iter()
                        .map(|s| (s.node_id, s.score, s.name, s.kind))
                        .collect(),
                    recall
                        .facts
                        .into_iter()
                        .map(|f| (f.edge_id, f.kind, f.src.name, f.dst.name, f.edge.weight))
                        .collect(),
                )
            })
            .map_err(|e| PyRuntimeError::new_err(format!("recall failed: {e}")))
    }

    /// The exact name/alias entrance: `(node_id, kind, name)` or None.
    fn entity(&mut self, scope: u64, name: &str) -> PyResult<Option<(u64, String, String)>> {
        match self.inner.entity_by_name(scope, name) {
            Some((node_id, row)) => {
                let kind = self.inner.entity_kind(row.kind_id).unwrap_or_default();
                Ok(Some((node_id, kind, row.name)))
            }
            None => Ok(None),
        }
    }
}

/// Python module entry: `mem_ffi`.
#[pymodule]
fn mem_ffi(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyGraphStore>()?;
    Ok(())
}