#![doc = include_str!("../README.md")]
#![deny(missing_docs)]
//!
//! ## Example
//!
//! ```
//! use covalence_lib_cid::{Bdasl, Cid, Cid32, Cid64, Codec, HashFn};
//!
//! let cid = Cid::<Bdasl>::of(Codec::Raw, HashFn::Sha256, b"hello");
//! assert_eq!(cid.to_string(), "bafkreibm6jg3ux5qumhcn2b3flc3tyu6dmlb4xa7u5bf44yegnrjhc4yeq");
//! assert_eq!(cid.to_string().parse::<Cid<Bdasl>>()?, cid);
//! assert_eq!(Cid::from_bytes(&cid.to_bytes())?, Cid32::from(cid));
//! assert_eq!(Cid::<Bdasl>::try_from(Cid64::from(cid))?, cid);
//! # Ok::<(), covalence_lib_cid::Error>(())
//! ```

mod dasl;
mod fact;
mod git;
mod multicodec;
mod o256;
/// Storages: how a [`Cid`] stores its bytes.
pub mod storage;

use std::{fmt, marker::PhantomData, str::FromStr};

use covalence_lib_error::snafu::{Snafu, ensure};
use data_encoding::BASE32_NOPAD_NOCASE;

pub use dasl::{Codec, HashFn};
pub use fact::{Claim, Fact};
pub use git::{Git, GitSha1, GitSha256};
pub use multicodec::{Format, Multicodec};
pub use o256::{CtxKey, O256};
pub use storage::{Bdasl, CidFormat, Dasl, Hashed, Packed32, Packed64, Storage};

/// A CIDv1, stored in the layout `S`.
///
/// `Ord` is the order of [`Cid::to_bytes`]. It is `Copy` when the stored
/// bytes are.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cid<S: Storage = Packed32> {
    bytes: S::Bytes,
    storage: PhantomData<S>,
}

/// Any CIDv1 with a digest of at most 32 bytes.
pub type Cid32 = Cid<Packed32>;
/// Any CIDv1 with a digest of at most 64 bytes.
pub type Cid64 = Cid<Packed64>;
/// A raw CID with a BLAKE3 hash.
pub type Blake3 = Cid<storage::Blake3>;
/// A raw CID with a SHA-256 hash.
pub type Sha256 = Cid<storage::Sha256>;

impl<S: Storage> Cid<S> {
    /// Stored bytes that are valid for `S`.
    fn stored(bytes: S::Bytes) -> Self {
        Cid {
            bytes,
            storage: PhantomData,
        }
    }

    /// The bytes as `S` stores them, which may not be the binary CID: see
    /// [`Cid::to_bytes`] for that.
    pub fn stored_bytes(&self) -> &S::Bytes {
        &self.bytes
    }

    /// The digest.
    pub fn digest(&self) -> &S::Digest {
        S::digest(&self.bytes)
    }
}

impl<S: CidFormat> Cid<S> {
    /// The CID with this format and digest.
    ///
    /// Fails with [`Error::Unsupported`] if `S` does not hold it, or if a code
    /// is 2^63 or more.
    ///
    /// ```
    /// use covalence_lib_cid::{Blake3, Cid32, Format};
    ///
    /// let digest = [7; 32];
    /// let cid = Cid32::new(Format::DRISL_BLAKE3, &digest)?;
    /// assert_eq!((cid.format(), cid.digest()), (Format::DRISL_BLAKE3, &digest[..]));
    /// assert!(Blake3::new(Format::DRISL_BLAKE3, &digest).is_err());
    /// # Ok::<(), covalence_lib_cid::Error>(())
    /// ```
    pub fn new(format: Format, digest: &[u8]) -> Result<Self, Error> {
        let len = digest.len();
        ensure!(
            format.codec.0 < 1 << 63 && format.hash.0 < 1 << 63 && S::holds(format, len),
            UnsupportedSnafu { format, len }
        );
        let (header, header_len) = storage::header(format, len);
        Ok(Self::stored(S::store(&header[..header_len], digest)))
    }

    /// The same CID in storage `T`, or [`Error::Unsupported`] if `T` does not
    /// hold it.
    ///
    /// ```
    /// use covalence_lib_cid::{Bdasl, Cid, Codec, Dasl, HashFn};
    ///
    /// let sha256 = Cid::<Bdasl>::of(Codec::Drisl, HashFn::Sha256, b"{}");
    /// let blake3 = Cid::<Bdasl>::of(Codec::Drisl, HashFn::Blake3, b"{}");
    /// assert_eq!(sha256.cast::<Dasl>()?.to_bytes(), sha256.to_bytes());
    /// assert!(blake3.cast::<Dasl>().is_err());
    /// # Ok::<(), covalence_lib_cid::Error>(())
    /// ```
    pub fn cast<T: CidFormat>(&self) -> Result<Cid<T>, Error> {
        Cid::new(self.format(), self.digest().as_ref())
    }

    /// Whether the two CIDs have the same multihash: the same hash function
    /// and digest, whatever their codecs.
    ///
    /// A fact checks only the multihash, so a fact for one of them proves the
    /// content of the other too.
    ///
    /// ```
    /// use covalence_lib_cid::{Blake3, Cid32, Format};
    ///
    /// let raw = Cid32::from(Blake3::of(b"hello"));
    /// let drisl = Cid32::new(Format::DRISL_BLAKE3, raw.digest())?;
    /// assert!(raw.same_multihash(&drisl));
    /// assert!(!raw.same_multihash(&Blake3::of(b"other").into()));
    /// # Ok::<(), covalence_lib_cid::Error>(())
    /// ```
    pub fn same_multihash(&self, cid: &Self) -> bool {
        self.hash() == cid.hash() && self.digest() == cid.digest()
    }

    /// The raw BLAKE3 CID with this digest, discarding the codec, or `None`
    /// unless this CID has a 32-byte BLAKE3 digest.
    ///
    /// ```
    /// use covalence_lib_cid::{Bdasl, Blake3, Cid, Codec, HashFn};
    ///
    /// let drisl = Cid::<Bdasl>::of(Codec::Drisl, HashFn::Blake3, b"x");
    /// assert_eq!(drisl.raw_blake3(), Some(Blake3::of(b"x")));
    /// assert_eq!(drisl.raw_sha256(), None);
    /// ```
    pub fn raw_blake3(&self) -> Option<Blake3> {
        self.same_digest(Format::BLAKE3)
    }

    /// The raw SHA-256 CID with this digest, discarding the codec, or `None`
    /// unless this CID has a 32-byte SHA-256 digest.
    pub fn raw_sha256(&self) -> Option<Sha256> {
        self.same_digest(Format::SHA2_256)
    }

    /// The CID of `format` with this digest, if this CID has its hash and `T`
    /// holds the digest.
    fn same_digest<T: CidFormat>(&self, format: Format) -> Option<Cid<T>> {
        (self.hash() == format.hash)
            .then(|| Cid::new(format, self.digest().as_ref()).ok())
            .flatten()
    }

    /// Parse exactly one binary CID.
    ///
    /// - [`Error::Malformed`] if the bytes are not exactly one binary CIDv1.
    /// - [`Error::Unsupported`] if `S` does not hold the CID.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let (format, digest) = storage::parse(bytes).ok_or(Error::Malformed)?;
        Self::new(format, digest)
    }

    /// The binary CID: the header (version, codec, hash function and digest
    /// length, each a varint), then the digest.
    pub fn to_bytes(&self) -> Vec<u8> {
        [S::header(&self.bytes), self.digest().as_ref()].concat()
    }

    /// The codec and hash function.
    pub fn format(&self) -> Format {
        let (format, ..) = storage::parse_header(S::header(&self.bytes)).expect("a valid header");
        format
    }

    /// The codec of the content.
    pub fn codec(&self) -> Multicodec {
        self.format().codec
    }

    /// The hash function.
    pub fn hash(&self) -> Multicodec {
        self.format().hash
    }
}

impl<S: Hashed> Cid<S> {
    /// The CID of `content`, in the one format of `S`.
    ///
    /// ```
    /// use covalence_lib_cid::{Blake3, Format, HashFn};
    ///
    /// let cid = Blake3::of(b"hello");
    /// assert_eq!(cid.format(), Format::BLAKE3);
    /// assert_eq!(*cid.digest(), HashFn::Blake3.digest(b"hello"));
    /// ```
    pub fn of(content: &[u8]) -> Self {
        Cid::new(S::FORMAT, S::digest_of(content).as_ref()).expect("`S` holds its CIDs")
    }
}

/// Parse `b` and lowercase, unpadded base32: exactly the output of `Display`.
///
/// - [`Error::Text`] for other text, including uppercase, padding or another
///   multibase prefix.
/// - The errors of [`Cid::from_bytes`] for the decoded bytes.
///
/// ```
/// use covalence_lib_cid::{Blake3, Error};
///
/// let text = Blake3::of(b"hello").to_string();
/// assert_eq!(text.parse::<Blake3>()?, Blake3::of(b"hello"));
/// assert!(matches!(text.to_uppercase().parse::<Blake3>(), Err(Error::Text)));
/// # Ok::<(), Error>(())
/// ```
impl<S: CidFormat> FromStr for Cid<S> {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Error> {
        let body = text.strip_prefix('b').ok_or(Error::Text)?;
        let bytes = BASE32_NOPAD_NOCASE
            .decode(body.as_bytes())
            .map_err(|_| Error::Text)?;
        let cid = Self::from_bytes(&bytes)?;
        ensure!(cid.to_string() == text, TextSnafu);
        Ok(cid)
    }
}

/// `b` and lowercase, unpadded base32.
impl<S: CidFormat> fmt::Display for Cid<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut text = BASE32_NOPAD_NOCASE.encode(&self.to_bytes());
        text.make_ascii_lowercase();
        write!(f, "b{text}")
    }
}

impl<S: CidFormat> fmt::Debug for Cid<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Cid({self})")
    }
}

/// `From` casts, where every source CID fits the target.
macro_rules! from {
    ($($source:ty => $($target:ty),+;)*) => {$($(
        impl From<Cid<$source>> for Cid<$target> {
            fn from(cid: Cid<$source>) -> Self {
                cid.cast().expect("the CID fits")
            }
        }
    )+)*};
}

/// `TryFrom` casts, failing with [`Error::Unsupported`].
macro_rules! try_from {
    ($($source:ty => $($target:ty),+;)*) => {$($(
        impl TryFrom<Cid<$source>> for Cid<$target> {
            type Error = Error;

            fn try_from(cid: Cid<$source>) -> Result<Self, Error> {
                cid.cast()
            }
        }
    )+)*};
}

from! {
    Packed32 => Packed64;
    Dasl => Packed32, Packed64, Bdasl;
    Bdasl => Packed32, Packed64;
    storage::Blake3 => Packed32, Packed64, Bdasl;
    storage::Sha256 => Packed32, Packed64, Dasl, Bdasl;
    GitSha1 => Packed32, Packed64, Git;
    GitSha256 => Packed32, Packed64, Git;
    Git => Packed32, Packed64;
}

try_from! {
    Packed64 => Packed32;
    Packed32 => Dasl, Bdasl, storage::Blake3, storage::Sha256;
    Packed64 => Dasl, Bdasl, storage::Blake3, storage::Sha256;
    Bdasl => Dasl, storage::Blake3, storage::Sha256;
    Dasl => storage::Sha256;
    Packed32 => Git, GitSha1, GitSha256;
    Packed64 => Git, GitSha1, GitSha256;
    Git => GitSha1, GitSha256;
}

/// Errors from parsing, building and checking CIDs.
#[derive(Debug, Snafu)]
#[snafu(crate_root(covalence_lib_error::snafu))]
#[non_exhaustive]
pub enum Error {
    /// The text is not `b` and lowercase, unpadded base32.
    #[snafu(display("CID text is not `b` and lowercase, unpadded base32"))]
    Text,
    /// The bytes are not exactly one binary CIDv1.
    #[snafu(display("not exactly one binary CIDv1"))]
    Malformed,
    /// The storage does not hold this CID.
    #[snafu(display(
        "this storage does not hold {:?} CIDs with a {len}-byte {:?} digest",
        format.codec,
        format.hash
    ))]
    Unsupported {
        /// The codec and hash function.
        format: Format,
        /// The digest length.
        len: usize,
    },
    /// The bytes do not hash to the CID.
    #[snafu(display("bytes do not hash to the CID"))]
    Mismatch,
    /// This crate cannot compute the CID's digest: it does not compute this
    /// hash function, or the digest has another length than its output.
    #[snafu(display("cannot compute a {len}-byte {hash:?} digest"))]
    Hash {
        /// The hash function.
        hash: Multicodec,
        /// The digest length.
        len: usize,
    },
}
