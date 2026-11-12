//! Read-only SQLite databases opened from a store: `SqliteDatabase`, its
//! connections and cursors, and `ChunkManifest`.

use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use covalence_lib_cid::Multicodec;
use covalence_lib_python::pyo3;
use covalence_sqlite_cas::{
    Database, Error, Options,
    cache::{FileCache, MemoryCache, NoCache},
    fetch::{Aligned, Exact, FetchPolicy, ReadAhead},
    rusqlite::{self, Batch, Connection, fallible_iterator::FallibleIterator, types::Value as Sql},
    verify::{Manifest, Verification},
};
use pyo3::{
    exceptions::PyTypeError,
    prelude::*,
    pybacked::{PyBackedBytes, PyBackedStr},
    types::{
        PyBool, PyByteArray, PyBytes, PyDict, PyFloat, PyInt, PyMapping, PySequence, PyString,
        PyTuple,
    },
};

use crate::{
    HashArg, PyCas, PyCid, PyHash,
    errors::{Class, raise, raise_as},
    fail,
};

/// `Ok(None)` for a store that has nothing under the CID.
fn found<T>(result: Result<T, Error>) -> PyResult<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(Error::Missing { .. }) => Ok(None),
        Err(error) => Err(raise(error)),
    }
}

/// The hash function `hash` names: BLAKE3 without one.
fn hash_code(hash: Option<HashArg<'_>>) -> PyResult<Multicodec> {
    hash.map_or(Ok(Multicodec::BLAKE3), |hash| hash.code())
}

/// The digests of the chunks of some bytes, to check ranges of the bytes with.
///
/// A CID can be checked only with all of its bytes at hand. A manifest
/// splits the bytes into chunks of `chunk_size` bytes and keeps the digest of
/// each, so any chunk can be checked alone. `SqliteDatabase.open` takes one
/// as `verify`, to read a database by range from a store it does not trust.
///
/// A manifest vouches for the bytes, so it must come from somewhere trusted:
/// make it from bytes that are, or `load` it by a CID that is.
///
/// >>> from covalence import Cas, ChunkManifest
/// >>> manifest = ChunkManifest.of(bytes(100_000), chunk_size=65536)
/// >>> manifest
/// ChunkManifest(size=100000, chunk_size=65536, hash='blake3')
/// >>> cas = Cas.memory()
/// >>> ChunkManifest.load(cas, manifest.put(cas)).chunks
/// 2
#[pyclass(frozen, name = "ChunkManifest", module = "covalence")]
#[pyo3(crate = "covalence_lib_python::pyo3")]
pub(crate) struct PyChunkManifest {
    inner: Manifest,
}

#[pymethods]
#[pyo3(crate = "covalence_lib_python::pyo3")]
impl PyChunkManifest {
    /// The manifest of `data` in chunks of `chunk_size` bytes.
    ///
    /// - `chunk_size` is from 1 to 2**30; another size raises ValueError.
    ///   Reading with the manifest fetches and checks whole chunks.
    /// - `hash` is the hash function of the digests: "blake3" (the default),
    ///   "sha256", "sha512" or "sha1". Another raises UnsupportedError.
    #[staticmethod]
    #[pyo3(signature = (data, *, chunk_size = 65536, hash = None))]
    fn of(
        py: Python<'_>,
        data: PyBackedBytes,
        chunk_size: u64,
        hash: Option<HashArg<'_>>,
    ) -> PyResult<Self> {
        let hash = hash_code(hash)?;
        let inner = py.detach(|| Manifest::of(hash, chunk_size, &data));
        Ok(Self {
            inner: inner.map_err(raise)?,
        })
    }

    /// The manifest of the file at `path`, as `of` makes it.
    ///
    /// The file is read in pieces, so it need not fit in memory. A file that
    /// cannot be read raises OSError.
    #[staticmethod]
    #[pyo3(signature = (path, *, chunk_size = 65536, hash = None))]
    fn of_file(
        py: Python<'_>,
        path: PathBuf,
        chunk_size: u64,
        hash: Option<HashArg<'_>>,
    ) -> PyResult<Self> {
        let hash = hash_code(hash)?;
        let inner = py.detach(|| {
            let file = File::open(&path)?;
            PyResult::Ok(Manifest::read(hash, chunk_size, file))
        })?;
        Ok(Self {
            inner: inner.map_err(raise)?,
        })
    }

    /// The manifest that `cas` holds under `cid`, or None if missing.
    ///
    /// - Bytes that do not hash to `cid` raise IntegrityError.
    /// - Bytes that are not a manifest raise ValueError.
    #[staticmethod]
    fn load(
        py: Python<'_>,
        cas: PyRef<'_, PyCas>,
        cid: PyRef<'_, PyCid>,
    ) -> PyResult<Option<Self>> {
        let (cas, cid) = (Arc::clone(&cas.inner), cid.key()?);
        let inner = found(py.detach(|| Manifest::load(&*cas, &cid)))?;
        Ok(inner.map(|inner| Self { inner }))
    }

    /// Store the manifest in `cas` and return the CID that `load` takes.
    ///
    /// The manifest is stored as DRISL: a map of the hash function's code,
    /// the size, the chunk size and the digests.
    fn put(&self, py: Python<'_>, cas: PyRef<'_, PyCas>) -> PyResult<PyCid> {
        let cas = Arc::clone(&cas.inner);
        let cid = py.detach(|| self.inner.put(&*cas)).map_err(raise)?;
        Ok(PyCid { inner: cid.into() })
    }

    /// The size in bytes of the content the manifest is of.
    #[getter]
    fn size(&self) -> u64 {
        self.inner.size()
    }

    /// The size in bytes of each chunk but the last, which may be shorter.
    #[getter]
    fn chunk_size(&self) -> u64 {
        self.inner.chunk_size()
    }

    /// The number of chunks.
    #[getter]
    fn chunks(&self) -> u64 {
        self.inner.chunks()
    }

    /// The hash function of the digests.
    #[getter]
    fn hash(&self) -> PyHash {
        PyHash(self.inner.hash())
    }

    fn __repr__(&self) -> String {
        let hash = PyHash(self.inner.hash());
        let hash = match hash.name() {
            Some(name) => format!("'{name}'"),
            None => format!("Hash.from_code({:#x})", hash.0.0),
        };
        format!(
            "ChunkManifest(size={}, chunk_size={}, hash={hash})",
            self.inner.size(),
            self.inner.chunk_size(),
        )
    }
}

/// `verify`: a mode by name, or the manifest to check ranges with.
#[derive(FromPyObject)]
#[pyo3(crate = "covalence_lib_python::pyo3")]
enum VerifyArg<'py> {
    Manifest(PyRef<'py, PyChunkManifest>),
    Name(PyBackedStr),
}

impl VerifyArg<'_> {
    fn verification(&self) -> PyResult<Verification> {
        match self {
            VerifyArg::Manifest(manifest) => Ok(Verification::manifest(manifest.inner.clone())),
            VerifyArg::Name(name) => match &**name {
                "download" => Ok(Verification::Download),
                "trusted-ranges" => Ok(Verification::TrustedRanges),
                "blake3-ranges" => Ok(Verification::Blake3Ranges),
                _ => Err(fail(
                    "verify must be 'download', 'trusted-ranges', 'blake3-ranges' or a ChunkManifest",
                )),
            },
        }
    }
}

/// The fetch policy `name`, sized by `bytes` where it has a size.
fn fetch_policy(name: &str, bytes: Option<u64>) -> PyResult<Arc<dyn FetchPolicy>> {
    Ok(match (name, bytes) {
        ("read-ahead", None) => Arc::new(ReadAhead::default()),
        ("read-ahead", Some(bytes)) => Arc::new(ReadAhead::new(bytes)),
        ("aligned", Some(bytes)) => Arc::new(Aligned::new(bytes)),
        ("aligned", None) => return Err(fail("fetch='aligned' needs fetch_bytes")),
        ("exact", _) => Arc::new(Exact),
        _ => return Err(fail("fetch must be 'read-ahead', 'aligned' or 'exact'")),
    })
}

/// A SQLite database held in a store, opened read-only.
///
/// `SqliteDatabase.open` makes the bytes under a CID ready for SQLite, and
/// `connect` gives connections that run SQL on them. The database is the
/// bytes of its CID and never changes: every write to it raises SqliteError.
///
/// >>> import sqlite3
/// >>> from covalence import Cas, SqliteDatabase
/// >>> source = sqlite3.connect(':memory:')
/// >>> _ = source.execute('CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)')
/// >>> _ = source.execute("INSERT INTO notes (body) VALUES ('first'), ('second')")
/// >>> source.commit()
/// >>> cas = Cas.memory()
/// >>> cid = cas.put(source.serialize())
/// >>> source.close()
/// >>> database = SqliteDatabase.open(cas, cid)
/// >>> with database.connect() as connection:
/// ...     connection.execute('SELECT id, body FROM notes WHERE id > ?', (1,)).fetchall()
/// [(2, 'second')]
#[pyclass(frozen, name = "SqliteDatabase", module = "covalence")]
#[pyo3(crate = "covalence_lib_python::pyo3")]
pub(crate) struct PySqliteDatabase {
    inner: Database,
}

#[pymethods]
#[pyo3(crate = "covalence_lib_python::pyo3")]
impl PySqliteDatabase {
    /// Open the database whose bytes `cas` holds under `cid`, or None if missing.
    ///
    /// `verify` says how far the store is trusted:
    ///
    /// - "download", the default, does not trust it. All the bytes are
    ///   downloaded to a local file with no name, and must hash to `cid` or
    ///   IntegrityError is raised. At most `max_bytes` bytes are accepted,
    ///   1 GiB by default; a larger database raises ValueError. A `cid` whose
    ///   hash function is not BLAKE3, SHA-256 or SHA-1 raises UnsupportedError.
    /// - "trusted-ranges" reads byte ranges as queries need them, and trusts
    ///   them: nothing checks the bytes against `cid`. Use it only for a
    ///   store trusted as much as a local file.
    /// - A `ChunkManifest` reads byte ranges and checks every chunk against
    ///   the manifest before SQLite sees it, so the store need not be
    ///   trusted. What is read is the content the manifest is of; `cid` only
    ///   says where to fetch. A stored size that is not the manifest's raises
    ///   IntegrityError.
    /// - "blake3-ranges", ranges checked against a BLAKE3 `cid` itself, is
    ///   not supported yet. It raises UnsupportedError before the store is
    ///   asked anything, and never falls back to another mode.
    ///
    /// The other options matter only when ranges are read:
    ///
    /// - `block_size` is the size in bytes of the blocks that are fetched and
    ///   cached: 4096 by default, from 1 to 2**30, or ValueError is raised.
    ///   With a manifest the blocks are its chunks.
    /// - `fetch` sizes the request when a read misses the cache. "read-ahead",
    ///   the default, fetches 64 KiB where reads begin, and four times as
    ///   much each time reads go on from there, up to `fetch_bytes` (4 MiB by
    ///   default). "aligned" fetches whole windows of `fetch_bytes` bytes,
    ///   which it needs. "exact" fetches only what is read.
    /// - `cache` keeps fetched blocks for every connection: "memory" (the
    ///   default), "file" for a local file with no name, or "none". It holds
    ///   up to `cache_bytes` bytes, 32 MiB by default. A cache smaller than
    ///   the database keeps a request to a quarter of what it holds, unless
    ///   one read needs more: with "none", only what is read is fetched.
    ///
    /// `dir` is the directory of the local files, the download and the file
    /// cache: the system's temporary directory by default.
    ///
    /// A store that fails raises StoreError. Whether the bytes are a SQLite
    /// database is found by `connect`.
    #[staticmethod]
    #[pyo3(signature = (
        cas, cid, *, verify = None, max_bytes = None, block_size = None,
        fetch = "read-ahead", fetch_bytes = None, cache = "memory", cache_bytes = None, dir = None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn open(
        py: Python<'_>,
        cas: PyRef<'_, PyCas>,
        cid: PyRef<'_, PyCid>,
        verify: Option<VerifyArg<'_>>,
        max_bytes: Option<u64>,
        block_size: Option<u64>,
        fetch: &str,
        fetch_bytes: Option<u64>,
        cache: &str,
        cache_bytes: Option<u64>,
        dir: Option<PathBuf>,
    ) -> PyResult<Option<Self>> {
        let verification =
            verify.map_or(Ok(Verification::Download), |verify| verify.verification())?;
        let cache_bytes = cache_bytes.unwrap_or(Options::CACHE_BYTES);
        let mut options = Options::new()
            .verification(verification)
            .fetch(fetch_policy(fetch, fetch_bytes)?)
            .max_download(max_bytes.unwrap_or(Options::MAX_DOWNLOAD))
            .block_size(block_size.unwrap_or(Options::BLOCK_SIZE));
        options = match (cache, &dir) {
            ("memory", _) => options.cache(MemoryCache::new(cache_bytes)),
            ("file", Some(dir)) => options.cache(FileCache::new(cache_bytes).in_dir(dir)),
            ("file", None) => options.cache(FileCache::new(cache_bytes)),
            ("none", _) => options.cache(NoCache),
            _ => return Err(fail("cache must be 'memory', 'file' or 'none'")),
        };
        if let Some(dir) = dir {
            options = options.dir(dir);
        }
        let (cas, cid) = (Arc::clone(&cas.inner), cid.key()?);
        let inner = found(py.detach(|| Database::open(cas, &cid, options)))?;
        Ok(inner.map(|inner| Self { inner }))
    }

    /// A new read-only connection to the database.
    ///
    /// - Connections share the database's cache, and each has SQLite's own
    ///   page cache.
    /// - A connection opens no other database: `ATTACH` and `VACUUM INTO`
    ///   raise SqliteError.
    /// - Bytes that are not a SQLite database raise SqliteError.
    /// - A read that fails raises its cause: IntegrityError for bytes that
    ///   were refused, StoreError for a store that failed.
    fn connect(&self, py: Python<'_>) -> PyResult<PySqliteConnection> {
        let connection = py.detach(|| self.inner.connect()).map_err(raise)?;
        Ok(PySqliteConnection {
            shared: Arc::new(Shared {
                database: self.inner.clone(),
                connection: Mutex::new(Some(connection)),
            }),
        })
    }

    /// The CID the database was opened with.
    #[getter]
    fn cid(&self) -> PyCid {
        PyCid {
            inner: (*self.inner.cid()).into(),
        }
    }

    /// The size of the database in bytes.
    #[getter]
    fn size(&self) -> u64 {
        self.inner.size()
    }

    /// Counts of what has been read so far, by every connection.
    ///
    /// - "reads" and "read_bytes": the reads SQLite made of the database, and
    ///   their bytes.
    /// - "fetches" and "fetched_bytes": the requests to the store for the
    ///   database's bytes, and the bytes it gave. A download is one fetch.
    ///
    /// `reads - fetches` reads were answered by the cache or the local file.
    fn stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let stats = self.inner.stats();
        let dict = PyDict::new(py);
        dict.set_item("reads", stats.reads)?;
        dict.set_item("read_bytes", stats.read_bytes)?;
        dict.set_item("fetches", stats.fetches)?;
        dict.set_item("fetched_bytes", stats.fetched_bytes)?;
        Ok(dict)
    }

    /// Size requests from now on with `fetch` and `fetch_bytes`, as in `open`.
    ///
    /// This holds for every connection, and the cache keeps its blocks. It
    /// does nothing for a database that was downloaded.
    #[pyo3(signature = (fetch, fetch_bytes = None))]
    fn set_fetch(&self, fetch: &str, fetch_bytes: Option<u64>) -> PyResult<()> {
        self.inner
            .set_fetch_policy(fetch_policy(fetch, fetch_bytes)?);
        Ok(())
    }

    fn __repr__(&self) -> String {
        format!(
            "SqliteDatabase(cid=Cid('{}'), size={})",
            self.inner.cid(),
            self.inner.size()
        )
    }
}

/// The values for a statement's placeholders.
enum Parameters {
    Positional(Vec<Sql>),
    Named(HashMap<String, Sql>),
}

impl Parameters {
    fn extract(parameters: Option<&Bound<'_, PyAny>>) -> PyResult<Self> {
        let Some(parameters) = parameters else {
            return Ok(Parameters::Positional(Vec::new()));
        };
        if let Ok(mapping) = parameters.cast::<PyMapping>() {
            let named = mapping.items()?.iter().map(|item| {
                let (name, value): (Bound<'_, PyAny>, Bound<'_, PyAny>) = item.extract()?;
                let name = name
                    .cast::<PyString>()
                    .map_err(|_| PyTypeError::new_err("parameter names must be str"))?;
                Ok((name.to_str()?.to_owned(), sql_value(&value)?))
            });
            return Ok(Parameters::Named(named.collect::<PyResult<_>>()?));
        }
        // A str or bytes is a sequence too, of its characters or bytes: given
        // as the parameters it is one value that was not wrapped in a tuple.
        let one_value = parameters.is_instance_of::<PyString>()
            || parameters.is_instance_of::<PyBytes>()
            || parameters.is_instance_of::<PyByteArray>();
        let Some(sequence) = parameters.cast::<PySequence>().ok().filter(|_| !one_value) else {
            return Err(PyTypeError::new_err(
                "parameters must be a sequence such as a tuple, or a mapping such as a dict",
            ));
        };
        let values = sequence.try_iter()?.map(|value| sql_value(&value?));
        Ok(Parameters::Positional(values.collect::<PyResult<_>>()?))
    }

    /// Bind the values to the placeholders of `statement`.
    fn bind(&self, statement: &mut rusqlite::Statement<'_>) -> Result<(), Failure> {
        let count = statement.parameter_count();
        match self {
            Parameters::Positional(values) => {
                if values.len() != count {
                    return Err(Failure::Usage(format!(
                        "the statement has {count} parameters, and {} values were given",
                        values.len()
                    )));
                }
                for (index, value) in values.iter().enumerate() {
                    statement.raw_bind_parameter(index + 1, value)?;
                }
            }
            Parameters::Named(named) => {
                for index in 1..=count {
                    let name = statement.parameter_name(index).map(str::to_owned);
                    let key = name.as_deref().and_then(|name| name.get(1..));
                    let value = key.and_then(|key| named.get(key)).ok_or_else(|| {
                        Failure::Usage(match &name {
                            Some(name) => format!("no value for the parameter {name}"),
                            None => {
                                format!("parameter {index} has no name: pass a tuple or a list")
                            }
                        })
                    })?;
                    statement.raw_bind_parameter(index, value)?;
                }
            }
        }
        Ok(())
    }
}

/// A Python value as SQLite takes it.
fn sql_value(value: &Bound<'_, PyAny>) -> PyResult<Sql> {
    if value.is_none() {
        Ok(Sql::Null)
    } else if value.is_instance_of::<PyBool>() {
        Ok(Sql::Integer(value.extract::<bool>()?.into()))
    } else if value.is_instance_of::<PyInt>() {
        Ok(Sql::Integer(value.extract()?))
    } else if value.is_instance_of::<PyFloat>() {
        Ok(Sql::Real(value.extract()?))
    } else if let Ok(text) = value.cast::<PyString>() {
        Ok(Sql::Text(text.to_str()?.to_owned()))
    } else if let Ok(bytes) = value.extract::<PyBackedBytes>() {
        Ok(Sql::Blob(bytes.to_vec()))
    } else {
        let class = value.get_type();
        let name = class.name()?;
        Err(PyTypeError::new_err(format!(
            "a parameter must be None, bool, int, float, str, bytes or bytearray, not {name}"
        )))
    }
}

/// A SQLite value as Python has it.
fn py_value(py: Python<'_>, value: Sql) -> PyResult<Bound<'_, PyAny>> {
    Ok(match value {
        Sql::Null => py.None().into_bound(py),
        Sql::Integer(integer) => integer.into_pyobject(py)?.into_any(),
        Sql::Real(real) => PyFloat::new(py, real).into_any(),
        Sql::Text(text) => PyString::new(py, &text).into_any(),
        Sql::Blob(blob) => PyBytes::new(py, &blob).into_any(),
    })
}

/// A row as a tuple.
fn py_row(py: Python<'_>, row: Vec<Sql>) -> PyResult<Bound<'_, PyTuple>> {
    let values = row.into_iter().map(|value| py_value(py, value));
    PyTuple::new(py, values.collect::<PyResult<Vec<_>>>()?)
}

/// Why a statement did not run.
enum Failure {
    /// The database's error, or SQLite's.
    Database(Error),
    /// A statement or parameters that cannot be run, as `ValueError`.
    Usage(String),
    /// A connection or a cursor that was closed, as `SqliteError`.
    Closed(&'static str),
}

impl From<rusqlite::Error> for Failure {
    fn from(source: rusqlite::Error) -> Self {
        Failure::Database(Error::Sqlite { source })
    }
}

impl From<Failure> for PyErr {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Database(error) => raise(error),
            Failure::Usage(message) => fail(message),
            Failure::Closed(what) => raise_as(Class::Sqlite, format!("the {what} is closed")),
        }
    }
}

/// What a statement gave: the names of its columns, if it has any, and its rows.
struct Outcome {
    columns: Option<Vec<String>>,
    rows: VecDeque<Vec<Sql>>,
}

/// A connection and the database it reads, shared with its cursors.
struct Shared {
    database: Database,
    /// `None` once closed.
    connection: Mutex<Option<Connection>>,
}

impl Shared {
    fn connection(&self) -> MutexGuard<'_, Option<Connection>> {
        self.connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Run the one statement in `sql` to its end.
    ///
    /// Call this without the interpreter attached: it waits for the
    /// connection, and reads the database.
    fn run(&self, sql: &str, parameters: &Parameters) -> Result<Outcome, Failure> {
        let guard = self.connection();
        let connection = guard.as_ref().ok_or(Failure::Closed("connection"))?;
        self.step(connection, sql, parameters)
            .map_err(|failure| match failure {
                // SQLite says only that a read failed: the database knows why.
                Failure::Database(Error::Sqlite { source }) => {
                    Failure::Database(self.database.explain(source))
                }
                other => other,
            })
    }

    fn step(
        &self,
        connection: &Connection,
        sql: &str,
        parameters: &Parameters,
    ) -> Result<Outcome, Failure> {
        let mut batch = Batch::new(connection, sql);
        let mut statement = batch
            .next()?
            .ok_or_else(|| Failure::Usage("there is no statement to run".into()))?;
        if batch.next()?.is_some() {
            return Err(Failure::Usage(
                "only one statement can be run at a time".into(),
            ));
        }
        parameters.bind(&mut statement)?;
        let width = statement.column_count();
        let names = statement.column_names();
        let columns = (width > 0).then(|| names.into_iter().map(str::to_owned).collect());
        let mut found = statement.raw_query();
        let mut rows = VecDeque::new();
        while let Some(row) = found.next()? {
            let values = (0..width).map(|column| row.get::<_, Sql>(column));
            rows.push_back(values.collect::<Result<_, _>>()?);
        }
        Ok(Outcome { columns, rows })
    }
}

/// A read-only connection to a `SqliteDatabase`, from `SqliteDatabase.connect`.
///
/// `execute`, `cursor` and `close` are those of a DB-API connection (PEP
/// 249). There is nothing to commit: the database cannot change.
///
/// - A connection can be used from any thread; it runs one statement at a
///   time.
/// - As a context manager it closes itself on exit.
///
/// >>> import sqlite3
/// >>> from covalence import Cas, SqliteDatabase
/// >>> source = sqlite3.connect(':memory:')
/// >>> _ = source.execute('CREATE TABLE t AS SELECT 1 AS x')
/// >>> source.commit()
/// >>> cas = Cas.memory()
/// >>> connection = SqliteDatabase.open(cas, cas.put(source.serialize())).connect()
/// >>> source.close()
/// >>> list(connection.execute('SELECT x, :y FROM t', {'y': 'two'}))
/// [(1, 'two')]
/// >>> connection.close()
#[pyclass(frozen, name = "SqliteConnection", module = "covalence")]
#[pyo3(crate = "covalence_lib_python::pyo3")]
pub(crate) struct PySqliteConnection {
    shared: Arc<Shared>,
}

#[pymethods]
#[pyo3(crate = "covalence_lib_python::pyo3")]
impl PySqliteConnection {
    /// Run one SQL statement and return a cursor over its rows.
    ///
    /// - `parameters` fills the statement's placeholders: a sequence such as
    ///   a tuple or a list for `?`, or a mapping such as a dict for `:name`,
    ///   `@name` and `$name`, keyed by the name without its first character.
    ///   Values are None, bool, int, float, str, bytes or bytearray. Another
    ///   type raises TypeError, as do str and bytes in place of a sequence;
    ///   an int outside 64 bits raises OverflowError.
    /// - A wrong number of values, a name without a value, more than one
    ///   statement and no statement raise ValueError.
    /// - The statement runs to its end here. Its rows stay in memory until
    ///   they are fetched: tuples of None, int, float, str and bytes.
    /// - SQL that SQLite refuses raises SqliteError, as do every write to
    ///   the database, `ATTACH` and `VACUUM INTO`. Temporary tables can be
    ///   written.
    /// - A read that fails raises its cause: IntegrityError for bytes that
    ///   were refused, StoreError for a store that failed.
    #[pyo3(signature = (sql, parameters = None))]
    fn execute(
        &self,
        py: Python<'_>,
        sql: &str,
        parameters: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<PySqliteCursor> {
        let cursor = self.cursor();
        cursor.run(py, sql, parameters)?;
        Ok(cursor)
    }

    /// A new cursor, with no rows until it runs a statement.
    fn cursor(&self) -> PySqliteCursor {
        PySqliteCursor {
            shared: Arc::clone(&self.shared),
            state: Mutex::new(Some(Outcome {
                columns: None,
                rows: VecDeque::new(),
            })),
        }
    }

    /// Close the connection. Closing again does nothing.
    ///
    /// Statements raise SqliteError from then on. Cursors keep the rows they
    /// have.
    fn close(&self, py: Python<'_>) {
        // Dropped unattached: closing waits for a running statement.
        py.detach(|| drop(self.shared.connection().take()));
    }

    /// The connection itself, for `with`.
    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// Close the connection at the end of `with`, whatever was raised.
    #[pyo3(signature = (*_exception))]
    fn __exit__(&self, py: Python<'_>, _exception: &Bound<'_, PyTuple>) {
        self.close(py);
    }
}

/// The rows of a statement, from `SqliteConnection.execute`.
///
/// `description` and the `fetch` methods are those of a DB-API cursor (PEP
/// 249). Iterating a cursor gives its remaining rows.
///
/// >>> import sqlite3
/// >>> from covalence import Cas, SqliteDatabase
/// >>> source = sqlite3.connect(':memory:')
/// >>> _ = source.execute('CREATE TABLE t AS SELECT 1 AS x UNION SELECT 2 UNION SELECT 3')
/// >>> source.commit()
/// >>> cas = Cas.memory()
/// >>> connection = SqliteDatabase.open(cas, cas.put(source.serialize())).connect()
/// >>> source.close()
/// >>> cursor = connection.execute('SELECT x, x * 1.5 AS y FROM t ORDER BY x')
/// >>> [column[0] for column in cursor.description]
/// ['x', 'y']
/// >>> cursor.fetchone(), cursor.fetchall()
/// ((1, 1.5), [(2, 3.0), (3, 4.5)])
#[pyclass(frozen, name = "SqliteCursor", module = "covalence")]
#[pyo3(crate = "covalence_lib_python::pyo3")]
pub(crate) struct PySqliteCursor {
    shared: Arc<Shared>,
    /// What the last statement gave and is not fetched yet; `None` once closed.
    state: Mutex<Option<Outcome>>,
}

impl PySqliteCursor {
    fn state(&self) -> MutexGuard<'_, Option<Outcome>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn run(
        &self,
        py: Python<'_>,
        sql: &str,
        parameters: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<()> {
        let parameters = Parameters::extract(parameters)?;
        if self.state().is_none() {
            return Err(Failure::Closed("cursor").into());
        }
        let outcome = py.detach(|| self.shared.run(sql, &parameters))?;
        if let Some(state) = &mut *self.state() {
            *state = outcome;
        }
        Ok(())
    }

    /// Up to `most` of the remaining rows.
    fn take(&self, most: usize) -> PyResult<Vec<Vec<Sql>>> {
        let mut state = self.state();
        let outcome = state.as_mut().ok_or(Failure::Closed("cursor"))?;
        let count = most.min(outcome.rows.len());
        Ok(outcome.rows.drain(..count).collect())
    }
}

#[pymethods]
#[pyo3(crate = "covalence_lib_python::pyo3")]
impl PySqliteCursor {
    /// Run one SQL statement and return this cursor over its rows.
    ///
    /// The statement runs as in `SqliteConnection.execute`. The rows of the
    /// cursor's earlier statement are dropped, unless this one raises.
    #[pyo3(signature = (sql, parameters = None))]
    fn execute(
        slf: Py<Self>,
        py: Python<'_>,
        sql: &str,
        parameters: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<Self>> {
        slf.get().run(py, sql, parameters)?;
        Ok(slf)
    }

    /// The columns of the last statement, or None if it has none.
    ///
    /// Each column is a tuple of its name and six None, as DB-API has it.
    /// It is None before any statement too.
    #[getter]
    fn description<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyTuple>>> {
        let state = self.state();
        let outcome = state.as_ref().ok_or(Failure::Closed("cursor"))?;
        let Some(columns) = &outcome.columns else {
            return Ok(None);
        };
        let columns = columns.iter().map(|name| {
            let none = || py.None().into_bound(py);
            let name = PyString::new(py, name).into_any();
            PyTuple::new(py, [name, none(), none(), none(), none(), none(), none()])
        });
        PyTuple::new(py, columns.collect::<PyResult<Vec<_>>>()?).map(Some)
    }

    /// The next row, or None when there are no more.
    fn fetchone<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyTuple>>> {
        let row = self.take(1)?.pop();
        row.map(|row| py_row(py, row)).transpose()
    }

    /// The next `size` rows, or as many as are left.
    #[pyo3(signature = (size = 1))]
    fn fetchmany<'py>(&self, py: Python<'py>, size: usize) -> PyResult<Vec<Bound<'py, PyTuple>>> {
        let rows = self.take(size)?.into_iter();
        rows.map(|row| py_row(py, row)).collect()
    }

    /// All the rows that are left.
    fn fetchall<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyTuple>>> {
        self.fetchmany(py, usize::MAX)
    }

    /// Close the cursor and drop its rows. Closing again does nothing.
    ///
    /// The cursor raises SqliteError from then on.
    fn close(&self) {
        *self.state() = None;
    }

    fn __iter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __next__<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyTuple>>> {
        self.fetchone(py)
    }
}

/// Adds the classes to the module.
pub(crate) fn add(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyChunkManifest>()?;
    m.add_class::<PySqliteDatabase>()?;
    m.add_class::<PySqliteConnection>()?;
    m.add_class::<PySqliteCursor>()
}
