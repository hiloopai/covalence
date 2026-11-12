#![doc = include_str!("../README.md")]
#![deny(missing_docs)]

pub mod cache;
mod database;
mod error;
pub mod fetch;
mod hash;
mod local;
mod options;
mod pio;
mod ranges;
mod registry;
pub mod verify;

pub use covalence_lib_sqlite::{Connection, rusqlite};
pub use database::{Database, Stats};
pub use error::Error;
pub use options::Options;

/// The most bytes a block or a chunk can have: 1 GiB.
///
/// One fetch from a store is at most this many bytes too, unless a single
/// read of SQLite's needs more.
pub const MAX_BLOCK_SIZE: u64 = 1 << 30;

/// This crate's error for a store's.
pub(crate) fn store(error: covalence_cas_trait::Error) -> Error {
    use covalence_cas_trait::Error as Store;
    let source = Box::new(error);
    match *source {
        Store::Mismatch | Store::Check { .. } => Error::Corrupt { offset: 0, source },
        _ => Error::Store { source },
    }
}
