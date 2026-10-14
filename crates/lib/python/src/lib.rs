//! Python bindings through [`pyo3`].
//!
//! An internal facade (a crate that re-exports other crates) for Covalence's
//! own crates. Its API may change without notice.
//!
//! - Re-exports [`pyo3`], built against the stable ABI for Python 3.11 and later.
//! - The `extension-module` feature enables PyO3's feature of the same name, for
//!   building Python extension modules.
//! - Downstream PyO3 attributes must use
//!   `#[pyo3(crate = "covalence_lib_python::pyo3")]`.
#![deny(missing_docs)]

pub use pyo3;
