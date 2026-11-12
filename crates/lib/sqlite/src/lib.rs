//! SQLite through [`rusqlite`], with SQLite built from source and Rust scalar functions.
//!
//! An internal facade (a crate that re-exports other crates) for Covalence's
//! own crates. Its API may change without notice.
//!
//! - Re-exports `rusqlite` with its `bundled`, `blob` and `functions` features,
//!   plus [`Connection`] and [`OptionalExtension`] at the root.
//! - The `hooks` feature enables `rusqlite`'s hooks, including authorizers,
//!   and the `limits` feature its run-time limits.
//! - [`vfs`] defines read-only SQLite virtual file systems in safe Rust and
//!   registers them by name. Its private callback module is the only code in
//!   this crate that uses `unsafe`.
#![deny(missing_docs)]

pub use rusqlite::{self, Connection, OptionalExtension};

pub mod vfs;
