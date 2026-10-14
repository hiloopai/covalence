use std::fmt;

use blake3::hazmat::HasherExt;
use data_encoding::HEXLOWER;

/// A 256-bit identifier: a BLAKE3 hash or a random GUID. It has no CID form.
///
/// It is also the key of keyed BLAKE3. It shows as lowercase hex.
///
/// ```
/// use covalence_lib_cid::{Blake3, CtxKey, O256};
///
/// let hash = O256::from_bytes(b"hello");
/// assert_eq!(hash, Blake3::of(b"hello").into());
/// assert_eq!(hash.to_string().len(), 64);
/// let keyed = O256::with_key(&hash, b"hello");
/// let derived = O256::with_ctx(&CtxKey::derive("example.com 2026 doc test"), b"hello");
/// assert_ne!(keyed, derived);
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct O256([u8; 32]);

/// A BLAKE3 key derivation context, hashed once. The context string should be
/// hardcoded, globally unique and application specific.
#[derive(Clone, Copy)]
pub struct CtxKey([u8; 32]);

impl CtxKey {
    /// The context key of `context`.
    pub fn derive(context: &str) -> Self {
        CtxKey(blake3::hazmat::hash_derive_key_context(context))
    }
}

impl O256 {
    /// The identifier with these bytes.
    pub const fn from_array(bytes: [u8; 32]) -> Self {
        O256(bytes)
    }

    /// The bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The bytes, by value.
    pub const fn into_bytes(self) -> [u8; 32] {
        self.0
    }

    /// The BLAKE3 hash of `bytes`. This hashes them; see
    /// [`O256::from_array`] to wrap bytes as they are.
    pub fn from_bytes(bytes: impl AsRef<[u8]>) -> Self {
        O256(blake3::hash(bytes.as_ref()).into())
    }

    /// The BLAKE3 hash of `bytes` keyed with `key`.
    pub fn with_key(key: &O256, bytes: impl AsRef<[u8]>) -> Self {
        O256(blake3::keyed_hash(&key.0, bytes.as_ref()).into())
    }

    /// The BLAKE3 key derived from the key material `bytes` under the context
    /// `key`.
    pub fn with_ctx(key: &CtxKey, bytes: impl AsRef<[u8]>) -> Self {
        let mut hasher = blake3::Hasher::new_from_context_key(&key.0);
        O256(hasher.update(bytes.as_ref()).finalize().into())
    }

    /// A random GUID drawn from `rng`.
    #[cfg(feature = "rand")]
    pub fn random<R>(rng: &mut R) -> Self
    where
        R: covalence_lib_rand::Rng + covalence_lib_rand::CryptoRng + ?Sized,
    {
        O256(rng.random())
    }
}

impl crate::Blake3 {
    /// The digest as an [`O256`].
    pub const fn into_o256(self) -> O256 {
        O256(self.bytes)
    }
}

impl From<crate::Blake3> for O256 {
    fn from(cid: crate::Blake3) -> Self {
        cid.into_o256()
    }
}

/// Lowercase hex.
impl fmt::Display for O256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&HEXLOWER.encode(&self.0))
    }
}

/// Lowercase hex.
impl fmt::Debug for O256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "O256({self})")
    }
}
