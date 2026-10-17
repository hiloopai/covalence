//! The key-value store trait.
//!
//! A key-value (KV) store maps byte-string keys to byte values, and a put
//! replaces any earlier value. [`Kv`] is the trait every store implements.
//!
//! - Sync methods `get`, `put` and `exists`, and async twins named as in
//!   Python: the sync name prefixed with `a`, so `aget`, `aput` and `aexists`.
//! - `Kv` is object safe: every method returns the shared [`Error`], and async
//!   ones a boxed [`KvFuture`]. Hold a store as `Arc<dyn Kv>` (or take
//!   `&dyn Kv`) rather than being generic; [`Kv::into_dyn`] makes one.
//! - [`Kv::size`] and [`Kv::get_range`] fetch the whole value and measure or
//!   slice it by default; stores with native ranges override them.
//! - [`Kv::put_from`] and [`Kv::get_to`] move a value between the store and a
//!   reader or writer. By default they hold it in memory; stores that can
//!   stream override them. They have no async twins: readers and writers block.
//! - [`Kv::put_new`], [`Kv::put_new_from`] and [`Kv::aput_new`] store a value
//!   atomically only under a key that holds none. Their defaults return
//!   [`Error::Unsupported`] without reading or writing; stores that can create
//!   atomically override them.
//! - Stores live in their own crates, such as `covalence-kv-memory` and
//!   `covalence-kv-s3`.
//!
//! A store needs only `get`, `put`, `aget` and `aput`; the defaults supply
//! the rest:
//!
//! ```
//! use std::{collections::BTreeMap, sync::RwLock};
//!
//! use covalence_kv_trait::{Error, Kv, KvFuture, ready};
//! use covalence_lib_data::Bytes;
//!
//! #[derive(Default)]
//! struct Map(RwLock<BTreeMap<Vec<u8>, Bytes>>);
//!
//! impl Kv for Map {
//!     fn get(&self, key: &[u8]) -> Result<Option<Bytes>, Error> {
//!         Ok(self.0.read().unwrap().get(key).cloned())
//!     }
//!     fn put(&self, key: &[u8], value: Bytes) -> Result<(), Error> {
//!         self.0.write().unwrap().insert(key.to_vec(), value);
//!         Ok(())
//!     }
//!     fn aget<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
//!         ready(self.get(key))
//!     }
//!     fn aput<'a>(&'a self, key: &'a [u8], value: Bytes) -> KvFuture<'a, ()> {
//!         ready(self.put(key, value))
//!     }
//! }
//!
//! let store = Map::default().into_dyn();
//! store.put(b"greeting", "hello".into())?;
//! assert_eq!(store.size(b"greeting")?, Some(5));
//! assert_eq!(store.get_range(b"greeting", 1, 3)?.unwrap(), "ell");
//! // Ranges stop at the end of the value; a missing key is `None`.
//! assert_eq!(store.get_range(b"greeting", 3, 10)?.unwrap(), "lo");
//! assert_eq!(store.get_range(b"greeting", 9, 1)?.unwrap(), "");
//! assert_eq!(store.get_range(b"missing", 0, 1)?, None);
//! assert!(!store.exists(b"missing")?);
//! # Ok::<(), Error>(())
//! ```
#![deny(missing_docs)]

use std::{
    io::{self, Read, Write},
    pin::Pin,
    sync::Arc,
};

use covalence_lib_dasl::Value;
use covalence_lib_data::Bytes;
use covalence_lib_error::{BoxError, snafu::Snafu};

/// The future an async store method returns.
pub type KvFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

/// A [`KvFuture`] already holding `result`.
pub fn ready<'a, T: Send + 'a>(result: Result<T, Error>) -> KvFuture<'a, T> {
    Box::pin(std::future::ready(result))
}

/// A store of byte values by byte-string key, sync and async.
///
/// A store rejects keys it cannot hold with an error. Sync and async methods
/// may see different data (sync might read only a local cache). Do not call
/// the sync methods on an async runtime thread: they may block.
pub trait Kv: Send + Sync {
    /// The value under `key`, or `None` if missing.
    fn get(&self, key: &[u8]) -> Result<Option<Bytes>, Error>;

    /// The size in bytes of the value under `key`, or `None` if missing.
    /// The length of [`Kv::get`] by default.
    fn size(&self, key: &[u8]) -> Result<Option<u64>, Error> {
        Ok(self.get(key)?.map(|value| value.len() as u64))
    }

    /// Up to `len` bytes under `key` from `start`, or `None` if missing.
    /// Slices [`Kv::get`] by default.
    ///
    /// - Fewer bytes at the end, and empty bytes at or past it.
    /// - Range ends beyond `u64::MAX` are clipped; replies never exceed `len`.
    fn get_range(&self, key: &[u8], start: u64, len: u64) -> Result<Option<Bytes>, Error> {
        Ok(self.get(key)?.map(|value| slice(&value, start, len)))
    }

    /// Store `value` under `key`, replacing any previous value.
    fn put(&self, key: &[u8], value: Bytes) -> Result<(), Error>;

    /// Store the next `len` bytes of `reader` under `key`, replacing any
    /// previous value. Reads no further; fails with [`Error::Io`] if `reader`
    /// fails or ends early, and then stores nothing. Reads them into memory
    /// and calls [`Kv::put`] by default.
    fn put_from(&self, key: &[u8], reader: &mut dyn Read, len: u64) -> Result<(), Error> {
        let mut value = Vec::new();
        let read = reader
            .take(len)
            .read_to_end(&mut value)
            .map_err(|source| Error::Io { source })?;
        if (read as u64) < len {
            return Err(Error::Io {
                source: io::ErrorKind::UnexpectedEof.into(),
            });
        }
        self.put(key, value.into())
    }

    /// Atomically store `value` under `key` only if the key holds no value,
    /// and return whether it stored.
    ///
    /// Never replaces a value, including when writers race. The default
    /// returns [`Error::Unsupported`] without reading or writing.
    fn put_new(&self, _key: &[u8], _value: Bytes) -> Result<bool, Error> {
        Err(Op::PutNew.unsupported())
    }

    /// As [`Kv::put_from`], but atomically store only if the key holds no
    /// value, and return whether it stored.
    ///
    /// Never replaces a value, including when writers race. When the key
    /// holds one, `reader` may have been read or not. The default returns
    /// [`Error::Unsupported`] without reading or writing.
    fn put_new_from(&self, _key: &[u8], _reader: &mut dyn Read, _len: u64) -> Result<bool, Error> {
        Err(Op::PutNew.unsupported())
    }

    /// Write the value under `key` to `writer` and return its length, or
    /// `None`, writing nothing, if missing. Fails with [`Error::Io`] if
    /// `writer` fails; after any failure `writer` may hold part of the value.
    /// Writes [`Kv::get`] by default.
    fn get_to(&self, key: &[u8], writer: &mut dyn Write) -> Result<Option<u64>, Error> {
        let Some(value) = self.get(key)? else {
            return Ok(None);
        };
        writer
            .write_all(&value)
            .map_err(|source| Error::Io { source })?;
        Ok(Some(value.len() as u64))
    }

    /// The value under `key`, or `None` if missing.
    fn aget<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<Bytes>>;

    /// As [`Kv::size`]. The length of [`Kv::aget`] by default.
    fn asize<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<u64>> {
        Box::pin(async move { Ok(self.aget(key).await?.map(|value| value.len() as u64)) })
    }

    /// As [`Kv::get_range`]. Slices [`Kv::aget`] by default.
    fn aget_range<'a>(
        &'a self,
        key: &'a [u8],
        start: u64,
        len: u64,
    ) -> KvFuture<'a, Option<Bytes>> {
        Box::pin(async move { Ok(self.aget(key).await?.map(|value| slice(&value, start, len))) })
    }

    /// Store `value` under `key`, replacing any previous value.
    fn aput<'a>(&'a self, key: &'a [u8], value: Bytes) -> KvFuture<'a, ()>;

    /// As [`Kv::put_new`], returning [`Error::Unsupported`] without reading
    /// or writing by default.
    fn aput_new<'a>(&'a self, _key: &'a [u8], _value: Bytes) -> KvFuture<'a, bool> {
        ready(Err(Op::PutNew.unsupported()))
    }

    /// Whether there is a value under `key`. [`Kv::get`] by default; stores
    /// override it to avoid reading the value.
    fn exists(&self, key: &[u8]) -> Result<bool, Error> {
        Ok(self.get(key)?.is_some())
    }

    /// Whether there is a value under `key`. [`Kv::aget`] by default; stores
    /// override it to avoid reading the value.
    fn aexists<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, bool> {
        Box::pin(async move { Ok(self.aget(key).await?.is_some()) })
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

    /// This store as an `Arc<dyn Kv>`: wrapped in a new `Arc`, except that an
    /// `Arc<dyn Kv>` returns itself.
    fn into_dyn(self) -> Arc<dyn Kv>
    where
        Self: Sized + 'static,
    {
        Arc::new(self)
    }
}

/// Up to `len` bytes of `value` from `start`, clipped at the end.
fn slice(value: &Bytes, start: u64, len: u64) -> Bytes {
    let size = value.len() as u64;
    let first = start.min(size) as usize;
    let last = start.saturating_add(len).min(size) as usize;
    value.slice(first..last)
}

/// A shared store is a store: each method forwards to it.
impl Kv for Arc<dyn Kv> {
    fn get(&self, key: &[u8]) -> Result<Option<Bytes>, Error> {
        (**self).get(key)
    }
    fn size(&self, key: &[u8]) -> Result<Option<u64>, Error> {
        (**self).size(key)
    }
    fn get_range(&self, key: &[u8], start: u64, len: u64) -> Result<Option<Bytes>, Error> {
        (**self).get_range(key, start, len)
    }
    fn put(&self, key: &[u8], value: Bytes) -> Result<(), Error> {
        (**self).put(key, value)
    }
    fn put_from(&self, key: &[u8], reader: &mut dyn Read, len: u64) -> Result<(), Error> {
        (**self).put_from(key, reader, len)
    }
    fn put_new(&self, key: &[u8], value: Bytes) -> Result<bool, Error> {
        (**self).put_new(key, value)
    }
    fn put_new_from(&self, key: &[u8], reader: &mut dyn Read, len: u64) -> Result<bool, Error> {
        (**self).put_new_from(key, reader, len)
    }
    fn get_to(&self, key: &[u8], writer: &mut dyn Write) -> Result<Option<u64>, Error> {
        (**self).get_to(key, writer)
    }
    fn aget<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
        (**self).aget(key)
    }
    fn asize<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<u64>> {
        (**self).asize(key)
    }
    fn aget_range<'a>(
        &'a self,
        key: &'a [u8],
        start: u64,
        len: u64,
    ) -> KvFuture<'a, Option<Bytes>> {
        (**self).aget_range(key, start, len)
    }
    fn aput<'a>(&'a self, key: &'a [u8], value: Bytes) -> KvFuture<'a, ()> {
        (**self).aput(key, value)
    }
    fn aput_new<'a>(&'a self, key: &'a [u8], value: Bytes) -> KvFuture<'a, bool> {
        (**self).aput_new(key, value)
    }
    fn exists(&self, key: &[u8]) -> Result<bool, Error> {
        (**self).exists(key)
    }
    fn aexists<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, bool> {
        (**self).aexists(key)
    }
    fn info(&self) -> Result<Value, Error> {
        (**self).info()
    }
    fn stats(&self) -> Result<Value, Error> {
        (**self).stats()
    }
    fn into_dyn(self) -> Arc<dyn Kv> {
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
    /// An atomic create-only write.
    PutNew,
}

impl Op {
    /// The error for a store that does not support this operation.
    pub fn unsupported(self) -> Error {
        Error::Unsupported { op: self }
    }
}

/// Errors from reading and storing.
#[derive(Debug, Snafu)]
#[snafu(crate_root(covalence_lib_error::snafu))]
#[non_exhaustive]
pub enum Error {
    /// The store does not support `op`.
    #[snafu(display("unsupported: {op:?}"))]
    Unsupported {
        /// The operation.
        op: Op,
    },
    /// A failure in the storage under a store, including a key it cannot hold.
    #[snafu(display("backend: {source}"))]
    Backend {
        /// The store's own error.
        source: BoxError,
    },
    /// The reader [`Kv::put_from`] reads or the writer [`Kv::get_to`] writes
    /// failed, or the reader ended early.
    #[snafu(display("{source}"))]
    Io {
        /// The I/O error.
        source: io::Error,
    },
}
