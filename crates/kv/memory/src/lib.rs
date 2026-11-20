//! An in-memory key-value store.
//!
//! [`MemoryKv`] keeps values in an ordered map in memory. It accepts every
//! key, including the empty one, and its async methods return ready futures,
//! except that [`Kv::aput_new_from`] waits for its stream.
//!
//! ```
//! use covalence_kv_memory::MemoryKv;
//! use covalence_kv_trait::Kv;
//! let store = MemoryKv::default();
//! store.put(b"greeting", "hello".into())?;
//! assert_eq!(store.get(b"greeting")?.unwrap(), "hello");
//! assert!(store.exists(b"greeting")?);
//! store.put(b"", "empty key".into())?;
//! assert_eq!(store.get(b"")?.unwrap(), "empty key");
//! assert_eq!(store.get(b"missing")?, None);
//! # Ok::<(), covalence_kv_trait::Error>(())
//! ```
#![deny(missing_docs)]

use std::{
    collections::BTreeMap,
    future::poll_fn,
    io::{self, Read},
    pin::Pin,
    sync::RwLock,
    task::{Context, Poll},
};

use covalence_kv_trait::{Error, Kv, KvFuture, ready};
use covalence_lib_dasl::Value;
use covalence_lib_data::{ByteStream, Bytes, BytesMut, Stream};

/// The largest chunk [`Kv::aget_stream`] yields.
const CHUNK: usize = 64 << 10;

/// An in-memory store. It accepts every key, and [`Kv::put_new`] is atomic.
///
/// [`Kv::aput_new_from`] reads the whole stream into memory before it stores
/// it, and [`Kv::aget_stream`] yields slices of the stored value of at most
/// 64 KiB, without copying it.
///
/// [`Kv::info`] is `{"name": "memory"}`, and [`Kv::stats`] counts `keys` and
/// the `bytes` of their values.
#[derive(Debug, Default)]
pub struct MemoryKv(RwLock<BTreeMap<Vec<u8>, Bytes>>);

impl Kv for MemoryKv {
    fn get(&self, key: &[u8]) -> Result<Option<Bytes>, Error> {
        Ok(self.0.read().unwrap().get(key).cloned())
    }

    fn put(&self, key: &[u8], value: Bytes) -> Result<(), Error> {
        self.0.write().unwrap().insert(key.into(), value);
        Ok(())
    }

    fn aget<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
        ready(self.get(key))
    }

    fn aput<'a>(&'a self, key: &'a [u8], value: Bytes) -> KvFuture<'a, ()> {
        ready(self.put(key, value))
    }

    fn put_new(&self, key: &[u8], value: Bytes) -> Result<bool, Error> {
        let mut map = self.0.write().unwrap();
        if map.contains_key(key) {
            return Ok(false);
        }
        map.insert(key.into(), value);
        Ok(true)
    }

    /// Reads the value into memory first, so the store is locked only to
    /// insert it.
    fn put_new_from(&self, key: &[u8], reader: &mut dyn Read, len: u64) -> Result<bool, Error> {
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
        self.put_new(key, value.into())
    }

    fn aput_new<'a>(&'a self, key: &'a [u8], value: Bytes) -> KvFuture<'a, bool> {
        ready(self.put_new(key, value))
    }

    fn aput_new_from<'a>(
        &'a self,
        key: &'a [u8],
        mut stream: ByteStream<'static>,
        len: u64,
    ) -> KvFuture<'a, bool> {
        Box::pin(async move {
            let mut value = BytesMut::new();
            while let Some(chunk) = poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
                let chunk = chunk.map_err(|source| Error::Io { source })?;
                if chunk.len() as u64 > len - value.len() as u64 {
                    return Err(Error::Io {
                        source: io::Error::new(
                            io::ErrorKind::InvalidData,
                            "stream is longer than its length",
                        ),
                    });
                }
                value.extend_from_slice(&chunk);
            }
            if (value.len() as u64) < len {
                return Err(Error::Io {
                    source: io::ErrorKind::UnexpectedEof.into(),
                });
            }
            self.put_new(key, value.freeze())
        })
    }

    fn aget_stream<'a>(
        &'a self,
        key: &'a [u8],
    ) -> KvFuture<'a, Option<(u64, ByteStream<'static>)>> {
        ready(self.get(key).map(|value| {
            value.map(|value| {
                let len = value.len() as u64;
                let chunks: ByteStream<'static> = Box::pin(Chunks(value));
                (len, chunks)
            })
        }))
    }

    fn exists(&self, key: &[u8]) -> Result<bool, Error> {
        Ok(self.0.read().unwrap().contains_key(key))
    }

    fn aexists<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, bool> {
        ready(self.exists(key))
    }

    fn info(&self) -> Result<Value, Error> {
        Ok(Value::Map(BTreeMap::from([(
            "name".into(),
            Value::String("memory".into()),
        )])))
    }

    fn stats(&self) -> Result<Value, Error> {
        let map = self.0.read().unwrap();
        let bytes = map.values().map(Bytes::len).sum::<usize>();
        Ok(Value::Map(BTreeMap::from([
            ("keys".into(), Value::Integer(map.len() as i64)),
            ("bytes".into(), Value::Integer(bytes as i64)),
        ])))
    }
}

/// The rest of a value, yielded in chunks of at most `CHUNK` bytes.
struct Chunks(Bytes);

impl Stream for Chunks {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let rest = &mut self.get_mut().0;
        if rest.is_empty() {
            return Poll::Ready(None);
        }
        let chunk = rest.split_to(rest.len().min(CHUNK));
        Poll::Ready(Some(Ok(chunk)))
    }
}
