//! Files another process may read while this one writes them: Lake's cache
//! and the host's mirrors are shared by every checkout on a host.

use std::{io::Write, path::Path};

use crate::error::{IoResult, Result};

/// Write `bytes` to `path` whole: a reader sees the old file or the new one,
/// never a partial one.
pub(crate) fn write(path: &Path, bytes: impl AsRef<[u8]>, operation: &str) -> Result<()> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let mut pending = tempfile::NamedTempFile::new_in(parent).operation(operation)?;
    pending.write_all(bytes.as_ref()).operation(operation)?;
    // As `fs::write` leaves it, not the temporary file's owner-only mode.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        pending
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o644))
            .operation(operation)?;
    }
    pending
        .persist(path)
        .map_err(|error| error.error)
        .operation(operation)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_write_replaces_the_file_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        super::write(&path, "{}", "write").unwrap();
        super::write(&path, "[1]", "write").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[1]");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names, ["record.json"]);
    }
}
