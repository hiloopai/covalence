//! An in-memory key-value store.
//!
//! [`MemoryKv`] keeps values in an ordered map in memory. It accepts every
//! key, including the empty one, and its async methods return ready futures.
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

use std::{collections::BTreeMap, sync::RwLock};

use covalence_kv_trait::{Error, Kv, KvFuture, ready};
use covalence_lib_dasl::Value;
use covalence_lib_data::Bytes;

/// An in-memory store. It accepts every key.
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
