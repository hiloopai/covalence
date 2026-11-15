//! Lake's name for an artifact: the hash of its bytes, `Hash.ofByteArray`
//! (lean4 `Lake/Build/Trace.lean`), as 16 hex digits before the extension.
//!
//! Lake trusts a cached artifact by its name alone, so a file whose bytes are
//! not its name's makes every host that restores it build against other
//! outputs than the ones its trace vouches for. Everything this tool writes
//! into Lake's artifact folder, and everything it publishes from it, is
//! checked against its name here.

use std::{
    fs::File,
    io::{self, Read},
    path::Path,
};

use crate::error::{IoResult, Result, invalid};

const M: u64 = 0xc6a4a793_5bd1e995;
const R: u32 = 47;
/// `Hash.nil`.
const NIL: u64 = 1723;
/// The seed of `lean_byte_array_hash`.
const SEED: u64 = 11;

/// MurmurHash64A of a stream whose length is known before its first byte,
/// as Lean's `hash_str` computes it.
pub(crate) struct Hasher {
    h: u64,
    pending: [u8; 8],
    filled: usize,
    left: u64,
}

impl Hasher {
    pub fn new(len: u64) -> Self {
        Self {
            h: SEED ^ len.wrapping_mul(M),
            pending: [0; 8],
            filled: 0,
            left: len,
        }
    }

    pub fn update(&mut self, mut bytes: &[u8]) {
        self.left = self.left.wrapping_sub(bytes.len() as u64);
        if self.filled > 0 {
            let take = (8 - self.filled).min(bytes.len());
            self.pending[self.filled..self.filled + take].copy_from_slice(&bytes[..take]);
            self.filled += take;
            bytes = &bytes[take..];
            if self.filled < 8 {
                return;
            }
            self.block(u64::from_le_bytes(self.pending));
            self.filled = 0;
        }
        let (blocks, tail) = bytes.as_chunks::<8>();
        for block in blocks {
            self.block(u64::from_le_bytes(*block));
        }
        self.pending[..tail.len()].copy_from_slice(tail);
        self.filled = tail.len();
    }

    fn block(&mut self, mut k: u64) {
        k = k.wrapping_mul(M);
        k ^= k >> R;
        k = k.wrapping_mul(M);
        self.h ^= k;
        self.h = self.h.wrapping_mul(M);
    }

    /// The name's hash; `None` if fewer or more bytes came than announced.
    pub fn finish(mut self) -> Option<u64> {
        if self.left != 0 {
            return None;
        }
        if self.filled > 0 {
            let mut tail = [0; 8];
            tail[..self.filled].copy_from_slice(&self.pending[..self.filled]);
            self.h ^= u64::from_le_bytes(tail);
            self.h = self.h.wrapping_mul(M);
        }
        let mut h = self.h;
        h ^= h >> R;
        h = h.wrapping_mul(M);
        h ^= h >> R;
        Some(mix(NIL, h))
    }
}

/// `lean_uint64_mix_hash`.
fn mix(mut h: u64, mut k: u64) -> u64 {
    k = k.wrapping_mul(M);
    k ^= k >> R;
    k ^= M;
    h ^= k;
    h.wrapping_mul(M)
}

/// The hash Lake names `bytes` by.
pub(crate) fn of(bytes: &[u8]) -> u64 {
    let mut hasher = Hasher::new(bytes.len() as u64);
    hasher.update(bytes);
    hasher.finish().unwrap_or_default()
}

/// The hash artifact `name` claims.
fn claimed(name: &str) -> Result<u64> {
    name.get(..16)
        .filter(|_| crate::mapping::artifact_name(name))
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())
        .ok_or_else(|| invalid(format!("{name} is no Lake artifact name")))
}

/// Fails unless `hash` is the one `name` claims.
pub(crate) fn expect(name: &str, hash: Option<u64>) -> Result<()> {
    let claimed = claimed(name)?;
    match hash {
        Some(hash) if hash == claimed => Ok(()),
        Some(hash) => Err(invalid(format!(
            "artifact {name} holds other bytes (Lake hash {hash:016x})"
        ))),
        None => Err(invalid(format!(
            "artifact {name} changed while it was read"
        ))),
    }
}

/// Fails unless `bytes` are what `name` claims.
pub(crate) fn check(name: &str, bytes: &[u8]) -> Result<()> {
    expect(name, Some(of(bytes)))
}

/// Fails unless the file at `path` holds what `name` claims.
pub(crate) fn check_file(name: &str, path: &Path) -> Result<()> {
    let mut file = File::open(path).operation(format!("open {}", path.display()))?;
    let len = file
        .metadata()
        .operation(format!("read {}", path.display()))?
        .len();
    let mut reader = Checking::new(&mut file, len);
    io::copy(&mut reader, &mut io::sink()).operation(format!("read {}", path.display()))?;
    expect(name, reader.finish())
}

/// A reader that hashes what passes through it, as Lake names it.
pub(crate) struct Checking<R> {
    inner: R,
    hasher: Hasher,
}

impl<R: Read> Checking<R> {
    pub fn new(inner: R, len: u64) -> Self {
        Self {
            inner,
            hasher: Hasher::new(len),
        }
    }

    pub fn finish(self) -> Option<u64> {
        self.hasher.finish()
    }
}

impl<R: Read> Read for Checking<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.hasher.update(&buffer[..read]);
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_lakes_hashes() {
        // From a reference implementation that reproduced the names of real
        // Lake 11-05 module artifacts (oleans, ileans, IR, C).
        let pattern: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
        for (bytes, hash) in [
            (b"".as_slice(), 0x21c4_2906_adea_99f5),
            (b"a", 0x6ccb_214a_7963_50f7),
            (b"module", 0x4742_0777_87b5_0d77),
            (b"0123456789abcdef", 0xa95a_f190_ab42_8e35),
            (&pattern, 0xf7cb_f638_7200_55be),
        ] {
            assert_eq!(of(bytes), hash, "{} bytes", bytes.len());
        }
    }

    #[test]
    fn a_lake_archive_and_its_files_hash_to_their_names() {
        // A module of plausible (Apache-2.0, leanprover-community/plausible
        // 1cfc4bdd) that Lake nightly-2026-11-05 built and packed, as the pool
        // holds it.
        let name = "cd9cef913c0321f4.ltar";
        let bytes = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(name),
        )
        .unwrap();
        check(name, &bytes).unwrap();
        let archive = covalence_lean_ltar::unpack(&bytes, 0).unwrap();
        let trace: serde_json::Value = serde_json::from_slice(&archive.trace.bytes).unwrap();
        let stem = archive.trace.path.strip_suffix(".trace").unwrap();
        let names = crate::mapping::names(&trace["outputs"]).unwrap();
        assert!(names.len() > 4, "{names:?}");
        for name in names {
            let extension = name
                .split_once('.')
                .map(|(_, extension)| extension)
                .unwrap();
            let file = archive
                .files
                .iter()
                .find(|file| file.path == format!("{stem}.{extension}"))
                .unwrap();
            check(&name, &file.bytes).unwrap();
        }
    }

    #[test]
    fn a_stream_in_any_pieces_hashes_as_one_read() {
        let bytes: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
        for split in [0, 1, 7, 8, 9, 15, 500, 999, 1000] {
            let mut hasher = Hasher::new(bytes.len() as u64);
            hasher.update(&bytes[..split]);
            for piece in bytes[split..].chunks(3) {
                hasher.update(piece);
            }
            assert_eq!(hasher.finish(), Some(of(&bytes)), "split {split}");
        }
        let mut short = Hasher::new(10);
        short.update(b"123");
        assert_eq!(short.finish(), None);
    }

    #[test]
    fn other_bytes_under_a_name_fail() {
        let name = format!("{:016x}.olean", of(b"module"));
        check(&name, b"module").unwrap();
        assert!(check(&name, b"other").is_err());
        assert!(check("not-a-name", b"module").is_err());
    }
}
