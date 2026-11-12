//! Databases, and the file their connections read.

use std::{
    fmt, io,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
};

use covalence_cas_trait::Cas;
use covalence_lib_cid::Cid;
use covalence_lib_error::snafu::{OptionExt, ResultExt, ensure};
use covalence_lib_sqlite::{
    Connection,
    rusqlite::{self, config::DbConfig, ffi::ErrorCode, limits::Limit},
    vfs,
};

use crate::{
    Error, Options,
    cache::Layout,
    error::{IoSnafu, LengthSnafu, MissingSnafu, OptionsSnafu, TooLargeSnafu, UnsupportedSnafu},
    fetch::FetchPolicy,
    local::Local,
    ranges::Ranges,
    registry::Ticket,
    verify::{Verification, check_chunk_size},
};

/// A SQLite database held in a content-addressed store, opened read-only.
///
/// A `Database` is the bytes of one CID made ready for SQLite, as its
/// [`Verification`] says: downloaded and checked, or read from the store by
/// range as queries need them. [`Database::connect`] gives connections to it.
///
/// - The database never changes: it is the bytes of its CID, and every write
///   fails.
/// - Clones are cheap, and with all their connections share one cache.
/// - Connections stay usable after the `Database` is dropped.
///
/// ```
/// use std::sync::Arc;
///
/// use covalence_cas_memory::MemoryCas;
/// use covalence_cas_trait::Put;
/// use covalence_lib_cid::Cid;
/// use covalence_sqlite_cas::{Database, Options, rusqlite::Connection};
///
/// // Make a small database file, and put its bytes in a store.
/// let dir = tempfile::tempdir()?;
/// let path = dir.path().join("example.db");
/// Connection::open(&path)?.execute_batch("CREATE TABLE t (x); INSERT INTO t VALUES (42);")?;
/// let cas = Arc::new(MemoryCas::default());
/// let cid: Cid = cas.put(std::fs::read(&path)?.into())?;
///
/// // Open it from the store. By default it is downloaded and checked first.
/// let database = Database::open(cas, &cid, Options::default())?;
/// let connection = database.connect()?;
/// let x: i64 = connection.query_row("SELECT x FROM t", [], |row| row.get(0))?;
/// assert_eq!(x, 42);
/// assert!(connection.execute("INSERT INTO t VALUES (43)", []).is_err());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone)]
pub struct Database {
    inner: Arc<Inner>,
}

struct Inner {
    cid: Cid,
    file: Arc<Shared>,
    ticket: Ticket,
}

impl Database {
    /// Open the database whose bytes `cas` holds under `cid`.
    ///
    /// What this does depends on [`Options::verification`]:
    ///
    /// - [`Verification::Download`], the default, downloads all the bytes to
    ///   a local file and checks that they hash to `cid`.
    /// - [`Verification::TrustedRanges`] asks the store only for the size.
    /// - [`Verification::VerifiedRanges`] also checks that size against the
    ///   verifier's.
    /// - [`Verification::Blake3Ranges`] fails with [`Error::Unsupported`]
    ///   before the store is asked anything.
    ///
    /// # Errors
    ///
    /// - [`Error::Missing`] if the store has nothing under `cid`.
    /// - [`Error::Store`] if the store fails.
    /// - [`Error::Corrupt`] if downloaded bytes do not hash to `cid`.
    /// - [`Error::TooLarge`] if a download is over
    ///   [`Options::max_download`], or the database has more than `i64::MAX`
    ///   bytes, the most SQLite can address.
    /// - [`Error::Length`] if the store's size is not the verifier's, or the
    ///   store miscounts a download.
    /// - [`Error::Unsupported`] if `cid` has a hash function that this crate
    ///   does not compute, so that a download cannot be checked. BLAKE3,
    ///   SHA-256 and SHA-1 CIDs can.
    /// - [`Error::Options`] if the block size, or a verifier's chunk size, is
    ///   not between 1 and [`MAX_BLOCK_SIZE`](crate::MAX_BLOCK_SIZE).
    /// - [`Error::Io`] if the local file or the cache cannot be made.
    ///
    /// Whether the bytes are a SQLite database is found by
    /// [`Database::connect`].
    pub fn open(cas: Arc<dyn Cas>, cid: &Cid, options: Options) -> Result<Self, Error> {
        let source = match &options.verification {
            Verification::Download => {
                let dir = options.dir.as_deref();
                Source::Local(Local::download(&*cas, cid, options.max_download, dir)?)
            }
            Verification::TrustedRanges => {
                let layout = layout(&*cas, cid, None, options.block_size)?;
                Source::ranges(cas, cid, layout, None, &options)?
            }
            Verification::VerifiedRanges(verifier) => {
                let size = Some(verifier.size());
                let layout = layout(&*cas, cid, size, verifier.chunk_size())?;
                Source::ranges(cas, cid, layout, Some(Arc::clone(verifier)), &options)?
            }
            Verification::Blake3Ranges => {
                return UnsupportedSnafu {
                    what: "ranges checked with BLAKE3 proofs",
                }
                .fail();
            }
        };
        let limit = i64::MAX as u64;
        ensure!(source.size() <= limit, TooLargeSnafu { limit });
        let file = Arc::new(Shared {
            size: source.size(),
            source,
            reads: AtomicU64::new(0),
            read_bytes: AtomicU64::new(0),
            failed: Mutex::new(None),
        });
        let ticket = Ticket::new(Arc::clone(&file) as Arc<dyn vfs::File>, cid);
        let inner = Inner {
            cid: *cid,
            file,
            ticket,
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// A new read-only connection to the database.
    ///
    /// - Each connection has its own SQLite page cache, and shares this
    ///   database's block cache and fetch policy with the others.
    /// - A connection is used from one thread at a time; open one for each
    ///   thread that reads.
    /// - The schema is not trusted (`trusted_schema` is off), and page
    ///   contents are checked as they are read (`cell_size_check` is on), as
    ///   SQLite advises for database files from elsewhere. Both can be
    ///   changed on the connection.
    /// - No other database can be attached: `SQLITE_LIMIT_ATTACHED` is 0, so
    ///   `ATTACH` and `VACUUM INTO` fail. SQLite would let either name
    ///   another VFS in a URI, and so read a local file or write a new one.
    ///   Code that wants to attach raises the limit with
    ///   [`Connection::set_limit`].
    /// - SQLite's temporary files, such as the spill of a large sort, are
    ///   local files of the system's default VFS.
    /// - Only the header is read here. The connection's first statement
    ///   reads the schema.
    ///
    /// # SQL functions of the application
    ///
    /// The schema is the database's, and may name any function. With
    /// `trusted_schema` off, SQLite lets it call only functions registered as
    /// [innocuous]. For any other function that the application registers:
    ///
    /// - A view or trigger that names it fails when it is used.
    /// - A generated column or index that names it makes every statement
    ///   fail, as a malformed schema, if the function is registered before
    ///   the connection's first statement.
    /// - Registered later, it is not checked there: SQLite checks generated
    ///   columns and indexes only while it reads the schema. Reading a
    ///   virtual column then calls the function, as does
    ///   `PRAGMA integrity_check` for an index, whatever the function's
    ///   flags.
    ///
    /// So register functions before the connection's first statement.
    ///
    /// [SQLite's documentation] lists what else the setting refuses.
    ///
    /// # Errors
    ///
    /// - [`Error::Sqlite`] if the bytes do not start as a SQLite database
    ///   does.
    /// - The error of the read, such as [`Error::Corrupt`] or
    ///   [`Error::Store`], if reading the start of the database failed.
    /// - [`Error::Vfs`] if SQLite could not open the connection otherwise.
    ///
    /// [SQLite's documentation]: https://www.sqlite.org/c3ref/c_dbconfig_defensive.html#sqlitedbconfigtrustedschema
    /// [innocuous]: rusqlite::functions::FunctionFlags::SQLITE_INNOCUOUS
    pub fn connect(&self) -> Result<Connection, Error> {
        let connection = match self.inner.ticket.connect() {
            Ok(connection) => connection,
            Err(vfs::Error::Open { source }) => return Err(self.explain(source)),
            Err(source) => return Err(Error::Vfs { source }),
        };
        let prepare = || {
            connection.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
            connection.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
            connection.pragma_update(None, "cell_size_check", true)?;
            // Reads the header, so that a file that is no database fails here.
            // The schema is left to the first statement: SQLite checks the
            // functions that indexes and generated columns name only while it
            // reads the schema, so the application registers its own first.
            connection.query_row("PRAGMA schema_version", [], |_| Ok(()))
        };
        prepare().map_err(|error| self.explain(error))?;
        Ok(connection)
    }

    /// Why a statement on a connection to this database failed.
    ///
    /// SQLite reports a read that failed only as a disk I/O error. If `error`
    /// is one, and a read of this database has failed since the last call,
    /// this is that read's error, such as [`Error::Corrupt`] or
    /// [`Error::Store`]; otherwise it is `error` as [`Error::Sqlite`].
    ///
    /// The failed read may be that of another connection to this database, if
    /// several fail at once.
    pub fn explain(&self, error: rusqlite::Error) -> Error {
        let failed = match error.sqlite_error_code() {
            Some(ErrorCode::SystemIoFailure) => self.inner.file.failed().take(),
            _ => None,
        };
        failed.unwrap_or(Error::Sqlite { source: error })
    }

    /// The CID the database was opened with.
    pub fn cid(&self) -> &Cid {
        &self.inner.cid
    }

    /// The size of the database in bytes.
    pub fn size(&self) -> u64 {
        self.inner.file.size
    }

    /// What has been read so far, by every connection.
    pub fn stats(&self) -> Stats {
        let file = &self.inner.file;
        let (fetches, fetched_bytes) = match &file.source {
            Source::Local(local) => (1, local.size()),
            Source::Ranges(ranges) => ranges.fetched(),
        };
        Stats {
            reads: file.reads.load(Relaxed),
            read_bytes: file.read_bytes.load(Relaxed),
            fetches,
            fetched_bytes,
        }
    }

    /// Use `policy` to size the fetches that start from now on, for every
    /// connection.
    ///
    /// The cache keeps its blocks. This does nothing for a database that was
    /// downloaded whole.
    pub fn set_fetch_policy(&self, policy: impl FetchPolicy + 'static) {
        if let Source::Ranges(ranges) = &self.inner.file.source {
            ranges.set_policy(Arc::new(policy));
        }
    }
}

impl fmt::Debug for Database {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Database")
            .field("cid", &self.inner.cid)
            .field("size", &self.inner.file.size)
            .finish_non_exhaustive()
    }
}

/// Counts of what has been read of a [`Database`] since it was opened.
///
/// `reads - fetches` is the number of reads that the cache or the local file
/// answered alone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Stats {
    /// The reads SQLite made of the database file.
    pub reads: u64,
    /// The bytes SQLite read.
    pub read_bytes: u64,
    /// The requests to the store for bytes of the database: its ranges, or
    /// its one download.
    pub fetches: u64,
    /// The bytes the store gave in those requests.
    pub fetched_bytes: u64,
}

/// The size and blocks of the database `cid` in `cas`.
///
/// The size is the store's, and must be `verified`'s if there is one.
fn layout(
    cas: &dyn Cas,
    cid: &Cid,
    verified: Option<u64>,
    block_size: u64,
) -> Result<Layout, Error> {
    check_chunk_size(block_size).map_err(|reason| OptionsSnafu { reason }.build())?;
    // As for the bytes: a size to be checked need not be vouched for.
    let stored = match verified {
        Some(_) => cas.size_unchecked(cid),
        None => cas.size(cid),
    };
    let stored = (stored.map_err(crate::store)?).context(MissingSnafu { cid: *cid })?;
    let size = verified.unwrap_or(stored);
    ensure!(
        stored == size,
        LengthSnafu {
            offset: 0u64,
            expected: size,
            actual: stored,
        }
    );
    Ok(Layout::new(size, block_size))
}

/// Where a database's bytes are read from.
enum Source {
    Local(Local),
    Ranges(Ranges),
}

impl Source {
    fn ranges(
        cas: Arc<dyn Cas>,
        cid: &Cid,
        layout: Layout,
        verifier: Option<Arc<dyn crate::verify::RangeVerifier>>,
        options: &Options,
    ) -> Result<Self, Error> {
        let cache = options.cache.build(layout).context(IoSnafu)?;
        let policy = Arc::clone(&options.fetch);
        Ok(Source::Ranges(Ranges::new(
            cas, *cid, layout, verifier, policy, cache,
        )))
    }

    fn size(&self) -> u64 {
        match self {
            Source::Local(local) => local.size(),
            Source::Ranges(ranges) => ranges.size(),
        }
    }
}

/// The file that every connection to a database reads.
struct Shared {
    source: Source,
    size: u64,
    reads: AtomicU64,
    read_bytes: AtomicU64,
    /// The error of the last read that failed, until it is asked for.
    failed: Mutex<Option<Error>>,
}

impl Shared {
    fn failed(&self) -> std::sync::MutexGuard<'_, Option<Error>> {
        self.failed.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl vfs::File for Shared {
    fn size(&self) -> io::Result<u64> {
        Ok(self.size)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.reads.fetch_add(1, Relaxed);
        self.read_bytes.fetch_add(buf.len() as u64, Relaxed);
        let read = match &self.source {
            Source::Local(local) => local.read_at(offset, buf),
            Source::Ranges(ranges) => ranges.read_at(offset, buf),
        };
        match read {
            Ok(()) => Ok(buf.len()),
            Err(error) => {
                // SQLite keeps only a result code: the cause waits here.
                let summary = io::Error::other(error.to_string());
                // The error it takes the place of is a store's or a
                // verifier's, and is dropped once the lock is released.
                let old = self.failed().replace(error);
                drop(old);
                Err(summary)
            }
        }
    }
}
