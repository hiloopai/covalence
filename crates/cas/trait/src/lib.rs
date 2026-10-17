//! The content-addressed storage traits.
//!
//! A content-addressed store (CAS) keeps blocks of bytes under keys computed
//! from the bytes. Keys are usually [CIDs](https://dasl.ing/cid.html): a CID
//! names a hash function, the hash's digest of the bytes, and a codec saying
//! how to read them. A [`Fact`] is bytes checked to hash to a CID's digest.
//!
//! - [DASL](https://dasl.ing/) (Data-Addressed Structures & Links) is a small
//!   set of specs for content-addressed data: CIDs, and DRISL for structured
//!   values. A DASL value here is null, a boolean, an integer, bytes, a
//!   string, a list, a map with string keys, or a CID link.
//! - [DRISL](https://dasl.ing/drisl.html) is DASL's deterministic CBOR
//!   encoding, so equal values encode to equal bytes and get equal CIDs.
//!
//! Each operation is its own trait, generic over the key type, so a store can
//! answer several key types:
//!
//! - [`Get`] and [`Put`] are sync; [`AsyncGet`] and [`AsyncPut`] are their
//!   async twins. As in Python, each async method is its sync name prefixed
//!   with `a`: `get` and `aget`, `put` and `aput`.
//! - [`Cas`] combines them over [`Cid`]. Hold a store as `Arc<dyn Cas>` (or
//!   take `&dyn Cas`) rather than being generic; [`Cas::into_dyn`] makes one.
//! - [`Cas::info`] and [`Cas::stats`] describe a store, for debugging, as a
//!   DASL value.
//! - Stores live in their own crates, such as `covalence-cas-memory`.
//!
//! Reads come at three levels:
//!
//! - `get_unchecked`: the fast path; the store need not check its answer.
//! - `get`: what the store vouches for. Checking means different things for
//!   different stores (a table of facts needs none, a remote store hashes), so
//!   stores override it; by default it is `get_unchecked`.
//! - `get_fact`, in [`GetExt`]: always checked, and cannot be overridden. It
//!   checks that the store's [`Fact`] has the key's hash and digest, then
//!   returns it under exactly the key asked for. A fact proves only the hash
//!   and digest; the codec says how to read the bytes, so asking for a DRISL
//!   CID gives a fact for that DRISL CID, however the store keeps the bytes.
//!
//! [`AsyncGet`] and [`AsyncGetExt`] mirror these.
//!
//! Sizes and byte ranges come at the first two levels, as `size_unchecked`
//! and `get_range_unchecked`, and `size` and `get_range`. By default they
//! fetch the whole value and measure or slice it; stores with native ranges
//! override them. Nothing in these traits checks a size or a range against
//! its key, as `get_fact` checks bytes.
//!
//! [`Cas::put_from`] and [`Cas::get_to`] move a block between the store and a
//! reader or writer, checked; by default they hold it in memory, and stores
//! that can stream override them.
//!
//! A store needs only `get_unchecked`, `aget_unchecked`, `put` and `aput`;
//! the defaults supply the rest. Here a store answers with corrupted bytes:
//! `get` passes them on, as it does by default, but `get_fact` catches them.
//!
//! ```
//! use std::{collections::HashMap, sync::RwLock};
//!
//! use covalence_cas_trait::{
//!     AsyncGet, AsyncPut, Cas, CasFuture, Error, Get, GetExt, Put, ready,
//! };
//! use covalence_lib_cid::{Blake3, Cid};
//! use covalence_lib_data::Bytes;
//!
//! #[derive(Default)]
//! struct Map(RwLock<HashMap<Cid, Bytes>>);
//!
//! impl Get for Map {
//!     fn get_unchecked(&self, key: &Cid) -> Result<Option<Bytes>, Error> {
//!         Ok(self.0.read().unwrap().get(key).cloned())
//!     }
//! }
//! impl AsyncGet for Map {
//!     fn aget_unchecked<'a>(&'a self, key: &'a Cid) -> CasFuture<'a, Option<Bytes>> {
//!         ready(self.get_unchecked(key))
//!     }
//! }
//! impl Put for Map {
//!     fn put(&self, bytes: Bytes) -> Result<Cid, Error> {
//!         let key = Blake3::of(&bytes).into();
//!         self.0.write().unwrap().insert(key, bytes);
//!         Ok(key)
//!     }
//! }
//! impl AsyncPut for Map {
//!     fn aput(&self, bytes: Bytes) -> CasFuture<'_, Cid> {
//!         ready(self.put(bytes))
//!     }
//! }
//! impl Cas for Map {}
//!
//! let store = Map::default();
//! let cid = store.put("hello".into())?;
//! assert_eq!(store.size(&cid)?, Some(5));
//! assert_eq!(store.get_range(&cid, 1, 3)?.unwrap(), "ell");
//! assert_eq!(store.get_fact(&cid)?.unwrap().bytes(), "hello");
//!
//! store.0.write().unwrap().insert(cid, "jello".into());
//! assert_eq!(store.get(&cid)?.unwrap(), "jello");
//! assert!(matches!(store.get_fact(&cid), Err(Error::Check { .. })));
//! # Ok::<(), Error>(())
//! ```
#![deny(missing_docs)]

use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    pin::Pin,
    sync::Arc,
};

use covalence_lib_cid::{Blake3, Cid, CidFormat, Claim, Fact};
use covalence_lib_dasl::Value;
use covalence_lib_data::Bytes;
use covalence_lib_error::{
    BoxError,
    snafu::{Snafu, ensure},
};

/// The future an async store method returns.
pub type CasFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

/// A [`CasFuture`] already holding `result`.
pub fn ready<'a, T: Send + 'a>(result: Result<T, Error>) -> CasFuture<'a, T> {
    Box::pin(std::future::ready(result))
}

/// A DASL map from `entries`, for [`Cas::info`] and [`Cas::stats`].
pub fn map<const N: usize>(entries: [(&str, Value); N]) -> Value {
    Value::Map(BTreeMap::from(
        entries.map(|(key, value)| (key.into(), value)),
    ))
}

/// A key stores can be asked for.
pub trait Key: Claim<Error = covalence_lib_cid::Error> + Clone + Send + Sync {
    /// Whether a fact for `self` also proves `key` (for CIDs: the same hash
    /// and digest).
    fn same_multihash(&self, key: &Self) -> bool;

    /// `fact`, which proves `self`, relabelled as a fact for `self` (for
    /// CIDs: the same bytes with `self`'s codec).
    fn relabel(&self, fact: Fact<Self>) -> Result<Fact<Self>, covalence_lib_cid::Error>;
}

/// A fact proves only a CID's hash and digest, so it proves any CID with the
/// same ones, and relabelling only changes the codec.
impl<S: CidFormat> Key for Cid<S>
where
    Self: Send + Sync,
{
    fn same_multihash(&self, key: &Self) -> bool {
        Cid::same_multihash(self, key)
    }

    fn relabel(&self, fact: Fact<Self>) -> Result<Fact<Self>, covalence_lib_cid::Error> {
        fact.with_codec(self.codec())
    }
}

/// Reads, blocking.
pub trait Get<K: Key = Cid> {
    /// The bytes under `key`, or `None` if missing. Not checked.
    fn get_unchecked(&self, key: &K) -> Result<Option<Bytes>, Error>;

    /// The size in bytes of the value under `key`, or `None` if missing. Not
    /// checked. The length of [`Get::get_unchecked`] by default.
    fn size_unchecked(&self, key: &K) -> Result<Option<u64>, Error> {
        Ok(self.get_unchecked(key)?.map(|bytes| bytes.len() as u64))
    }

    /// Up to `len` bytes under `key` from `start`, or `None` if missing. Not
    /// checked. Slices [`Get::get_unchecked`] by default.
    ///
    /// - Fewer bytes at the end, and empty bytes at or past it.
    /// - Range ends beyond `u64::MAX` are clipped; replies never exceed `len`.
    fn get_range_unchecked(&self, key: &K, start: u64, len: u64) -> Result<Option<Bytes>, Error> {
        Ok(self
            .get_unchecked(key)?
            .map(|bytes| slice(&bytes, start, len)))
    }

    /// The bytes under `key`, as the store vouches for them.
    /// [`Get::get_unchecked`] by default.
    fn get(&self, key: &K) -> Result<Option<Bytes>, Error> {
        self.get_unchecked(key)
    }

    /// The size under `key`, as the store vouches for it.
    /// [`Get::size_unchecked`] by default.
    fn size(&self, key: &K) -> Result<Option<u64>, Error> {
        self.size_unchecked(key)
    }

    /// A range under `key`, as the store vouches for it.
    /// [`Get::get_range_unchecked`] by default.
    fn get_range(&self, key: &K, start: u64, len: u64) -> Result<Option<Bytes>, Error> {
        self.get_range_unchecked(key, start, len)
    }

    /// A fact the store holds for `key`, perhaps for another key. Hashes
    /// [`Get::get_unchecked`] by default.
    fn get_fact_unchecked(&self, key: &K) -> Result<Option<Fact<K>>, Error> {
        to_fact(key, self.get_unchecked(key)?)
    }
}

/// Reads, async.
pub trait AsyncGet<K: Key = Cid>: Send + Sync {
    /// The bytes under `key`, or `None` if missing. Not checked.
    fn aget_unchecked<'a>(&'a self, key: &'a K) -> CasFuture<'a, Option<Bytes>>;

    /// As [`Get::size_unchecked`]. The length of [`AsyncGet::aget_unchecked`]
    /// by default.
    fn asize_unchecked<'a>(&'a self, key: &'a K) -> CasFuture<'a, Option<u64>> {
        Box::pin(async move {
            Ok(self
                .aget_unchecked(key)
                .await?
                .map(|bytes| bytes.len() as u64))
        })
    }

    /// As [`Get::get_range_unchecked`]. Slices [`AsyncGet::aget_unchecked`]
    /// by default.
    fn aget_range_unchecked<'a>(
        &'a self,
        key: &'a K,
        start: u64,
        len: u64,
    ) -> CasFuture<'a, Option<Bytes>> {
        Box::pin(async move {
            Ok(self
                .aget_unchecked(key)
                .await?
                .map(|bytes| slice(&bytes, start, len)))
        })
    }

    /// The bytes under `key`, as the store vouches for them.
    /// [`AsyncGet::aget_unchecked`] by default.
    fn aget<'a>(&'a self, key: &'a K) -> CasFuture<'a, Option<Bytes>> {
        self.aget_unchecked(key)
    }

    /// The size under `key`, as the store vouches for it.
    /// [`AsyncGet::asize_unchecked`] by default.
    fn asize<'a>(&'a self, key: &'a K) -> CasFuture<'a, Option<u64>> {
        self.asize_unchecked(key)
    }

    /// A range under `key`, as the store vouches for it.
    /// [`AsyncGet::aget_range_unchecked`] by default.
    fn aget_range<'a>(&'a self, key: &'a K, start: u64, len: u64) -> CasFuture<'a, Option<Bytes>> {
        self.aget_range_unchecked(key, start, len)
    }

    /// A fact the store holds for `key`, perhaps for another key. Hashes
    /// [`AsyncGet::aget_unchecked`] by default.
    fn aget_fact_unchecked<'a>(&'a self, key: &'a K) -> CasFuture<'a, Option<Fact<K>>> {
        Box::pin(async move { to_fact(key, self.aget_unchecked(key).await?) })
    }
}

/// Writes, blocking.
pub trait Put<K = Cid> {
    /// Store `bytes` and return their key.
    fn put(&self, bytes: Bytes) -> Result<K, Error>;

    /// Store `fact`'s bytes and return the key [`Put::put`] would, which has
    /// the same multihash as the fact's claim when the store keys by the claim's hash. The
    /// claim already holds, so a store may skip hashing. [`Put::put`] by
    /// default.
    fn put_fact(&self, fact: Fact<K>) -> Result<K, Error>
    where
        K: Claim,
    {
        self.put(fact.bytes().clone())
    }
}

/// Writes, async.
pub trait AsyncPut<K = Cid>: Send + Sync {
    /// Store `bytes` and return their key.
    fn aput(&self, bytes: Bytes) -> CasFuture<'_, K>;

    /// Store `fact`'s bytes and return the key [`AsyncPut::aput`] would, as
    /// [`Put::put_fact`]. [`AsyncPut::aput`] by default.
    fn aput_fact(&self, fact: Fact<K>) -> CasFuture<'_, K>
    where
        K: Claim,
    {
        self.aput(fact.bytes().clone())
    }
}

/// Checked blocking reads, for every [`Get`]. Stores cannot override them.
pub trait GetExt<K: Key>: Get<K> {
    /// The fact under `key`, checked, for exactly `key`.
    fn get_fact(&self, key: &K) -> Result<Option<Fact<K>>, Error> {
        check(key, self.get_fact_unchecked(key)?)
    }
}

impl<K: Key, T: Get<K> + ?Sized> GetExt<K> for T {}

/// Checked async reads, for every [`AsyncGet`]. Stores cannot override them.
pub trait AsyncGetExt<K: Key>: AsyncGet<K> {
    /// The fact under `key`, checked, for exactly `key`.
    fn aget_fact<'a>(&'a self, key: &'a K) -> CasFuture<'a, Option<Fact<K>>> {
        Box::pin(async move { check(key, self.aget_fact_unchecked(key).await?) })
    }
}

impl<K: Key, T: AsyncGet<K> + ?Sized> AsyncGetExt<K> for T {}

/// A store of bytes by CID, sync and async.
///
/// Sync and async methods may see different data (sync might read only a
/// local cache), but `get_fact` and `aget_fact` are correct either way. Do not
/// call the sync methods on an async runtime thread: they may block.
pub trait Cas: Get + Put + AsyncGet + AsyncPut {
    /// Whether the store holds bytes for `key`, unchecked, as
    /// [`Get::get_unchecked`] would find them. Reads them by default; stores
    /// override it to avoid that.
    fn has(&self, key: &Cid) -> Result<bool, Error> {
        Ok(self.get_unchecked(key)?.is_some())
    }

    /// Those of `keys` that [`Cas::has`] says the store lacks, in order.
    fn missing(&self, keys: &[Cid]) -> Result<Vec<Cid>, Error> {
        let mut missing = Vec::new();
        for key in keys {
            if !self.has(key)? {
                missing.push(*key);
            }
        }
        Ok(missing)
    }

    /// A DASL value describing the store, for debugging: `null` or a map.
    /// `null` by default; an error if the store cannot answer.
    fn info(&self) -> Result<Value, Error> {
        Ok(Value::Null)
    }

    /// A DASL value of whatever the store counts, for debugging: `null` or a
    /// map. `null` by default; an error if the store cannot answer.
    fn stats(&self) -> Result<Value, Error> {
        Ok(Value::Null)
    }

    /// Store the next `len` bytes of `reader`, whose raw BLAKE3 CID is `cid`,
    /// and return the key [`Put::put_fact`] would.
    ///
    /// - A store may return an existing block without consuming all of
    ///   `reader`; unread bytes are not checked.
    /// - Otherwise, reads exactly `len` bytes and no further. Stores nothing
    ///   and fails with [`Error::Check`] if the bytes have another CID, or
    ///   with [`Error::Io`] if `reader` fails or ends early.
    /// - Reads into memory by default; stores that can stream override it.
    fn put_from(&self, cid: Blake3, reader: &mut dyn Read, len: u64) -> Result<Cid, Error> {
        let mut bytes = Vec::new();
        let read = reader
            .take(len)
            .read_to_end(&mut bytes)
            .map_err(|source| Error::Io { source })?;
        if (read as u64) < len {
            return Err(Error::Io {
                source: io::ErrorKind::UnexpectedEof.into(),
            });
        }
        self.put_fact(Fact::new(cid, bytes.into())?.widen())
    }

    /// Write the bytes under `key` to `writer`, checked as
    /// [`GetExt::get_fact`] checks them, and return their length, or `None`,
    /// writing nothing, if missing. Fails with [`Error::Check`] if they do not
    /// match `key`, or with [`Error::Io`] if `writer` fails; after a failure
    /// `writer` may hold unchecked bytes, so discard them. Writes
    /// [`GetExt::get_fact`] by default; stores that can stream override it.
    fn get_to(&self, key: &Cid, writer: &mut dyn Write) -> Result<Option<u64>, Error> {
        let Some(fact) = self.get_fact(key)? else {
            return Ok(None);
        };
        writer
            .write_all(fact.bytes())
            .map_err(|source| Error::Io { source })?;
        Ok(Some(fact.bytes().len() as u64))
    }

    /// This store as an `Arc<dyn Cas>`: wrapped in a new `Arc`, except that an
    /// `Arc<dyn Cas>` returns itself.
    fn into_dyn(self) -> Arc<dyn Cas>
    where
        Self: Sized + 'static,
    {
        Arc::new(self)
    }
}

/// Up to `len` bytes of `bytes` from `start`, clipped at the end.
fn slice(bytes: &Bytes, start: u64, len: u64) -> Bytes {
    let size = bytes.len() as u64;
    let first = start.min(size) as usize;
    let last = start.saturating_add(len).min(size) as usize;
    bytes.slice(first..last)
}

/// `bytes` as a fact for `key`.
fn to_fact<K: Key>(key: &K, bytes: Option<Bytes>) -> Result<Option<Fact<K>>, Error> {
    Ok(bytes
        .map(|bytes| Fact::new(key.clone(), bytes))
        .transpose()?)
}

/// `fact` relabelled for `key`, or `Mismatch` if it proves another key.
fn check<K: Key>(key: &K, fact: Option<Fact<K>>) -> Result<Option<Fact<K>>, Error> {
    let Some(fact) = fact else {
        return Ok(None);
    };
    ensure!(fact.claim().same_multihash(key), MismatchSnafu);
    Ok(Some(key.relabel(fact)?))
}

/// A shared store is a store: each method forwards to it.
impl Get for Arc<dyn Cas> {
    fn get_unchecked(&self, key: &Cid) -> Result<Option<Bytes>, Error> {
        (**self).get_unchecked(key)
    }
    fn size_unchecked(&self, key: &Cid) -> Result<Option<u64>, Error> {
        (**self).size_unchecked(key)
    }
    fn get_range_unchecked(&self, key: &Cid, start: u64, len: u64) -> Result<Option<Bytes>, Error> {
        (**self).get_range_unchecked(key, start, len)
    }
    fn size(&self, key: &Cid) -> Result<Option<u64>, Error> {
        (**self).size(key)
    }
    fn get_range(&self, key: &Cid, start: u64, len: u64) -> Result<Option<Bytes>, Error> {
        (**self).get_range(key, start, len)
    }
    fn get(&self, key: &Cid) -> Result<Option<Bytes>, Error> {
        (**self).get(key)
    }
    fn get_fact_unchecked(&self, key: &Cid) -> Result<Option<Fact>, Error> {
        (**self).get_fact_unchecked(key)
    }
}

impl AsyncGet for Arc<dyn Cas> {
    fn aget_unchecked<'a>(&'a self, key: &'a Cid) -> CasFuture<'a, Option<Bytes>> {
        (**self).aget_unchecked(key)
    }
    fn asize_unchecked<'a>(&'a self, key: &'a Cid) -> CasFuture<'a, Option<u64>> {
        (**self).asize_unchecked(key)
    }
    fn aget_range_unchecked<'a>(
        &'a self,
        key: &'a Cid,
        start: u64,
        len: u64,
    ) -> CasFuture<'a, Option<Bytes>> {
        (**self).aget_range_unchecked(key, start, len)
    }
    fn asize<'a>(&'a self, key: &'a Cid) -> CasFuture<'a, Option<u64>> {
        (**self).asize(key)
    }
    fn aget_range<'a>(
        &'a self,
        key: &'a Cid,
        start: u64,
        len: u64,
    ) -> CasFuture<'a, Option<Bytes>> {
        (**self).aget_range(key, start, len)
    }
    fn aget<'a>(&'a self, key: &'a Cid) -> CasFuture<'a, Option<Bytes>> {
        (**self).aget(key)
    }
    fn aget_fact_unchecked<'a>(&'a self, key: &'a Cid) -> CasFuture<'a, Option<Fact>> {
        (**self).aget_fact_unchecked(key)
    }
}

impl Put for Arc<dyn Cas> {
    fn put(&self, bytes: Bytes) -> Result<Cid, Error> {
        (**self).put(bytes)
    }
    fn put_fact(&self, fact: Fact) -> Result<Cid, Error> {
        (**self).put_fact(fact)
    }
}

impl AsyncPut for Arc<dyn Cas> {
    fn aput(&self, bytes: Bytes) -> CasFuture<'_, Cid> {
        (**self).aput(bytes)
    }
    fn aput_fact(&self, fact: Fact) -> CasFuture<'_, Cid> {
        (**self).aput_fact(fact)
    }
}

impl Cas for Arc<dyn Cas> {
    fn has(&self, key: &Cid) -> Result<bool, Error> {
        (**self).has(key)
    }
    fn missing(&self, keys: &[Cid]) -> Result<Vec<Cid>, Error> {
        (**self).missing(keys)
    }
    fn info(&self) -> Result<Value, Error> {
        (**self).info()
    }
    fn stats(&self) -> Result<Value, Error> {
        (**self).stats()
    }
    fn put_from(&self, cid: Blake3, reader: &mut dyn Read, len: u64) -> Result<Cid, Error> {
        (**self).put_from(cid, reader, len)
    }
    fn get_to(&self, key: &Cid, writer: &mut dyn Write) -> Result<Option<u64>, Error> {
        (**self).get_to(key, writer)
    }
    fn into_dyn(self) -> Arc<dyn Cas> {
        self
    }
}

/// A store operation, named in [`Error::Unsupported`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Op {
    /// A read.
    Get,
    /// A write.
    Put,
}

impl Op {
    /// The error for a store that does not support this operation.
    pub fn unsupported(self) -> Error {
        Error::Unsupported { op: self }
    }
}

/// Errors from reading, checking and storing. Large payloads are boxed to
/// keep it small.
#[derive(Debug, Snafu)]
#[snafu(crate_root(covalence_lib_error::snafu))]
#[non_exhaustive]
pub enum Error {
    /// The store returned a fact for another multihash than the key asked for.
    #[snafu(display("the store returned a fact for another key"))]
    Mismatch,
    /// The bytes do not match their key.
    #[snafu(context(false), display("{source}"))]
    Check {
        /// The check that failed.
        #[snafu(source(from(covalence_lib_cid::Error, Box::new)))]
        source: Box<covalence_lib_cid::Error>,
    },
    /// The store does not support `op`.
    #[snafu(display("unsupported: {op:?}"))]
    Unsupported {
        /// The operation.
        op: Op,
    },
    /// A failure in the storage under a store.
    #[snafu(display("backend: {source}"))]
    Backend {
        /// The store's own error.
        source: BoxError,
    },
    /// The reader [`Cas::put_from`] reads or the writer [`Cas::get_to`]
    /// writes failed, or the reader ended early.
    #[snafu(display("{source}"))]
    Io {
        /// The I/O error.
        source: io::Error,
    },
}
