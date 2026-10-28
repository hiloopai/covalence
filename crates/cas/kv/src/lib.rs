//! A content-addressed store over a key-value store.
//!
//! [`KvCas`] keeps blocks in any [`Kv`], such as `covalence-kv-memory`'s
//! `MemoryKv` or `covalence-kv-s3`'s `S3Kv`. Each block lies under
//! `blake3/<digest>/bytes`, after an optional prefix, where `<digest>` is its
//! [BLAKE3](https://github.com/BLAKE3-team/BLAKE3-specs) digest in lowercase,
//! unpadded [base32](https://www.rfc-editor.org/rfc/rfc4648#section-6).
//! A [content identifier (CID)](https://github.com/multiformats/cid)
//! addresses the uncompressed bytes of a block.
//! [`Compression::identity`] is the default. [`Compression::zstd`] instead stores
//! a Zstandard frame under `bytes.zst`; [`Compression::deflate`] stores a zlib
//! stream under `bytes.zlib`, and [`Compression::gzip`] uses `bytes.gz`.
//! All use the same digest of the uncompressed
//! bytes; select a representation with [`KvCas::with_compression`].
//! These format names do not implement HTTP negotiation. Stored Zstandard
//! frames are not restricted to [HTTP's 8 MiB window](https://www.rfc-editor.org/rfc/rfc9659.html#section-3.1.1).
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

mod gzip;
mod zlib;
mod zstd;

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

/// The stored representation of a block: identity by default.
#[derive(Clone, Debug, Default)]
pub struct Compression {
    codec: Option<Codec>,
}

#[derive(Clone, Debug)]
enum Codec {
    Zstd { level: i32 },
    Zlib { level: u32 },
    Gzip { level: u32 },
}

impl Compression {
    /// Uncompressed bytes, stored under `bytes` without changing their representation.
    ///
    /// This is the default. Streaming writes pass through to the key-value
    /// backend, and sizes and ranges use its native operations.
    pub const fn identity() -> Self {
        Self { codec: None }
    }

    /// Zstandard frames, stored under `bytes.zst`.
    ///
    /// Zero selects Zstandard's default level; other levels are clamped to
    /// its supported range. Compression does not change the block's CID.
    pub fn zstd(level: i32) -> Self {
        Self {
            codec: Some(Codec::Zstd { level }),
        }
    }

    /// A zlib-wrapped DEFLATE stream, stored under `bytes.zlib`.
    ///
    /// Levels clamp to 0 through 9; zero stores uncompressed DEFLATE blocks.
    /// This is RFC 1950 zlib, as used by browser `CompressionStream("deflate")`,
    /// rather than raw DEFLATE or gzip. Reads require one complete stream with
    /// a valid checksum and no trailing bytes. Compression keeps the block's CID.
    pub fn deflate(level: i32) -> Self {
        Self {
            codec: Some(Codec::Zlib {
                level: level.clamp(0, 9) as u32,
            }),
        }
    }

    /// A single gzip member, stored under `bytes.gz`.
    ///
    /// Levels clamp to 0 through 9; zero stores uncompressed DEFLATE blocks.
    /// Reads follow browser `DecompressionStream("gzip")`: the header and
    /// checksum must be valid and trailing bytes, including another member,
    /// are rejected. Compression keeps the block's CID.
    pub fn gzip(level: i32) -> Self {
        Self {
            codec: Some(Codec::Gzip {
                level: level.clamp(0, 9) as u32,
            }),
        }
    }

    fn suffix(&self) -> &'static str {
        match self.codec {
            None => "",
            Some(Codec::Zstd { .. }) => ".zst",
            Some(Codec::Zlib { .. }) => ".zlib",
            Some(Codec::Gzip { .. }) => ".gz",
        }
    }

    fn is_identity(&self) -> bool {
        self.codec.is_none()
    }

    fn encode(&self, bytes: Bytes) -> Result<Bytes, Error> {
        match self.codec {
            None => Ok(bytes),
            Some(Codec::Zstd { level }) => zstd::encode(bytes, level),
            Some(Codec::Zlib { level }) => zlib::encode(bytes, level),
            Some(Codec::Gzip { level }) => gzip::encode(bytes, level),
        }
    }

    /// Raw streams pass directly to the backend, without staging.
    fn stage(
        &self,
        reader: &mut dyn Read,
        len: u64,
        cid: &Blake3,
    ) -> Result<Option<std::fs::File>, Error> {
        match self.codec {
            None => Ok(None),
            Some(Codec::Zstd { level }) => zstd::stage(reader, len, cid, level).map(Some),
            Some(Codec::Zlib { level }) => zlib::stage(reader, len, cid, level).map(Some),
            Some(Codec::Gzip { level }) => gzip::stage(reader, len, cid, level).map(Some),
        }
    }

    fn decode_to(
        &self,
        writer: &mut dyn Write,
        read: impl FnOnce(&mut dyn Write) -> Result<Option<u64>, Error>,
    ) -> Result<Option<(blake3::Hasher, u64)>, Error> {
        match self.codec {
            None => {
                let mut hashing = Hashing::new(writer);
                Ok(read(&mut hashing)?.map(|len| (hashing.hasher, len)))
            }
            Some(Codec::Zstd { .. }) => zstd::decode_to(writer, read),
            Some(Codec::Zlib { .. }) => zlib::decode_to(writer, read),
            Some(Codec::Gzip { .. }) => gzip::decode_to(writer, read),
        }
    }
}

/// A store of blocks in a [`Kv`]; see the [crate] docs for the key layout.
///
/// - A CID with a 32-byte BLAKE3 digest finds its bytes whatever its codec;
///   other CIDs find nothing. [`Put`] returns the raw BLAKE3 CID.
/// - The key-value store may be remote or shared, so `get`, `aget`,
///   `get_fact` and `aget_fact` check the bytes against the CID and fail with
///   [`Error::Check`] if they do not match. `get_unchecked`, sizes and ranges
///   are not checked.
/// - `has` asks the key-value store whether the key exists, without reading
///   the bytes.
/// - `put`, `aput` and `put_from` never replace a block: they store through
///   the key-value store's [`Kv::put_new`], [`Kv::aput_new`] and
///   [`Kv::put_new_from`], so concurrent writers of one block race safely.
///   Writes fail if the backend does not implement atomic create-only writes.
/// - Compression keeps CIDs of the uncompressed bytes. Streaming writes
///   finish and check a compressed temporary file before publishing it;
///   streamed reads decode and hash the bytes as they pass. Sizes and ranges
///   scan the complete frame, retaining only the requested output. Async
///   reads fetch the encoded value whole, as the key-value trait requires.
/// - Without compression, `put_from` and `get_to` stream through the
///   key-value store's `put_new_from` and `get_to`, hashing the bytes as they
///   pass.
///   If the backend skips an existing block, `put_from` returns without
///   consuming or checking any remaining input.
/// - Without compression, if the bytes `put_from` reads do not hash to the
///   CID, the read that would hand over the last byte fails instead. The
///   key-value store's `put_new_from` then fails, storing nothing, and
///   `put_from` fails with [`Error::Check`]; or with [`Error::Backend`] if the
///   key-value store reports a further failure, such as failing to clean up.
/// - [`Cas::info`] is `{"name": "kv", "prefix": <prefix>, "kv": <the
///   key-value store's info>}`, with `compression` and `level` fields when
///   compressed; [`Cas::stats`] is the key-value store's.
///
/// # S3
///
/// Writes use conditional requests to preserve existing blocks. They do not
/// make a preliminary HEAD request, so they need only write access.
///
/// `has` uses a HEAD request. S3 answers a HEAD for a missing key with 403 to
/// a client that may not list the bucket.
///
/// For uncompressed blocks, S3 may consume the source before discovering that
/// a block already exists. Once the backend skips insertion, `put_from`
/// leaves any remaining input unread.
pub struct KvCas {
    kv: Arc<dyn Kv>,
    prefix: String,
    compression: Compression,
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
    /// A store of raw bytes with keys at the root of `kv`.
    pub fn new(kv: Arc<dyn Kv>) -> Self {
        Self {
            kv,
            prefix: String::new(),
            compression: Compression::default(),
        }
    }

    /// A store of raw bytes with keys under `prefix/` in `kv`; a trailing `/`
    /// on `prefix` is optional.
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
            compression: Compression::default(),
        })
    }

    /// Select the stored representation, keeping CIDs of the uncompressed bytes.
    ///
    /// Raw bytes are the default; [`Compression::identity`] also restores that
    /// representation. Each codec selects a separate key suffix, so stores
    /// using different codecs do not read each other's blocks. Compressed
    /// streaming writes stage on temporary disk before uploading; the
    /// key-value backend controls transfer buffering.
    ///
    /// ```
    /// use covalence_cas_kv::{Compression, KvCas};
    /// use covalence_cas_trait::{Get, Put};
    /// use covalence_kv_memory::MemoryKv;
    /// use covalence_kv_trait::Kv;
    /// use covalence_lib_cid::Cid;
    /// let store = KvCas::new(MemoryKv::default().into_dyn())
    ///     .with_compression(Compression::zstd(3));
    /// let cid: Cid = store.put("hello".into())?;
    /// assert_eq!(store.get(&cid)?.unwrap(), "hello");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn with_compression(mut self, compression: Compression) -> Self {
        self.compression = compression;
        self
    }

    /// Read logical bytes through the backend, optionally checking the CID.
    fn read_to(
        &self,
        cid: &Blake3,
        writer: &mut dyn Write,
        check: bool,
    ) -> Result<Option<u64>, Error> {
        let key = self.key(cid);
        let result = self.compression.decode_to(writer, |decoding| {
            self.kv.get_to(key.as_bytes(), decoding).map_err(backend)
        })?;
        result
            .map(|(hasher, len)| {
                if check && !hashes_to(&hasher, cid) {
                    return Err(corrupt());
                }
                Ok(len)
            })
            .transpose()
    }

    /// The key for `cid`.
    fn key(&self, cid: &Blake3) -> String {
        let digest = BASE32_NOPAD.encode(cid.digest()).to_ascii_lowercase();
        format!(
            "{}blake3/{digest}/bytes{}",
            self.prefix,
            self.compression.suffix()
        )
    }
}

fn corrupt() -> Error {
    covalence_lib_cid::Error::Mismatch.into()
}

fn io_error(source: io::Error) -> Error {
    Error::Io { source }
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
    len: u64,
}

impl<'a> Hashing<'a> {
    fn new(writer: &'a mut dyn Write) -> Self {
        Self {
            writer,
            hasher: blake3::Hasher::new(),
            len: 0,
        }
    }
}

impl Write for Hashing<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.writer.write(buf)?;
        self.hasher.update(&buf[..written]);
        self.len += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// A slice sink that discards bytes before and after the requested range.
struct Range {
    skip: u64,
    left: u64,
    bytes: Vec<u8>,
}

impl Range {
    fn new(start: u64, len: u64) -> Self {
        Self {
            skip: start,
            left: len,
            bytes: Vec::new(),
        }
    }
}

impl Write for Range {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let skipped = self.skip.min(bytes.len() as u64) as usize;
        self.skip -= skipped as u64;
        let selected = self.left.min((bytes.len() - skipped) as u64) as usize;
        self.bytes
            .extend_from_slice(&bytes[skipped..skipped + selected]);
        self.left -= selected as u64;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Get<Blake3> for KvCas {
    fn get_unchecked(&self, key: &Blake3) -> Result<Option<Bytes>, Error> {
        if !self.compression.is_identity() {
            let mut bytes = Vec::new();
            return Ok(self.read_to(key, &mut bytes, false)?.map(|_| bytes.into()));
        }
        self.kv.get(self.key(key).as_bytes()).map_err(backend)
    }

    fn size_unchecked(&self, key: &Blake3) -> Result<Option<u64>, Error> {
        if !self.compression.is_identity() {
            return self.read_to(key, &mut io::sink(), false);
        }
        self.kv.size(self.key(key).as_bytes()).map_err(backend)
    }

    fn get_range_unchecked(
        &self,
        key: &Blake3,
        start: u64,
        len: u64,
    ) -> Result<Option<Bytes>, Error> {
        if !self.compression.is_identity() {
            let mut range = Range::new(start, len);
            return Ok(self
                .read_to(key, &mut range, false)?
                .map(|_| range.bytes.into()));
        }
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
            if !self.compression.is_identity() {
                let Some(bytes) = self.kv.aget(key.as_bytes()).await.map_err(backend)? else {
                    return Ok(None);
                };
                return Ok(self
                    .compression
                    .decode_to(&mut io::sink(), |writer| {
                        writer.write_all(&bytes).map_err(io_error)?;
                        Ok(Some(bytes.len() as u64))
                    })?
                    .map(|(_, len)| len));
            }
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
            if !self.compression.is_identity() {
                let Some(bytes) = self.kv.aget(key.as_bytes()).await.map_err(backend)? else {
                    return Ok(None);
                };
                let mut range = Range::new(start, len);
                return Ok(self
                    .compression
                    .decode_to(&mut range, |writer| {
                        writer.write_all(&bytes).map_err(io_error)?;
                        Ok(Some(bytes.len() as u64))
                    })?
                    .map(|_| range.bytes.into()));
            }
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
            let Some(bytes) = self.kv.aget(key.as_bytes()).await.map_err(backend)? else {
                return Ok(None);
            };
            if !self.compression.is_identity() {
                let mut decoded = Vec::new();
                return Ok(self
                    .compression
                    .decode_to(&mut decoded, |writer| {
                        writer.write_all(&bytes).map_err(io_error)?;
                        Ok(Some(bytes.len() as u64))
                    })?
                    .map(|_| decoded.into()));
            }
            Ok(Some(bytes))
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
            .put_new(self.key(&cid).as_bytes(), self.compression.encode(bytes)?)
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
            self.kv
                .aput_new(key.as_bytes(), self.compression.encode(bytes)?)
                .await
                .map_err(backend)?;
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

    /// The name, key prefix, backend info, and any compression settings.
    fn info(&self) -> Result<Value, Error> {
        let name = Value::String("kv".into());
        let prefix = Value::String(self.prefix.as_str().into());
        let kv = self.kv.info().map_err(backend)?;
        Ok(match &self.compression.codec {
            None => map([("name", name), ("prefix", prefix), ("kv", kv)]),
            Some(Codec::Zstd { level }) => map([
                ("name", name),
                ("prefix", prefix),
                ("kv", kv),
                ("compression", Value::String("zstd".into())),
                ("level", Value::Integer((*level).into())),
            ]),
            Some(Codec::Zlib { level }) => map([
                ("name", name),
                ("prefix", prefix),
                ("kv", kv),
                ("compression", Value::String("deflate".into())),
                ("level", Value::Integer((*level).into())),
            ]),
            Some(Codec::Gzip { level }) => map([
                ("name", name),
                ("prefix", prefix),
                ("kv", kv),
                ("compression", Value::String("gzip".into())),
                ("level", Value::Integer((*level).into())),
            ]),
        })
    }

    fn stats(&self) -> Result<Value, Error> {
        self.kv.stats().map_err(backend)
    }

    fn put_from(&self, cid: Blake3, reader: &mut dyn Read, len: u64) -> Result<Cid, Error> {
        if let Some(mut file) = self.compression.stage(reader, len, &cid)? {
            let encoded_len = file.metadata().map_err(io_error)?.len();
            self.kv
                .put_new_from(self.key(&cid).as_bytes(), &mut file, encoded_len)
                .map_err(backend)?;
            return Ok(cid.into());
        }
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
        match self.kv.put_new_from(key.as_bytes(), &mut checked, len) {
            Ok(_) => Ok(cid.into()),
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
        self.read_to(&cid, writer, true)
    }
}
