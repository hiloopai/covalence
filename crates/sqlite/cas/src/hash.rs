//! Digests computed as bytes arrive, for the hash functions
//! `covalence-lib-cid` computes over whole buffers.

use covalence_lib_cid::Multicodec;
use sha2::Digest;

/// A digest in progress.
pub(crate) enum Hasher {
    Blake3(Box<blake3::Hasher>),
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    Sha512(sha2::Sha512),
}

impl Hasher {
    /// A hasher for `hash`, or `None` if this crate does not compute it.
    pub(crate) fn new(hash: Multicodec) -> Option<Self> {
        Some(match hash {
            Multicodec::BLAKE3 => Hasher::Blake3(Box::default()),
            Multicodec::SHA1 => Hasher::Sha1(sha1::Sha1::new()),
            Multicodec::SHA2_256 => Hasher::Sha256(sha2::Sha256::new()),
            Multicodec::SHA2_512 => Hasher::Sha512(sha2::Sha512::new()),
            _ => return None,
        })
    }

    /// Add `bytes` to the input.
    pub(crate) fn update(&mut self, bytes: &[u8]) {
        match self {
            Hasher::Blake3(hasher) => {
                hasher.update(bytes);
            }
            Hasher::Sha1(hasher) => hasher.update(bytes),
            Hasher::Sha256(hasher) => hasher.update(bytes),
            Hasher::Sha512(hasher) => hasher.update(bytes),
        }
    }

    /// The digest of the input.
    pub(crate) fn finish(self) -> Vec<u8> {
        match self {
            Hasher::Blake3(hasher) => hasher.finalize().as_bytes().to_vec(),
            Hasher::Sha1(hasher) => hasher.finalize().to_vec(),
            Hasher::Sha256(hasher) => hasher.finalize().to_vec(),
            Hasher::Sha512(hasher) => hasher.finalize().to_vec(),
        }
    }
}

/// The digest of `bytes` and its length for `hash`, or `None` if this crate
/// does not compute it.
pub(crate) fn digest(hash: Multicodec, bytes: &[u8]) -> Option<Vec<u8>> {
    let mut hasher = Hasher::new(hash)?;
    hasher.update(bytes);
    Some(hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_match_the_cid_crate_however_the_input_is_cut() {
        let input: Vec<u8> = (0..5_000u32).map(|byte| (byte * 31 % 251) as u8).collect();
        let hashes = [
            Multicodec::BLAKE3,
            Multicodec::SHA1,
            Multicodec::SHA2_256,
            Multicodec::SHA2_512,
        ];
        for hash in hashes {
            for cut in [0, 1, 63, 64, 1024, 4_999, 5_000] {
                let mut hasher = Hasher::new(hash).unwrap();
                hasher.update(&input[..cut]);
                hasher.update(&[]);
                hasher.update(&input[cut..]);
                assert_eq!(Some(hasher.finish()), hash.digest(&input));
            }
            assert_eq!(digest(hash, b""), hash.digest(b""));
        }
    }

    #[test]
    fn hashes_the_cid_crate_does_not_compute_have_no_hasher() {
        // Every code is asked both crates: they compute the same four.
        for code in 0..0x200 {
            let hash = Multicodec(code);
            assert_eq!(
                Hasher::new(hash).is_some(),
                hash.digest(b"").is_some(),
                "{hash:?}"
            );
        }
        assert!(Hasher::new(Multicodec(0xb220)).is_none());
    }
}
