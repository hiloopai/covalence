//! Error handling tools shared by Covalence crates.
//!
//! An internal facade (a crate that re-exports other crates) for Covalence's
//! own crates. Its API may change without notice.
//!
//! - [`snafu`]: concrete, matchable library errors, defined in the crate that
//!   owns the operation.
//! - [`miette`]: diagnostics at user-facing boundaries. Native applications may
//!   enable `fancy` for terminal rendering.
//! - Derive through the re-exported crate roots. For SNAFU, set
//!   `#[snafu(crate_root(covalence_lib_error::snafu))]` in downstream crates.
//! - This crate does not define a universal domain error type.
#![deny(missing_docs)]

pub use miette;
pub use snafu;

/// A boxed, thread-safe error for sources whose concrete type is not exposed.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;
