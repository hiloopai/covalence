//! Bounded Python sinks for the checked Rust CAS download path.

use std::io::{self, Write};

use covalence_cas_trait::{Cas, Error};
use covalence_lib_cid::Cid;
use covalence_lib_python::pyo3::{
    exceptions::{PyOSError, PyTypeError, PyValueError},
    prelude::*,
    types::{PyBool, PyBytes, PyInt},
};

use super::{CHUNK_BYTES, check_limits};
use crate::errors::{Class, raise, raise_as};

/// A Python writer whose acknowledged bytes cannot exceed `limit`.
struct Sink {
    writer: Py<PyAny>,
    limit: u64,
    written: u64,
    error: Option<PyErr>,
}

impl Sink {
    /// Save the first Python failure while returning an error to the Rust store.
    fn failed(&mut self, error: PyErr) -> io::Error {
        let message = error.to_string();
        if self.error.is_none() {
            self.error = Some(error);
        }
        io::Error::other(message)
    }
}

impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.error.is_some() {
            return Err(io::Error::other("the Python writer already failed"));
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        // Reject the entire incoming buffer before forwarding any of it. Earlier buffers may
        // already have reached the caller's private sink, but none can cross its byte ceiling.
        if bytes.len() as u64 > self.limit - self.written {
            return Err(self.failed(PyValueError::new_err("download exceeds its byte limit")));
        }
        let offered = &bytes[..bytes.len().min(CHUNK_BYTES)];
        let result = Python::attach(|py| {
            let value = self
                .writer
                .bind(py)
                .call_method1("write", (PyBytes::new(py, offered),))?;
            if value.is_instance_of::<PyBool>() || !value.is_instance_of::<PyInt>() {
                return Err(PyTypeError::new_err(
                    "writer.write must return an integer byte count",
                ));
            }
            let written = value.extract::<usize>().map_err(|_| {
                PyValueError::new_err("writer.write returned a byte count outside its input")
            })?;
            if written > offered.len() {
                return Err(PyValueError::new_err(
                    "writer.write returned a byte count outside its input",
                ));
            }
            if written == 0 {
                return Err(PyOSError::new_err("writer.write made no progress"));
            }
            Ok(written)
        });
        match result {
            Ok(written) => {
                self.written += written as u64;
                Ok(written)
            }
            Err(error) => Err(self.failed(error)),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        // This adapter adds no buffering and does not own the caller's stream lifecycle.
        Ok(())
    }
}

/// Download through Rust's checked CAS stream into a bounded Python writer.
pub(crate) fn download(
    py: Python<'_>,
    cas: &dyn Cas,
    cid: &Cid,
    writer: Py<PyAny>,
    max_bytes: u64,
    expected_length: Option<u64>,
) -> PyResult<Option<u64>> {
    check_limits(max_bytes, expected_length)?;
    let mut sink = Sink {
        writer,
        limit: expected_length.unwrap_or(max_bytes),
        written: 0,
        error: None,
    };
    let result = py.detach(|| cas.get_to(cid, &mut sink));
    if let Some(source) = sink.error.take() {
        return match result {
            Err(error @ Error::Backend { .. }) => {
                let error = raise(error);
                error.set_cause(py, Some(source));
                Err(error)
            }
            _ => Err(source),
        };
    }
    let reported = result.map_err(raise)?;
    match reported {
        None if sink.written != 0 => Err(raise_as(
            Class::Integrity,
            "the store reported a missing block after writing bytes",
        )),
        Some(length) if length != sink.written => Err(raise_as(
            Class::Integrity,
            "the store reported a length different from the bytes written",
        )),
        Some(length) if expected_length.is_some_and(|expected| length != expected) => Err(
            PyValueError::new_err("download length differs from expected_length"),
        ),
        _ => Ok(reported),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use covalence_cas_trait::{AsyncGet, AsyncPut, CasFuture, Get, Op, Put, ready};
    use covalence_lib_cid::Blake3;
    use covalence_lib_data::Bytes;
    use covalence_lib_python::pyo3::types::PyModule;

    use super::*;

    /// A store whose stream can contradict its acknowledged bytes.
    struct Stream {
        bytes: &'static [u8],
        reported: Option<u64>,
        cleanup_failure: bool,
        calls: AtomicUsize,
    }

    impl Stream {
        fn new(bytes: &'static [u8], reported: Option<u64>) -> Self {
            Self {
                bytes,
                reported,
                cleanup_failure: false,
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl Get for Stream {
        fn get_unchecked(&self, _: &Cid) -> Result<Option<Bytes>, Error> {
            Err(Op::Get.unsupported())
        }
    }

    impl AsyncGet for Stream {
        fn aget_unchecked<'a>(&'a self, cid: &'a Cid) -> CasFuture<'a, Option<Bytes>> {
            ready(self.get_unchecked(cid))
        }
    }

    impl Put for Stream {
        fn put(&self, _: Bytes) -> Result<Cid, Error> {
            Err(Op::Put.unsupported())
        }
    }

    impl AsyncPut for Stream {
        fn aput(&self, bytes: Bytes) -> CasFuture<'_, Cid> {
            ready(self.put(bytes))
        }
    }

    impl Cas for Stream {
        fn get_to(&self, _: &Cid, writer: &mut dyn Write) -> Result<Option<u64>, Error> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if let Err(source) = writer.write_all(self.bytes) {
                return if self.cleanup_failure {
                    Err(Error::Backend {
                        source: "download cleanup failed".into(),
                    })
                } else {
                    Err(Error::Io { source })
                };
            }
            Ok(self.reported)
        }
    }

    fn sink(py: Python<'_>) -> Py<PyAny> {
        let module = PyModule::from_code(
            py,
            c"class Sink:\n    def __init__(self):\n        self.data = bytearray()\n    def write(self, data):\n        self.data.extend(data)\n        return len(data)\n",
            c"download_test.py",
            c"download_test",
        )
        .unwrap();
        module.getattr("Sink").unwrap().call0().unwrap().unbind()
    }

    #[test]
    fn invalid_expected_length_is_rejected_before_io() {
        Python::initialize();
        Python::attach(|py| {
            let stream = Stream::new(b"x", Some(1));
            let cid: Cid = Blake3::of(b"x").into();
            let error = download(py, &stream, &cid, py.None(), 1, Some(2)).unwrap_err();
            assert!(error.is_instance_of::<PyValueError>(py));
            assert_eq!(stream.calls.load(Ordering::Relaxed), 0);
        });
    }

    #[test]
    fn reported_lengths_must_match_actual_writes() {
        Python::initialize();
        Python::attach(|py| {
            let cid: Cid = Blake3::of(b"x").into();
            let integrity = crate::errors::integrity_error(py).unwrap();
            for reported in [Some(0), Some(2), None] {
                let stream = Stream::new(b"x", reported);
                let error = download(py, &stream, &cid, sink(py), 1, None).unwrap_err();
                assert!(error.value(py).is_instance(&integrity).unwrap());
            }
            let missing = Stream::new(b"", None);
            assert_eq!(
                download(py, &missing, &cid, py.None(), 1, Some(1)).unwrap(),
                None
            );
            let empty = Stream::new(b"", Some(0));
            assert_eq!(
                download(py, &empty, &cid, py.None(), 0, Some(0)).unwrap(),
                Some(0)
            );
            let error = download(py, &empty, &cid, py.None(), 1, Some(1)).unwrap_err();
            assert!(error.is_instance_of::<PyValueError>(py));
        });
    }

    #[test]
    fn byte_limit_is_enforced_before_forwarding_a_buffer() {
        Python::initialize();
        Python::attach(|py| {
            let stream = Stream::new(b"too many bytes", Some(14));
            let cid: Cid = Blake3::of(stream.bytes).into();
            for expected in [None, Some(4)] {
                let writer = sink(py);
                let error =
                    download(py, &stream, &cid, writer.clone_ref(py), 4, expected).unwrap_err();
                assert!(error.is_instance_of::<PyValueError>(py));
                assert_eq!(writer.bind(py).getattr("data").unwrap().len().unwrap(), 0);
            }
        });
    }

    #[test]
    fn callback_errors_keep_identity_and_survive_cleanup_failure() {
        Python::initialize();
        Python::attach(|py| {
            let module = PyModule::from_code(
                py,
                c"class Sink:\n    def __init__(self):\n        self.error = RuntimeError('writer failed')\n    def write(self, data):\n        raise self.error\n",
                c"download_error_test.py",
                c"download_error_test",
            )
            .unwrap();
            let cid: Cid = Blake3::of(b"x").into();
            for cleanup_failure in [false, true] {
                let writer = module.getattr("Sink").unwrap().call0().unwrap();
                let source = writer.getattr("error").unwrap();
                let mut stream = Stream::new(b"x", Some(1));
                stream.cleanup_failure = cleanup_failure;
                let error = download(py, &stream, &cid, writer.unbind(), 1, None).unwrap_err();
                if cleanup_failure {
                    let store = crate::errors::store_error(py).unwrap();
                    assert!(error.value(py).is_instance(&store).unwrap());
                    assert!(error.value(py).getattr("__cause__").unwrap().is(&source));
                    assert!(error.to_string().contains("download cleanup failed"));
                } else {
                    assert!(error.value(py).is(&source));
                }
            }
        });
    }
}
