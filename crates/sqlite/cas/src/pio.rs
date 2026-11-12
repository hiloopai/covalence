//! Reads and writes at a position of a file, shared between threads.

use std::{fs::File, io};

/// Fill `buf` from the bytes of `file` at `at`.
#[cfg(unix)]
pub(crate) fn read_exact_at(file: &File, at: u64, buf: &mut [u8]) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, at)
}

/// Write `bytes` to `file` at `at`.
#[cfg(unix)]
pub(crate) fn write_all_at(file: &File, at: u64, bytes: &[u8]) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, bytes, at)
}

/// Where a file has one cursor and no positional calls, one thread at a time
/// moves it.
#[cfg(not(unix))]
static CURSOR: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Fill `buf` from the bytes of `file` at `at`.
#[cfg(not(unix))]
pub(crate) fn read_exact_at(mut file: &File, at: u64, buf: &mut [u8]) -> io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let _cursor = CURSOR.lock().unwrap_or_else(|poison| poison.into_inner());
    file.seek(SeekFrom::Start(at))?;
    file.read_exact(buf)
}

/// Write `bytes` to `file` at `at`.
#[cfg(not(unix))]
pub(crate) fn write_all_at(mut file: &File, at: u64, bytes: &[u8]) -> io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let _cursor = CURSOR.lock().unwrap_or_else(|poison| poison.into_inner());
    file.seek(SeekFrom::Start(at))?;
    file.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_was_written_and_fails_past_the_end() {
        let file = tempfile::tempfile().unwrap();
        write_all_at(&file, 3, b"hello").unwrap();
        let mut buf = [0; 4];
        read_exact_at(&file, 4, &mut buf).unwrap();
        assert_eq!(&buf, b"ello");
        read_exact_at(&file, 0, &mut buf).unwrap();
        assert_eq!(&buf, b"\0\0\0h");
        let error = read_exact_at(&file, 6, &mut buf).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }
}
