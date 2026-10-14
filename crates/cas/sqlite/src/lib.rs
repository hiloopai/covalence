//! A content-addressed store in SQLite.
//!
//! [`SqliteCas`] keeps blocks in a SQLite database, in a file or in memory,
//! and finds them by [BLAKE3](https://github.com/BLAKE3-team/BLAKE3-specs) or
//! SHA-256 digest. [`Sha256Policy`] decides which blocks SHA-256 finds.
//!
//! A [multicodec](https://github.com/multiformats/multicodec) code is a
//! registered number naming a format or hash function; the store records each
//! digest with the code of its hash function.
//!
//! ```
//! use covalence_cas_sqlite::SqliteCas;
//! use covalence_cas_trait::{Cas, Get, Put};
//! use covalence_lib_cid::{Cid, Sha256};
//! let store = SqliteCas::open_in_memory()?;
//! let cid: Cid = store.put("hello".into())?;
//! assert_eq!(store.get(&cid)?.unwrap(), "hello");
//! assert!(store.has(&cid)?);
//! let sha256: Sha256 = store.put("hello".into())?;
//! assert_eq!(store.get(&sha256)?.unwrap(), "hello");
//! # Ok::<(), covalence_cas_trait::Error>(())
//! ```
#![deny(missing_docs)]

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use covalence_cas_trait::{
    AsyncGet, AsyncPut, Cas, CasFuture, Error, Get, GetExt, Op, Put, map, ready,
};
use covalence_lib_cid::{Blake3, Cid, Fact, Hashed, Multicodec, O256, Sha256, storage};
use covalence_lib_dasl::Value;
use covalence_lib_data::Bytes;
use covalence_lib_error::snafu::Snafu;
use covalence_lib_sqlite::rusqlite::functions::{Context, FunctionFlags};
use covalence_lib_sqlite::rusqlite::{self, TransactionBehavior, blob::Blob, params};
use covalence_lib_sqlite::{Connection, OptionalExtension};

/// A store in SQLite.
///
/// The database has three tables:
///
/// - `blobs`: one row per block, its bytes inline in `data` when `storage` is
///   empty. Reading a row with other `storage` fails with a backend error.
/// - `hashes`: maps a digest and its hash function's multicodec code to a
///   block. Every block has a BLAKE3 row, and a SHA-256 row when indexed. Other
///   hashes, such as the SHA-1 of Git objects, fit the same table.
/// - `meta`: the [`Sha256Policy`] and the database's [`id`](SqliteCas::id).
///
/// Reads and writes:
///
/// - A BLAKE3 or SHA-256 CID with any codec finds its bytes, if it has the
///   row; [`Put`] over [`Cid`] returns the raw BLAKE3 CID.
/// - Rows can be corrupted on disk, so `get` checks the bytes it returns
///   against the requested hash, whichever row found them; `get_unchecked`,
///   sizes and ranges do not.
/// - Sizes and ranges read only the bytes they need from the block.
/// - `has` consults the hash index and block metadata, without loading or
///   checking the bytes.
/// - Puts hash in Rust. The SQL function `hash(code, data)`, registered on the
///   store's connection, serves only [`SqliteCas::index_sha256`] and
///   [`SqliteCas::verify`]: it never decides what a read checks.
/// - [`Cas::info`] is `{"name": "sqlite", "id": <hex id or null>}`;
///   [`Cas::stats`] counts `blocks` and the `bytes` of inline data.
///
/// One connection, behind a mutex, serves every caller. Files use SQLite's
/// write-ahead log (`journal_mode = WAL`) with `synchronous = NORMAL`. The
/// async methods call the sync ones, so they block.
pub struct SqliteCas {
    db: Mutex<Connection>,
    policy: Sha256Policy,
}

/// When a store gives a block its SHA-256 row.
///
/// - Reads by SHA-256 find only blocks with the row.
/// - [`SqliteCas::index_sha256`] adds the missing rows under any policy.
/// - The policy is recorded in `meta`, as the codes hashed on every put
///   (`hashes`) and those hashed on request (`on-demand`).
///
/// ```
/// use covalence_cas_sqlite::{Sha256Policy, SqliteCas};
/// use covalence_cas_trait::{Get, Put};
/// use covalence_lib_cid::{Cid, Sha256};
/// let store = SqliteCas::open_in_memory_with(Sha256Policy::OnDemand)?;
/// let _: Cid = store.put("hello".into())?;
/// let sha256 = Sha256::of(b"hello");
/// assert_eq!(store.get(&sha256)?, None);
/// assert_eq!(store.index_sha256()?, 1);
/// assert_eq!(store.get(&sha256)?.unwrap(), "hello");
/// # Ok::<(), covalence_cas_trait::Error>(())
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sha256Policy {
    /// Every put hashes with SHA-256 too.
    Always,
    /// A [`Put`] over [`Sha256`] adds the row, including for a block already
    /// put by BLAKE3; a BLAKE3 put does not.
    #[default]
    OnDemand,
    /// No put adds the row: a [`Put`] over [`Sha256`] is unsupported.
    Never,
}

impl Sha256Policy {
    /// The codes hashed on every put, then on request, as `meta` records them.
    fn codes(self) -> [&'static str; 2] {
        match self {
            Sha256Policy::Always => ["blake3,sha2-256", ""],
            Sha256Policy::OnDemand => ["blake3", "sha2-256"],
            Sha256Policy::Never => ["blake3", ""],
        }
    }

    /// The policy `meta` records as `codes`, if any.
    fn from_codes(codes: [&str; 2]) -> Option<Self> {
        [Self::Always, Self::OnDemand, Self::Never]
            .into_iter()
            .find(|policy| policy.codes() == codes)
    }
}

/// A block kept in a storage this store cannot read: its `storage` column is
/// not empty.
#[derive(Debug, Snafu)]
#[snafu(crate_root(covalence_lib_error::snafu))]
#[snafu(display("unknown blob storage {storage:?}"))]
pub struct UnknownStorage {
    storage: String,
}

/// Hashes recorded in `meta` that this store does not support.
#[derive(Debug, Snafu)]
#[snafu(crate_root(covalence_lib_error::snafu))]
#[snafu(display("unsupported hashes {hashes:?}, on demand {on_demand:?}"))]
pub struct UnknownHashes {
    hashes: String,
    on_demand: String,
}

impl SqliteCas {
    /// A store in a fresh in-memory database, with the default policy.
    pub fn open_in_memory() -> Result<Self, Error> {
        Self::open_in_memory_with(Sha256Policy::default())
    }

    /// A store in a fresh in-memory database, with `policy`.
    pub fn open_in_memory_with(policy: Sha256Policy) -> Result<Self, Error> {
        Self::new(Connection::open_in_memory().map_err(backend)?, Some(policy))
    }

    /// A store in the database file at `path`, created if missing, with the
    /// policy recorded there, or the default. Fails with a backend error,
    /// [`UnknownHashes`], if the recorded policy is not one this store knows.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Self::new(open_file(path.as_ref())?, None)
    }

    /// A store in the database file at `path`, created if missing, recording
    /// `policy` for later opens in place of any recorded before.
    pub fn open_with(path: impl AsRef<Path>, policy: Sha256Policy) -> Result<Self, Error> {
        Self::new(open_file(path.as_ref())?, Some(policy))
    }

    fn new(mut db: Connection, policy: Option<Sha256Policy>) -> Result<Self, Error> {
        db.set_transaction_behavior(TransactionBehavior::Immediate);
        add_hash_function(&db).map_err(backend)?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS blobs (
                blobid INTEGER PRIMARY KEY,
                storage TEXT NOT NULL DEFAULT '',
                data BLOB
            );
            CREATE TABLE IF NOT EXISTS hashes (
                digest BLOB NOT NULL,
                hashfn INTEGER NOT NULL,
                blobid INTEGER NOT NULL,
                PRIMARY KEY (digest, hashfn)
            ) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value BLOB NOT NULL
            );",
        )
        .map_err(backend)?;
        let policy = match policy {
            Some(policy) => {
                let [hashes, on_demand] = policy.codes();
                db.execute(
                    "INSERT OR REPLACE INTO meta (key, value)
                    VALUES ('hashes', ?1), ('on-demand', ?2)",
                    [hashes.as_bytes(), on_demand.as_bytes()],
                )
                .map_err(backend)?;
                policy
            }
            None => recorded_policy(&db)?.unwrap_or_default(),
        };
        Ok(Self {
            db: Mutex::new(db),
            policy,
        })
    }

    /// The connection.
    fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap()
    }

    /// When this store gives a block its SHA-256 row.
    pub fn policy(&self) -> Sha256Policy {
        self.policy
    }

    /// Adds a SHA-256 row for every inline block that lacks one, whatever the
    /// policy; returns how many it added.
    pub fn index_sha256(&self) -> Result<usize, Error> {
        self.db()
            .execute(
                "INSERT OR IGNORE INTO hashes (digest, hashfn, blobid)
                SELECT hash(?1, data), ?1, blobid FROM blobs
                WHERE storage = '' AND data IS NOT NULL AND blobid NOT IN
                    (SELECT blobid FROM hashes WHERE hashfn = ?1)",
                [Multicodec::SHA2_256.0 as i64],
            )
            .map_err(backend)
    }

    /// The `blobid`s of inline blocks with a digest that does not match their
    /// bytes, in increasing order, hashing each inline block once per digest.
    /// Fails with a backend error if a digest's hash function is not one
    /// `hash` computes.
    pub fn verify(&self) -> Result<Vec<i64>, Error> {
        self.db()
            .prepare(
                "SELECT DISTINCT blobid FROM hashes JOIN blobs USING (blobid)
                WHERE storage = '' AND digest IS NOT hash(hashfn, data)
                ORDER BY blobid",
            )
            .and_then(|mut query| query.query_map([], |row| row.get(0))?.collect())
            .map_err(backend)
    }

    /// The database's id, if one was set.
    pub fn id(&self) -> Result<Option<O256>, Error> {
        let id = self
            .db()
            .prepare_cached("SELECT value FROM meta WHERE key = 'id'")
            .and_then(|mut query| query.query_row([], |row| row.get(0)).optional())
            .map_err(backend)?;
        Ok(id.map(O256::from_array))
    }

    /// Names the database with `id`, replacing any earlier one.
    pub fn set_id(&self, id: O256) -> Result<(), Error> {
        self.db()
            .execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('id', ?1)",
                [id.as_bytes()],
            )
            .map_err(backend)?;
        Ok(())
    }

    /// The inline bytes of the blob with `digest` under `hash`.
    fn select(&self, hash: Multicodec, digest: &[u8]) -> Result<Option<Bytes>, Error> {
        let blob = self
            .db()
            .prepare_cached(
                "SELECT storage, data FROM hashes JOIN blobs USING (blobid)
                WHERE digest = ?1 AND hashfn = ?2",
            )
            .and_then(|mut query| {
                query
                    .query_row(params![digest, hash.0 as i64], |row| {
                        let storage: String = row.get(0)?;
                        Ok(if storage.is_empty() {
                            Ok(row.get::<_, Vec<u8>>(1)?)
                        } else {
                            Err(UnknownStorage { storage })
                        })
                    })
                    .optional()
            })
            .map_err(backend)?;
        blob.map(|blob| blob.map(Bytes::from).map_err(backend))
            .transpose()
    }

    /// The id of the inline blob under `digest`, or `None` if missing,
    /// without reading its payload.
    fn find_blob(db: &Connection, hash: Multicodec, digest: &[u8]) -> Result<Option<i64>, Error> {
        let found = db
            .prepare_cached(
                "SELECT blobid, storage FROM hashes JOIN blobs USING (blobid)
             WHERE digest = ?1 AND hashfn = ?2",
            )
            .and_then(|mut query| {
                query
                    .query_row(params![digest, hash.0 as i64], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                    })
                    .optional()
            })
            .map_err(backend)?;
        let Some((blobid, storage)) = found else {
            return Ok(None);
        };
        if !storage.is_empty() {
            return Err(backend(UnknownStorage { storage }));
        }
        Ok(Some(blobid))
    }

    /// Opens the inline blob under `digest` for incremental reads, or `None`
    /// if missing. Reads through it load only the bytes asked for.
    fn open_blob<'a>(
        db: &'a Connection,
        hash: Multicodec,
        digest: &[u8],
    ) -> Result<Option<Blob<'a>>, Error> {
        let Some(blobid) = Self::find_blob(db, hash, digest)? else {
            return Ok(None);
        };
        db.blob_open("main", "blobs", "data", blobid, true)
            .map(Some)
            .map_err(backend)
    }

    /// Check the hash index and storage without reading the blob payload.
    fn contains(&self, hash: Multicodec, digest: &[u8]) -> Result<bool, Error> {
        Ok(Self::find_blob(&self.db(), hash, digest)?.is_some())
    }

    /// The size of the inline blob under `digest`, not checked.
    fn select_size(&self, hash: Multicodec, digest: &[u8]) -> Result<Option<u64>, Error> {
        let db = self.db();
        Ok(Self::open_blob(&db, hash, digest)?.map(|blob| blob.len() as u64))
    }

    /// Up to `len` bytes from `start` of the inline blob under `digest`, not
    /// checked.
    fn select_range(
        &self,
        hash: Multicodec,
        digest: &[u8],
        start: u64,
        len: u64,
    ) -> Result<Option<Bytes>, Error> {
        let db = self.db();
        let Some(blob) = Self::open_blob(&db, hash, digest)? else {
            return Ok(None);
        };
        let size = blob.len() as u64;
        let first = start.min(size);
        let mut bytes = vec![0; len.min(size - first) as usize];
        blob.read_at_exact(&mut bytes, first as usize)
            .map_err(backend)?;
        Ok(Some(bytes.into()))
    }

    /// Stores `data` unless a blob has its BLAKE3 digest, and adds a SHA-256
    /// row if `sha256` is given, in one write transaction.
    fn insert(&self, blake3: &[u8], sha256: Option<&[u8]>, data: &[u8]) -> Result<(), Error> {
        let db = self.db();
        let insert = || {
            let tx = db.unchecked_transaction()?;
            let found = tx
                .prepare_cached("SELECT blobid FROM hashes WHERE digest = ?1 AND hashfn = ?2")?
                .query_row(params![blake3, Multicodec::BLAKE3.0 as i64], |row| {
                    row.get(0)
                })
                .optional()?;
            let blobid: i64 = match found {
                Some(blobid) => blobid,
                None => {
                    tx.prepare_cached("INSERT INTO blobs (data) VALUES (?1)")?
                        .execute([data])?;
                    let blobid = tx.last_insert_rowid();
                    add_row(&tx, blake3, Multicodec::BLAKE3, blobid)?;
                    blobid
                }
            };
            if let Some(sha256) = sha256 {
                add_row(&tx, sha256, Multicodec::SHA2_256, blobid)?;
            }
            tx.commit()
        };
        insert().map_err(backend)
    }
}

/// Maps `digest` under `hash` to `blobid`, unless it maps somewhere already.
fn add_row(db: &Connection, digest: &[u8], hash: Multicodec, blobid: i64) -> rusqlite::Result<()> {
    db.prepare_cached("INSERT OR IGNORE INTO hashes (digest, hashfn, blobid) VALUES (?1, ?2, ?3)")?
        .execute(params![digest, hash.0 as i64, blobid])?;
    Ok(())
}

/// A file database in WAL mode.
fn open_file(path: &Path) -> Result<Connection, Error> {
    let db = Connection::open(path).map_err(backend)?;
    db.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
        .map_err(backend)?;
    Ok(db)
}

/// The policy recorded in `meta`, if any.
fn recorded_policy(db: &Connection) -> Result<Option<Sha256Policy>, Error> {
    let codes = |key: &str| -> Result<Option<String>, Error> {
        let codes: Option<Vec<u8>> = db
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(backend)?;
        Ok(codes.map(|codes| String::from_utf8_lossy(&codes).into_owned()))
    };
    let (Some(hashes), on_demand) = (codes("hashes")?, codes("on-demand")?) else {
        return Ok(None);
    };
    let on_demand = on_demand.unwrap_or_default();
    Sha256Policy::from_codes([&hashes, &on_demand])
        .map(Some)
        .ok_or_else(|| backend(UnknownHashes { hashes, on_demand }))
}

/// Registers `hash(code, data)`: the digest of `data` under the hash function
/// with multicodec `code`, or null for null data. An error for a code
/// [`Multicodec::digest`] does not compute.
fn add_hash_function(db: &Connection) -> rusqlite::Result<()> {
    db.create_scalar_function(
        "hash",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context: &Context<'_>| {
            let code: i64 = context.get(0)?;
            let Some(data) = context.get_raw(1).as_bytes_or_null()? else {
                return Ok(None);
            };
            Multicodec(code as u64)
                .digest(data)
                .map(Some)
                .ok_or_else(|| {
                    rusqlite::Error::UserFunctionError(
                        format!("unknown hash function {code:#x}").into(),
                    )
                })
        },
    )
}

fn backend(error: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::Backend {
        source: error.into(),
    }
}

impl Get<Blake3> for SqliteCas {
    fn get_unchecked(&self, key: &Blake3) -> Result<Option<Bytes>, Error> {
        self.select(Multicodec::BLAKE3, key.digest())
    }

    fn size_unchecked(&self, key: &Blake3) -> Result<Option<u64>, Error> {
        self.select_size(Multicodec::BLAKE3, key.digest())
    }

    fn get_range_unchecked(
        &self,
        key: &Blake3,
        start: u64,
        len: u64,
    ) -> Result<Option<Bytes>, Error> {
        self.select_range(Multicodec::BLAKE3, key.digest(), start, len)
    }

    /// The bytes under `key`, checked against it.
    fn get(&self, key: &Blake3) -> Result<Option<Bytes>, Error> {
        Ok(self.get_fact(key)?.map(|fact| fact.bytes().clone()))
    }
}

impl Get<Sha256> for SqliteCas {
    fn get_unchecked(&self, key: &Sha256) -> Result<Option<Bytes>, Error> {
        self.select(Multicodec::SHA2_256, key.digest())
    }

    fn size_unchecked(&self, key: &Sha256) -> Result<Option<u64>, Error> {
        self.select_size(Multicodec::SHA2_256, key.digest())
    }

    fn get_range_unchecked(
        &self,
        key: &Sha256,
        start: u64,
        len: u64,
    ) -> Result<Option<Bytes>, Error> {
        self.select_range(Multicodec::SHA2_256, key.digest(), start, len)
    }

    /// The bytes under `key`, checked against it.
    fn get(&self, key: &Sha256) -> Result<Option<Bytes>, Error> {
        Ok(self.get_fact(key)?.map(|fact| fact.bytes().clone()))
    }
}

impl Get for SqliteCas {
    fn get_unchecked(&self, key: &Cid) -> Result<Option<Bytes>, Error> {
        if let Some(key) = key.raw_blake3() {
            self.get_unchecked(&key)
        } else {
            key.raw_sha256()
                .map_or(Ok(None), |key| self.get_unchecked(&key))
        }
    }

    fn size_unchecked(&self, key: &Cid) -> Result<Option<u64>, Error> {
        if let Some(key) = key.raw_blake3() {
            self.size_unchecked(&key)
        } else {
            key.raw_sha256()
                .map_or(Ok(None), |key| self.size_unchecked(&key))
        }
    }

    fn get_range_unchecked(&self, key: &Cid, start: u64, len: u64) -> Result<Option<Bytes>, Error> {
        if let Some(key) = key.raw_blake3() {
            self.get_range_unchecked(&key, start, len)
        } else {
            key.raw_sha256()
                .map_or(Ok(None), |key| self.get_range_unchecked(&key, start, len))
        }
    }

    /// The bytes under `key`, checked against it.
    fn get(&self, key: &Cid) -> Result<Option<Bytes>, Error> {
        if let Some(key) = key.raw_blake3() {
            self.get(&key)
        } else {
            key.raw_sha256().map_or(Ok(None), |key| self.get(&key))
        }
    }
}

impl AsyncGet for SqliteCas {
    fn aget_unchecked<'a>(&'a self, key: &'a Cid) -> CasFuture<'a, Option<Bytes>> {
        ready(self.get_unchecked(key))
    }

    fn asize_unchecked<'a>(&'a self, key: &'a Cid) -> CasFuture<'a, Option<u64>> {
        ready(self.size_unchecked(key))
    }

    fn aget_range_unchecked<'a>(
        &'a self,
        key: &'a Cid,
        start: u64,
        len: u64,
    ) -> CasFuture<'a, Option<Bytes>> {
        ready(self.get_range_unchecked(key, start, len))
    }

    fn aget<'a>(&'a self, key: &'a Cid) -> CasFuture<'a, Option<Bytes>> {
        ready(self.get(key))
    }
}

impl AsyncGet<Sha256> for SqliteCas {
    fn aget_unchecked<'a>(&'a self, key: &'a Sha256) -> CasFuture<'a, Option<Bytes>> {
        ready(self.get_unchecked(key))
    }

    fn asize_unchecked<'a>(&'a self, key: &'a Sha256) -> CasFuture<'a, Option<u64>> {
        ready(self.size_unchecked(key))
    }

    fn aget_range_unchecked<'a>(
        &'a self,
        key: &'a Sha256,
        start: u64,
        len: u64,
    ) -> CasFuture<'a, Option<Bytes>> {
        ready(self.get_range_unchecked(key, start, len))
    }

    fn aget<'a>(&'a self, key: &'a Sha256) -> CasFuture<'a, Option<Bytes>> {
        ready(self.get(key))
    }
}

impl Put<Blake3> for SqliteCas {
    /// Also adds the SHA-256 row under [`Sha256Policy::Always`].
    fn put(&self, bytes: Bytes) -> Result<Blake3, Error> {
        self.put_fact(Fact::of(bytes))
    }

    /// Stores `fact` without hashing it again; also adds the SHA-256 row
    /// under [`Sha256Policy::Always`].
    fn put_fact(&self, fact: Fact<Blake3>) -> Result<Blake3, Error> {
        let sha256 =
            (self.policy == Sha256Policy::Always).then(|| storage::Sha256::digest_of(fact.bytes()));
        let sha256 = sha256.as_ref().map(|digest| &digest[..]);
        self.insert(fact.claim().digest(), sha256, fact.bytes())?;
        Ok(*fact.claim())
    }
}

impl Put<Sha256> for SqliteCas {
    /// Stores the block once, with both rows; unsupported under
    /// [`Sha256Policy::Never`].
    fn put(&self, bytes: Bytes) -> Result<Sha256, Error> {
        if self.policy == Sha256Policy::Never {
            return Err(Op::Put.unsupported());
        }
        let fact = Fact::<Sha256>::of(bytes);
        let blake3 = storage::Blake3::digest_of(fact.bytes());
        self.insert(&blake3, Some(fact.claim().digest()), fact.bytes())?;
        Ok(*fact.claim())
    }
}

impl Put for SqliteCas {
    fn put(&self, bytes: Bytes) -> Result<Cid, Error> {
        Ok(Put::<Blake3>::put(self, bytes)?.into())
    }

    /// Stores a BLAKE3 `fact` without hashing; puts any other.
    fn put_fact(&self, fact: Fact) -> Result<Cid, Error> {
        match fact.raw_blake3() {
            Some(fact) => Ok(self.put_fact(fact)?.into()),
            None => self.put(fact.bytes().clone()),
        }
    }
}

impl AsyncPut for SqliteCas {
    fn aput(&self, bytes: Bytes) -> CasFuture<'_, Cid> {
        ready(self.put(bytes))
    }

    fn aput_fact(&self, fact: Fact) -> CasFuture<'_, Cid> {
        ready(self.put_fact(fact))
    }
}

impl AsyncPut<Sha256> for SqliteCas {
    fn aput(&self, bytes: Bytes) -> CasFuture<'_, Sha256> {
        ready(self.put(bytes))
    }
}

impl Cas for SqliteCas {
    /// Check the hash index and block storage without reading the payload.
    ///
    /// - BLAKE3 and indexed SHA-256 CIDs match regardless of codec.
    /// - Missing keys, unsupported hashes and dangling index rows return false.
    /// - Payload corruption is not checked; unknown storage is a backend error.
    fn has(&self, key: &Cid) -> Result<bool, Error> {
        if key.raw_blake3().is_none() && key.raw_sha256().is_none() {
            return Ok(false);
        }
        self.contains(key.hash(), key.digest())
    }

    /// The name `sqlite` and the database's id as hex, or null if unset.
    fn info(&self) -> Result<Value, Error> {
        let id = match self.id()? {
            Some(id) => Value::String(id.to_string()),
            None => Value::Null,
        };
        Ok(map([("name", Value::String("sqlite".into())), ("id", id)]))
    }

    fn stats(&self) -> Result<Value, Error> {
        let (blocks, bytes) = self
            .db()
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(LENGTH(data)), 0) FROM blobs",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(backend)?;
        Ok(map([
            ("blocks", Value::Integer(blocks)),
            ("bytes", Value::Integer(bytes)),
        ]))
    }
}

#[cfg(test)]
mod tests {
    use covalence_lib_sqlite::rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    use super::*;

    #[test]
    fn existence_checks_cannot_read_payloads() -> Result<(), Error> {
        use covalence_lib_cid::Format;

        let store = SqliteCas::open_in_memory()?;
        let cid: Cid = store.put(vec![7; 8 << 20].into())?;
        let empty: Cid = store.put(Bytes::new())?;
        let unindexed: Cid = store.put("not indexed".into())?;
        let unindexed_sha: Cid = Sha256::of(b"not indexed").into();
        let sha256: Sha256 = store.put("indexed".into())?;
        let sha256: Cid = sha256.into();
        let drisl = Cid::new(Format::DRISL_BLAKE3, cid.digest()).unwrap();
        let unsupported = Cid::new(Format::GIT_SHA1, &[0; 20]).unwrap();
        let missing: Cid = Blake3::of(b"missing").into();
        store
            .db()
            .authorizer(Some(|context: AuthContext<'_>| match context.action {
                AuthAction::Read {
                    table_name: "blobs",
                    column_name: "data",
                } => Authorization::Deny,
                _ => Authorization::Allow,
            }))
            .map_err(backend)?;
        // Exercise the public API through the shared-store forwarding boundary.
        let store = store.into_dyn();
        assert!(store.has(&cid)?);
        assert!(store.has(&empty)?);
        assert!(store.has(&sha256)?);
        assert!(store.has(&drisl)?);
        assert!(store.has(&unindexed)?);
        assert!(!store.has(&unindexed_sha)?);
        assert!(!store.has(&unsupported)?);
        assert!(!store.has(&missing)?);
        assert_eq!(
            store.missing(&[cid, missing, empty, missing, sha256])?,
            vec![missing, missing]
        );
        // Verify that the guard really rejects a payload read.
        assert!(matches!(
            store.get_unchecked(&cid),
            Err(Error::Backend { .. })
        ));
        Ok(())
    }

    #[test]
    fn dangling_hash_rows_are_missing() -> Result<(), Error> {
        let store = SqliteCas::open_in_memory()?;
        let cid: Cid = store.put("gone".into())?;
        store
            .db()
            .execute("DELETE FROM blobs", [])
            .map_err(backend)?;
        assert!(!store.has(&cid)?);
        assert_eq!(store.missing(&[cid])?, vec![cid]);
        Ok(())
    }
}
