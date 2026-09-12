//! mem-ffi: PyO3 bindings for mem-core (thin shell).
//!
//! Every method forwards 1:1 to MemoryStore; no logic lives here.

use mem_core::MemoryStore;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

#[pyclass]
struct PyMemoryStore {
    inner: MemoryStore,
}

#[pymethods]
impl PyMemoryStore {
    #[new]
    fn new(path: &str) -> PyResult<Self> {
        MemoryStore::open(std::path::Path::new(path))
            .map(|inner| Self { inner })
            .map_err(|e| PyRuntimeError::new_err(format!("failed to open memory store: {e}")))
    }

    /// Store one memory for user_id; returns the assigned id.
    fn store(&mut self, user_id: u64, text: &str, created_at: u64) -> u64 {
        self.inner.store(user_id, text, created_at)
    }

    /// Brute-force substring search over the user's memories.
    fn search(&self, user_id: u64, query: &str) -> Vec<(u64, String)> {
        self.inner.search(user_id, query)
    }

    fn persist(&self) -> PyResult<()> {
        self.inner
            .persist()
            .map_err(|e| PyRuntimeError::new_err(format!("failed to persist: {e}")))
    }
}

/// Python module entry: `mem_ffi`.
#[pymodule]
fn mem_ffi(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyMemoryStore>()?;
    Ok(())
}
