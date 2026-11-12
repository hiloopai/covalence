//! A database downloaded whole to a local file, and checked before it is read.

use std::{
    fs::File,
    io::{self, BufWriter, Write},
    path::Path,
};

use covalence_cas_trait::Cas;
use covalence_lib_cid::Cid;
use covalence_lib_error::snafu::{OptionExt, ResultExt, ensure};

use crate::{
    Error,
    error::{IoSnafu, LengthSnafu, MissingSnafu, UnsupportedSnafu},
    hash::Hasher,
    pio,
};

/// A database's bytes in a private temporary file: it is gone when it is
/// closed.
///
/// On Unix the file has no name. On Windows it has one, and nothing else can
/// read or write it while it is open.
pub(crate) struct Local {
    file: File,
    size: u64,
}

impl Local {
    /// Download the bytes of `cid` from `cas` to a new file in `dir`, or in
    /// the system's directory for temporary files, and check them.
    ///
    /// - The bytes are hashed here as they arrive, whatever checks the store
    ///   makes, and the file is used only if the digest is that of `cid`.
    /// - At most `limit` bytes are accepted, so a store cannot fill the disk.
    pub(crate) fn download(
        cas: &dyn Cas,
        cid: &Cid,
        limit: u64,
        dir: Option<&Path>,
    ) -> Result<Self, Error> {
        let hash = cid.hash();
        let hasher = Hasher::new(hash).context(UnsupportedSnafu {
            what: format!("checking a download with the hash function {hash:?}"),
        })?;
        let file = match dir {
            Some(dir) => tempfile::tempfile_in(dir),
            None => tempfile::tempfile(),
        };
        let mut sink = Sink {
            file: BufWriter::new(file.context(IoSnafu)?),
            hasher,
            written: 0,
            limit,
            failed: None,
        };
        let reported = cas.get_to(cid, &mut sink);
        // What stopped the sink explains the store's failure better than the
        // store can, and counts even if the store went on regardless.
        if let Some(failed) = sink.failed {
            return Err(failed);
        }
        let reported = reported
            .map_err(crate::store)?
            .context(MissingSnafu { cid: *cid })?;
        let size = sink.written;
        if sink.hasher.finish() != cid.digest() {
            let source = format!("{size} downloaded bytes do not hash to {cid}").into();
            return Err(Error::Corrupt { offset: 0, source });
        }
        ensure!(
            reported == size,
            LengthSnafu {
                offset: 0u64,
                expected: reported,
                actual: size,
            }
        );
        let file = (sink.file.into_inner())
            .map_err(io::IntoInnerError::into_error)
            .context(IoSnafu)?;
        Ok(Self { file, size })
    }

    /// The size of the database in bytes.
    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    /// Fill `buf` with the bytes at `offset`.
    pub(crate) fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Error> {
        pio::read_exact_at(&self.file, offset, buf).context(IoSnafu)
    }
}

/// Writes a download to a file, hashing and counting it.
struct Sink {
    file: BufWriter<File>,
    hasher: Hasher,
    written: u64,
    limit: u64,
    /// Why the sink stopped taking bytes.
    failed: Option<Error>,
}

impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.failed.is_some() {
            return Err(io::Error::other("the download has already failed"));
        }
        let total = self.written.checked_add(bytes.len() as u64);
        let Some(total) = total.filter(|total| *total <= self.limit) else {
            self.failed = Some(Error::TooLarge { limit: self.limit });
            return Err(io::Error::other("the download is over its limit"));
        };
        if let Err(source) = self.file.write_all(bytes) {
            let kind = source.kind();
            self.failed = Some(Error::Io { source });
            return Err(kind.into());
        }
        self.hasher.update(bytes);
        self.written = total;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // The file is flushed once, when the download is done.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use covalence_cas_memory::MemoryCas;
    use covalence_cas_trait::Put;
    use covalence_lib_data::Bytes;

    use super::*;

    #[test]
    fn a_download_holds_the_bytes_of_its_cid() {
        let cas = MemoryCas::default();
        let bytes: Vec<u8> = (0..200_000u32).map(|byte| (byte % 241) as u8).collect();
        let cid: Cid = cas.put(Bytes::from(bytes.clone())).unwrap();
        let local = Local::download(&cas, &cid, 200_000, None).unwrap();
        assert_eq!(local.size(), 200_000);
        let mut buf = vec![0; 70_000];
        local.read_at(130_000, &mut buf).unwrap();
        assert_eq!(buf, bytes[130_000..]);
        // Reads past the end fail: they never give bytes that are not there.
        assert!(matches!(
            local.read_at(130_001, &mut buf),
            Err(Error::Io { .. })
        ));
    }

    #[test]
    fn a_download_over_its_limit_stops_and_keeps_nothing() {
        let cas = MemoryCas::default();
        let cid: Cid = cas.put(Bytes::from(vec![1; 10_000])).unwrap();
        let error = Local::download(&cas, &cid, 9_999, None).err().unwrap();
        assert!(matches!(error, Error::TooLarge { limit: 9_999 }), "{error}");
        assert!(Local::download(&cas, &cid, 10_000, None).is_ok());
    }

    #[test]
    fn a_directory_that_cannot_hold_the_file_fails_the_download() {
        let cas = MemoryCas::default();
        let cid: Cid = cas.put(Bytes::from_static(b"bytes")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let error = Local::download(&cas, &cid, 100, Some(&missing))
            .err()
            .unwrap();
        assert!(matches!(error, Error::Io { .. }), "{error}");
        let local = Local::download(&cas, &cid, 100, Some(dir.path())).unwrap();
        assert_eq!(local.size(), 5);
        let files = || std::fs::read_dir(dir.path()).unwrap().count();
        // On Unix the file has no name: the directory it is in stays empty.
        if cfg!(unix) {
            assert_eq!(files(), 0);
        }
        // Everywhere, nothing is left once it is closed.
        drop(local);
        assert_eq!(files(), 0);
    }
}
