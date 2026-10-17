//! The file-object wrapper checks sources even when a store skips their bytes.

use std::sync::atomic::{AtomicUsize, Ordering};

use covalence_cas_trait::{AsyncGet, AsyncPut, CasFuture, Error, Get, Op, Put, ready};
use covalence_lib_data::Bytes;
use covalence_lib_python::pyo3::types::{PyBytes, PyModule};

use super::*;

/// A create-only store may reuse a block without reading the source at all.
struct Reuse {
    calls: AtomicUsize,
    cleanup_failure: bool,
}

impl Reuse {
    fn new(cleanup_failure: bool) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            cleanup_failure,
        }
    }
}

impl Get for Reuse {
    fn get_unchecked(&self, _: &Cid) -> Result<Option<Bytes>, Error> {
        Err(Op::Get.unsupported())
    }
}

impl AsyncGet for Reuse {
    fn aget_unchecked<'a>(&'a self, cid: &'a Cid) -> CasFuture<'a, Option<Bytes>> {
        ready(self.get_unchecked(cid))
    }
}

impl Put for Reuse {
    fn put(&self, _: Bytes) -> Result<Cid, Error> {
        Err(Op::Put.unsupported())
    }
}

impl AsyncPut for Reuse {
    fn aput(&self, bytes: Bytes) -> CasFuture<'_, Cid> {
        ready(self.put(bytes))
    }
}

impl Cas for Reuse {
    fn put_from(&self, cid: Blake3, reader: &mut dyn Read, _: u64) -> Result<Cid, Error> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.cleanup_failure {
            let mut buffer = [0; 4];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => {
                        return Err(Error::Backend {
                            source: "upload cleanup failed".into(),
                        });
                    }
                }
            }
        }
        Ok(cid.into())
    }
}

/// Change the bytes only when the wrapper starts its second complete pass.
fn source(py: Python<'_>, first: &[u8], second: &[u8], fails: bool) -> Py<PyAny> {
    let module = PyModule::from_code(
        py,
        cr#"
import io

class Source(io.BytesIO):
    def __init__(self, first, second, fails):
        super().__init__(first)
        self.second = second
        self.seeks = 0
        self.second_reads = 0
        self.second_eof = False
        self.failure = RuntimeError('source disconnected') if fails else None

    def seek(self, offset, whence=0):
        self.seeks += 1
        if self.seeks == 2:
            super().seek(0)
            super().truncate(0)
            super().write(self.second)
        return super().seek(offset, whence)

    def read(self, count):
        if self.seeks >= 2:
            self.second_reads += 1
            if self.failure is not None:
                raise self.failure
        result = super().read(count)
        if self.seeks >= 2 and not result:
            self.second_eof = True
        return result
"#,
        c"upload_test.py",
        c"upload_test",
    )
    .unwrap();
    module
        .getattr("Source")
        .unwrap()
        .call1((PyBytes::new(py, first), PyBytes::new(py, second), fails))
        .unwrap()
        .unbind()
}

fn second_reads(py: Python<'_>, source: &Py<PyAny>) -> usize {
    source
        .bind(py)
        .getattr("second_reads")
        .unwrap()
        .extract()
        .unwrap()
}

#[test]
fn reused_input_is_drained_to_eof_after_the_store_returns() {
    Python::initialize();
    Python::attach(|py| {
        let bytes = b"verified source";
        let source = source(py, bytes, bytes, false);
        let store = Reuse::new(false);
        let key = upload(
            py,
            &store,
            source.clone_ref(py),
            bytes.len() as u64,
            Some(bytes.len() as u64),
        )
        .unwrap();
        assert_eq!(key, Blake3::of(bytes).into());
        assert_eq!(store.calls.load(Ordering::Relaxed), 1);
        assert!(second_reads(py, &source) > 0);
        assert!(
            source
                .bind(py)
                .getattr("second_eof")
                .unwrap()
                .extract::<bool>()
                .unwrap()
        );
        assert_eq!(
            source
                .bind(py)
                .call_method0("tell")
                .unwrap()
                .extract::<usize>()
                .unwrap(),
            bytes.len()
        );
        assert!(
            !source
                .bind(py)
                .getattr("closed")
                .unwrap()
                .extract::<bool>()
                .unwrap()
        );
    });
}

#[test]
fn reuse_drain_rejects_changed_content() {
    Python::initialize();
    Python::attach(|py| {
        let first = b"first content";
        let source = source(py, first, b"other content", false);
        let store = Reuse::new(false);
        let error = upload(py, &store, source.clone_ref(py), first.len() as u64, None).unwrap_err();
        let integrity = crate::errors::integrity_error(py).unwrap();
        assert!(error.value(py).is_instance(&integrity).unwrap());
        assert_eq!(store.calls.load(Ordering::Relaxed), 1);
        assert!(second_reads(py, &source) > 0);
    });
}

#[test]
fn reuse_drain_rejects_appended_or_truncated_content() {
    Python::initialize();
    Python::attach(|py| {
        let first = b"verified source";
        for second in [&b"verified source!"[..], &first[..first.len() - 1]] {
            let source = source(py, first, second, false);
            let store = Reuse::new(false);
            let error = upload(
                py,
                &store,
                source.clone_ref(py),
                first.len() as u64 + 1,
                None,
            )
            .unwrap_err();
            assert!(error.is_instance_of::<PyValueError>(py));
            assert_eq!(store.calls.load(Ordering::Relaxed), 1);
            assert!(second_reads(py, &source) > 0);
        }
    });
}

#[test]
fn reuse_drain_preserves_the_source_exception() {
    Python::initialize();
    Python::attach(|py| {
        let bytes = b"verified source";
        let source = source(py, bytes, bytes, true);
        let failure = source.bind(py).getattr("failure").unwrap();
        let store = Reuse::new(false);
        let error = upload(py, &store, source.clone_ref(py), bytes.len() as u64, None).unwrap_err();
        assert!(error.value(py).is(&failure));
        assert_eq!(store.calls.load(Ordering::Relaxed), 1);
        assert!(second_reads(py, &source) > 0);
    });
}

#[test]
fn cleanup_failure_keeps_the_source_exception_as_its_cause() {
    Python::initialize();
    Python::attach(|py| {
        let bytes = b"verified source";
        let source = source(py, bytes, bytes, true);
        let failure = source.bind(py).getattr("failure").unwrap();
        let store = Reuse::new(true);
        let error = upload(py, &store, source.clone_ref(py), bytes.len() as u64, None).unwrap_err();
        let store_error = crate::errors::store_error(py).unwrap();
        assert!(error.value(py).is_instance(&store_error).unwrap());
        assert!(error.value(py).getattr("__cause__").unwrap().is(&failure));
        assert!(error.to_string().contains("upload cleanup failed"));
        assert_eq!(store.calls.load(Ordering::Relaxed), 1);
        assert!(second_reads(py, &source) > 0);
    });
}
