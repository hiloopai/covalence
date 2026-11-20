#![doc = include_str!("../README.md")]
#![deny(missing_docs)]

mod digest;
mod read;
mod write;

pub use digest::{DigestAccumulator, DigestAlgorithms, DigestError, DigestResult};
pub use read::VerifyingReader;
pub use write::VerifyingWriter;

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

pub use covalence_lib_cid::HashFn;
use covalence_lib_error::snafu::Snafu;
use sha2::Digest as _;

/// The size and digest a stream must have.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Expected {
    size: u64,
    hash: HashFn,
    digest: [u8; 32],
}

impl Expected {
    /// A stream of exactly `size` bytes whose `hash` is `digest`.
    pub const fn new(size: u64, hash: HashFn, digest: [u8; 32]) -> Self {
        Self { size, hash, digest }
    }

    /// The size and `hash` digest of `bytes`, which are in memory.
    ///
    /// ```
    /// use covalence_blob_verify::{Expected, HashFn};
    ///
    /// let expected = Expected::of(HashFn::Sha256, b"hello");
    /// assert_eq!(expected.size(), 5);
    /// assert_eq!(expected.digest(), &HashFn::Sha256.digest(b"hello"));
    /// ```
    pub fn of(hash: HashFn, bytes: &[u8]) -> Self {
        Self::new(bytes.len() as u64, hash, hash.digest(bytes))
    }

    /// The size in bytes.
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// The hash function.
    pub const fn hash(&self) -> HashFn {
        self.hash
    }

    /// The digest.
    pub const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}

/// Proof that a stream had exactly the expected size and digest.
///
/// Only a [`Verifier`], directly or inside a reader or writer, makes one, and
/// only after it has seen every byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Verified(Expected);

impl Verified {
    /// The size, digest and hash function that were checked.
    pub const fn expected(&self) -> &Expected {
        &self.0
    }
}

/// Checks bytes, given in order and in any number of pieces, against an
/// [`Expected`] size and digest.
///
/// It does no I/O and holds no bytes, only the hash state. Use it directly
/// when bytes arrive other than through [`std::io`] or Tokio, for example from
/// a channel of chunks.
///
/// ```
/// use covalence_blob_verify::{Error, Expected, HashFn, Verifier};
///
/// let mut verifier = Verifier::new(Expected::of(HashFn::Blake3, b"hello"));
/// verifier.update(b"hel")?;
/// assert!(matches!(verifier.finish(), Err(Error::TooShort { received: 3, .. })));
/// verifier.update(b"lo")?;
/// assert_eq!(verifier.finish()?.expected().size(), 5);
/// assert!(matches!(verifier.update(b"!"), Err(Error::TooLong { .. })));
/// # Ok::<(), Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct Verifier {
    expected: Expected,
    received: u64,
    hasher: Hasher,
}

impl Verifier {
    /// A verifier that has seen no bytes yet.
    pub fn new(expected: Expected) -> Self {
        let hasher = match expected.hash {
            HashFn::Sha256 => Hasher::Sha256(sha2::Sha256::new()),
            HashFn::Blake3 => Hasher::Blake3(Box::new(blake3::Hasher::new())),
        };
        Self {
            expected,
            received: 0,
            hasher,
        }
    }

    /// The size and digest being checked.
    pub fn expected(&self) -> &Expected {
        &self.expected
    }

    /// The number of bytes seen so far.
    pub fn received(&self) -> u64 {
        self.received
    }

    /// The number of bytes still expected.
    pub fn remaining(&self) -> u64 {
        self.expected.size - self.received
    }

    /// Hash the next `bytes`.
    ///
    /// Fails with [`Error::TooLong`], hashing none of them, if they would make
    /// the stream longer than expected. The verifier is unchanged, so its
    /// state still describes the bytes before them.
    pub fn update(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if bytes.len() as u64 > self.remaining() {
            return Err(self.too_long());
        }
        self.received += bytes.len() as u64;
        match &mut self.hasher {
            Hasher::Sha256(hasher) => hasher.update(bytes),
            Hasher::Blake3(hasher) => {
                hasher.update(bytes);
            }
        }
        Ok(())
    }

    /// Check the bytes seen so far against the expected size and digest.
    ///
    /// - [`Error::TooShort`] if fewer bytes than expected were seen; more may
    ///   follow.
    /// - [`Error::Mismatch`] if all were seen and the digest differs.
    ///
    /// It does not consume the verifier, so it can be asked again.
    pub fn finish(&self) -> Result<Verified, Error> {
        if self.remaining() > 0 {
            return Err(Error::TooShort {
                expected: self.expected.size,
                received: self.received,
            });
        }
        let digest: [u8; 32] = match &self.hasher {
            Hasher::Sha256(hasher) => hasher.clone().finalize().into(),
            Hasher::Blake3(hasher) => hasher.finalize().into(),
        };
        if digest != self.expected.digest {
            return Err(Error::Mismatch {
                hash: self.expected.hash,
            });
        }
        Ok(Verified(self.expected))
    }

    fn too_long(&self) -> Error {
        Error::TooLong {
            expected: self.expected.size,
        }
    }
}

/// The running hash of a [`Verifier`].
#[derive(Clone, Debug)]
enum Hasher {
    Sha256(sha2::Sha256),
    // Boxed: BLAKE3's state is much larger than SHA-256's.
    Blake3(Box<blake3::Hasher>),
}

/// A flag that asks readers and writers to stop.
///
/// Clones share the flag. Readers and writers check it before each read or
/// write they pass on; it does not interrupt one in progress.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    /// A flag that is not yet set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the flag, for this handle and every clone.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether the flag is set.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// When a reader or writer should give up: on a [`Cancel`] flag, at a
/// deadline, or neither.
#[derive(Clone, Debug, Default)]
struct Stop {
    cancel: Option<Cancel>,
    deadline: Option<Instant>,
}

impl Stop {
    fn check(&self) -> Result<(), Error> {
        if self.cancel.as_ref().is_some_and(Cancel::is_cancelled) {
            return Err(Error::Cancelled);
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(Error::DeadlineExceeded);
        }
        Ok(())
    }
}

/// Why a stream failed its check, or was stopped before it finished.
///
/// Readers and writers return these inside an [`io::Error`]; get them back
/// with [`Error::of`]. Each maps to an [`io::ErrorKind`] that copy loops do
/// not retry:
///
/// - [`io::ErrorKind::InvalidData`] for [`Error::TooLong`] and
///   [`Error::Mismatch`].
/// - [`io::ErrorKind::UnexpectedEof`] for [`Error::TooShort`].
/// - [`io::ErrorKind::Other`] for [`Error::Cancelled`].
/// - [`io::ErrorKind::TimedOut`] for [`Error::DeadlineExceeded`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Snafu)]
#[snafu(crate_root(covalence_lib_error::snafu))]
#[non_exhaustive]
pub enum Error {
    /// The stream has more bytes than expected.
    #[snafu(display("stream is longer than the expected {expected} bytes"))]
    TooLong {
        /// The expected size.
        expected: u64,
    },
    /// The stream ended before the expected size.
    #[snafu(display("stream ended after {received} of the expected {expected} bytes"))]
    TooShort {
        /// The expected size.
        expected: u64,
        /// The bytes received.
        received: u64,
    },
    /// The stream had the expected size but another digest.
    #[snafu(display("stream does not match the expected {hash:?} digest"))]
    Mismatch {
        /// The hash function checked.
        hash: HashFn,
    },
    /// The [`Cancel`] flag was set.
    #[snafu(display("stream was cancelled"))]
    Cancelled,
    /// The deadline passed.
    #[snafu(display("stream deadline passed"))]
    DeadlineExceeded,
}

impl Error {
    /// The verification error inside `error`, if it holds one.
    ///
    /// ```
    /// use std::io::Read;
    ///
    /// use covalence_blob_verify::{Error, Expected, HashFn, VerifyingReader};
    ///
    /// let expected = Expected::of(HashFn::Sha256, b"hello");
    /// let mut reader = VerifyingReader::new(&b"hellO"[..], expected);
    /// let error = reader.read_to_end(&mut Vec::new()).unwrap_err();
    /// assert_eq!(Error::of(&error), Some(&Error::Mismatch { hash: HashFn::Sha256 }));
    /// ```
    pub fn of(error: &io::Error) -> Option<&Error> {
        error.get_ref()?.downcast_ref()
    }

    fn kind(&self) -> io::ErrorKind {
        match self {
            Error::TooLong { .. } | Error::Mismatch { .. } => io::ErrorKind::InvalidData,
            Error::TooShort { .. } => io::ErrorKind::UnexpectedEof,
            Error::Cancelled => io::ErrorKind::Other,
            Error::DeadlineExceeded => io::ErrorKind::TimedOut,
        }
    }
}

impl From<Error> for io::Error {
    fn from(error: Error) -> Self {
        io::Error::new(error.kind(), error)
    }
}
