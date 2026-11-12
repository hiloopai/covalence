//! Verification: how far a store is trusted with a database's bytes.
//!
//! A CID is the hash of all of a database's bytes, so it can be checked only
//! with all the bytes at hand. A [`Verification`] says what to do about that:
//! fetch everything first, trust the store, or check each range against
//! something else that is trusted.
//!
//! [`Manifest`] is that something else for any hash function: a list of the
//! digests of the database's chunks, small enough to fetch and check whole.
//!
//! ```
//! use covalence_lib_cid::Multicodec;
//! use covalence_sqlite_cas::verify::{Manifest, RangeVerifier};
//!
//! let content: Vec<u8> = (0..10_000u32).map(|byte| (byte % 251) as u8).collect();
//! let manifest = Manifest::blake3(4096, &content)?;
//! assert_eq!((manifest.size(), manifest.chunks()), (10_000, 3));
//! assert_eq!(manifest, Manifest::of(Multicodec::BLAKE3, 4096, &content)?);
//!
//! // Whole chunks verify from the chunk they start at; other bytes do not.
//! manifest.verify(1, &content[4096..])?;
//! assert!(manifest.verify(1, &content[4095..8191]).is_err());
//! assert!(manifest.verify(2, &[8; 1808]).is_err());
//! # Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
//! ```

use std::{collections::BTreeMap, fmt, io::Read, sync::Arc};

use covalence_cas_trait::{Cas, GetExt};
use covalence_lib_cid::{Blake3, Cid, Fact, Multicodec};
use covalence_lib_dasl::Value;
use covalence_lib_data::Bytes;
use covalence_lib_error::{
    BoxError,
    snafu::{OptionExt, Snafu, ensure},
};

use crate::{
    Error, MAX_BLOCK_SIZE,
    cache::Layout,
    error::{ManifestSnafu, MissingSnafu, OptionsSnafu, UnsupportedSnafu},
    hash::{self, Hasher},
};

/// How a database's bytes are checked before SQLite reads them.
#[derive(Clone, Default)]
#[non_exhaustive]
pub enum Verification {
    /// Download the whole database to a local file, check that it hashes to
    /// its CID, and only then read it, from that file.
    ///
    /// The store is not trusted, and nothing is read by range from it. Use
    /// this for a store that others can write to, when there is nothing to
    /// check ranges against.
    #[default]
    Download,
    /// Read ranges from the store and trust them as they are.
    ///
    /// Nothing checks that the bytes are the CID's: the store can give any
    /// database, or change its answers between reads. Use this only for a
    /// store trusted as much as a local file, such as one that checks what it
    /// holds, on a disk only its owner can write.
    TrustedRanges,
    /// Read ranges from the store and check each with a [`RangeVerifier`]
    /// before SQLite or a cache sees it.
    ///
    /// The store is not trusted. What is read is the content the verifier
    /// vouches for, such as that of a [`Manifest`], so the verifier must come
    /// from somewhere trusted: the CID given at open only says where to fetch.
    VerifiedRanges(Arc<dyn RangeVerifier>),
    /// Read ranges from the store and check each against a BLAKE3 CID itself,
    /// with proofs from BLAKE3's hash tree.
    ///
    /// Not supported yet: opening fails with [`Error::Unsupported`], and
    /// never falls back to another verification.
    Blake3Ranges,
}

impl Verification {
    /// [`Verification::VerifiedRanges`] with the chunk digests of `manifest`.
    pub fn manifest(manifest: Manifest) -> Self {
        Verification::VerifiedRanges(Arc::new(manifest))
    }
}

impl fmt::Debug for Verification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Verification::Download => "Download",
            Verification::TrustedRanges => "TrustedRanges",
            Verification::VerifiedRanges(_) => "VerifiedRanges",
            Verification::Blake3Ranges => "Blake3Ranges",
        })
    }
}

/// Checks ranges of a database against something trusted, chunk by chunk.
///
/// The content is cut into chunks of [`RangeVerifier::chunk_size`] bytes; the
/// last holds what is left. A database with a verifier is read in blocks of
/// that size, and every fetch is of whole chunks.
///
/// A verifier for another proof scheme, such as a Merkle tree whose proofs
/// come from a store, implements this trait, and may fetch what it needs in
/// [`RangeVerifier::verify`].
pub trait RangeVerifier: Send + Sync {
    /// The size in bytes of the content this verifier vouches for.
    fn size(&self) -> u64;

    /// The size of a chunk in bytes: at least 1.
    fn chunk_size(&self) -> u64;

    /// Check that `bytes` are the content's chunks from chunk `chunk` on.
    ///
    /// `bytes` holds whole chunks: each has [`RangeVerifier::chunk_size`]
    /// bytes, but the content's last, which has what is left. An error means
    /// the bytes were not shown to be the content's, and they are not used.
    fn verify(&self, chunk: u64, bytes: &[u8]) -> Result<(), BoxError>;
}

/// The digests of a content's chunks, with any hash function a CID can use.
///
/// A manifest is small: one digest for each chunk. Kept under its own CID, it
/// can be fetched and checked whole, and then vouches for every range of the
/// content. Whoever makes the manifest decides what content that is.
///
/// - Chunks have [`Manifest::chunk_size`] bytes; the last has what is left.
/// - The hash functions are those `covalence-lib-cid` computes: BLAKE3,
///   SHA-256, SHA-512 and SHA-1.
/// - A manifest is a DASL map of `chunk_size`, `digests` (all the digests in
///   order, as one byte string), `hash` (the hash function's multicodec
///   code) and `size`, so it has one DRISL encoding and one CID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    hash: Multicodec,
    digest_len: usize,
    layout: Layout,
    digests: Bytes,
}

impl Manifest {
    /// The manifest of `content`, in chunks of `chunk_size` bytes hashed with `hash`.
    ///
    /// Fails with [`Error::Unsupported`] if `hash` is not a hash function
    /// this crate computes, and with [`Error::Options`] if `chunk_size` is 0
    /// or more than [`MAX_BLOCK_SIZE`].
    pub fn of(hash: Multicodec, chunk_size: u64, content: &[u8]) -> Result<Self, Error> {
        Self::read(hash, chunk_size, content)
    }

    /// The manifest of `content`, in chunks of `chunk_size` bytes hashed with BLAKE3.
    ///
    /// Fails as [`Manifest::of`] does.
    pub fn blake3(chunk_size: u64, content: &[u8]) -> Result<Self, Error> {
        Self::read(Multicodec::BLAKE3, chunk_size, content)
    }

    /// The manifest of all the bytes of `content`, read to its end.
    ///
    /// Fails as [`Manifest::of`] does, and with [`Error::Io`] if `content`
    /// fails.
    pub fn read(hash: Multicodec, chunk_size: u64, mut content: impl Read) -> Result<Self, Error> {
        let digest_len = digest_len(hash)?;
        check_chunk_size(chunk_size).map_err(|reason| OptionsSnafu { reason }.build())?;
        let mut digests = Vec::new();
        let mut size = 0u64;
        // The bytes of the chunk in progress, and its hasher once it has bytes.
        let mut filled = 0u64;
        let mut hasher = None;
        let mut buf = vec![0; 64 << 10];
        loop {
            let read = match content.read(&mut buf) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(source) => return Err(Error::Io { source }),
            };
            let mut rest = &buf[..read];
            while !rest.is_empty() {
                let take = (chunk_size - filled).min(rest.len() as u64) as usize;
                let chunk = match &mut hasher {
                    Some(chunk) => chunk,
                    empty => empty.insert(Hasher::new(hash).context(UnsupportedSnafu {
                        what: format!("the hash function {hash:?}"),
                    })?),
                };
                chunk.update(&rest[..take]);
                rest = &rest[take..];
                filled += take as u64;
                size = size.checked_add(take as u64).context(TooLargeSnafu)?;
                if filled == chunk_size {
                    digests.extend(hasher.take().map(Hasher::finish).unwrap_or_default());
                    filled = 0;
                }
            }
        }
        digests.extend(hasher.map(Hasher::finish).unwrap_or_default());
        Self::new(hash, digest_len, chunk_size, size, digests.into())
            .map_err(|reason| ManifestSnafu { reason }.build())
    }

    /// A manifest of these parts, if they fit together.
    fn new(
        hash: Multicodec,
        digest_len: usize,
        chunk_size: u64,
        size: u64,
        digests: Bytes,
    ) -> Result<Self, String> {
        check_chunk_size(chunk_size)?;
        if size > i64::MAX as u64 {
            return Err(format!("a size of {size} bytes is too large"));
        }
        let layout = Layout::new(size, chunk_size);
        let expected = layout.blocks().checked_mul(digest_len as u64);
        if expected != Some(digests.len() as u64) {
            return Err(format!(
                "{} bytes of digests are not {} digests of {digest_len} bytes",
                digests.len(),
                layout.blocks(),
            ));
        }
        Ok(Self {
            hash,
            digest_len,
            layout,
            digests,
        })
    }

    /// The hash function of the chunk digests.
    pub fn hash(&self) -> Multicodec {
        self.hash
    }

    /// The size of the content in bytes.
    pub fn size(&self) -> u64 {
        self.layout.size()
    }

    /// The size of a chunk in bytes.
    pub fn chunk_size(&self) -> u64 {
        self.layout.block_size()
    }

    /// The number of chunks.
    pub fn chunks(&self) -> u64 {
        self.layout.blocks()
    }

    /// The digest of chunk `chunk`, or `None` past the last chunk.
    pub fn digest(&self, chunk: u64) -> Option<&[u8]> {
        let start = usize::try_from(chunk).ok()?.checked_mul(self.digest_len)?;
        self.digests.get(start..start.checked_add(self.digest_len)?)
    }

    /// The manifest as a DASL value: a map of `chunk_size`, `digests`, `hash` and `size`.
    pub fn to_value(&self) -> Value {
        // A manifest's numbers are at most `i64::MAX`: `new` checks them.
        let integer = |number: u64| Value::Integer(number as i64);
        Value::Map(BTreeMap::from([
            ("chunk_size".into(), integer(self.chunk_size())),
            ("digests".into(), Value::Bytes(self.digests.clone())),
            ("hash".into(), integer(self.hash.0)),
            ("size".into(), integer(self.size())),
        ]))
    }

    /// The manifest that `value` is.
    ///
    /// Fails with [`Error::Manifest`] unless `value` is what
    /// [`Manifest::to_value`] gives for some manifest: a map of exactly its
    /// four keys, with a hash function this crate computes and one digest for
    /// each chunk.
    pub fn from_value(value: &Value) -> Result<Self, Error> {
        Self::parse(value).map_err(|reason| ManifestSnafu { reason }.build())
    }

    fn parse(value: &Value) -> Result<Self, String> {
        let Value::Map(map) = value else {
            return Err("the value is not a map".into());
        };
        let keys = ["chunk_size", "digests", "hash", "size"];
        if !map.keys().map(String::as_str).eq(keys) {
            return Err(format!("the map's keys are not {keys:?}"));
        }
        let integer = |key: &str| match &map[key] {
            Value::Integer(number) => {
                u64::try_from(*number).map_err(|_| format!("{key} is negative"))
            }
            _ => Err(format!("{key} is not an integer")),
        };
        let Value::Bytes(digests) = &map["digests"] else {
            return Err("digests is not a byte string".into());
        };
        let hash = Multicodec(integer("hash")?);
        let digest_len = digest_len(hash).map_err(|error| error.to_string())?;
        Self::new(
            hash,
            digest_len,
            integer("chunk_size")?,
            integer("size")?,
            digests.clone(),
        )
    }

    /// The manifest's DRISL encoding as a fact, under its BLAKE3 CID.
    ///
    /// That CID stands for the content: whoever trusts it can check the
    /// manifest, and with the manifest every range.
    pub fn fact(&self) -> Result<Fact, Error> {
        let encoding = (self.to_value().encode()).map_err(|error| reason(error.to_string()))?;
        Fact::<Blake3>::of(encoding.into())
            .widen()
            .with_codec(Multicodec::DAG_CBOR)
            .map_err(|error| reason(error.to_string()))
    }

    /// Store the manifest's DRISL encoding in `cas`, and return the store's
    /// CID for it.
    ///
    /// - A store that keys by BLAKE3 gives a CID with the digest of
    ///   [`Manifest::fact`]'s.
    /// - Fails with [`Error::Store`] if the store fails.
    pub fn put(&self, cas: &dyn Cas) -> Result<Cid, Error> {
        cas.put_fact(self.fact()?).map_err(crate::store)
    }

    /// The manifest stored in `cas` under `cid`, checked against `cid`.
    ///
    /// - The store is not trusted: the bytes are used only if they hash to
    ///   `cid`, which is checked however the store answers. They are decoded
    ///   as DRISL whatever the codec of `cid`.
    /// - Fails with [`Error::Missing`] if the store has nothing for `cid`,
    ///   with [`Error::Corrupt`] if its bytes do not hash to `cid`, with
    ///   [`Error::Store`] if it fails, and with [`Error::Manifest`] if the
    ///   bytes are not a manifest.
    /// - The manifest is read into memory whole, whatever its size.
    pub fn load(cas: &dyn Cas, cid: &Cid) -> Result<Self, Error> {
        let fact = cas
            .get_fact(cid)
            .map_err(crate::store)?
            .context(MissingSnafu { cid: *cid })?;
        let value = Value::decode(fact.bytes()).map_err(|error| reason(error.to_string()))?;
        Self::from_value(&value)
    }
}

impl RangeVerifier for Manifest {
    fn size(&self) -> u64 {
        self.layout.size()
    }

    fn chunk_size(&self) -> u64 {
        self.layout.block_size()
    }

    fn verify(&self, chunk: u64, bytes: &[u8]) -> Result<(), BoxError> {
        let mut rest = bytes;
        let mut chunk = chunk;
        while !rest.is_empty() {
            let len = self.layout.block_len(chunk);
            let expected = self.digest(chunk).filter(|_| len <= rest.len() as u64);
            let expected = expected.context(CutSnafu {
                chunk,
                len: rest.len(),
            })?;
            let (head, tail) = rest.split_at(len as usize);
            let actual = hash::digest(self.hash, head);
            ensure!(actual.as_deref() == Some(expected), DigestSnafu { chunk });
            rest = tail;
            chunk += 1;
        }
        Ok(())
    }
}

/// Why a manifest refused bytes.
#[derive(Debug, Snafu)]
#[snafu(crate_root(covalence_lib_error::snafu))]
enum Refused {
    #[snafu(display("chunk {chunk} does not hash to its digest in the manifest"))]
    Digest { chunk: u64 },
    #[snafu(display("{len} bytes from chunk {chunk} on are not whole chunks of the content"))]
    Cut { chunk: u64, len: usize },
    #[snafu(display("the content has more bytes than can be counted"))]
    TooLarge,
}

impl From<Refused> for Error {
    fn from(refused: Refused) -> Self {
        reason(refused.to_string())
    }
}

/// An [`Error::Manifest`] for `reason`.
fn reason(reason: String) -> Error {
    ManifestSnafu { reason }.build()
}

/// The length of the digests of `hash`.
fn digest_len(hash: Multicodec) -> Result<usize, Error> {
    let digest = hash::digest(hash, &[]).context(UnsupportedSnafu {
        what: format!("the hash function {hash:?}"),
    })?;
    Ok(digest.len())
}

/// Whether blocks or chunks can have `size` bytes.
pub(crate) fn check_chunk_size(size: u64) -> Result<(), String> {
    if (1..=MAX_BLOCK_SIZE).contains(&size) {
        Ok(())
    } else {
        Err(format!(
            "a block or chunk size of {size} bytes is not between 1 and {MAX_BLOCK_SIZE}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use covalence_cas_memory::MemoryCas;
    use covalence_cas_trait::Put;

    use super::*;

    const HASHES: [Multicodec; 4] = [
        Multicodec::BLAKE3,
        Multicodec::SHA1,
        Multicodec::SHA2_256,
        Multicodec::SHA2_512,
    ];

    fn content(len: usize) -> Vec<u8> {
        (0..len).map(|index| (index * 7 % 253) as u8).collect()
    }

    /// A reader that gives its bytes a few at a time, with interruptions.
    struct Trickle<'a>(&'a [u8], usize);

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.1 += 1;
            if self.1.is_multiple_of(3) {
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            let take = self.0.len().min(buf.len()).min(self.1 % 7 + 1);
            buf[..take].copy_from_slice(&self.0[..take]);
            self.0 = &self.0[take..];
            Ok(take)
        }
    }

    #[test]
    fn digests_are_of_each_chunk_for_every_hash() {
        let content = content(10_000);
        for hash in HASHES {
            let manifest = Manifest::of(hash, 4096, &content).unwrap();
            assert_eq!((manifest.hash(), manifest.size()), (hash, 10_000));
            assert_eq!((manifest.chunk_size(), manifest.chunks()), (4096, 3));
            for (index, chunk) in content.chunks(4096).enumerate() {
                let digest = hash.digest(chunk).unwrap();
                assert_eq!(manifest.digest(index as u64), Some(&digest[..]));
            }
            assert_eq!(manifest.digest(3), None);
            assert_eq!(manifest.digest(u64::MAX), None);
            // However the reader cuts the bytes, the manifest is the same.
            let trickled = Manifest::read(hash, 4096, Trickle(&content, 0)).unwrap();
            assert_eq!(trickled, manifest);
        }
    }

    #[test]
    fn sizes_at_chunk_boundaries_have_the_right_number_of_chunks() {
        for (len, chunks) in [(0, 0), (1, 1), (4095, 1), (4096, 1), (4097, 2), (8192, 2)] {
            let manifest = Manifest::blake3(4096, &content(len)).unwrap();
            assert_eq!((manifest.size(), manifest.chunks()), (len as u64, chunks));
            manifest.verify(0, &content(len)).unwrap();
        }
        // One byte to a chunk, and one chunk for everything.
        assert_eq!(Manifest::blake3(1, &content(50)).unwrap().chunks(), 50);
        let whole = Manifest::blake3(MAX_BLOCK_SIZE, &content(50)).unwrap();
        assert_eq!(
            whole.digest(0),
            Some(&blake3::hash(&content(50)).as_bytes()[..])
        );
    }

    #[test]
    fn bad_hashes_chunk_sizes_and_readers_are_refused() {
        let unsupported = Manifest::of(Multicodec(0xb220), 4096, b"x").unwrap_err();
        assert!(
            matches!(unsupported, Error::Unsupported { .. }),
            "{unsupported}"
        );
        let raw = Manifest::of(Multicodec::RAW, 4096, b"").unwrap_err();
        assert!(matches!(raw, Error::Unsupported { .. }));
        for size in [0, MAX_BLOCK_SIZE + 1, u64::MAX] {
            let error = Manifest::blake3(size, b"x").unwrap_err();
            assert!(matches!(error, Error::Options { .. }), "{error}");
        }
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }
        let error = Manifest::read(Multicodec::BLAKE3, 4096, Broken).unwrap_err();
        assert!(matches!(error, Error::Io { .. }), "{error}");
    }

    #[test]
    fn only_whole_chunks_of_the_content_verify() {
        let content = content(10_000);
        for hash in HASHES {
            let manifest = Manifest::of(hash, 4096, &content).unwrap();
            manifest.verify(0, &content).unwrap();
            manifest.verify(0, &content[..4096]).unwrap();
            manifest.verify(1, &content[4096..]).unwrap();
            manifest.verify(2, &content[8192..]).unwrap();
            manifest.verify(1, &[]).unwrap();
            manifest.verify(3, &[]).unwrap();

            let refused =
                |chunk, bytes: &[u8]| manifest.verify(chunk, bytes).unwrap_err().to_string();
            // Another chunk's bytes, and a flipped bit in each chunk.
            assert!(refused(0, &content[4096..8192]).contains("chunk 0 does not hash"));
            for flip in [0, 4095, 4096, 8191, 8192, 9_999] {
                let mut bytes = content.clone();
                bytes[flip] ^= 1;
                let chunk = flip / 4096;
                assert!(refused(0, &bytes).contains(&format!("chunk {chunk} does not hash")));
            }
            // Bytes that are not whole chunks: cut short, too long, or past the end.
            assert!(refused(0, &content[..4095]).contains("not whole chunks"));
            assert!(refused(0, &content[..4097]).contains("not whole chunks"));
            assert!(refused(2, &content[8192..9_999]).contains("not whole chunks"));
            let mut long = content.clone();
            long.push(0);
            assert!(refused(0, &long).contains("not whole chunks"));
            assert!(refused(3, &[0]).contains("not whole chunks"));
            assert!(refused(u64::MAX, &[0]).contains("not whole chunks"));
        }
    }

    #[test]
    fn values_round_trip_and_malformed_values_are_refused() {
        let manifest = Manifest::of(Multicodec::SHA2_256, 100, &content(250)).unwrap();
        let value = manifest.to_value();
        assert_eq!(Manifest::from_value(&value).unwrap(), manifest);
        let Value::Map(map) = &value else {
            panic!("a manifest is a map")
        };
        assert_eq!(map["chunk_size"], Value::Integer(100));
        assert_eq!(map["hash"], Value::Integer(0x12));
        assert_eq!(map["size"], Value::Integer(250));
        assert!(matches!(&map["digests"], Value::Bytes(bytes) if bytes.len() == 96));

        let change = |key: &str, value: Option<Value>| {
            let mut map = map.clone();
            match value {
                Some(value) => map.insert(key.to_owned(), value),
                None => map.remove(key),
            };
            let error = Manifest::from_value(&Value::Map(map)).unwrap_err();
            assert!(matches!(error, Error::Manifest { .. }), "{error}");
            error.to_string()
        };
        assert!(change("size", None).contains("keys"));
        assert!(change("extra", Some(Value::Null)).contains("keys"));
        assert!(change("size", Some(Value::Integer(-1))).contains("negative"));
        assert!(change("size", Some(Value::String("250".into()))).contains("not an integer"));
        assert!(change("digests", Some(Value::Null)).contains("not a byte string"));
        // The digests must be one for each chunk.
        assert!(change("size", Some(Value::Integer(301))).contains("digests"));
        assert!(change("size", Some(Value::Integer(200))).contains("digests"));
        assert!(change("chunk_size", Some(Value::Integer(50))).contains("digests"));
        assert!(change("digests", Some(Value::Bytes(vec![0; 95].into()))).contains("digests"));
        assert!(change("hash", Some(Value::Integer(0x13))).contains("digests"));
        assert!(change("chunk_size", Some(Value::Integer(0))).contains("size of 0"));
        assert!(change("hash", Some(Value::Integer(0x55))).contains("hash function"));
        let error = Manifest::from_value(&Value::Null).unwrap_err();
        assert!(error.to_string().contains("not a map"));

        // Sizes whose chunk count or digest length would overflow are refused.
        let huge = Value::Map(BTreeMap::from([
            ("chunk_size".into(), Value::Integer(1)),
            ("digests".into(), Value::Bytes(Bytes::new())),
            ("hash".into(), Value::Integer(0x1e)),
            ("size".into(), Value::Integer(i64::MAX)),
        ]));
        assert!(Manifest::from_value(&huge).is_err());
    }

    #[test]
    fn manifests_are_stored_and_loaded_by_cid() {
        let cas = MemoryCas::default();
        let manifest = Manifest::blake3(64, &content(1_000)).unwrap();
        let cid = manifest.put(&cas).unwrap();
        // The manifest's own CID says DRISL; the store's has the same digest.
        let own = *manifest.fact().unwrap().claim();
        assert_eq!(own.codec(), Multicodec::DAG_CBOR);
        assert_eq!(
            (own.hash(), own.digest()),
            (Multicodec::BLAKE3, cid.digest())
        );
        assert_eq!(Manifest::load(&cas, &cid).unwrap(), manifest);
        assert_eq!(Manifest::load(&cas, &own).unwrap(), manifest);
        // The same content gives the same CID; other content another.
        assert_eq!(
            Manifest::blake3(64, &content(1_000))
                .unwrap()
                .put(&cas)
                .unwrap(),
            cid
        );
        assert_ne!(
            Manifest::blake3(64, &content(1_001))
                .unwrap()
                .put(&cas)
                .unwrap(),
            cid
        );

        let missing = Manifest::load(&MemoryCas::default(), &cid).unwrap_err();
        assert!(matches!(missing, Error::Missing { cid: asked } if asked == cid));
        let other: Cid = cas.put(Bytes::from_static(b"not a manifest")).unwrap();
        let error = Manifest::load(&cas, &other).unwrap_err();
        assert!(matches!(error, Error::Manifest { .. }), "{error}");
    }
}
