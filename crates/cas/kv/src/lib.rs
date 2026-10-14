//! A content-addressed store over a key-value store.
//!
//! [`KvCas`] keeps blocks in any [`Kv`], such as `covalence-kv-memory`'s
//! `MemoryKv` or `covalence-kv-s3`'s `S3Kv`. Each block lies under
//! `blake3/<digest>/bytes`, after an optional prefix, where `<digest>` is its
//! [BLAKE3](https://github.com/BLAKE3-team/BLAKE3-specs) digest in lowercase,
//! unpadded [base32](https://www.rfc-editor.org/rfc/rfc4648#section-6).
//!
//! ```
//! use covalence_cas_kv::KvCas;
//! use covalence_cas_trait::{GetExt, Put};
//! use covalence_kv_memory::MemoryKv;
//! use covalence_kv_trait::Kv;
//! use covalence_lib_cid::Cid;
//! let kv = MemoryKv::default().into_dyn();
//! let store = KvCas::with_prefix(kv.clone(), "cas")?;
//! let cid: Cid = store.put("hello".into())?;
//! assert_eq!(store.get_fact(&cid)?.unwrap().bytes(), "hello");
//! // BLAKE3("hello") is ea8f163d...4a67200f in hex.
//! let key = "cas/blake3/5khrmpntq2bjexseshc6ldklwnig56gbj23yvbxjbdcwestheahq/bytes";
//! assert_eq!(kv.get(key.as_bytes())?.unwrap(), "hello");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
#![deny(missing_docs)]

use std::{
    io::{self, Read, Write},
    sync::Arc,
};

use covalence_cas_trait::{
    AsyncGet, AsyncGetExt, AsyncPut, Cas, CasFuture, Error, Get, GetExt, Put, map,
};
use covalence_kv_trait::Kv;
use covalence_lib_cid::{Blake3, Cid};
use covalence_lib_dasl::Value;
use covalence_lib_data::Bytes;
use covalence_lib_error::snafu::Snafu;
use data_encoding::BASE32_NOPAD;

/// A store of blocks in a [`Kv`]; see the [crate] docs for the key layout.
///
/// - A CID with a 32-byte BLAKE3 digest finds its bytes whatever its codec;
///   other CIDs find nothing. [`Put`] returns the raw BLAKE3 CID.
/// - The key-value store may be remote or shared, so `get`, `aget`,
///   `get_fact` and `aget_fact` check the bytes against the CID and fail with
///   [`Error::Check`] if they do not match. `get_unchecked`, sizes and ranges
///   are not checked.
/// - `has` asks the key-value store whether the key exists (a HEAD request on
///   S3), without reading the bytes.
/// - `put` writes without asking first, so it needs only write access. (S3
///   answers a HEAD for a missing key with 403 to a client that may not list
///   the bucket.)
/// - `put_from` and `get_to` stream through the key-value store's own
///   `put_from` and `get_to`, hashing the bytes as they pass.
/// - If the bytes `put_from` reads do not hash to the CID, the read that would
///   hand over the last byte fails instead. The key-value store's `put_from`
///   then fails as [`Kv::put_from`] says, storing nothing, and `put_from`
///   fails with [`Error::Check`]; or with [`Error::Backend`] if the key-value
///   store reports a further failure, such as failing to clean up.
/// - [`Cas::info`] is `{"name": "kv", "prefix": <prefix>, "kv": <the
///   key-value store's info>}`; [`Cas::stats`] is the key-value store's.
pub struct KvCas {
    kv: Arc<dyn Kv>,
    prefix: String,
}

/// A prefix that is not `/`-separated, non-empty segments other than `.` and
/// `..`, optionally ending in `/`.
#[derive(Debug, Snafu)]
#[snafu(crate_root(covalence_lib_error::snafu))]
#[snafu(display("invalid key prefix {prefix:?}"))]
pub struct InvalidPrefix {
    prefix: String,
}

impl KvCas {
    /// A store with keys at the root of `kv`.
    pub fn new(kv: Arc<dyn Kv>) -> Self {
        Self {
            kv,
            prefix: String::new(),
        }
    }

    /// A store with keys under `prefix/` in `kv`; a trailing `/` on `prefix`
    /// is optional.
    ///
    /// Fails with [`InvalidPrefix`] unless `prefix` is `/`-separated,
    /// non-empty segments other than `.` and `..`, so the empty prefix fails:
    /// use [`KvCas::new`] instead.
    ///
    /// ```
    /// use covalence_cas_kv::KvCas;
    /// use covalence_kv_memory::MemoryKv;
    /// use covalence_kv_trait::Kv;
    /// let kv = MemoryKv::default().into_dyn();
    /// assert!(KvCas::with_prefix(kv.clone(), "a/b/").is_ok());
    /// for prefix in ["", "/", "/a", "a//b", "a/../b"] {
    ///     assert!(KvCas::with_prefix(kv.clone(), prefix).is_err());
    /// }
    /// ```
    pub fn with_prefix(kv: Arc<dyn Kv>, prefix: &str) -> Result<Self, InvalidPrefix> {
        let trimmed = prefix.strip_suffix('/').unwrap_or(prefix);
        if trimmed.split('/').any(|s| matches!(s, "" | "." | "..")) {
            return InvalidPrefixSnafu { prefix }.fail();
        }
        Ok(Self {
            kv,
            prefix: format!("{trimmed}/"),
        })
    }

    /// The key for `cid`.
    fn key(&self, cid: &Blake3) -> String {
        let digest = BASE32_NOPAD.encode(cid.digest()).to_ascii_lowercase();
        format!("{}blake3/{digest}/bytes", self.prefix)
    }
}

fn backend(error: covalence_kv_trait::Error) -> Error {
    match error {
        covalence_kv_trait::Error::Io { source } => Error::Io { source },
        error => Error::Backend {
            source: Box::new(error),
        },
    }
}

/// Whether `hasher` has hashed exactly the bytes of `cid`.
fn hashes_to(hasher: &blake3::Hasher, cid: &Blake3) -> bool {
    hasher.finalize().as_bytes() == cid.digest()
}

/// The next `left` bytes of `reader`, which must hash to `cid`: the read
/// returning the last of them fails instead if they do not.
struct Checked<'a> {
    reader: &'a mut dyn Read,
    hasher: blake3::Hasher,
    left: u64,
    cid: Blake3,
    mismatch: bool,
}

impl Read for Checked<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let max = buf
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        if max == 0 {
            return Ok(0);
        }
        let read = self.reader.read(&mut buf[..max])?;
        self.hasher.update(&buf[..read]);
        self.left -= read as u64;
        if self.left == 0 && !hashes_to(&self.hasher, &self.cid) {
            self.mismatch = true;
            let error = covalence_lib_cid::Error::Mismatch;
            return Err(io::Error::new(io::ErrorKind::InvalidData, error));
        }
        Ok(read)
    }
}

/// A writer hashing what it passes on.
struct Hashing<'a> {
    writer: &'a mut dyn Write,
    hasher: blake3::Hasher,
}

impl Write for Hashing<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.writer.write(buf)?;
        self.hasher.update(&buf[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

impl Get<Blake3> for KvCas {
    fn get_unchecked(&self, key: &Blake3) -> Result<Option<Bytes>, Error> {
        self.kv.get(self.key(key).as_bytes()).map_err(backend)
    }

    fn size_unchecked(&self, key: &Blake3) -> Result<Option<u64>, Error> {
        self.kv.size(self.key(key).as_bytes()).map_err(backend)
    }

    fn get_range_unchecked(
        &self,
        key: &Blake3,
        start: u64,
        len: u64,
    ) -> Result<Option<Bytes>, Error> {
        self.kv
            .get_range(self.key(key).as_bytes(), start, len)
            .map_err(backend)
    }

    /// The bytes under `key`, checked.
    fn get(&self, key: &Blake3) -> Result<Option<Bytes>, Error> {
        Ok(self.get_fact(key)?.map(|fact| fact.bytes().clone()))
    }
}

impl Get for KvCas {
    fn get_unchecked(&self, key: &Cid) -> Result<Option<Bytes>, Error> {
        key.raw_blake3()
            .map_or(Ok(None), |key| self.get_unchecked(&key))
    }

    fn size_unchecked(&self, key: &Cid) -> Result<Option<u64>, Error> {
        key.raw_blake3()
            .map_or(Ok(None), |key| self.size_unchecked(&key))
    }

    fn get_range_unchecked(&self, key: &Cid, start: u64, len: u64) -> Result<Option<Bytes>, Error> {
        key.raw_blake3()
            .map_or(Ok(None), |key| self.get_range_unchecked(&key, start, len))
    }

    /// The bytes under `key`, checked.
    fn get(&self, key: &Cid) -> Result<Option<Bytes>, Error> {
        Ok(self.get_fact(key)?.map(|fact| fact.bytes().clone()))
    }
}

impl AsyncGet for KvCas {
    fn asize_unchecked<'a>(&'a self, key: &'a Cid) -> CasFuture<'a, Option<u64>> {
        Box::pin(async move {
            let Some(key) = key.raw_blake3() else {
                return Ok(None);
            };
            let key = self.key(&key);
            self.kv.asize(key.as_bytes()).await.map_err(backend)
        })
    }

    fn aget_range_unchecked<'a>(
        &'a self,
        key: &'a Cid,
        start: u64,
        len: u64,
    ) -> CasFuture<'a, Option<Bytes>> {
        Box::pin(async move {
            let Some(key) = key.raw_blake3() else {
                return Ok(None);
            };
            let key = self.key(&key);
            self.kv
                .aget_range(key.as_bytes(), start, len)
                .await
                .map_err(backend)
        })
    }

    fn aget_unchecked<'a>(&'a self, key: &'a Cid) -> CasFuture<'a, Option<Bytes>> {
        Box::pin(async move {
            let Some(key) = key.raw_blake3() else {
                return Ok(None);
            };
            let key = self.key(&key);
            self.kv.aget(key.as_bytes()).await.map_err(backend)
        })
    }

    /// The bytes under `key`, checked.
    fn aget<'a>(&'a self, key: &'a Cid) -> CasFuture<'a, Option<Bytes>> {
        Box::pin(async move { Ok(self.aget_fact(key).await?.map(|fact| fact.bytes().clone())) })
    }
}

impl Put<Blake3> for KvCas {
    fn put(&self, bytes: Bytes) -> Result<Blake3, Error> {
        let cid = Blake3::of(&bytes);
        self.kv
            .put(self.key(&cid).as_bytes(), bytes)
            .map_err(backend)?;
        Ok(cid)
    }
}

impl Put for KvCas {
    fn put(&self, bytes: Bytes) -> Result<Cid, Error> {
        Ok(Put::<Blake3>::put(self, bytes)?.into())
    }
}

impl AsyncPut for KvCas {
    fn aput(&self, bytes: Bytes) -> CasFuture<'_, Cid> {
        Box::pin(async move {
            let cid = Blake3::of(&bytes);
            let key = self.key(&cid);
            self.kv.aput(key.as_bytes(), bytes).await.map_err(backend)?;
            Ok(cid.into())
        })
    }
}

impl Cas for KvCas {
    fn has(&self, key: &Cid) -> Result<bool, Error> {
        key.raw_blake3().map_or(Ok(false), |key| {
            let key = self.key(&key);
            self.kv.exists(key.as_bytes()).map_err(backend)
        })
    }

    /// The name `kv`, the key prefix, and the key-value store's info.
    fn info(&self) -> Result<Value, Error> {
        Ok(map([
            ("name", Value::String("kv".into())),
            ("prefix", Value::String(self.prefix.as_str().into())),
            ("kv", self.kv.info().map_err(backend)?),
        ]))
    }

    fn stats(&self) -> Result<Value, Error> {
        self.kv.stats().map_err(backend)
    }

    fn put_from(&self, cid: Blake3, reader: &mut dyn Read, len: u64) -> Result<Cid, Error> {
        // No read hands over the last byte of nothing, so check it here.
        if len == 0 && !hashes_to(&blake3::Hasher::new(), &cid) {
            return Err(covalence_lib_cid::Error::Mismatch.into());
        }
        let mut checked = Checked {
            reader,
            hasher: blake3::Hasher::new(),
            left: len,
            cid,
            mismatch: false,
        };
        let key = self.key(&cid);
        match self.kv.put_from(key.as_bytes(), &mut checked, len) {
            Ok(()) => Ok(cid.into()),
            // The store failed as asked; a store error beyond that, such as a
            // failed cleanup, is the one to report.
            Err(covalence_kv_trait::Error::Io { .. }) if checked.mismatch => {
                Err(covalence_lib_cid::Error::Mismatch.into())
            }
            Err(error) => Err(backend(error)),
        }
    }

    fn get_to(&self, key: &Cid, writer: &mut dyn Write) -> Result<Option<u64>, Error> {
        let Some(cid) = key.raw_blake3() else {
            return Ok(None);
        };
        let mut hashing = Hashing {
            writer,
            hasher: blake3::Hasher::new(),
        };
        let key = self.key(&cid);
        let Some(len) = self
            .kv
            .get_to(key.as_bytes(), &mut hashing)
            .map_err(backend)?
        else {
            return Ok(None);
        };
        if !hashes_to(&hashing.hasher, &cid) {
            return Err(covalence_lib_cid::Error::Mismatch.into());
        }
        Ok(Some(len))
    }
}
