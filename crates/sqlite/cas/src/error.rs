//! The crate's error.

use std::io;

use covalence_lib_cid::Cid;
use covalence_lib_error::{BoxError, snafu::Snafu};
use covalence_lib_sqlite::{rusqlite, vfs};

/// Why a database could not be opened or read.
#[derive(Debug, Snafu)]
#[snafu(crate_root(covalence_lib_error::snafu), visibility(pub(crate)))]
#[non_exhaustive]
pub enum Error {
    /// The store has no bytes for the CID.
    #[snafu(display("the store has no bytes for {cid}"))]
    Missing {
        /// The CID asked for.
        cid: Cid,
    },
    /// The store failed to answer.
    #[snafu(display("the store failed: {source}"))]
    Store {
        /// The store's error.
        source: BoxError,
    },
    /// The bytes the store gave are not the database: they do not hash to the
    /// CID, or a verifier refused them. They were not used.
    #[snafu(display("the bytes at {offset} are not the database's: {source}"))]
    Corrupt {
        /// Where the refused bytes start in the database.
        offset: u64,
        /// What refused them.
        source: BoxError,
    },
    /// The store gave another number of bytes than the database has there.
    #[snafu(display("the store gave {actual} bytes at {offset}, not {expected}"))]
    Length {
        /// Where the bytes start in the database.
        offset: u64,
        /// The number of bytes the database has there.
        expected: u64,
        /// The number of bytes the store gave.
        actual: u64,
    },
    /// The database is larger than the limit that applies.
    #[snafu(display("the database has more than {limit} bytes"))]
    TooLarge {
        /// The limit in bytes.
        limit: u64,
    },
    /// This crate cannot do what was asked, and did nothing in its place.
    #[snafu(display("unsupported: {what}"))]
    Unsupported {
        /// What is not supported.
        what: String,
    },
    /// An option has a value that cannot be used.
    #[snafu(display("{reason}"))]
    Options {
        /// What is wrong.
        reason: String,
    },
    /// The bytes or the value are not a chunk manifest.
    #[snafu(display("not a chunk manifest: {reason}"))]
    Manifest {
        /// What is wrong.
        reason: String,
    },
    /// A local file could not be made, written or read, or a reader failed.
    #[snafu(display("{source}"))]
    Io {
        /// The I/O error.
        source: io::Error,
    },
    /// SQLite's VFS could not be registered or a connection opened.
    #[snafu(display("{source}"))]
    Vfs {
        /// The VFS's error.
        source: vfs::Error,
    },
    /// SQLite failed: the bytes are not a database file, a statement is wrong
    /// or tried to write, or a read failed.
    /// [`Database::explain`](crate::Database::explain) gives the cause of a
    /// failed read.
    #[snafu(display("{source}"))]
    Sqlite {
        /// SQLite's error.
        source: rusqlite::Error,
    },
}
