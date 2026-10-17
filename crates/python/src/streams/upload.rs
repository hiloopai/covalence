//! Bounded ingestion from a caller-owned seekable binary file object.

#[cfg(test)]
mod tests;

use std::io::{self, Read};

use covalence_cas_trait::Cas;
use covalence_lib_cid::{Blake3, Cid, Format};
use covalence_lib_python::pyo3::{
    exceptions::{PyTypeError, PyValueError},
    prelude::*,
    pybacked::PyBackedBytes,
    types::{PyBool, PyInt},
};

use super::{CHUNK_BYTES, check_limits};
use crate::{check_key, errors::raise};

/// Hash and ingest the same Python source, checking both complete passes.
pub(crate) fn upload(
    py: Python<'_>,
    cas: &dyn Cas,
    source: Py<PyAny>,
    max_bytes: u64,
    expected_length: Option<u64>,
) -> PyResult<Cid> {
    let mut reader = hashed(py, source, max_bytes, expected_length)?;
    let length = reader.total;
    let claim = reader.claim();
    rewind(py, &reader.source)?;
    reader.reset(length);
    // A store need not read an empty input, so check its EOF before storage.
    if length == 0 {
        py.detach(|| reader.finish())?;
    }
    let result = py.detach(|| cas.put_from(claim, &mut reader, length));
    if let Some(source) = reader.error.take() {
        return match result {
            Err(error @ covalence_cas_trait::Error::Backend { .. }) => {
                let error = raise(error);
                error.set_cause(py, Some(source));
                Err(error)
            }
            _ => Err(source),
        };
    }
    let key = result.map_err(raise)?;
    // Create-only stores can deduplicate before reading. Check the complete
    // second pass anyway; this verifies the source, not existing stored bytes.
    py.detach(|| reader.finish())?;
    if reader.claim() != claim {
        return Err(raise(covalence_lib_cid::Error::Mismatch));
    }
    check_key(key, claim)?;
    Ok(key)
}

/// The raw BLAKE3 CID of a complete bounded source, without storing it.
pub(crate) fn hash_fileobj(
    py: Python<'_>,
    source: Py<PyAny>,
    max_bytes: u64,
    expected_length: Option<u64>,
) -> PyResult<Blake3> {
    Ok(hashed(py, source, max_bytes, expected_length)?.claim())
}

/// One complete hash pass from offset zero, shared by hashing and ingestion.
fn hashed(
    py: Python<'_>,
    source: Py<PyAny>,
    max_bytes: u64,
    expected_length: Option<u64>,
) -> PyResult<FileReader> {
    check_limits(max_bytes, expected_length)?;
    rewind(py, &source)?;
    let mut reader = FileReader::new(source, max_bytes, expected_length);
    py.detach(|| reader.finish())?;
    Ok(reader)
}

/// Seek to offset zero without taking ownership of the source's lifetime.
fn rewind(py: Python<'_>, source: &Py<PyAny>) -> PyResult<()> {
    let position = source.bind(py).call_method1("seek", (0,))?;
    if position.is_instance_of::<PyBool>() || !position.is_instance_of::<PyInt>() {
        return Err(PyTypeError::new_err("seek(0) must return integer zero"));
    }
    if position.extract::<u64>()? != 0 {
        return Err(PyValueError::new_err("seek(0) did not return zero"));
    }
    Ok(())
}

/// A bounded Python binary reader whose final read checks EOF first.
struct FileReader {
    source: Py<PyAny>,
    max_bytes: u64,
    expected_length: Option<u64>,
    total: u64,
    hasher: blake3::Hasher,
    ended: bool,
    error: Option<PyErr>,
}

impl FileReader {
    fn new(source: Py<PyAny>, max_bytes: u64, expected_length: Option<u64>) -> Self {
        Self {
            source,
            max_bytes,
            expected_length,
            total: 0,
            hasher: blake3::Hasher::new(),
            ended: false,
            error: None,
        }
    }

    /// Start the checked storage pass, bounded by the first pass's length.
    fn reset(&mut self, length: u64) {
        self.expected_length = Some(length);
        self.total = 0;
        self.hasher = blake3::Hasher::new();
        self.ended = false;
    }

    fn claim(&self) -> Blake3 {
        Blake3::new(Format::BLAKE3, self.hasher.finalize().as_bytes()).expect("a BLAKE3 digest")
    }

    /// One bounded read, retaining only the bytes this call returns.
    fn chunk(&self, length: usize) -> PyResult<PyBackedBytes> {
        Python::attach(|py| {
            let result = self.source.bind(py).call_method1("read", (length,))?;
            let bytes: PyBackedBytes = result.extract()?;
            if bytes.len() > length {
                return Err(PyValueError::new_err(
                    "read returned more bytes than requested",
                ));
            }
            Ok(bytes)
        })
    }

    /// No extra byte may follow the declared end, even if a store stops there.
    fn end(&mut self) -> PyResult<()> {
        if !self.chunk(1)?.is_empty() {
            return Err(PyValueError::new_err(
                "source holds more bytes than its length limit",
            ));
        }
        self.ended = true;
        Ok(())
    }

    fn take(&mut self, buffer: &mut [u8]) -> PyResult<usize> {
        if self.ended || buffer.is_empty() {
            return Ok(0);
        }
        let limit = self.expected_length.unwrap_or(self.max_bytes);
        let remaining = limit - self.total;
        let requested = remaining
            .saturating_add(1)
            .min(buffer.len().min(CHUNK_BYTES) as u64) as usize;
        let chunk = self.chunk(requested)?;
        if chunk.is_empty() {
            if self
                .expected_length
                .is_some_and(|length| length != self.total)
            {
                return Err(PyValueError::new_err(
                    "source holds fewer bytes than expected_length",
                ));
            }
            self.ended = true;
            return Ok(0);
        }
        if chunk.len() as u64 > remaining {
            return Err(PyValueError::new_err("source exceeds its length limit"));
        }
        let total = self.total + chunk.len() as u64;
        if self.expected_length == Some(total) {
            self.end()?;
        }
        buffer[..chunk.len()].copy_from_slice(&chunk);
        self.hasher.update(&chunk);
        self.total = total;
        Ok(chunk.len())
    }

    /// Consume and check the complete pass, including a deduplicated input.
    fn finish(&mut self) -> PyResult<()> {
        let mut buffer = vec![0; CHUNK_BYTES];
        loop {
            match self.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(error) => return Err(self.error.take().unwrap_or_else(|| error.into())),
            }
        }
    }
}

impl Read for FileReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.error.is_some() {
            return Err(io::Error::other("the Python source already failed"));
        }
        self.take(buffer).map_err(|error| {
            let message = error.to_string();
            self.error = Some(error);
            io::Error::other(message)
        })
    }
}
