//! Read-only SQLite virtual file systems, written in safe Rust.
//!
//! A [VFS](https://www.sqlite.org/vfs.html) is how SQLite reaches the files
//! of a database. Implement [`Vfs`] and [`File`] to serve database files from
//! anywhere bytes can be read by range, then [`register`] the VFS under a
//! name and open connections through it.
//!
//! - Files are immutable and read-only: SQLite takes no locks, looks for no
//!   journal or write-ahead log, and every write fails with `SQLITE_READONLY`.
//! - SQLite's temporary files, such as the spill of a large sort, go to the
//!   VFS that was the default at registration, never to a [`Vfs`].
//! - A [`Vfs`] sees only main database files. Opening a named journal or
//!   write-ahead log fails.
//! - A registration lasts for the life of the process: SQLite keeps pointers
//!   to a VFS, so it is never freed. Register a few long-lived VFSes, not one
//!   per database.
//! - A panic in a [`Vfs`] or a [`File`] becomes an I/O error of the statement
//!   that caused it. It never unwinds into SQLite.
//! - SQLite finds a VFS by its name alone. [`register`] refuses a name that
//!   is taken, but other code can still give the same name to a VFS of its
//!   own by registering it with SQLite directly. [`Registered::open`] checks
//!   which VFS serves the connection, and fails rather than open through
//!   another one.
//!
//! ```
//! use std::io;
//!
//! use covalence_lib_sqlite::vfs::{self, File};
//!
//! /// A database held in memory.
//! struct Bytes(Vec<u8>);
//!
//! impl File for Bytes {
//!     fn size(&self) -> io::Result<u64> {
//!         Ok(self.0.len() as u64)
//!     }
//!     fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
//!         let start = offset as usize;
//!         buf.copy_from_slice(&self.0[start..start + buf.len()]);
//!         Ok(buf.len())
//!     }
//! }
//!
//! // An empty file is an empty database.
//! let registered = vfs::register("example-memory", |name: &str| {
//!     Ok((name == "empty").then(|| Box::new(Bytes(Vec::new())) as Box<dyn File>))
//! })?;
//! let connection = registered.open("empty")?;
//! let tables: i64 =
//!     connection.query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))?;
//! assert_eq!(tables, 0);
//! assert!(connection.execute("CREATE TABLE t (x)", []).is_err());
//! assert!(registered.open("missing").is_err());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::{ffi::CString, io, sync::Arc};

use covalence_lib_error::snafu::{ResultExt, Snafu, ensure};
use rusqlite::{Connection, OpenFlags};

#[allow(unsafe_code)]
mod ffi;

/// An immutable database file: bytes read by range.
///
/// SQLite may read a file from any thread, and several connections may read
/// files that share state, so implementations are `Send` and `Sync`.
pub trait File: Send + Sync {
    /// The file's size in bytes.
    ///
    /// Asked once, when SQLite opens the file; the answer is the file's size
    /// until it is closed. A size over `i64::MAX` fails the open.
    fn size(&self) -> io::Result<u64>;

    /// Fill `buf` with the bytes from `offset`, and return `buf.len()`.
    ///
    /// - `buf` is never empty, and never reaches past the size the file gave
    ///   when opened: SQLite's reads are cut to the bytes that exist.
    /// - `buf` holds zeros when this is called.
    /// - Returning any other count fails the read, as an error does: the file
    ///   gave a wrong size or changed after it was opened.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
}

impl<T: File + ?Sized> File for Arc<T> {
    fn size(&self) -> io::Result<u64> {
        (**self).size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        (**self).read_at(offset, buf)
    }
}

impl<T: File + ?Sized> File for Box<T> {
    fn size(&self) -> io::Result<u64> {
        (**self).size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        (**self).read_at(offset, buf)
    }
}

/// A read-only virtual file system: finds database files by name.
///
/// Every function `Fn(&str) -> io::Result<Option<Box<dyn File>>>` that is
/// `Send`, `Sync` and `'static` is a `Vfs`.
pub trait Vfs: Send + Sync + 'static {
    /// The main database file named `name`, or `None` if there is none.
    ///
    /// - `name` is the name the connection was opened with, as given.
    /// - `None` fails the open with `SQLITE_CANTOPEN`, and an error with
    ///   `SQLITE_IOERR`.
    fn open(&self, name: &str) -> io::Result<Option<Box<dyn File>>>;
}

impl<F> Vfs for F
where
    F: Fn(&str) -> io::Result<Option<Box<dyn File>>> + Send + Sync + 'static,
{
    fn open(&self, name: &str) -> io::Result<Option<Box<dyn File>>> {
        self(name)
    }
}

/// A VFS registered with SQLite under a name, for the life of the process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Registered {
    record: ffi::Record,
}

/// Register `vfs` with SQLite under `name`, for the life of the process.
///
/// - The VFS never becomes SQLite's default: connections use it only when
///   they name it, as [`Registered::open`] does.
/// - Fails with [`Error::Name`] if `name` is empty or has a NUL byte, and with
///   [`Error::Taken`] if SQLite already has a VFS of that name, such as
///   `unix`. `vfs` is dropped on failure.
pub fn register(name: &str, vfs: impl Vfs) -> Result<Registered, Error> {
    ensure!(!name.is_empty(), NameSnafu { name });
    let Ok(name) = CString::new(name) else {
        return NameSnafu { name }.fail();
    };
    let record = ffi::register(name, Box::new(vfs))?;
    Ok(Registered { record })
}

impl Registered {
    /// The name the VFS is registered under.
    pub fn name(&self) -> &'static str {
        // The name was a `&str` when registered.
        self.record.name.to_str().unwrap_or_default()
    }

    /// A read-only connection to the database file `file` of this VFS.
    ///
    /// - `file` reaches [`Vfs::open`] as given. Names of at most 1024 bytes
    ///   are supported; longer ones fail to open.
    /// - Fails with [`Error::Name`] if `file` is empty, is `:memory:`, starts
    ///   with `file:` or has a NUL byte: SQLite would read such a name as a
    ///   temporary database, an in-memory database or a URI, and bypass the VFS.
    /// - Fails with [`Error::Open`] if SQLite cannot open the connection: the
    ///   VFS has no such file, or the file's size or first 100 bytes cannot be
    ///   read. SQLite checks what it reads lazily, so a file that is not a
    ///   database, or a later read that fails, is an error of the statement
    ///   that needed the bytes.
    /// - Fails with [`Error::Shadowed`] if SQLite opened the connection
    ///   through another VFS of the same name. No connection is returned that
    ///   reads through any VFS but this one.
    /// - The connection is not serialized (`SQLITE_OPEN_NO_MUTEX`): use it
    ///   from one thread at a time, as `rusqlite` enforces.
    /// - SQL run on the connection can still reach other files. `ATTACH`
    ///   with a plain name opens another file of this VFS. A URI that names
    ///   another VFS lets `ATTACH` read a local file and `VACUUM INTO` write
    ///   a new one. Callers that run SQL they do not trust should set
    ///   `SQLITE_LIMIT_ATTACHED` to 0 (the `limits` feature), which refuses
    ///   all three.
    pub fn open(&self, file: &str) -> Result<Connection, Error> {
        let uri = file
            .get(..5)
            .is_some_and(|start| start.eq_ignore_ascii_case("file:"));
        let special = file.is_empty() || file == ":memory:" || uri || file.contains('\0');
        ensure!(!special, NameSnafu { name: file });
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let name = self.record.name;
        let connection =
            Connection::open_with_flags_and_vfs(file, flags, name).context(OpenSnafu)?;
        // SQLite found a VFS by this name, which need not be this one.
        let name = self.name();
        ensure!(self.record.serves(&connection), ShadowedSnafu { name });
        Ok(connection)
    }
}

/// Why a VFS could not be registered or a connection opened.
#[derive(Debug, Snafu)]
#[snafu(crate_root(covalence_lib_error::snafu))]
#[non_exhaustive]
pub enum Error {
    /// The name cannot name a VFS, or a database file of one.
    #[snafu(display("{name:?} cannot be used as a name here"))]
    Name {
        /// The refused name.
        name: String,
    },
    /// SQLite already has a VFS of this name.
    #[snafu(display("SQLite already has a VFS named {name:?}"))]
    Taken {
        /// The name that is taken.
        name: String,
    },
    /// SQLite refused the registration.
    #[snafu(display("SQLite refused to register the VFS (result code {code})"))]
    Register {
        /// SQLite's result code.
        code: i32,
    },
    /// SQLite could not open the connection.
    #[snafu(display("could not open the database: {source}"))]
    Open {
        /// SQLite's error.
        source: rusqlite::Error,
    },
    /// SQLite opened the connection through another VFS of the same name.
    ///
    /// Other code registered that VFS with SQLite directly, where names are
    /// not checked, after this one was registered.
    #[snafu(display("SQLite finds another VFS under the name {name:?}"))]
    Shadowed {
        /// The name both VFSes are registered under.
        name: String,
    },
}
