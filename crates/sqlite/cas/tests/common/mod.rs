//! A store that holds one value, records what it is asked and can misbehave,
//! and sample databases to keep in it.

#![allow(dead_code)]

use std::{
    io::Write,
    sync::{Arc, Mutex},
};

use covalence_cas_trait::{AsyncGet, AsyncPut, Cas, CasFuture, Error, Get, Put, ready};
use covalence_lib_cid::{Cid, Fact, Format, Multicodec};
use covalence_lib_data::Bytes;
use covalence_sqlite_cas::rusqlite::{Connection, types::ValueRef};

/// What a [`Store`] was asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Call {
    /// All the bytes, in memory.
    Get,
    /// All the bytes, to a writer.
    GetTo,
    /// The size.
    Size,
    /// A range.
    Range { start: u64, len: u64 },
}

/// How a [`Store`] misbehaves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Fault {
    /// It does not.
    #[default]
    None,
    /// Flip a bit of the byte at this offset of the value.
    Flip(u64),
    /// Give this many bytes fewer than asked.
    Short(usize),
    /// Give this many bytes more than asked.
    Long(usize),
    /// Fail every read of bytes.
    Fail,
    /// Have nothing.
    Missing,
}

/// A store of one value that answers for any CID.
pub struct Store {
    bytes: Bytes,
    /// The size it reports.
    pub size: Mutex<u64>,
    pub fault: Mutex<Fault>,
    pub calls: Mutex<Vec<Call>>,
    /// Whether `get_to` writes what it has without checking it.
    pub unchecked_get_to: bool,
}

impl Store {
    pub fn new(bytes: impl Into<Bytes>) -> Arc<Self> {
        Self::build(bytes, false)
    }

    /// A store whose `get_to` does not check what it writes.
    pub fn careless(bytes: impl Into<Bytes>) -> Arc<Self> {
        Self::build(bytes, true)
    }

    fn build(bytes: impl Into<Bytes>, unchecked_get_to: bool) -> Arc<Self> {
        let bytes = bytes.into();
        Arc::new(Self {
            size: Mutex::new(bytes.len() as u64),
            bytes,
            fault: Mutex::default(),
            calls: Mutex::default(),
            unchecked_get_to,
        })
    }

    /// The CID of the value with `hash`.
    pub fn cid(&self, hash: Multicodec) -> Cid {
        let digest = hash
            .digest(&self.bytes)
            .expect("a hash that can be computed");
        let format = Format {
            codec: Multicodec::RAW,
            hash,
        };
        Cid::new(format, &digest).unwrap()
    }

    /// The BLAKE3 CID of the value.
    pub fn blake3(&self) -> Cid {
        self.cid(Multicodec::BLAKE3)
    }

    /// The number of blocks of `block_size` bytes in the value.
    pub fn blake3_blocks(&self, block_size: u64) -> u64 {
        (self.bytes.len() as u64).div_ceil(block_size)
    }

    pub fn set(&self, fault: Fault) {
        *self.fault.lock().unwrap() = fault;
    }

    /// The calls so far, forgetting them.
    pub fn take_calls(&self) -> Vec<Call> {
        std::mem::take(&mut self.calls.lock().unwrap())
    }

    /// The ranges asked for so far, forgetting all calls.
    pub fn take_ranges(&self) -> Vec<(u64, u64)> {
        let calls = self.take_calls();
        let ranges = calls.iter().map(|call| match call {
            Call::Range { start, len } => (*start, *len),
            other => panic!("{other:?} is not a range"),
        });
        ranges.collect()
    }

    /// The bytes from `start` to `end`, as the fault has them.
    fn read(&self, call: Call, start: u64, end: u64) -> Result<Option<Bytes>, Error> {
        self.calls.lock().unwrap().push(call);
        let (start, end) = (start as usize, end.min(self.bytes.len() as u64) as usize);
        let exact = || self.bytes.slice(start.min(end)..end);
        Ok(Some(match *self.fault.lock().unwrap() {
            Fault::None => exact(),
            Fault::Flip(at) => {
                let mut bytes = exact().to_vec();
                if let Some(byte) = (at as usize)
                    .checked_sub(start)
                    .and_then(|at| bytes.get_mut(at))
                {
                    *byte ^= 1;
                }
                bytes.into()
            }
            Fault::Short(by) => self
                .bytes
                .slice(start.min(end)..end.saturating_sub(by).max(start)),
            Fault::Long(by) => {
                let mut bytes = exact().to_vec();
                bytes.extend(std::iter::repeat_n(0, by));
                bytes.into()
            }
            Fault::Fail => {
                return Err(Error::Backend {
                    source: "the store is down".into(),
                });
            }
            Fault::Missing => return Ok(None),
        }))
    }
}

impl Get for Store {
    fn get_unchecked(&self, _key: &Cid) -> Result<Option<Bytes>, Error> {
        self.read(Call::Get, 0, u64::MAX)
    }

    fn size_unchecked(&self, _key: &Cid) -> Result<Option<u64>, Error> {
        self.calls.lock().unwrap().push(Call::Size);
        match *self.fault.lock().unwrap() {
            Fault::Missing => Ok(None),
            Fault::Fail => Err(Error::Backend {
                source: "the store is down".into(),
            }),
            _ => Ok(Some(*self.size.lock().unwrap())),
        }
    }

    fn get_range_unchecked(
        &self,
        _key: &Cid,
        start: u64,
        len: u64,
    ) -> Result<Option<Bytes>, Error> {
        self.read(Call::Range { start, len }, start, start.saturating_add(len))
    }
}

impl Put for Store {
    fn put(&self, _bytes: Bytes) -> Result<Cid, Error> {
        panic!("a database is never written to its store")
    }
}

impl AsyncGet for Store {
    fn aget_unchecked<'a>(&'a self, key: &'a Cid) -> CasFuture<'a, Option<Bytes>> {
        ready(self.get_unchecked(key))
    }
}

impl AsyncPut for Store {
    fn aput(&self, bytes: Bytes) -> CasFuture<'_, Cid> {
        ready(self.put(bytes))
    }
}

impl Cas for Store {
    fn get_to(&self, key: &Cid, writer: &mut dyn Write) -> Result<Option<u64>, Error> {
        let Some(bytes) = self.read(Call::GetTo, 0, u64::MAX)? else {
            return Ok(None);
        };
        if !self.unchecked_get_to {
            // As the trait's own `get_to`: only bytes that match `key`.
            Fact::new(*key, bytes.clone())?;
        }
        // In pieces, as a store that streams would.
        for piece in bytes.chunks(10_000) {
            writer
                .write_all(piece)
                .map_err(|source| Error::Io { source })?;
        }
        Ok(Some(*self.size.lock().unwrap()))
    }
}

/// The path of a new database file with `rows` rows, in a new directory.
///
/// - `items(id INTEGER PRIMARY KEY, name TEXT, payload BLOB)` with an index on
///   `name`; row `id` has the name `name-{id * 7919 % rows}` and a payload of
///   200 bytes of `id % 251`.
/// - `blobs(id INTEGER PRIMARY KEY, body BLOB)` with one row of 300 000 bytes,
///   which spans many overflow pages.
pub fn sample_file(rows: u32) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sample.db");
    let mut connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "PRAGMA page_size = 4096;
             CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL, payload BLOB NOT NULL);
             CREATE TABLE blobs (id INTEGER PRIMARY KEY, body BLOB NOT NULL);",
        )
        .unwrap();
    let transaction = connection.transaction().unwrap();
    {
        let mut insert = transaction
            .prepare("INSERT INTO items (id, name, payload) VALUES (?1, ?2, ?3)")
            .unwrap();
        for id in 0..rows {
            let name = format!("name-{}", u64::from(id) * 7919 % u64::from(rows.max(1)));
            insert
                .execute((id, name, vec![(id % 251) as u8; 200]))
                .unwrap();
        }
        let body: Vec<u8> = (0..300_000u32).map(|byte| (byte % 239) as u8).collect();
        transaction
            .execute("INSERT INTO blobs (id, body) VALUES (1, ?1)", [body])
            .unwrap();
    }
    transaction.commit().unwrap();
    connection
        .execute_batch("CREATE INDEX items_name ON items (name);")
        .unwrap();
    drop(connection);
    (dir, path)
}

/// The bytes of [`sample_file`].
pub fn sample(rows: u32) -> Vec<u8> {
    let (_dir, path) = sample_file(rows);
    std::fs::read(path).unwrap()
}

/// Queries whose answers depend on every part of a sample database.
pub const QUERIES: &[&str] = &[
    "SELECT count(*), sum(id), sum(length(payload)) FROM items",
    "SELECT id, name, hex(substr(payload, 1, 4)) FROM items WHERE id IN (0, 1, 777, 1999)",
    "SELECT id FROM items WHERE name = 'name-1234'",
    "SELECT count(*), min(name), max(name) FROM items WHERE name BETWEEN 'name-100' AND 'name-200'",
    "SELECT id, name FROM items ORDER BY name DESC LIMIT 5",
    "SELECT length(body), hex(substr(body, 299990)) FROM blobs",
    "SELECT sum(id) FROM (SELECT id FROM items ORDER BY payload, name)",
    "SELECT name, sql FROM sqlite_schema ORDER BY name",
    "PRAGMA integrity_check",
];

/// The rows of `sql` as text.
pub fn rows(connection: &Connection, sql: &str) -> Vec<String> {
    let mut statement = connection.prepare(sql).unwrap();
    let columns = statement.column_count();
    let rows = statement.query_map([], |row| {
        let values = (0..columns).map(|column| row.get_ref(column).map(text));
        Ok(values.collect::<Result<Vec<_>, _>>()?.join("|"))
    });
    rows.unwrap().map(Result::unwrap).collect()
}

fn text(value: ValueRef<'_>) -> String {
    match value {
        ValueRef::Null => "NULL".into(),
        ValueRef::Integer(integer) => integer.to_string(),
        ValueRef::Real(real) => real.to_string(),
        ValueRef::Text(text) => String::from_utf8_lossy(text).into_owned(),
        ValueRef::Blob(blob) => format!("x'{}'", blob.len()),
    }
}

/// The answers to [`QUERIES`].
pub fn answers(connection: &Connection) -> Vec<Vec<String>> {
    QUERIES.iter().map(|sql| rows(connection, sql)).collect()
}
