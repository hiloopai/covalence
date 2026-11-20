use std::fmt::Write as _;

use covalence_blob_verify::{DigestAccumulator, DigestAlgorithms, DigestResult};
use covalence_lib_python::pyo3::{
    prelude::*, pybacked::PyBackedBytes, types::PyBytes, wrap_pyfunction,
};

use crate::{
    PyCid,
    errors::{Class, raise_as},
};

const CHUNK_BYTES: u64 = 64 * 1024;

fn value_error(error: impl std::fmt::Display) -> PyErr {
    raise_as(Class::Value, error)
}

fn measure(algorithms: &str, max_len: Option<u64>) -> PyResult<DigestAccumulator> {
    let algorithms = match algorithms {
        "sha256" => DigestAlgorithms::Sha256,
        "blake3" => DigestAlgorithms::Blake3,
        "both" => DigestAlgorithms::Both,
        _ => {
            return Err(value_error(
                "algorithms must be 'sha256', 'blake3' or 'both'",
            ));
        }
    };
    Ok(DigestAccumulator::new(algorithms, max_len))
}

/// The observed byte length and requested digests, not proof of integrity.
///
/// Results compare and hash by value.
#[derive(PartialEq, Eq, Hash)]
#[pyclass(
    skip_from_py_object,
    frozen,
    eq,
    hash,
    name = "DigestResult",
    module = "covalence"
)]
#[pyo3(crate = "covalence_lib_python::pyo3")]
struct PyDigestResult(DigestResult);

#[pymethods]
#[pyo3(crate = "covalence_lib_python::pyo3")]
impl PyDigestResult {
    /// The exact number of bytes hashed.
    #[getter]
    fn len(&self) -> u64 {
        self.0.len()
    }

    /// The 32-byte SHA-256 digest, or None if not requested.
    #[getter]
    fn sha256<'py>(&self, py: Python<'py>) -> Option<Bound<'py, PyBytes>> {
        self.0.sha256().map(|digest| PyBytes::new(py, digest))
    }

    /// The 32-byte BLAKE3 digest, or None if not requested.
    #[getter]
    fn blake3<'py>(&self, py: Python<'py>) -> Option<Bound<'py, PyBytes>> {
        self.0.blake3().map(|digest| PyBytes::new(py, digest))
    }

    /// The raw BLAKE3 CID, or None if BLAKE3 was not requested.
    #[getter]
    fn raw_blake3(&self) -> Option<PyCid> {
        self.0.raw_blake3().map(|cid| PyCid { inner: cid.into() })
    }

    fn __repr__(&self) -> String {
        let mut out = format!("DigestResult(len={}", self.0.len());
        if let Some(sha256) = self.0.sha256() {
            out.push_str(", sha256=");
            for byte in sha256 {
                let _ = write!(out, "{byte:02x}");
            }
        }
        if let Some(blake3) = self.0.blake3() {
            out.push_str(", blake3=");
            for byte in blake3 {
                let _ = write!(out, "{byte:02x}");
            }
        }
        out.push(')');
        out
    }
}

/// Measure bytes in order without retaining the input.
///
/// Choose 'sha256', 'blake3' or 'both' (the default); other names raise
/// ValueError. The optional max_len is an inclusive byte limit. The effective
/// limit is at most 2**64 - 1 bytes, or 2**61 - 1 when computing SHA-256.
///
/// >>> from covalence import DigestAccumulator
/// >>> digest = DigestAccumulator(max_len=5)
/// >>> digest.update(b'he')
/// >>> digest.update(bytearray(b'llo'))
/// >>> digest.finish().len
/// 5
#[pyclass(name = "DigestAccumulator", module = "covalence")]
#[pyo3(crate = "covalence_lib_python::pyo3")]
struct PyDigestAccumulator(Option<DigestAccumulator>);

#[pymethods]
#[pyo3(crate = "covalence_lib_python::pyo3")]
impl PyDigestAccumulator {
    #[new]
    #[pyo3(signature = (algorithms="both", *, max_len=None))]
    fn new(algorithms: &str, max_len: Option<u64>) -> PyResult<Self> {
        Ok(Self(Some(measure(algorithms, max_len)?)))
    }

    /// Hash the next bytes or bytearray, releasing the GIL while hashing.
    ///
    /// Exceeding the byte limit raises ValueError, hashes none of the supplied
    /// bytes, and prevents any later update or result. Updating after finish
    /// also raises ValueError.
    fn update(&mut self, py: Python<'_>, chunk: PyBackedBytes) -> PyResult<()> {
        let digest = self.0.as_mut().ok_or_else(finished)?;
        py.detach(|| digest.update(&chunk)).map_err(value_error)
    }

    /// Return the measurement and close the accumulator.
    ///
    /// Raises ValueError after a limit error or if already closed.
    fn finish(&mut self, py: Python<'_>) -> PyResult<PyDigestResult> {
        finish(py, self.0.take().ok_or_else(finished)?)
    }
}

fn finished() -> PyErr {
    value_error("the digest accumulator has already finished")
}

fn finish(py: Python<'_>, digest: DigestAccumulator) -> PyResult<PyDigestResult> {
    py.detach(|| digest.finish())
        .map(PyDigestResult)
        .map_err(value_error)
}

/// Measure a binary stream from its current position through EOF in one pass.
///
/// The source must implement read(size) and return bytes or bytearray, with
/// b'' marking EOF. It is not rewound or closed. Reads are at most 64 KiB; if
/// the byte limit is reached, one more byte is requested to check for EOF.
/// Hashing runs without the GIL. The algorithms and limits are those of
/// DigestAccumulator.
///
/// - ValueError if the limit is exceeded or read returns too many bytes.
/// - TypeError if read returns anything other than bytes or bytearray.
/// - Exceptions from read pass through unchanged.
///
/// >>> from io import BytesIO
/// >>> from covalence import digest_fileobj
/// >>> digest_fileobj(BytesIO(b'hello'), algorithms='blake3', max_len=5).len
/// 5
#[pyfunction]
#[pyo3(crate = "covalence_lib_python::pyo3", signature = (source, /, *, algorithms="both", max_len=None))]
fn digest_fileobj(
    py: Python<'_>,
    source: &Bound<'_, PyAny>,
    algorithms: &str,
    max_len: Option<u64>,
) -> PyResult<PyDigestResult> {
    let mut digest = measure(algorithms, max_len)?;
    loop {
        let size = digest
            .remaining()
            .map_err(value_error)?
            .clamp(1, CHUNK_BYTES);
        let result = source.call_method1("read", (size,))?;
        let bytes: PyBackedBytes = result.extract()?;
        if bytes.len() as u64 > size {
            return Err(value_error("read returned more bytes than requested"));
        }
        if bytes.is_empty() {
            return finish(py, digest);
        }
        py.detach(|| digest.update(&bytes)).map_err(value_error)?;
    }
}

pub(super) fn add(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyDigestAccumulator>()?;
    m.add_class::<PyDigestResult>()?;
    let function = wrap_pyfunction!(digest_fileobj, m)?;
    function.setattr("__module__", "covalence")?;
    m.add_function(function)
}
