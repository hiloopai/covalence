//! Command-line parsing tools for Covalence binaries.
//!
//! An internal facade (a crate that re-exports other crates) for Covalence's
//! own crates. Its API may change without notice.
//!
//! - Re-exports [`clap`], with derives enabled.
//! - Import `covalence_lib_cli::clap::{self, Parser, Subcommand}` so
//!   derive-generated paths resolve through this crate.
//! - Argument definitions belong in binaries.
#![deny(missing_docs)]

pub use clap;
