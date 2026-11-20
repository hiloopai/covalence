use std::io;

use covalence_lib_cid::{Blake3, Format};
use covalence_lib_error::snafu::Snafu;
use sha2::Digest as _;

const CHUNK_BYTES: usize = 64 * 1024;
const SHA256_MAX_LEN: u64 = u64::MAX / 8;

/// Which digests to compute in one pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DigestAlgorithms {
    /// Compute only SHA-256.
    Sha256,
    /// Compute only BLAKE3-256.
    Blake3,
    /// Compute SHA-256 and BLAKE3-256 together.
    Both,
}

/// The observed byte length and the requested digests, not proof of integrity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DigestResult {
    len: u64,
    sha256: Option<[u8; 32]>,
    blake3: Option<[u8; 32]>,
}

impl DigestResult {
    /// The exact number of bytes hashed.
    pub const fn len(&self) -> u64 {
        self.len
    }

    /// Whether no bytes were hashed.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The SHA-256 digest, if requested.
    pub const fn sha256(&self) -> Option<&[u8; 32]> {
        self.sha256.as_ref()
    }

    /// The BLAKE3-256 digest, if requested.
    pub const fn blake3(&self) -> Option<&[u8; 32]> {
        self.blake3.as_ref()
    }

    /// The raw CID, if BLAKE3 was requested.
    pub fn raw_blake3(&self) -> Option<Blake3> {
        self.blake3()
            .map(|digest| Blake3::new(Format::BLAKE3, digest).expect("a 32-byte BLAKE3 digest"))
    }
}

/// Why an input could not be measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Snafu)]
#[snafu(crate_root(covalence_lib_error::snafu))]
#[non_exhaustive]
pub enum DigestError {
    /// The input exceeds the configured or algorithm byte limit.
    #[snafu(display("input exceeds its limit of {max_len} bytes"))]
    TooLong {
        /// The effective maximum length in bytes.
        max_len: u64,
    },
}

impl DigestError {
    /// The measurement error inside `error`, if it holds one.
    pub fn of(error: &io::Error) -> Option<&Self> {
        error.get_ref()?.downcast_ref()
    }
}

impl From<DigestError> for io::Error {
    fn from(error: DigestError) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, error)
    }
}

/// Measures the length and digests of bytes supplied in order.
///
/// Only the hash state and count are kept between calls. This does not compare
/// the input to an expected length or digest; use [`crate::Verifier`] for that.
///
/// ```
/// use covalence_blob_verify::{DigestAccumulator, DigestAlgorithms, HashFn};
/// use covalence_lib_cid::Blake3;
///
/// let mut digest = DigestAccumulator::new(DigestAlgorithms::Both, Some(5));
/// digest.update(b"he")?;
/// digest.update(b"llo")?;
/// let result = digest.finish()?;
/// assert_eq!(result.len(), 5);
/// assert_eq!(result.sha256(), Some(&HashFn::Sha256.digest(b"hello")));
/// assert_eq!(result.raw_blake3(), Some(Blake3::of(b"hello")));
/// # Ok::<(), covalence_blob_verify::DigestError>(())
/// ```
#[derive(Clone, Debug)]
pub struct DigestAccumulator {
    sha256: Option<sha2::Sha256>,
    blake3: Option<Box<blake3::Hasher>>,
    max_len: u64,
    len: Result<u64, DigestError>,
}

impl DigestAccumulator {
    /// Start measuring with the chosen algorithms and an optional byte limit.
    ///
    /// The effective limit is at most `u64::MAX` bytes, or `2^61 - 1` bytes
    /// when computing SHA-256, whose input must be shorter than `2^64` bits.
    pub fn new(algorithms: DigestAlgorithms, max_len: Option<u64>) -> Self {
        let sha256 = (algorithms != DigestAlgorithms::Blake3).then(sha2::Sha256::new);
        let blake3 =
            (algorithms != DigestAlgorithms::Sha256).then(|| Box::new(blake3::Hasher::new()));
        let algorithm_limit = if sha256.is_some() {
            SHA256_MAX_LEN
        } else {
            u64::MAX
        };
        Self {
            sha256,
            blake3,
            max_len: max_len.unwrap_or(u64::MAX).min(algorithm_limit),
            len: Ok(0),
        }
    }

    /// The number of additional bytes allowed, or the first limit error.
    pub fn remaining(&self) -> Result<u64, DigestError> {
        self.len.map(|len| self.max_len - len)
    }

    /// Hash the next bytes, or fail without hashing any of them.
    ///
    /// A limit error is permanent, even for later empty updates or `finish`.
    ///
    /// ```
    /// use covalence_blob_verify::{DigestAccumulator, DigestAlgorithms, DigestError};
    ///
    /// let mut digest = DigestAccumulator::new(DigestAlgorithms::Blake3, Some(1));
    /// let error = DigestError::TooLong { max_len: 1 };
    /// assert_eq!(digest.update(b"ab"), Err(error));
    /// assert_eq!(digest.update(b""), Err(error));
    /// assert_eq!(digest.finish(), Err(error));
    /// ```
    pub fn update(&mut self, bytes: &[u8]) -> Result<(), DigestError> {
        let remaining = self.remaining()?;
        let Ok(count) = u64::try_from(bytes.len()) else {
            return self.too_long();
        };
        if count > remaining {
            return self.too_long();
        }
        self.len = Ok(self.max_len - remaining + count);
        if let Some(hasher) = &mut self.sha256 {
            hasher.update(bytes);
        }
        if let Some(hasher) = &mut self.blake3 {
            hasher.update(bytes);
        }
        Ok(())
    }

    fn too_long(&mut self) -> Result<(), DigestError> {
        let error = DigestError::TooLong {
            max_len: self.max_len,
        };
        self.len = Err(error);
        Err(error)
    }

    /// Consume the accumulator and return its measurement, or its limit error.
    ///
    /// Call this only after every intended byte has been supplied.
    pub fn finish(self) -> Result<DigestResult, DigestError> {
        Ok(DigestResult {
            len: self.len?,
            sha256: self.sha256.map(|hasher| hasher.finalize().into()),
            blake3: self.blake3.map(|hasher| hasher.finalize().into()),
        })
    }

    /// Hash from the reader's current position through EOF and finish.
    ///
    /// Includes bytes already fed to the accumulator. Reads at most one byte
    /// past the limit to detect extra input. A limit error is wrapped in
    /// [`io::ErrorKind::InvalidData`]; other read errors pass through unchanged
    /// except [`io::ErrorKind::Interrupted`], which is retried.
    pub fn read(mut self, mut reader: impl io::Read) -> io::Result<DigestResult> {
        let mut buffer = vec![0; self.read_size()?];
        loop {
            let size = self.read_size()?;
            let count = match reader.read(&mut buffer[..size]) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if count > size {
                return Err(io::Error::other(
                    "the reader returned more than it was asked",
                ));
            }
            if count == 0 {
                return Ok(self.finish()?);
            }
            self.update(&buffer[..count])?;
        }
    }

    /// Hash from a Tokio reader's current position through EOF and finish.
    ///
    /// Like [`Self::read`], it reads at most one byte past the limit and
    /// returns no measurement on failure. Available with the `tokio` feature.
    #[cfg(feature = "tokio")]
    pub async fn read_async(
        mut self,
        mut reader: impl tokio::io::AsyncRead + Unpin,
    ) -> io::Result<DigestResult> {
        let mut buffer = vec![0; self.read_size()?];
        loop {
            let size = self.read_size()?;
            let mut read_buf = tokio::io::ReadBuf::new(&mut buffer[..size]);
            let result = std::future::poll_fn(|cx| {
                std::pin::Pin::new(&mut reader).poll_read(cx, &mut read_buf)
            })
            .await;
            let count = match result {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
                Ok(()) => read_buf.filled().len(),
            };
            if count == 0 {
                return Ok(self.finish()?);
            }
            self.update(&buffer[..count])?;
        }
    }

    fn read_size(&self) -> Result<usize, DigestError> {
        Ok(self.remaining()?.clamp(1, CHUNK_BYTES as u64) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_counts_that_would_wrap_or_exceed_sha256s_limit() {
        for (algorithms, max_len) in [
            (DigestAlgorithms::Blake3, u64::MAX),
            (DigestAlgorithms::Both, SHA256_MAX_LEN),
        ] {
            let mut digest = DigestAccumulator::new(algorithms, Some(u64::MAX));
            digest.len = Ok(max_len - 1);
            digest.update(b"a").unwrap();
            assert_eq!(digest.remaining(), Ok(0));
            assert_eq!(digest.update(b"b"), Err(DigestError::TooLong { max_len }));
        }
    }
}
