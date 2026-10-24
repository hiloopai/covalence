//! The native implementation of `covalence.config`.

use std::path::{Path, PathBuf};

use covalence_lib_config as config;
use covalence_lib_python::pyo3;
use pyo3::{
    exceptions::{PyOSError, PyValueError},
    prelude::*,
    types::{PyDict, PyList},
};

fn error(error: config::Error) -> PyErr {
    match error {
        config::Error::Name { .. } | config::Error::Parse { .. } => {
            PyValueError::new_err(error.to_string())
        }
        config::Error::Read { path, source } => {
            // Windows OS error codes are winerror, rather than POSIX errno.
            #[cfg(windows)]
            let error = PyOSError::new_err((0, source.to_string(), path, source.raw_os_error()));
            #[cfg(not(windows))]
            let error = PyOSError::new_err((source.raw_os_error(), source.to_string(), path));
            error
        }
        _ => PyOSError::new_err(error.to_string()),
    }
}

fn path_object(py: Python<'_>, path: PathBuf) -> PyResult<Bound<'_, PyAny>> {
    py.import("pathlib")?.getattr("Path")?.call1((path,))
}

/// Return the application's config directory without creating it.
///
/// An explicit override_dir is returned unchanged. Otherwise name must be a
/// single non-empty path component and the platform's XDG-style directory is
/// used. Invalid names raise ValueError; an unavailable home raises OSError.
#[pyfunction]
#[pyo3(crate = "covalence_lib_python::pyo3", signature = (name, *, override_dir=None))]
fn config_dir<'py>(
    py: Python<'py>,
    name: &str,
    override_dir: Option<PathBuf>,
) -> PyResult<Bound<'py, PyAny>> {
    path_object(py, config::config_dir(name, override_dir).map_err(error)?)
}

/// Return the application's cache directory without creating it.
///
/// An explicit override_dir is returned unchanged. Otherwise name must be a
/// single non-empty path component and the platform's XDG-style directory is
/// used. Invalid names raise ValueError; an unavailable home raises OSError.
#[pyfunction]
#[pyo3(crate = "covalence_lib_python::pyo3", signature = (name, *, override_dir=None))]
fn cache_dir<'py>(
    py: Python<'py>,
    name: &str,
    override_dir: Option<PathBuf>,
) -> PyResult<Bound<'py, PyAny>> {
    path_object(py, config::cache_dir(name, override_dir).map_err(error)?)
}

/// Return the application's data directory without creating it.
///
/// An explicit override_dir is returned unchanged. Otherwise name must be a
/// single non-empty path component and the platform's XDG-style directory is
/// used. Invalid names raise ValueError; an unavailable home raises OSError.
#[pyfunction]
#[pyo3(crate = "covalence_lib_python::pyo3", signature = (name, *, override_dir=None))]
fn data_dir<'py>(
    py: Python<'py>,
    name: &str,
    override_dir: Option<PathBuf>,
) -> PyResult<Bound<'py, PyAny>> {
    path_object(py, config::data_dir(name, override_dir).map_err(error)?)
}

/// Return the application's state directory without creating it.
///
/// An explicit override_dir is returned unchanged. Otherwise name must be a
/// single non-empty path component. Windows falls back to its data directory.
/// Invalid names raise ValueError; an unavailable home raises OSError.
#[pyfunction]
#[pyo3(crate = "covalence_lib_python::pyo3", signature = (name, *, override_dir=None))]
fn state_dir<'py>(
    py: Python<'py>,
    name: &str,
    override_dir: Option<PathBuf>,
) -> PyResult<Bound<'py, PyAny>> {
    path_object(py, config::state_dir(name, override_dir).map_err(error)?)
}

/// Return the application's runtime directory without creating it.
///
/// An explicit override_dir is returned unchanged. Otherwise name must be a
/// single non-empty path component. When no runtime directory is configured,
/// this falls back to the state directory. Permissions and session lifetime
/// are not enforced. Invalid names raise ValueError; an unavailable home
/// raises OSError.
#[pyfunction]
#[pyo3(crate = "covalence_lib_python::pyo3", signature = (name, *, override_dir=None))]
fn runtime_dir<'py>(
    py: Python<'py>,
    name: &str,
    override_dir: Option<PathBuf>,
) -> PyResult<Bound<'py, PyAny>> {
    path_object(py, config::runtime_dir(name, override_dir).map_err(error)?)
}

fn table_object(py: Python<'_>, table: toml::Table) -> PyResult<Bound<'_, PyDict>> {
    let result = PyDict::new(py);
    for (key, value) in table {
        result.set_item(key, value_object(py, value)?)?;
    }
    Ok(result)
}

fn value_object(py: Python<'_>, value: toml::Value) -> PyResult<Bound<'_, PyAny>> {
    Ok(match value {
        toml::Value::String(value) => value.into_pyobject(py)?.into_any(),
        toml::Value::Integer(value) => value.into_pyobject(py)?.into_any(),
        toml::Value::Float(value) => value.into_pyobject(py)?.into_any(),
        toml::Value::Boolean(value) => value.into_pyobject(py)?.to_owned().into_any(),
        toml::Value::Datetime(value) => {
            let class = match (value.date, value.time) {
                (Some(_), Some(_)) => "datetime",
                (Some(_), None) => "date",
                (None, Some(_)) => "time",
                (None, None) => {
                    return Err(PyValueError::new_err("TOML datetime has no date or time"));
                }
            };
            py.import("datetime")?
                .getattr(class)?
                .call_method1("fromisoformat", (value.to_string(),))?
        }
        toml::Value::Array(values) => {
            let result = PyList::empty(py);
            for value in values {
                result.append(value_object(py, value)?)?;
            }
            result.into_any()
        }
        toml::Value::Table(table) => table_object(py, table)?.into_any(),
    })
}

/// Parse TOML text into a dict of Python values.
///
/// Tables and arrays become dicts and lists. Other values become str, bool,
/// signed 64-bit int, float, datetime.datetime, datetime.date or datetime.time.
/// Date/time fractions are truncated to microseconds. Invalid TOML, integers
/// outside the signed 64-bit range and dates Python cannot represent raise
/// ValueError. This does not interpolate variables or merge configuration.
///
/// >>> from covalence.config import parse
/// >>> parse('enabled = true\nworkers = 2')
/// {'enabled': True, 'workers': 2}
#[pyfunction]
#[pyo3(crate = "covalence_lib_python::pyo3")]
fn parse<'py>(py: Python<'py>, text: &str) -> PyResult<Bound<'py, PyDict>> {
    let table = config::parse(text).map_err(|error| PyValueError::new_err(error.to_string()))?;
    table_object(py, table)
}

/// Read a UTF-8 TOML file into a dict, returning an empty dict if missing.
///
/// Values follow parse's conversion rules. Invalid TOML raises ValueError;
/// other read failures raise OSError. No files or directories are created.
#[pyfunction]
#[pyo3(crate = "covalence_lib_python::pyo3")]
fn read<'py>(py: Python<'py>, path: PathBuf) -> PyResult<Bound<'py, PyDict>> {
    table_object(py, config::read(Path::new(&path)).map_err(error)?)
}

/// Read a required UTF-8 TOML file into a dict.
///
/// Values follow parse's conversion rules. A missing file raises
/// FileNotFoundError, invalid TOML raises ValueError and other read failures
/// raise OSError. No files or directories are created.
#[pyfunction]
#[pyo3(crate = "covalence_lib_python::pyo3")]
fn read_required<'py>(py: Python<'py>, path: PathBuf) -> PyResult<Bound<'py, PyDict>> {
    table_object(py, config::read_required(Path::new(&path)).map_err(error)?)
}

pub(crate) fn add(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let config = PyModule::new(m.py(), "covalence.config")?;
    config.add(
        "__doc__",
        "Application directories and TOML configuration, backed by Rust.",
    )?;
    config.add_function(wrap_pyfunction!(config_dir, &config)?)?;
    config.add_function(wrap_pyfunction!(cache_dir, &config)?)?;
    config.add_function(wrap_pyfunction!(data_dir, &config)?)?;
    config.add_function(wrap_pyfunction!(state_dir, &config)?)?;
    config.add_function(wrap_pyfunction!(runtime_dir, &config)?)?;
    config.add_function(wrap_pyfunction!(parse, &config)?)?;
    config.add_function(wrap_pyfunction!(read, &config)?)?;
    config.add_function(wrap_pyfunction!(read_required, &config)?)?;
    m.add("config", config)
}
