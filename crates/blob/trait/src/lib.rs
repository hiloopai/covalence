#![doc = include_str!("../README.md")]
#![deny(missing_docs)]
//!
//! ## Examples
//!
//! Reading by range:
//!
//! ```
//! use std::sync::Arc;
//!
//! use covalence_blob_trait::{Blob, Slice};
//! use covalence_lib_data::Bytes;
//!
//! let blob: Arc<dyn Blob> = Arc::new(Bytes::from_static(b"hello world"));
//! assert_eq!(blob.size()?, 11);
//! assert_eq!(blob.read_range(6, 100)?, "world");
//! assert_eq!(blob.read_range(20, 1)?, "");
//! assert_eq!(blob.read_all()?, "hello world");
//!
//! let world = Slice::new(blob, 6, u64::MAX);
//! assert_eq!(world.size()?, 5);
//! assert_eq!(world.slice(1, 3).read_all()?, "orl");
//! # Ok::<(), covalence_blob_trait::Error>(())
//! ```
//!
//! Checked facts, relabelled to the codec asked for:
//!
//! ```
//! use covalence_blob_trait::{Blob, BlobExt};
//! use covalence_lib_cid::{Blake3, Cid32, Format};
//! use covalence_lib_data::Bytes;
//!
//! let blob = Bytes::from_static(b"hello");
//! let cid = Cid32::from(Blake3::of(b"hello"));
//! let fact = blob.fact(&cid)?.expect("the bytes hash to the CID");
//! assert_eq!(fact.bytes(), "hello");
//! assert_eq!(fact.read_range(1, 3)?, "ell");
//! assert!(blob.fact(&Blake3::of(b"other").into())?.is_none());
//!
//! let drisl = Cid32::new(Format::DRISL_BLAKE3, cid.digest())?;
//! assert_eq!(fact.fact(&drisl)?.expect("the same multihash").claim(), &drisl);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! An override that returns a fact for other content is caught by the checked
//! step:
//!
//! ```
//! use covalence_blob_trait::{Blob, BlobExt, Error};
//! use covalence_lib_cid::{Blake3, Cid, Fact};
//! use covalence_lib_data::Bytes;
//!
//! struct Liar(Bytes);
//!
//! impl Blob for Liar {
//!     fn size(&self) -> Result<u64, Error> {
//!         self.0.size()
//!     }
//!     fn read_range(&self, start: u64, len: u64) -> Result<Bytes, Error> {
//!         self.0.read_range(start, len)
//!     }
//!     fn fact_unchecked(&self, _cid: &Cid) -> Result<Option<Fact>, Error> {
//!         Ok(Some(Fact::<Blake3>::of(Bytes::from_static(b"other")).widen()))
//!     }
//! }
//!
//! let blob = Liar(Bytes::from_static(b"hello"));
//! let cid = Blake3::of(b"hello").into();
//! assert!(blob.fact_unchecked(&cid)?.is_some());
//! assert!(matches!(blob.fact(&cid), Err(Error::Mismatch)));
//! # Ok::<(), Error>(())
//! ```

use std::sync::Arc;

use covalence_lib_cid::{Cid, Fact};
use covalence_lib_data::Bytes;
use covalence_lib_error::{
    BoxError,
    snafu::{OptionExt, Snafu},
};

/// Bytes read whole or by range. See the [crate docs](crate) for the range
/// contract and for checked and unchecked facts.
pub trait Blob: Send + Sync {
    /// The size in bytes, as the backend reports it.
    fn size(&self) -> Result<u64, Error>;

    /// `len` bytes from `start`, or fewer only where the blob ends.
    ///
    /// - Empty at or past the end.
    /// - Range ends beyond `u64::MAX` are clipped.
    /// - No short reads elsewhere: implementations reading in parts loop until done.
    /// - Callers of untrusted blobs check what comes back.
    fn read_range(&self, start: u64, len: u64) -> Result<Bytes, Error>;

    /// All the bytes: [`Blob::read_range`] from 0 for [`Blob::size`] by default.
    fn read_all(&self) -> Result<Bytes, Error> {
        self.read_range(0, self.size()?)
    }

    /// A fact for `cid` from the blob's bytes, as the blob reports it. Call
    /// [`BlobExt::fact`], which checks the result, instead.
    ///
    /// By default, copies [`Blob::read_all`] once and checks the copy against
    /// `cid`:
    ///
    /// - `Some` fact for exactly `cid` if the bytes hash to its digest.
    /// - `None` if they do not.
    /// - [`Error::Check`] if lib-cid cannot compute `cid`'s digest.
    /// - The read's error if the read fails.
    ///
    /// Overrides should return a fact for `cid`'s multihash, or `None` for
    /// other content; [`BlobExt::fact`] rejects facts with another multihash
    /// and relabels the rest to `cid`.
    fn fact_unchecked(&self, cid: &Cid) -> Result<Option<Fact>, Error> {
        to_fact(cid, Bytes::copy_from_slice(&self.read_all()?))
    }
}

/// Checked facts, for every [`Blob`]. Blobs cannot override them.
pub trait BlobExt: Blob {
    /// A fact for exactly `cid`, checked, or `None`.
    ///
    /// Calls [`Blob::fact_unchecked`], then:
    ///
    /// - `Some` fact relabelled for exactly `cid` if it has `cid`'s multihash.
    /// - [`Error::Mismatch`] if it has another multihash.
    /// - `None` or the error, as `fact_unchecked` returned them.
    fn fact(&self, cid: &Cid) -> Result<Option<Fact>, Error> {
        let Some(fact) = self.fact_unchecked(cid)? else {
            return Ok(None);
        };
        Ok(Some(relabel(fact, cid)?.context(MismatchSnafu)?))
    }
}

impl<T: Blob + ?Sized> BlobExt for T {}

/// `fact` relabelled for exactly `cid`, or `None` if their multihashes differ.
fn relabel(fact: Fact, cid: &Cid) -> Result<Option<Fact>, Error> {
    if !fact.claim().same_multihash(cid) {
        return Ok(None);
    }
    Ok(Some(fact.with_codec(cid.codec())?))
}

/// `bytes` as a fact for `cid`, or `None` if they do not hash to it.
fn to_fact(cid: &Cid, bytes: Bytes) -> Result<Option<Fact>, Error> {
    match Fact::new(*cid, bytes) {
        Ok(fact) => Ok(Some(fact)),
        Err(covalence_lib_cid::Error::Mismatch) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// In-memory bytes; ranges are zero-copy slices.
impl Blob for Bytes {
    fn size(&self) -> Result<u64, Error> {
        Ok(self.len() as u64)
    }
    fn read_range(&self, start: u64, len: u64) -> Result<Bytes, Error> {
        let size = self.len() as u64;
        let first = start.min(size) as usize;
        let last = start.saturating_add(len).min(size) as usize;
        Ok(self.slice(first..last))
    }
    fn read_all(&self) -> Result<Bytes, Error> {
        Ok(self.clone())
    }
}

/// A fact's bytes; ranges are zero-copy slices, and facts for its own
/// multihash need no rehash.
impl Blob for Fact {
    fn size(&self) -> Result<u64, Error> {
        self.bytes().size()
    }
    fn read_range(&self, start: u64, len: u64) -> Result<Bytes, Error> {
        self.bytes().read_range(start, len)
    }
    fn read_all(&self) -> Result<Bytes, Error> {
        self.bytes().read_all()
    }
    /// Itself, relabelled for `cid` with no rehash, if they have the same
    /// multihash; otherwise its bytes, hashed and checked against `cid`.
    fn fact_unchecked(&self, cid: &Cid) -> Result<Option<Fact>, Error> {
        match relabel(self.clone(), cid)? {
            Some(fact) => Ok(Some(fact)),
            None => to_fact(cid, self.bytes().clone()),
        }
    }
}

/// A shared blob is a blob: each method forwards to it.
impl Blob for Arc<dyn Blob> {
    fn size(&self) -> Result<u64, Error> {
        (**self).size()
    }
    fn read_range(&self, start: u64, len: u64) -> Result<Bytes, Error> {
        (**self).read_range(start, len)
    }
    fn read_all(&self) -> Result<Bytes, Error> {
        (**self).read_all()
    }
    fn fact_unchecked(&self, cid: &Cid) -> Result<Option<Fact>, Error> {
        (**self).fact_unchecked(cid)
    }
}

/// Up to `len` bytes of another blob from `start`, read relative to `start`.
///
/// - Clipped to the blob's size when read, so the slice may be shorter.
/// - As untrusted as the blob under it.
#[derive(Clone)]
pub struct Slice {
    blob: Arc<dyn Blob>,
    start: u64,
    len: u64,
}

impl Slice {
    /// Up to `len` bytes of `blob` from `start`; ends beyond `u64::MAX` are clipped.
    pub fn new(blob: Arc<dyn Blob>, start: u64, len: u64) -> Self {
        let len = len.min(u64::MAX - start);
        Self { blob, start, len }
    }

    /// Up to `len` bytes of this slice from `start`, over the same blob.
    pub fn slice(&self, start: u64, len: u64) -> Self {
        let (start, len) = self.clip(start, len);
        let blob = self.blob.clone();
        Self { blob, start, len }
    }

    /// The range of the blob under `start` and `len` of this slice.
    fn clip(&self, start: u64, len: u64) -> (u64, u64) {
        let start = start.min(self.len);
        (self.start + start, len.min(self.len - start))
    }
}

impl Blob for Slice {
    fn size(&self) -> Result<u64, Error> {
        Ok(self.blob.size()?.saturating_sub(self.start).min(self.len))
    }
    fn read_range(&self, start: u64, len: u64) -> Result<Bytes, Error> {
        let (start, len) = self.clip(start, len);
        self.blob.read_range(start, len)
    }
    fn read_all(&self) -> Result<Bytes, Error> {
        self.blob.read_range(self.start, self.len)
    }
}

/// Errors from reading a blob and checking its facts.
#[derive(Debug, Snafu)]
#[snafu(crate_root(covalence_lib_error::snafu))]
#[non_exhaustive]
pub enum Error {
    /// An I/O failure, such as reading a file.
    #[snafu(display("I/O: {source}"), context(false))]
    Io {
        /// The I/O error.
        source: std::io::Error,
    },
    /// A failure in the storage under a blob.
    #[snafu(display("backend: {source}"))]
    Backend {
        /// The storage's error.
        source: BoxError,
    },
    /// The blob returned a fact for another multihash than the CID asked for.
    #[snafu(display("the blob returned a fact for another CID"))]
    Mismatch,
    /// The CID cannot be checked, such as for a hash lib-cid cannot compute.
    #[snafu(context(false), display("{source}"))]
    Check {
        /// The lib-cid error.
        #[snafu(source(from(covalence_lib_cid::Error, Box::new)))]
        source: Box<covalence_lib_cid::Error>,
    },
}
