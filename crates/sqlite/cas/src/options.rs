//! The options a database is opened with.

use std::{fmt, path::PathBuf, sync::Arc};

use crate::{
    cache::{CachePolicy, MemoryCache},
    fetch::{FetchPolicy, ReadAhead},
    verify::Verification,
};

/// How to open a database: how its bytes are checked, fetched and cached.
///
/// The default downloads the whole database and checks it against its CID.
/// The other options matter only when ranges are read from the store.
///
/// ```
/// use covalence_sqlite_cas::{
///     Options,
///     cache::FileCache,
///     fetch::Aligned,
///     verify::Verification,
/// };
///
/// let options = Options::new()
///     .verification(Verification::TrustedRanges)
///     .block_size(16 << 10)
///     .fetch(Aligned::new(256 << 10))
///     .cache(FileCache::new(1 << 30));
/// # let _ = options;
/// ```
#[derive(Clone)]
pub struct Options {
    pub(crate) verification: Verification,
    pub(crate) fetch: Arc<dyn FetchPolicy>,
    pub(crate) cache: Arc<dyn CachePolicy>,
    pub(crate) block_size: u64,
    pub(crate) max_download: u64,
    pub(crate) dir: Option<PathBuf>,
}

impl Options {
    /// The size of a block unless [`Options::block_size`] sets another: 4 KiB.
    pub const BLOCK_SIZE: u64 = 4 << 10;

    /// The most bytes a download accepts unless [`Options::max_download`]
    /// sets another: 1 GiB.
    pub const MAX_DOWNLOAD: u64 = 1 << 30;

    /// The memory of the cache unless [`Options::cache`] sets another: 32 MiB.
    pub const CACHE_BYTES: u64 = 32 << 20;

    /// The default options.
    pub fn new() -> Self {
        Self {
            verification: Verification::default(),
            fetch: Arc::new(ReadAhead::default()),
            cache: Arc::new(MemoryCache::new(Self::CACHE_BYTES)),
            block_size: Self::BLOCK_SIZE,
            max_download: Self::MAX_DOWNLOAD,
            dir: None,
        }
    }

    /// How the database's bytes are checked: [`Verification::Download`] by
    /// default.
    pub fn verification(mut self, verification: Verification) -> Self {
        self.verification = verification;
        self
    }

    /// How much to fetch when a read misses the cache: a default
    /// [`ReadAhead`] unless set.
    ///
    /// [`Database::set_fetch_policy`](crate::Database::set_fetch_policy)
    /// changes it on an open database.
    pub fn fetch(mut self, policy: impl FetchPolicy + 'static) -> Self {
        self.fetch = Arc::new(policy);
        self
    }

    /// What keeps fetched blocks: a [`MemoryCache`] of
    /// [`Options::CACHE_BYTES`] unless set.
    ///
    /// Each database opened with these options gets a cache of its own.
    pub fn cache(mut self, policy: impl CachePolicy + 'static) -> Self {
        self.cache = Arc::new(policy);
        self
    }

    /// The size in bytes of the blocks that are fetched and cached:
    /// [`Options::BLOCK_SIZE`] unless set.
    ///
    /// - Opening fails with [`Error::Options`](crate::Error::Options) unless
    ///   it is between 1 and [`MAX_BLOCK_SIZE`](crate::MAX_BLOCK_SIZE).
    /// - With [`Verification::VerifiedRanges`] the blocks are the verifier's
    ///   chunks, and this is not used.
    pub fn block_size(mut self, bytes: u64) -> Self {
        self.block_size = bytes;
        self
    }

    /// The most bytes [`Verification::Download`] accepts from the store:
    /// [`Options::MAX_DOWNLOAD`] unless set.
    ///
    /// A store that gives more fails the open with
    /// [`Error::TooLarge`](crate::Error::TooLarge), so it cannot fill the
    /// disk before its bytes are checked.
    pub fn max_download(mut self, bytes: u64) -> Self {
        self.max_download = bytes;
        self
    }

    /// The directory for the file of [`Verification::Download`]: the system's
    /// directory for temporary files unless set.
    ///
    /// The file is private: it has no name on Unix, and on Windows nothing
    /// else can read or write it. It is gone when the database and its
    /// connections are.
    pub fn dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.dir = Some(dir.into());
        self
    }
}

impl Default for Options {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Options {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Options")
            .field("verification", &self.verification)
            .field("block_size", &self.block_size)
            .field("max_download", &self.max_download)
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}
