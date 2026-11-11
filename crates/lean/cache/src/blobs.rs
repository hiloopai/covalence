//! Lake artifacts in the pool, under SHA-256 CIDs of their bytes.
//!
//! `KvCas` keys blocks by BLAKE3 only. Artifacts use SHA-256, the hash that
//! receipts and Lake's planned content hashes use, so this keeps the same
//! representation under `sha2-256/` instead: a Zstandard frame of the bytes at
//! `<prefix>sha2-256/<digest>/bytes.zst`, the digest in lowercase unpadded
//! base32. Retire this when `KvCas` takes a hash function.

use std::{
    fs::File,
    io::{self, Seek, Write},
    path::Path,
    sync::Arc,
};

use covalence_kv_trait::Kv;
use covalence_lib_cid::{Format, Sha256};
use data_encoding::BASE32_NOPAD;
use sha2::Digest;

use crate::error::{Error, IoResult, Result, invalid};

pub(crate) struct Blobs {
    kv: Arc<dyn Kv>,
    prefix: String,
}

/// A writer that hashes everything it passes on.
pub(crate) struct Hashing<W> {
    inner: W,
    hash: sha2::Sha256,
    bytes: u64,
}

impl<W> Hashing<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            hash: sha2::Sha256::new(),
            bytes: 0,
        }
    }

    /// The writer, the SHA-256 of what passed through, and its length.
    pub fn finish(self) -> (W, [u8; 32], u64) {
        (self.inner, self.hash.finalize().into(), self.bytes)
    }
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(bytes)?;
        self.hash.update(&bytes[..written]);
        self.bytes += written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn store_error(operation: String) -> impl FnOnce(covalence_kv_trait::Error) -> Error {
    move |source| Error::Store { operation, source }
}

/// The SHA-256 CID of a digest.
pub(crate) fn cid(digest: &[u8]) -> Result<Sha256> {
    Sha256::new(Format::SHA2_256, digest).map_err(|source| Error::Cid {
        operation: "address Lake artifact".into(),
        source,
    })
}

/// The SHA-256 CID of a file's bytes, and its length.
pub(crate) fn hash_file(path: &Path) -> Result<(Sha256, u64)> {
    let mut file = File::open(path).operation(format!("open {}", path.display()))?;
    let mut hashing = Hashing {
        inner: io::sink(),
        hash: sha2::Sha256::new(),
        bytes: 0,
    };
    io::copy(&mut file, &mut hashing).operation("hash Lake artifact")?;
    Ok((cid(&hashing.hash.finalize())?, hashing.bytes))
}

impl Blobs {
    /// The artifacts of the pool scope whose CAS lies under `prefix`.
    pub fn new(kv: Arc<dyn Kv>, prefix: &str) -> Self {
        Self {
            kv,
            prefix: format!("{prefix}/sha2-256/"),
        }
    }

    fn key(&self, cid: &Sha256) -> String {
        let digest = BASE32_NOPAD.encode(cid.digest()).to_ascii_lowercase();
        format!("{}{digest}/bytes.zst", self.prefix)
    }

    pub fn has(&self, cid: &Sha256) -> Result<bool> {
        self.kv
            .exists(self.key(cid).as_bytes())
            .map_err(store_error(format!("inspect artifact {cid}")))
    }

    /// Store the file at `path`, which must hash to `cid`; existing blocks
    /// are kept.
    pub fn put_file(&self, cid: &Sha256, path: &Path) -> Result<()> {
        let mut input = File::open(path).operation(format!("open {}", path.display()))?;
        let mut staged = tempfile::tempfile().operation("stage compressed artifact")?;
        let encoder =
            zstd::stream::write::Encoder::new(&mut staged, 1).operation("start Zstandard")?;
        let mut hashing = Hashing {
            inner: encoder,
            hash: sha2::Sha256::new(),
            bytes: 0,
        };
        io::copy(&mut input, &mut hashing).operation("compress Lake artifact")?;
        if cid != &self::cid(&hashing.hash.finalize())? {
            return Err(invalid(format!(
                "{} does not hash to {cid}",
                path.display()
            )));
        }
        hashing
            .inner
            .finish()
            .operation("finish compressed artifact")?;
        let len = staged
            .stream_position()
            .operation("measure compressed artifact")?;
        staged.rewind().operation("rewind compressed artifact")?;
        self.kv
            .put_new_from(self.key(cid).as_bytes(), &mut staged, len)
            .map_err(store_error(format!("upload {cid}")))?;
        Ok(())
    }

    /// Store the Zstandard stream at `path` byte for byte, under the CID of
    /// the bytes it decodes to, and return that CID and the stream's length.
    /// Existing blocks are kept.
    pub fn put_frame(&self, path: &Path) -> Result<(Sha256, u64)> {
        let mut input = File::open(path).operation(format!("open {}", path.display()))?;
        let mut decoder =
            zstd::stream::read::Decoder::new(&mut input).operation("start Zstandard")?;
        let mut hashing = Hashing::new(io::sink());
        io::copy(&mut decoder, &mut hashing).operation("decode Zstandard stream")?;
        let (_, digest, _) = hashing.finish();
        let cid = self::cid(&digest)?;
        let len = input
            .metadata()
            .operation("measure Zstandard stream")?
            .len();
        input.rewind().operation("rewind Zstandard stream")?;
        self.kv
            .put_new_from(self.key(&cid).as_bytes(), &mut input, len)
            .map_err(store_error(format!("upload {cid}")))?;
        Ok((cid, len))
    }

    /// Write the bytes of `cid` to `writer` and return their length, or
    /// `None` if missing. Fails if they do not hash to `cid`; after a failure
    /// `writer` holds unchecked bytes, so discard them.
    pub fn get_to(&self, cid: &Sha256, writer: &mut dyn Write) -> Result<Option<u64>> {
        decode_checked(cid, writer, |stored| {
            self.kv
                .get_to(self.key(cid).as_bytes(), stored)
                .map_err(store_error(format!("download {cid}")))
        })
    }

    /// As [`Blobs::get_to`], also writing the stored Zstandard bytes to
    /// `kept`, unchecked until this returns.
    pub fn get_keeping(
        &self,
        cid: &Sha256,
        writer: &mut dyn Write,
        kept: &mut dyn Write,
    ) -> Result<Option<u64>> {
        decode_checked(cid, writer, |stored| {
            self.kv
                .get_to(self.key(cid).as_bytes(), &mut Tee(kept, stored))
                .map_err(store_error(format!("download {cid}")))
        })
    }
}

/// A writer that passes everything to both of its writers.
struct Tee<'a>(&'a mut dyn Write, &'a mut dyn Write);

impl Write for Tee<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write_all(bytes)?;
        self.1.write_all(bytes)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()?;
        self.1.flush()
    }
}

/// Write what the Zstandard stream `feed` writes decodes to into `writer`,
/// and return its length, or `None` if `feed` finds no stream. Fails unless
/// it hashes to `cid`; after a failure `writer` holds unchecked bytes.
pub(crate) fn decode_checked(
    cid: &Sha256,
    writer: &mut dyn Write,
    feed: impl FnOnce(&mut dyn Write) -> Result<Option<u64>>,
) -> Result<Option<u64>> {
    let decoder = zstd::stream::raw::Decoder::new().operation("start artifact decoder")?;
    let hashing = Hashing::new(writer);
    let mut decoding = zstd::stream::zio::Writer::new(hashing, decoder);
    if feed(&mut decoding)?.is_none() {
        return Ok(None);
    }
    decoding
        .finish()
        .operation("finish artifact decompression")?;
    let (_, digest, bytes) = decoding.into_inner().0.finish();
    if cid != &self::cid(&digest)? {
        return Err(invalid(format!("artifact {cid} failed its hash check")));
    }
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use covalence_kv_memory::MemoryKv;

    #[test]
    fn artifacts_round_trip_and_corruption_fails_the_check() {
        let kv = MemoryKv::default().into_dyn();
        let blobs = Blobs::new(kv.clone(), "lake/linux-x86_64/t/cas");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0000000000000001.ltar");
        let bytes = vec![b'x'; 100_000];
        std::fs::write(&path, &bytes).unwrap();
        let (cid, len) = hash_file(&path).unwrap();
        assert_eq!(cid, Sha256::of(&bytes));
        assert_eq!(len, bytes.len() as u64);
        assert!(!blobs.has(&cid).unwrap());
        assert!(blobs.put_file(&Sha256::of(b"other"), &path).is_err());
        blobs.put_file(&cid, &path).unwrap();
        assert!(blobs.has(&cid).unwrap());
        let mut out = Vec::new();
        assert_eq!(blobs.get_to(&cid, &mut out).unwrap(), Some(len));
        assert_eq!(out, bytes);
        let mut corrupt = bytes.clone();
        corrupt[0] ^= 1;
        let frame = zstd::stream::encode_all(corrupt.as_slice(), 1).unwrap();
        kv.put(blobs.key(&cid).as_bytes(), frame.into()).unwrap();
        assert!(blobs.get_to(&cid, &mut Vec::new()).is_err());
        assert_eq!(
            blobs
                .get_to(&Sha256::of(b"absent"), &mut Vec::new())
                .unwrap(),
            None
        );
    }

    #[test]
    fn a_stored_frame_keeps_its_bytes_and_reads_back_decoded() {
        let kv = MemoryKv::default().into_dyn();
        let blobs = Blobs::new(kv.clone(), "lake/toolchains/cas");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("release.tar.zst");
        let bytes = b"tar bytes ".repeat(10_000);
        let frame = zstd::stream::encode_all(bytes.as_slice(), 19).unwrap();
        std::fs::write(&path, &frame).unwrap();
        let (cid, len) = blobs.put_frame(&path).unwrap();
        assert_eq!(cid, Sha256::of(&bytes));
        assert_eq!(len, frame.len() as u64);
        assert_eq!(
            &kv.get(blobs.key(&cid).as_bytes()).unwrap().unwrap()[..],
            &frame
        );
        let mut out = Vec::new();
        assert_eq!(
            blobs.get_to(&cid, &mut out).unwrap(),
            Some(bytes.len() as u64)
        );
        assert_eq!(out, bytes);
    }
}
