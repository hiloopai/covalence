//! The exceptions Covalence raises, and which one each Rust error becomes.

use std::fmt::Display;

use covalence_lib_python::pyo3::{
    create_exception,
    exceptions::{PyException, PyNotImplementedError, PyOSError, PyValueError},
    prelude::*,
    sync::PyOnceLock,
    types::{PyDict, PyTuple, PyType},
};

create_exception!(
    covalence,
    Error,
    PyException,
    "The base class of IntegrityError, StoreError and UnsupportedError."
);

/// A subclass of [`Error`] and `base`, made once. `create_exception!` takes
/// a single base.
fn subclass<'py>(
    py: Python<'py>,
    cell: &'static PyOnceLock<Py<PyType>>,
    name: &str,
    base: Bound<'py, PyType>,
    doc: &str,
) -> PyResult<Bound<'py, PyType>> {
    let class = cell.get_or_try_init(py, || {
        let bases = PyTuple::new(py, [py.get_type::<Error>(), base])?;
        let namespace = PyDict::new(py);
        namespace.set_item("__module__", "covalence")?;
        namespace.set_item("__doc__", doc)?;
        let class = py.get_type::<PyType>().call1((name, bases, namespace))?;
        PyResult::Ok(class.cast_into::<PyType>()?.unbind())
    })?;
    Ok(class.bind(py).clone())
}

/// `IntegrityError(Error, ValueError)`.
pub(crate) fn integrity_error(py: Python<'_>) -> PyResult<Bound<'_, PyType>> {
    static CLASS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
    let doc = "Data does not match its CID.";
    subclass(
        py,
        &CLASS,
        "IntegrityError",
        py.get_type::<PyValueError>(),
        doc,
    )
}

/// `StoreError(Error, OSError)`.
pub(crate) fn store_error(py: Python<'_>) -> PyResult<Bound<'_, PyType>> {
    static CLASS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
    let doc = "The storage under a store failed.";
    subclass(py, &CLASS, "StoreError", py.get_type::<PyOSError>(), doc)
}

/// `UnsupportedError(Error, NotImplementedError)`.
pub(crate) fn unsupported_error(py: Python<'_>) -> PyResult<Bound<'_, PyType>> {
    static CLASS: PyOnceLock<Py<PyType>> = PyOnceLock::new();
    let doc = "Covalence or a store does not support the operation.";
    let base = py.get_type::<PyNotImplementedError>();
    subclass(py, &CLASS, "UnsupportedError", base, doc)
}

/// Adds the exceptions to the module.
pub(crate) fn add(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add("Error", py.get_type::<Error>())?;
    m.add("IntegrityError", integrity_error(py)?)?;
    m.add("StoreError", store_error(py)?)?;
    m.add("UnsupportedError", unsupported_error(py)?)
}

/// Which exception an error becomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    /// `ValueError`: bad input.
    Value,
    /// `IntegrityError`.
    Integrity,
    /// `StoreError`.
    Store,
    /// `UnsupportedError`.
    Unsupported,
    /// `OSError`: a local file failed.
    Os,
}

/// `message` as an exception of `class`.
pub(crate) fn raise_as(class: Class, message: impl Display) -> PyErr {
    let message = message.to_string();
    let class = match class {
        Class::Value => return PyValueError::new_err(message),
        Class::Os => return PyOSError::new_err(message),
        Class::Integrity => integrity_error,
        Class::Store => store_error,
        Class::Unsupported => unsupported_error,
    };
    Python::attach(|py| match class(py) {
        Ok(class) => PyErr::from_type(class, message),
        Err(error) => error,
    })
}

/// A Rust error that Python sees as an exception of [`Raise::class`].
pub(crate) trait Raise: Display {
    fn class(&self) -> Class;
}

/// `error` as its Python exception.
pub(crate) fn raise(error: impl Raise) -> PyErr {
    raise_as(error.class(), error)
}

impl Raise for covalence_lib_cid::Error {
    fn class(&self) -> Class {
        use covalence_lib_cid::Error::*;
        match self {
            Mismatch => Class::Integrity,
            Hash { .. } => Class::Unsupported,
            _ => Class::Value,
        }
    }
}

impl Raise for covalence_lib_dasl::Error {
    fn class(&self) -> Class {
        use covalence_lib_dasl::Error::*;
        match self {
            Cid { source } => source.class(),
            Codec { .. } => Class::Unsupported,
            _ => Class::Value,
        }
    }
}

impl Raise for covalence_kv_trait::Error {
    fn class(&self) -> Class {
        use covalence_kv_trait::Error::*;
        match self {
            Unsupported { .. } => Class::Unsupported,
            Backend { .. } => Class::Store,
            Io { .. } => Class::Os,
            _ => Class::Value,
        }
    }
}

impl Raise for covalence_kv_s3::Error {
    fn class(&self) -> Class {
        use covalence_kv_s3::Error::*;
        match self {
            Http { .. }
            | Status { .. }
            | MissingLength
            | RangeResponse
            | MissingField { .. }
            | Changed
            | Abort { .. }
            | Credentials { .. }
            | Runtime { .. } => Class::Store,
            Io { .. } => Class::Os,
            _ => Class::Value,
        }
    }
}

impl Raise for covalence_blob_trait::Error {
    fn class(&self) -> Class {
        use covalence_blob_trait::Error::*;
        match self {
            Mismatch => Class::Integrity,
            Check { source } => source.class(),
            Io { .. } | Backend { .. } => Class::Store,
            _ => Class::Value,
        }
    }
}

impl Raise for covalence_cas_trait::Error {
    fn class(&self) -> Class {
        use covalence_cas_trait::Error::*;
        match self {
            Mismatch | Check { .. } => Class::Integrity,
            Unsupported { .. } => Class::Unsupported,
            Backend { .. } => Class::Store,
            Io { .. } => Class::Os,
            _ => Class::Value,
        }
    }
}

#[cfg(test)]
mod tests {
    use covalence_lib_cid::{Error as CidError, Multicodec};
    use covalence_lib_dasl::Error as DaslError;

    use super::*;

    /// Whether `error` raises an instance of `class`.
    fn raises(py: Python<'_>, error: PyErr, class: &Bound<'_, PyType>) -> bool {
        error.value(py).is_instance(class).unwrap()
    }

    #[test]
    fn rust_errors_raise_their_classes() {
        Python::initialize();
        Python::attach(|py| {
            let base = py.get_type::<Error>();
            let integrity = integrity_error(py).unwrap();
            let store = store_error(py).unwrap();
            let unsupported = unsupported_error(py).unwrap();
            let value = py.get_type::<PyValueError>();
            let cases = [
                (raise(CidError::Mismatch), &integrity),
                (
                    raise(DaslError::Cid {
                        source: CidError::Mismatch,
                    }),
                    &integrity,
                ),
                (
                    raise(CidError::Hash {
                        hash: Multicodec(0xb220),
                        len: 32,
                    }),
                    &unsupported,
                ),
                (
                    raise(DaslError::Codec {
                        codec: Multicodec(0x70),
                    }),
                    &unsupported,
                ),
                (
                    raise(covalence_kv_trait::Op::Get.unsupported()),
                    &unsupported,
                ),
                (
                    raise(covalence_kv_trait::Error::Backend {
                        source: "disk".into(),
                    }),
                    &store,
                ),
                (
                    raise(covalence_kv_s3::Error::Status {
                        status: 403,
                        code: None,
                    }),
                    &store,
                ),
                (raise(covalence_blob_trait::Error::Mismatch), &integrity),
                (
                    raise(covalence_blob_trait::Error::from(CidError::Hash {
                        hash: Multicodec(0x1b),
                        len: 32,
                    })),
                    &unsupported,
                ),
                (
                    raise(covalence_blob_trait::Error::Backend {
                        source: "disk".into(),
                    }),
                    &store,
                ),
                (
                    raise(covalence_blob_trait::Error::from(std::io::Error::other(
                        "disk",
                    ))),
                    &store,
                ),
                (raise(covalence_cas_trait::Error::Mismatch), &integrity),
                (
                    raise(covalence_cas_trait::Error::from(CidError::Mismatch)),
                    &integrity,
                ),
                (
                    raise(covalence_cas_trait::Op::Put.unsupported()),
                    &unsupported,
                ),
                (
                    raise(covalence_cas_trait::Error::Backend {
                        source: "disk".into(),
                    }),
                    &store,
                ),
            ];
            for (error, class) in cases {
                assert!(raises(py, error.clone_ref(py), class));
                assert!(raises(py, error, &base));
            }
            let error = raise(covalence_kv_s3::Error::Region);
            assert!(raises(py, error.clone_ref(py), &value));
            assert!(!raises(py, error, &base));
            let os = py.get_type::<PyOSError>();
            for error in [
                raise(covalence_cas_trait::Error::Io {
                    source: std::io::Error::other("disk"),
                }),
                raise(covalence_kv_trait::Error::Io {
                    source: std::io::Error::other("disk"),
                }),
            ] {
                assert!(raises(py, error.clone_ref(py), &os));
                assert!(!raises(py, error, &base));
            }
            let error = raise(CidError::Malformed);
            assert!(raises(py, error.clone_ref(py), &value));
            assert!(!raises(py, error, &base));
            assert!(integrity.is_subclass(&value).unwrap());
            let not_implemented = py.get_type::<PyNotImplementedError>();
            assert!(unsupported.is_subclass(&not_implemented).unwrap());
        });
    }
}
