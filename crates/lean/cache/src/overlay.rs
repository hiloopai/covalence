//! The bundled Lake overlay: Lake files built from a patched Lake for one Lean
//! toolchain, which this command installs into that toolchain before it runs
//! Lake.
//!
//! The release archive carries `lake-overlay/<githash>/` beside the executable,
//! the files at their paths under `lean --print-prefix`, for the toolchain whose
//! `lean --githash` is `<githash>`. Any other toolchain keeps its own Lake.
//! Each file the overlay replaces is kept under `lib/lean/.lake-overlay-stock/`,
//! and `LAKE_POOL_STOCK_LAKE=1` puts those back instead of installing.
//!
//! The overlay changes how Lake walks a build, not what it builds: traces and
//! outputs match the stock Lake's, so the pool scope, which hashes `leantar`
//! and not `lake`, stays shared between patched and stock hosts.

use std::{
    env,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    process::Command,
};

use crate::{
    error::{IoResult, Result, invalid},
    project::{Project, output},
};

const STOCK: &str = "lib/lean/.lake-overlay-stock";
const LOCK: &str = "lib/lean/.lake-overlay.lock";

/// Install the bundled overlay for the project's toolchain, or with
/// `LAKE_POOL_STOCK_LAKE=1` restore the toolchain's own Lake.
pub(crate) fn apply(project: &Project) -> Result<()> {
    let stock = env::var_os("LAKE_POOL_STOCK_LAKE").is_some_and(|value| value == "1");
    let overlays = env::current_exe()
        .operation("locate cov-lake-cache")?
        .with_file_name("lake-overlay");
    if !stock && !overlays.is_dir() {
        return Ok(());
    }
    // Elan selects the toolchain from the project's cwd; an empty Lake
    // directory avoids loading the workspace.
    let empty = tempfile::tempdir().operation("create toolchain query directory")?;
    let sysroot = PathBuf::from(output(
        "Lean",
        project
            .lake()
            .arg("--dir")
            .arg(empty.path())
            .args(["env", "lean", "--print-prefix"]),
    )?);
    if stock {
        return restore(&sysroot);
    }
    let githash = output(
        "Lean",
        Command::new(sysroot.join("bin/lean")).arg("--githash"),
    )?;
    let overlay = overlays.join(&githash);
    if !overlay.is_dir() {
        return Ok(());
    }
    install(&overlay, &sysroot)
}

/// Make every file of `overlay` the toolchain's, keeping the first stock copy
/// of each. A toolchain that already matches is not written, so a read-only
/// toolchain with the overlay unpacked in place works.
fn install(overlay: &Path, sysroot: &Path) -> Result<()> {
    let mut files = Vec::new();
    collect(overlay, Path::new(""), &mut files)?;
    if all_installed(overlay, sysroot, &files)? {
        return Ok(());
    }
    let _lock = lock(sysroot)?;
    for file in &files {
        let target = sysroot.join(file);
        if same(&overlay.join(file), &target)? {
            continue;
        }
        let kept = sysroot.join(STOCK).join(file);
        if !kept.exists() {
            if !target.is_file() {
                return Err(invalid(format!(
                    "the Lake overlay adds {}, which the toolchain lacks",
                    file.display()
                )));
            }
            replace(&target, &kept)?;
        }
        replace(&overlay.join(file), &target)?;
    }
    Ok(())
}

/// Put back every stock file an overlay replaced.
fn restore(sysroot: &Path) -> Result<()> {
    let kept = sysroot.join(STOCK);
    if !kept.exists() {
        return Ok(());
    }
    let _lock = lock(sysroot)?;
    let mut files = Vec::new();
    collect(&kept, Path::new(""), &mut files)?;
    for file in &files {
        replace(&kept.join(file), &sysroot.join(file))?;
    }
    fs::remove_dir_all(&kept).operation(format!("remove {}", kept.display()))
}

fn all_installed(overlay: &Path, sysroot: &Path, files: &[PathBuf]) -> Result<bool> {
    for file in files {
        if !same(&overlay.join(file), &sysroot.join(file))? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn lock(sysroot: &Path) -> Result<File> {
    let path = sysroot.join(LOCK);
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .operation(format!("open {}", path.display()))?;
    file.lock().operation(format!("lock {}", path.display()))?;
    Ok(file)
}

fn collect(root: &Path, relative: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let directory = root.join(relative);
    let entries = fs::read_dir(&directory).operation(format!("list {}", directory.display()))?;
    for entry in entries {
        let entry = entry.operation(format!("list {}", directory.display()))?;
        let path = relative.join(entry.file_name());
        if entry
            .file_type()
            .operation("read overlay entry type")?
            .is_dir()
        {
            collect(root, &path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

fn same(a: &Path, b: &Path) -> Result<bool> {
    let b_bytes = match fs::read(b) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(source) => return Err(source).operation(format!("read {}", b.display())),
    };
    Ok(fs::read(a).operation(format!("read {}", a.display()))? == b_bytes)
}

/// Write `source`'s bytes and mode to `target` in one rename, so a Lake that
/// is running or starting sees the old file or the new one, never a part.
fn replace(source: &Path, target: &Path) -> Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| invalid(format!("{} has no parent", target.display())))?;
    fs::create_dir_all(parent).operation(format!("create {}", parent.display()))?;
    let mut staged =
        tempfile::NamedTempFile::new_in(parent).operation(format!("stage {}", target.display()))?;
    let mut from = File::open(source).operation(format!("open {}", source.display()))?;
    io::copy(&mut from, &mut staged).operation(format!("copy {}", source.display()))?;
    let mode = fs::metadata(source)
        .operation(format!("read {}", source.display()))?
        .permissions();
    staged
        .as_file()
        .set_permissions(mode)
        .operation(format!("set the mode of {}", target.display()))?;
    staged
        .persist(target)
        .map_err(|e| e.error)
        .operation(format!("replace {}", target.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, file: &str, bytes: &str) {
        let path = root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn read(root: &Path, file: &str) -> String {
        fs::read_to_string(root.join(file)).unwrap()
    }

    #[test]
    fn install_keeps_stock_files_and_restore_puts_them_back() {
        let overlay = tempfile::tempdir().unwrap();
        let sysroot = tempfile::tempdir().unwrap();
        write(sysroot.path(), "bin/lake", "stock lake");
        write(sysroot.path(), "lib/lean/Lake/Build/Run.olean", "stock run");
        write(sysroot.path(), "lib/lean/Lake/Build/Job.olean", "stock job");
        write(overlay.path(), "bin/lake", "patched lake");
        write(
            overlay.path(),
            "lib/lean/Lake/Build/Run.olean",
            "patched run",
        );

        install(overlay.path(), sysroot.path()).unwrap();
        assert_eq!(read(sysroot.path(), "bin/lake"), "patched lake");
        assert_eq!(
            read(sysroot.path(), "lib/lean/Lake/Build/Run.olean"),
            "patched run"
        );
        assert_eq!(
            read(sysroot.path(), "lib/lean/Lake/Build/Job.olean"),
            "stock job"
        );

        // A newer overlay replaces the installed one; the kept copy stays stock.
        write(overlay.path(), "bin/lake", "newer lake");
        install(overlay.path(), sysroot.path()).unwrap();
        assert_eq!(read(sysroot.path(), "bin/lake"), "newer lake");
        assert_eq!(
            read(sysroot.path(), &format!("{STOCK}/bin/lake")),
            "stock lake"
        );

        restore(sysroot.path()).unwrap();
        assert_eq!(read(sysroot.path(), "bin/lake"), "stock lake");
        assert_eq!(
            read(sysroot.path(), "lib/lean/Lake/Build/Run.olean"),
            "stock run"
        );
        assert!(!sysroot.path().join(STOCK).exists());
    }

    #[test]
    fn installed_toolchain_is_not_written() {
        let overlay = tempfile::tempdir().unwrap();
        let sysroot = tempfile::tempdir().unwrap();
        write(overlay.path(), "bin/lake", "patched lake");
        write(sysroot.path(), "bin/lake", "patched lake");
        install(overlay.path(), sysroot.path()).unwrap();
        assert!(!sysroot.path().join(LOCK).exists());
        assert!(!sysroot.path().join(STOCK).exists());
    }

    #[test]
    fn overlay_must_not_add_files() {
        let overlay = tempfile::tempdir().unwrap();
        let sysroot = tempfile::tempdir().unwrap();
        write(sysroot.path(), "lib/lean/Lake.olean", "stock");
        write(overlay.path(), "lib/lean/Lake/New.olean", "new");
        let error = install(overlay.path(), sysroot.path()).unwrap_err();
        assert!(error.to_string().contains("adds lib/lean/Lake/New.olean"));
    }
}
