//! Serialization traits and derives.
//!
//! An internal facade (a crate that re-exports other crates) for Covalence's
//! own crates. Its API may change without notice.
//!
//! - Re-exports [`serde`] with derives enabled, and its [`Serialize`] and
//!   [`Deserialize`] traits and derives at the root.
//! - Formats own their codecs and validation.
//! - Downstream derives must use `#[serde(crate = "covalence_lib_serde::serde")]`:
//!
//! ```
//! use covalence_lib_serde::{Deserialize, Serialize};
//! #[derive(Serialize, Deserialize)]
//! #[serde(crate = "covalence_lib_serde::serde")]
//! struct Record { label: String }
//! ```
#![deny(missing_docs)]

pub use serde;
pub use serde::{Deserialize, Serialize};
