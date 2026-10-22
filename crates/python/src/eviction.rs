//! Python eviction metadata and the borrowed native policy adapter.

use std::{
    sync::Mutex,
    time::{Duration, SystemTime},
};

use covalence_cas_trait::{EvictionCandidate, EvictionPolicy};
use covalence_lib_python::pyo3;
use pyo3::{exceptions::PyTypeError, prelude::*, types::PyBool};

use crate::{PyCid, fail};

/// A durably backed cached copy offered to an eviction policy.
///
/// Properties are read-only. `stored_size` counts the stored representation,
/// which may be compressed. `modified_at` is Unix seconds, or None if unknown.
/// A policy can approve releasing this copy, never the backing content.
#[pyclass(frozen, name = "EvictionCandidate", module = "covalence")]
#[pyo3(crate = "covalence_lib_python::pyo3")]
pub(crate) struct PyEvictionCandidate {
    inner: EvictionCandidate,
}

#[pymethods]
#[pyo3(crate = "covalence_lib_python::pyo3")]
impl PyEvictionCandidate {
    #[new]
    #[pyo3(signature = (cid, stored_size, modified_at = None))]
    fn new(cid: PyRef<'_, PyCid>, stored_size: u64, modified_at: Option<f64>) -> PyResult<Self> {
        let modified_at = modified_at.map(timestamp).transpose()?;
        Ok(Self {
            inner: EvictionCandidate::new(cid.key()?, stored_size, modified_at),
        })
    }

    /// The content identifier of the cached block.
    #[getter]
    fn cid(&self) -> PyCid {
        PyCid {
            inner: self.inner.cid.into(),
        }
    }

    /// The number of bytes occupied by the cached representation.
    #[getter]
    fn stored_size(&self) -> u64 {
        self.inner.stored_size
    }

    /// The cached representation's modification time in Unix seconds, if known.
    #[getter]
    fn modified_at(&self) -> Option<f64> {
        self.inner.modified_at.map(|time| {
            time.duration_since(SystemTime::UNIX_EPOCH)
                .map(|duration| duration.as_secs_f64())
                .unwrap_or_else(|error| -error.duration().as_secs_f64())
        })
    }
}

/// Validate a Python timestamp without allowing floating-point conversion panics.
fn timestamp(seconds: f64) -> PyResult<SystemTime> {
    let duration = Duration::try_from_secs_f64(seconds.abs())
        .map_err(|_| fail("modified_at must be a finite supported Unix timestamp"))?;
    if seconds.is_sign_negative() {
        SystemTime::UNIX_EPOCH.checked_sub(duration)
    } else {
        SystemTime::UNIX_EPOCH.checked_add(duration)
    }
    .ok_or_else(|| fail("modified_at is outside the supported timestamp range"))
}

/// An owned Python reference borrowed by one native flush call.
pub(crate) struct Policy {
    object: Py<PyAny>,
    error: Mutex<Option<PyErr>>,
}

impl Policy {
    pub(crate) fn new(object: Py<PyAny>) -> Self {
        Self {
            object,
            error: Mutex::new(None),
        }
    }

    pub(crate) fn take_error(&self) -> Option<PyErr> {
        self.error.lock().expect("policy error lock").take()
    }
}

impl EvictionPolicy for Policy {
    fn should_evict(&self, candidate: &EvictionCandidate) -> bool {
        if self.error.lock().expect("policy error lock").is_some() {
            return false;
        }
        let result = Python::attach(|py| {
            let candidate = Py::new(py, PyEvictionCandidate { inner: *candidate })?;
            let decision = self
                .object
                .bind(py)
                .call_method1("should_evict", (candidate,))?;
            if !decision.is_instance_of::<PyBool>() {
                return Err(PyTypeError::new_err("should_evict must return bool"));
            }
            decision.extract::<bool>()
        });
        match result {
            Ok(decision) => decision,
            Err(error) => {
                let mut first = self.error.lock().expect("policy error lock");
                if first.is_none() {
                    *first = Some(error);
                }
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use covalence_lib_cid::Blake3;
    use pyo3::{exceptions::PyLookupError, types::PyModule};

    use super::*;

    #[test]
    fn callback_failures_keep_candidates_and_preserve_the_first_exception() {
        Python::initialize();
        Python::attach(|py| {
            let module = PyModule::from_code(
                py,
                c"class Policy:\n    def __init__(self, result):\n        self.result = result\n        self.calls = 0\n        self.error = LookupError('policy failed')\n    def should_evict(self, candidate):\n        self.calls += 1\n        assert candidate.stored_size == 3\n        if self.result == 'raise':\n            raise self.error\n        return self.result\n",
                c"eviction_test.py",
                c"eviction_test",
            ).unwrap();
            let candidate = EvictionCandidate::new(Blake3::of(b"value").into(), 3, None);
            for result in [
                py.None(),
                1_i32.into_pyobject(py).unwrap().into_any().unbind(),
            ] {
                let object = module.getattr("Policy").unwrap().call1((result,)).unwrap();
                let policy = Policy::new(object.clone().unbind());
                assert!(!policy.should_evict(&candidate));
                assert!(!policy.should_evict(&candidate));
                assert_eq!(
                    object.getattr("calls").unwrap().extract::<usize>().unwrap(),
                    1
                );
                assert!(
                    policy
                        .take_error()
                        .unwrap()
                        .is_instance_of::<PyTypeError>(py)
                );
            }
            let object = module.getattr("Policy").unwrap().call1(("raise",)).unwrap();
            let source = object.getattr("error").unwrap();
            let policy = Policy::new(object.clone().unbind());
            assert!(!policy.should_evict(&candidate));
            let error = policy.take_error().unwrap();
            assert!(error.is_instance_of::<PyLookupError>(py));
            assert!(error.value(py).is(&source));
            for result in [true, false] {
                let object = module.getattr("Policy").unwrap().call1((result,)).unwrap();
                let policy = Policy::new(object.unbind());
                assert_eq!(policy.should_evict(&candidate), result);
                assert!(policy.take_error().is_none());
            }
        });
    }
}
