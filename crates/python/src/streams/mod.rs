//! Python-owned binary sources and sinks for the native streaming methods.

mod download;
mod upload;

pub(super) use download::download;
pub(super) use upload::{hash_fileobj, upload};

use covalence_lib_python::pyo3::{exceptions::PyValueError, prelude::*};

/// The most bytes one Python callback is asked to transfer at once.
const CHUNK_BYTES: usize = 1 << 20;

/// Reject an impossible exact length before calling the source or sink.
fn check_limits(max_bytes: u64, expected_length: Option<u64>) -> PyResult<()> {
    if expected_length.is_some_and(|length| length > max_bytes) {
        return Err(PyValueError::new_err("expected_length exceeds max_bytes"));
    }
    Ok(())
}
