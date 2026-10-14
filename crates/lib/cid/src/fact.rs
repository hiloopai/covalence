use covalence_lib_data::Bytes;
use covalence_lib_error::snafu::ensure;

use crate::{Cid, CidFormat, Error, Format, Hashed, MismatchSnafu, Multicodec};

/// A claim about bytes, such as a CID, that [`Fact::new`] checks.
pub trait Claim {
    /// Why the claim does not hold.
    type Error;

    /// Check that the claim holds of `bytes`.
    fn check(&self, bytes: &[u8]) -> Result<(), Self::Error>;
}

/// The bytes hash to the CID's digest. The codec is not checked.
///
/// - [`Error::Mismatch`] if they do not.
/// - [`Error::Hash`] if this crate cannot compute the digest.
impl<S: CidFormat> Claim for Cid<S> {
    type Error = Error;

    fn check(&self, bytes: &[u8]) -> Result<(), Error> {
        let (hash, digest) = (self.hash(), self.digest().as_ref());
        let len = digest.len();
        let actual = hash.digest(bytes).filter(|actual| actual.len() == len);
        ensure!(
            actual.ok_or(Error::Hash { hash, len })? == digest,
            MismatchSnafu
        );
        Ok(())
    }
}

/// Bytes and a claim that holds of them.
///
/// Only [`Fact::new`], which checks the claim, and [`Fact::of`], which hashes
/// the bytes, build a fact. Other methods relabel one without hashing again,
/// keeping its hash function, digest and bytes.
///
/// ```
/// use covalence_lib_cid::{Blake3, Cid32, Error, Fact, Multicodec};
/// use covalence_lib_data::Bytes;
///
/// let hello = Bytes::from_static(b"hello");
/// let fact = Fact::<Blake3>::of(hello.clone());
/// assert_eq!(Fact::new(Blake3::of(b"hello"), hello.clone())?, fact);
/// assert!(matches!(Fact::new(Blake3::of(b"other"), hello), Err(Error::Mismatch)));
///
/// let drisl = fact.widen::<covalence_lib_cid::Packed32>().with_codec(Multicodec::DAG_CBOR)?;
/// assert_eq!(drisl.claim().codec(), Multicodec::DAG_CBOR);
/// assert_eq!(drisl.bytes(), "hello");
/// # Ok::<(), Error>(())
/// ```
///
/// The fields are private:
///
/// ```compile_fail
/// use covalence_lib_cid::{Cid32, Fact};
/// let fact = Fact { claim: "bafkqaaa".parse::<Cid32>().unwrap(), bytes: "x".into() };
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fact<C: Claim = Cid> {
    claim: C,
    bytes: Bytes,
}

impl<C: Claim> Fact<C> {
    /// Check that `claim` holds of `bytes`.
    ///
    /// The fact keeps `bytes`, so they must not change afterwards: copy
    /// storage that may, such as a memory map, first.
    pub fn new(claim: C, bytes: Bytes) -> Result<Self, C::Error> {
        claim.check(&bytes)?;
        Ok(Fact { claim, bytes })
    }

    /// The claim.
    pub fn claim(&self) -> &C {
        &self.claim
    }

    /// The bytes.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
}

impl<S: Hashed> Fact<Cid<S>> {
    /// `bytes` and their CID in the one format of `S`, hashing once.
    pub fn of(bytes: Bytes) -> Self {
        let digest = S::digest_of(&bytes);
        let claim = Cid::new(S::FORMAT, digest.as_ref()).expect("`S` holds its CIDs");
        Fact { claim, bytes }
    }
}

/// A fact about a CID depends only on its hash function and digest, so it
/// holds in any storage that holds the CID, and for any codec.
impl<S: CidFormat> Fact<Cid<S>> {
    /// The same fact in storage `T`, or [`Error::Unsupported`] if `T` does
    /// not hold the CID.
    pub fn cast<T: CidFormat>(self) -> Result<Fact<Cid<T>>, Error> {
        let claim = self.claim.cast()?;
        Ok(Fact {
            claim,
            bytes: self.bytes,
        })
    }

    /// The same fact in storage `T`, which holds every CID of `S`.
    pub fn widen<T: CidFormat>(self) -> Fact<Cid<T>>
    where
        Cid<T>: From<Cid<S>>,
    {
        Fact {
            claim: self.claim.into(),
            bytes: self.bytes,
        }
    }

    /// The same bytes under the raw BLAKE3 CID, if the CID has a 32-byte
    /// BLAKE3 digest.
    pub fn raw_blake3(&self) -> Option<Fact<crate::Blake3>> {
        Some(Fact {
            claim: self.claim.raw_blake3()?,
            bytes: self.bytes.clone(),
        })
    }

    /// The same bytes as a fact for the CID with codec `codec`.
    ///
    /// - No rehashing: a fact proves only the hash function and digest.
    /// - The bytes are not checked against `codec`.
    /// - [`Error::Unsupported`] if `S` does not hold the new CID.
    pub fn with_codec(self, codec: Multicodec) -> Result<Self, Error> {
        let format = Format {
            codec,
            hash: self.claim.hash(),
        };
        let claim = Cid::new(format, self.claim.digest().as_ref())?;
        Ok(Fact {
            claim,
            bytes: self.bytes,
        })
    }
}
