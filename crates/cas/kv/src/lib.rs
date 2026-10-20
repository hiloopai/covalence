//! A content-addressed store over a key-value store.
//!
//! [`KvCas`] keeps blocks in any [`Kv`], such as `covalence-kv-memory`'s
//! `MemoryKv` or `covalence-kv-s3`'s `S3Kv`. Each block lies under
//! `blake3/<digest>/bytes`, after an optional prefix, where `<digest>` is its
//! [BLAKE3](https://github.com/BLAKE3-team/BLAKE3-specs) digest in lowercase,
//! unpadded [base32](https://www.rfc-editor.org/rfc/rfc4648#section-6).
//! [`KvCas::compressed`] instead stores a Zstandard frame under `bytes.zst`,
//! compatible with the corresponding compressed key layout.
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
    io::{self, Read, Seek, Write},
    sync::Arc,
};

use covalence_cas_trait::{
    AsyncGet, AsyncGetExt, AsyncPut, Cas, CasFuture, Error, EvictionPolicy, Flush, Get, GetExt,
    Put, map,
};
use covalence_kv_trait::Kv;
use covalence_lib_cid::{Blake3, Cid, Format};
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
/// - Uncompressed writes atomically create blocks through [`Kv::put_new`],
///   [`Kv::aput_new`] and [`Kv::put_new_from`], preserving existing blocks.
///   A backend without those operations rejects writes. A reused block may
///   leave the source unread; its remaining bytes are not checked.
/// - Compressed writes retain the key-value store's replacement behavior.
///   They check the complete declared source before publishing a new frame.
/// - Uncompressed `put_from` and `get_to` stream through the key-value
///   store, hashing the bytes as they pass. Compressed `put_from` stages
///   to a temporary file before publishing the value.
/// - For uncompressed blocks, if `put_from` reads bytes with another CID,
///   the read that would hand over the last byte fails instead. The key-value store's `put_from`
///   then fails as [`Kv::put_from`] says, storing nothing, and `put_from`
///   fails with [`Error::Check`]; or with [`Error::Backend`] if the key-value
///   store reports a further failure, such as failing to clean up.
/// - [`Cas::info`] is `{"name": "kv", "prefix": <prefix>, "kv": <the
///   key-value store's info>}`; [`Cas::stats`] is the key-value store's.
///
/// # S3
///
/// Uncompressed writes use conditional requests to preserve existing blocks. They do not
/// make a preliminary HEAD request, so they need only write access.
///
/// `has` uses a HEAD request. S3 answers a HEAD for a missing key with 403 to
/// a client that may not list the bucket.
///
/// S3 may consume the source before discovering that a block already exists.
/// Once the backend skips an uncompressed insertion, `put_from` leaves any remaining input
/// unread.
pub struct KvCas {
    kv: Arc<dyn Kv>,
    prefix: String,
    compression: Option<i32>,
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
            compression: None,
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
            compression: None,
        })
    }

    /// A store of Zstandard-compressed blocks under `prefix/` in `kv`.
    ///
    /// The empty prefix selects the root; other prefixes follow
    /// [`KvCas::with_prefix`]'s rules. `level` is passed to Zstandard (`0`
    /// selects its default). Invalid compression settings fail when writing.
    /// Reads, sizes and ranges describe the decompressed bytes. Streaming
    /// writes use a temporary file to finish and check a frame before storage.
    pub fn compressed(kv: Arc<dyn Kv>, prefix: &str, level: i32) -> Result<Self, InvalidPrefix> {
        let mut store = if prefix.is_empty() {
            Self::new(kv)
        } else {
            Self::with_prefix(kv, prefix)?
        };
        store.compression = Some(level);
        Ok(store)
    }

    fn decode(&self, bytes: Bytes) -> Result<Bytes, Error> {
        if self.compression.is_some() {
            zstd::stream::decode_all(bytes.as_ref())
                .map(Into::into)
                .map_err(|_| corrupt())
        } else {
            Ok(bytes)
        }
    }

    fn encode(&self, bytes: Bytes) -> Result<Bytes, Error> {
        if let Some(level) = self.compression {
            zstd::stream::encode_all(bytes.as_ref(), level)
                .map(Into::into)
                .map_err(io_error)
        } else {
            Ok(bytes)
        }
    }

    /// Hash and stage an unknown-length source without holding it in memory.
    fn stage(&self, reader: &mut dyn Read) -> Result<(std::fs::File, Blake3, u64), Error> {
        let mut file = tempfile::tempfile().map_err(io_error)?;
        let (hasher, len) = if let Some(level) = self.compression {
            let mut encoder =
                zstd::stream::write::Encoder::new(&mut file, level).map_err(io_error)?;
            let mut hashing = Hashing {
                writer: &mut encoder,
                hasher: blake3::Hasher::new(),
                len: 0,
                failed: false,
            };
            io::copy(reader, &mut hashing).map_err(io_error)?;
            let result = (hashing.hasher, hashing.len);
            encoder.finish().map_err(io_error)?;
            result
        } else {
            let mut hashing = Hashing {
                writer: &mut file,
                hasher: blake3::Hasher::new(),
                len: 0,
                failed: false,
            };
            io::copy(reader, &mut hashing).map_err(io_error)?;
            (hashing.hasher, hashing.len)
        };
        let cid = Blake3::new(Format::BLAKE3, hasher.finalize().as_bytes())?;
        file.rewind().map_err(io_error)?;
        Ok((file, cid, len))
    }

    fn publish(&self, cid: Blake3, file: &mut std::fs::File) -> Result<Cid, Error> {
        let len = file.metadata().map_err(io_error)?.len();
        self.kv
            .put_from(self.key(&cid).as_bytes(), file, len)
            .map_err(backend)?;
        Ok(cid.into())
    }

    /// The key for `cid`.
    fn key(&self, cid: &Blake3) -> String {
        let digest = BASE32_NOPAD.encode(cid.digest()).to_ascii_lowercase();
        format!(
            "{}blake3/{digest}/bytes{}",
            self.prefix,
            if self.compression.is_some() {
                ".zst"
            } else {
                ""
            }
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
        covalence_kv_trait::Error::Unsupported {
            op: covalence_kv_trait::Op::PutNew,
        } => covalence_cas_trait::Op::Put.unsupported(),
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
    failed: bool,
}

impl Write for Hashing<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.writer.write(buf).inspect_err(|_| self.failed = true)?;
        if written == 0 && !buf.is_empty() {
            // The enclosing decoder turns this into WriteZero. It comes from
            // the destination rather than from corrupt compressed data.
            self.failed = true;
        }
        self.hasher.update(&buf[..written]);
        self.len += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush().inspect_err(|_| self.failed = true)
    }
}

/// Distinguishes corrupt compressed data from output and storage failures.
struct Decoding<'a, 'b> {
    inner: zstd::stream::zio::Writer<&'a mut Hashing<'b>, zstd::stream::raw::Decoder<'static>>,
    invalid: bool,
}

impl Write for Decoding<'_, '_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.inner.write(bytes).inspect_err(|_| {
            self.invalid = !self.inner.writer().failed;
        })
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush().inspect_err(|_| {
            self.invalid = !self.inner.writer().failed;
        })
    }
}

impl Get<Blake3> for KvCas {
    fn get_unchecked(&self, key: &Blake3) -> Result<Option<Bytes>, Error> {
        self.kv
            .get(self.key(key).as_bytes())
            .map_err(backend)?
            .map(|bytes| self.decode(bytes))
            .transpose()
    }

    fn size_unchecked(&self, key: &Blake3) -> Result<Option<u64>, Error> {
        if self.compression.is_some() {
            return Ok(self.get_unchecked(key)?.map(|bytes| bytes.len() as u64));
        }
        self.kv.size(self.key(key).as_bytes()).map_err(backend)
    }

    fn get_range_unchecked(
        &self,
        key: &Blake3,
        start: u64,
        len: u64,
    ) -> Result<Option<Bytes>, Error> {
        if self.compression.is_some() {
            return Ok(self
                .get_unchecked(key)?
                .map(|bytes| slice(&bytes, start, len)));
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
            if self.compression.is_some() {
                return Ok(self
                    .aget_unchecked(&key.into())
                    .await?
                    .map(|bytes| bytes.len() as u64));
            }
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
            if self.compression.is_some() {
                return Ok(self
                    .aget_unchecked(&key.into())
                    .await?
                    .map(|bytes| slice(&bytes, start, len)));
            }
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
            self.kv
                .aget(key.as_bytes())
                .await
                .map_err(backend)?
                .map(|bytes| self.decode(bytes))
                .transpose()
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
        let key = self.key(&cid);
        if self.compression.is_some() {
            self.kv
                .put(key.as_bytes(), self.encode(bytes)?)
                .map_err(backend)?;
        } else {
            self.kv.put_new(key.as_bytes(), bytes).map_err(backend)?;
        }
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
            if self.compression.is_some() {
                self.kv
                    .aput(key.as_bytes(), self.encode(bytes)?)
                    .await
                    .map_err(backend)?;
            } else {
                self.kv
                    .aput_new(key.as_bytes(), bytes)
                    .await
                    .map_err(backend)?;
            }
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

    fn flush(&self, mode: Flush, _policy: Option<&dyn EvictionPolicy>) -> Result<(), Error> {
        if !(mode - Flush::PERSIST).is_empty() {
            return Err(covalence_cas_trait::Op::Flush.unsupported());
        }
        if mode.contains(Flush::PERSIST) {
            self.kv.flush().map_err(|error| match error {
                covalence_kv_trait::Error::Unsupported {
                    op: covalence_kv_trait::Op::Flush,
                } => covalence_cas_trait::Op::Flush.unsupported(),
                error => backend(error),
            })?;
        }
        Ok(())
    }

    fn aflush<'a>(
        &'a self,
        mode: Flush,
        _policy: Option<&'a dyn EvictionPolicy>,
    ) -> CasFuture<'a, ()> {
        Box::pin(async move {
            if !(mode - Flush::PERSIST).is_empty() {
                return Err(covalence_cas_trait::Op::Flush.unsupported());
            }
            if mode.contains(Flush::PERSIST) {
                self.kv.aflush().await.map_err(|error| match error {
                    covalence_kv_trait::Error::Unsupported {
                        op: covalence_kv_trait::Op::Flush,
                    } => covalence_cas_trait::Op::Flush.unsupported(),
                    error => backend(error),
                })?;
            }
            Ok(())
        })
    }

    fn put_from(&self, cid: Blake3, reader: &mut dyn Read, len: u64) -> Result<Cid, Error> {
        if self.compression.is_some() {
            let (mut file, actual, read) = self.stage(&mut reader.take(len))?;
            if read != len {
                return Err(io_error(io::ErrorKind::UnexpectedEof.into()));
            }
            if actual != cid {
                return Err(covalence_lib_cid::Error::Mismatch.into());
            }
            return self.publish(cid, &mut file);
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
        let mut hashing = Hashing {
            writer,
            hasher: blake3::Hasher::new(),
            len: 0,
            failed: false,
        };
        let key = self.key(&cid);
        if self.compression.is_some() {
            let decoder = zstd::stream::raw::Decoder::new().map_err(io_error)?;
            let mut decoding = Decoding {
                inner: zstd::stream::zio::Writer::new(&mut hashing, decoder),
                invalid: false,
            };
            let result = self.kv.get_to(key.as_bytes(), &mut decoding);
            if decoding.invalid {
                return Err(corrupt());
            }
            if result.map_err(backend)?.is_none() {
                return Ok(None);
            }
            if let Err(error) = decoding.inner.finish() {
                return Err(if decoding.inner.writer().failed {
                    io_error(error)
                } else {
                    corrupt()
                });
            }
            drop(decoding);
            if !hashes_to(&hashing.hasher, &cid) {
                return Err(covalence_lib_cid::Error::Mismatch.into());
            }
            return Ok(Some(hashing.len));
        }
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

/// A clipped range of decompressed bytes.
fn slice(bytes: &Bytes, start: u64, len: u64) -> Bytes {
    let size = bytes.len() as u64;
    bytes.slice(start.min(size) as usize..start.saturating_add(len).min(size) as usize)
}
