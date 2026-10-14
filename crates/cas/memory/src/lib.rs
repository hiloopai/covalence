//! An in-memory content-addressed store.
//!
//! [`MemoryCas`] keeps each block once, as a [`Fact`] for its raw BLAKE3 CID.
//!
//! - A CID with a 32-byte [BLAKE3](https://github.com/BLAKE3-team/BLAKE3-specs)
//!   digest finds its bytes whatever its codec; other CIDs find nothing.
//! - [`Put`] returns the raw BLAKE3 CID, and `put_fact` of a BLAKE3 fact
//!   stores it without hashing again.
//! - The async methods return ready futures.
//!
//! ```
//! use covalence_cas_memory::MemoryCas;
//! use covalence_cas_trait::{GetExt, Put};
//! use covalence_lib_cid::{Cid, Format};
//! let store = MemoryCas::default();
//! let cid: Cid = store.put("hello".into())?;
//! assert_eq!(store.get_fact(&cid)?.unwrap().bytes(), "hello");
//! // The same digest under another codec finds the same bytes.
//! let drisl: Cid = Cid::new(Format::DRISL_BLAKE3, cid.digest())?;
//! assert_eq!(store.get_fact(&drisl)?.unwrap().claim(), &drisl);
//! // A block is stored once.
//! let _: Cid = store.put("hello".into())?;
//! assert_eq!(store.len(), 1);
//! # Ok::<(), covalence_cas_trait::Error>(())
//! ```
#![deny(missing_docs)]

use std::sync::RwLock;

use covalence_cas_trait::{AsyncGet, AsyncPut, Cas, CasFuture, Error, Get, Put, map, ready};
use covalence_lib_cid::{Blake3, Cid, Fact};
use covalence_lib_dasl::Value;
use covalence_lib_data::{Bytes, HashTable};

/// An in-memory store of BLAKE3 facts; see the [crate] docs.
///
/// [`Cas::info`] is `{"name": "memory"}`, and [`Cas::stats`] counts `blocks`
/// and their `bytes`.
///
/// # Panics
///
/// Putting a new block panics if the store already holds 2³² blocks.
#[derive(Default)]
pub struct MemoryCas(RwLock<Table>);

/// Facts in insertion order, indexed by key.
#[derive(Default)]
struct Table {
    facts: Vec<Fact<Blake3>>,
    index: HashTable<u32>,
}

/// A table hash for `key`: its digest is already uniform.
fn hash(key: &Blake3) -> u64 {
    u64::from_le_bytes(key.digest()[..8].try_into().unwrap())
}

impl Table {
    fn find(&self, key: &Blake3) -> Option<&Fact<Blake3>> {
        let facts = &self.facts;
        let i = self
            .index
            .find(hash(key), |&i| facts[i as usize].claim() == key)?;
        Some(&facts[*i as usize])
    }

    fn insert(&mut self, fact: Fact<Blake3>) {
        if self.find(fact.claim()).is_some() {
            return;
        }
        let Table { facts, index } = self;
        let i = u32::try_from(facts.len()).expect("fewer than 2^32 blocks");
        index.insert_unique(hash(fact.claim()), i, |&i| hash(facts[i as usize].claim()));
        facts.push(fact);
    }
}

impl MemoryCas {
    /// The number of stored blocks.
    pub fn len(&self) -> usize {
        self.0.read().unwrap().facts.len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Get<Blake3> for MemoryCas {
    fn get_unchecked(&self, key: &Blake3) -> Result<Option<Bytes>, Error> {
        Ok(self.0.read().unwrap().find(key).map(|f| f.bytes().clone()))
    }

    fn get_fact_unchecked(&self, key: &Blake3) -> Result<Option<Fact<Blake3>>, Error> {
        Ok(self.0.read().unwrap().find(key).cloned())
    }
}

impl Get for MemoryCas {
    fn get_unchecked(&self, key: &Cid) -> Result<Option<Bytes>, Error> {
        key.raw_blake3()
            .map_or(Ok(None), |key| self.get_unchecked(&key))
    }

    fn get_fact_unchecked(&self, key: &Cid) -> Result<Option<Fact>, Error> {
        let Some(key) = key.raw_blake3() else {
            return Ok(None);
        };
        Ok(self.get_fact_unchecked(&key)?.map(Fact::widen))
    }
}

impl AsyncGet for MemoryCas {
    fn aget_unchecked<'a>(&'a self, key: &'a Cid) -> CasFuture<'a, Option<Bytes>> {
        ready(self.get_unchecked(key))
    }

    fn aget_fact_unchecked<'a>(&'a self, key: &'a Cid) -> CasFuture<'a, Option<Fact>> {
        ready(self.get_fact_unchecked(key))
    }
}

impl Put<Blake3> for MemoryCas {
    fn put(&self, bytes: Bytes) -> Result<Blake3, Error> {
        self.put_fact(Fact::of(bytes))
    }

    /// Stores `fact` without hashing.
    fn put_fact(&self, fact: Fact<Blake3>) -> Result<Blake3, Error> {
        let key = *fact.claim();
        self.0.write().unwrap().insert(fact);
        Ok(key)
    }
}

impl Put for MemoryCas {
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

impl AsyncPut for MemoryCas {
    fn aput(&self, bytes: Bytes) -> CasFuture<'_, Cid> {
        ready(self.put(bytes))
    }

    fn aput_fact(&self, fact: Fact) -> CasFuture<'_, Cid> {
        ready(self.put_fact(fact))
    }
}

impl Cas for MemoryCas {
    fn info(&self) -> Result<Value, Error> {
        Ok(map([("name", Value::String("memory".into()))]))
    }

    fn stats(&self) -> Result<Value, Error> {
        let table = self.0.read().unwrap();
        let bytes = table.facts.iter().map(|f| f.bytes().len()).sum::<usize>();
        Ok(map([
            ("blocks", Value::Integer(table.facts.len() as i64)),
            ("bytes", Value::Integer(bytes as i64)),
        ]))
    }
}
