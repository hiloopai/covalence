//! The Lean toolchain the project names, which every Lake run waits for.
//!
//! A `lean-toolchain` naming a Lean release or nightly
//! (`leanprover/lean4:v4.22.0`, `leanprover/lean4:nightly-2026-11-05`) runs
//! from elan's directory for it, `$ELAN_HOME/toolchains/<elan's name>`
//! (`~/.elan` without `ELAN_HOME`), without elan. When that directory is
//! missing, `get`, `dependencies` and `build` install it while they check the
//! Git dependencies out and download the seed: from the pool, else from the
//! release archive elan downloads. `put` publishes that archive to the pool.
//! `LAKE`, or any other toolchain name, runs that Lake as found.
//!
//! In the pool, under `lake/toolchains/`:
//!
//! - `<os>-<arch>/<elan's name>`: the JSON [`Record`] of the release.
//! - `cas/sha2-256/<digest>/bytes.zst`: the release's `.tar.zst` archive
//!   byte for byte, which is a Zstandard frame of the tar under whose SHA-256
//!   CID it is kept. Every read checks it.
//!
//! The record names `lean --version`, so a restore knows its pool scope, and
//! downloads its seed, before the toolchain arrives.

use std::{
    env,
    ffi::OsString,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, OnceLock},
    thread,
    time::Instant,
};

use covalence_kv_trait::Kv;
use covalence_lib_cid::Sha256;
use covalence_lib_data::Bytes;
use covalence_lib_serde::{Deserialize, Serialize};

use crate::{
    blobs::Blobs,
    error::{Error, IoResult, Result, encode, invalid, json},
    overlay,
    project::{output, run},
    storage,
};

/// The index elan reads for the releases of `leanprover/lean4` and its
/// nightlies.
const RELEASES: &str = "https://release.lean-lang.org";
const CAS: &str = "lake/toolchains/cas";
/// 8 MiB ranges of the archive read at once.
const PARTS: usize = 16;

/// A release toolchain, as elan installs it.
#[derive(Debug, PartialEq, Eq)]
struct Release {
    /// The `lean-toolchain` line, such as `leanprover/lean4:v4.22.0`.
    name: String,
    /// The GitHub repository elan downloads it from.
    origin: &'static str,
    tag: String,
    /// `$ELAN_HOME/toolchains/<elan's name>`.
    home: PathBuf,
}

impl Release {
    /// The release `name` names, if it names a Lean release or nightly.
    fn parse(name: &str, toolchains: &Path) -> Option<Self> {
        let tag = name.strip_prefix("leanprover/lean4:")?;
        let origin = if nightly(tag) {
            "leanprover/lean4-nightly"
        } else if stable(tag) {
            "leanprover/lean4"
        } else {
            return None;
        };
        Some(Self {
            name: name.to_owned(),
            origin,
            tag: tag.to_owned(),
            // elan writes `/` as `--` and `:` as `---`.
            home: toolchains.join(format!("{}---{tag}", origin.replace('/', "--"))),
        })
    }

    fn directory(&self) -> &str {
        self.home
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
    }

    fn key(&self) -> Result<String> {
        Ok(format!(
            "lake/toolchains/{}/{}",
            platform()?.0,
            self.directory()
        ))
    }

    /// The release's `.tar.zst` archive for this platform, as elan finds it.
    fn url(&self) -> Result<String> {
        let index: serde_json::Value = json(output("curl", &mut curl(RELEASES))?.as_bytes())?;
        let suffix = format!("-{}.tar.zst", platform()?.1);
        let url = index
            .as_object()
            .into_iter()
            .flat_map(|channels| channels.values())
            .filter_map(serde_json::Value::as_array)
            .flatten()
            .filter(|release| release["name"].as_str() == Some(&self.tag))
            .filter_map(|release| release["assets"].as_array())
            .flatten()
            .filter(|asset| {
                asset["name"]
                    .as_str()
                    .is_some_and(|name| name.ends_with(&suffix))
            })
            .find_map(|asset| asset["browser_download_url"].as_str())
            .ok_or_else(|| {
                invalid(format!(
                    "{RELEASES} lists no {suffix} archive of {}",
                    self.name
                ))
            })?;
        if !url.starts_with("https://") {
            return Err(invalid(format!(
                "{RELEASES} names a non-HTTPS archive {url}"
            )));
        }
        Ok(url.to_owned())
    }
}

fn nightly(tag: &str) -> bool {
    tag.strip_prefix("nightly-").is_some_and(|date| {
        date.len() == 10
            && date.bytes().enumerate().all(|(index, b)| {
                if index == 4 || index == 7 {
                    b == b'-'
                } else {
                    b.is_ascii_digit()
                }
            })
    })
}

fn stable(tag: &str) -> bool {
    let Some(version) = tag.strip_prefix('v') else {
        return false;
    };
    let (version, candidate) = version.split_once("-rc").unwrap_or((version, "1"));
    let numbers = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    numbers(candidate) && version.split('.').count() == 3 && version.split('.').all(numbers)
}

/// The pool's platform, and elan's name for it in release archives.
fn platform() -> Result<(String, String)> {
    let (os, informal) = match env::consts::OS {
        "linux" => ("linux", "linux"),
        "macos" => ("darwin", "darwin"),
        os => return Err(invalid(format!("no Lean release archive for {os}"))),
    };
    let informal = match env::consts::ARCH {
        "x86_64" => informal.to_owned(),
        "aarch64" => format!("{informal}_aarch64"),
        arch => return Err(invalid(format!("no Lean release archive for {arch}"))),
    };
    Ok((format!("{os}-{}", env::consts::ARCH), informal))
}

fn curl(url: &str) -> Command {
    let mut command = Command::new("curl");
    command
        .args(["--fail", "--location", "--silent", "--show-error"])
        .args([
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--retry",
            "3",
        ])
        .arg(url);
    command
}

/// A release in the pool.
#[derive(Debug, Deserialize, Serialize)]
#[serde(crate = "covalence_lib_serde::serde", deny_unknown_fields)]
struct Record {
    /// The archive these bytes are.
    url: String,
    /// The SHA-256 CID of the tar the archive decodes to.
    tar: String,
    /// What the toolchain's `lean --version` prints.
    version: String,
}

/// How this run gets its toolchain.
enum Plan {
    /// As found.
    Present,
    /// From the pool.
    Pool(Arc<dyn Kv>, Record),
    /// From the release archive.
    Release,
}

/// The Lake this command runs, and the install every Lake run waits for.
#[derive(Default)]
pub(crate) struct Lean {
    /// The release `lean-toolchain` names; `None` runs `LAKE`, else `lake`
    /// from `PATH`.
    release: Option<Release>,
    /// Whether this run installs the toolchain, the bundled `leantar` and the
    /// Lake overlay before Lake runs.
    prepares: bool,
    plan: OnceLock<std::result::Result<Plan, Arc<Error>>>,
    ready: OnceLock<std::result::Result<(), Arc<Error>>>,
}

fn shared<T>(result: &std::result::Result<T, Arc<Error>>) -> Result<&T> {
    result.as_ref().map_err(|source| Error::Toolchain {
        source: source.clone(),
    })
}

/// The `leantar` beside this executable, which the release archive bundles.
fn bundled_leantar() -> Result<Option<PathBuf>> {
    let path = env::current_exe()
        .operation("locate cov-lake-cache")?
        .with_file_name("leantar")
        .with_extension(env::consts::EXE_EXTENSION);
    Ok(path.is_file().then_some(path))
}

impl Lean {
    /// The Lake for the project at `root`. With `prepares`, the first Lake
    /// run waits for [`Lean::ready`].
    pub fn new(root: &Path, prepares: bool) -> Result<Self> {
        let release = match env::var_os("LAKE") {
            Some(_) => None,
            None => match name(root)? {
                Some(name) => {
                    let elan = env::var_os("ELAN_HOME")
                        .filter(|home| !home.is_empty())
                        .map(PathBuf::from)
                        .or_else(|| {
                            env::var_os("HOME").map(|home| PathBuf::from(home).join(".elan"))
                        });
                    // Elsewhere, elan installs what Lake needs.
                    elan.filter(|_| platform().is_ok())
                        .and_then(|elan| Release::parse(&name, &elan.join("toolchains")))
                }
                None => None,
            },
        };
        Ok(Self {
            release,
            prepares,
            ..Self::default()
        })
    }

    /// The Lake executable.
    pub fn program(&self) -> OsString {
        match &self.release {
            Some(release) => release.home.join("bin/lake").into(),
            None => env::var_os("LAKE").unwrap_or_else(|| "lake".into()),
        }
    }

    fn plan(&self) -> Result<&Plan> {
        shared(self.plan.get_or_init(|| {
            let plan = || {
                let Some(release) = self
                    .release
                    .as_ref()
                    .filter(|r| self.prepares && !r.home.is_dir())
                else {
                    return Ok(Plan::Present);
                };
                let kv = storage::store_with(PARTS)?;
                let key = release.key()?;
                let record = kv.get(key.as_bytes()).map_err(|source| Error::Store {
                    operation: format!("read {key}"),
                    source,
                })?;
                Ok(match record {
                    Some(bytes) => Plan::Pool(kv, json(&bytes)?),
                    None => Plan::Release,
                })
            };
            plan().map_err(Arc::new)
        }))
    }

    /// `lean --version` and the `leantar` executable a toolchain still
    /// arriving from the pool will have: its record's, and the bundled one.
    /// `None` when it is installed or comes from elsewhere.
    pub fn predicted(&self) -> Result<Option<(String, PathBuf)>> {
        Ok(match self.plan()? {
            Plan::Pool(_, record) => {
                bundled_leantar()?.map(|leantar| (record.version.clone(), leantar))
            }
            Plan::Present | Plan::Release => None,
        })
    }

    /// Wait until the toolchain is installed, with the bundled `leantar` and
    /// Lake overlay, when this run prepares it; the first caller installs it.
    pub fn ready(&self, root: &Path) -> Result<()> {
        if !self.prepares {
            return Ok(());
        }
        shared(
            self.ready
                .get_or_init(|| self.prepare(root).map_err(Arc::new)),
        )
        .copied()
    }

    fn prepare(&self, root: &Path) -> Result<()> {
        let started = Instant::now();
        let sysroot = match &self.release {
            Some(release) => {
                match self.plan()? {
                    Plan::Present => {}
                    Plan::Pool(kv, record) => {
                        let bytes = install(release, |directory| from_pool(kv, record, directory))?;
                        let version = output(
                            "Lean",
                            Command::new(release.home.join("bin/lean")).arg("--version"),
                        )?;
                        if version != record.version {
                            return Err(invalid(format!(
                                "the pool's {} reports {version:?}, its record {:?}",
                                release.name, record.version
                            )));
                        }
                        println!(
                            "Lake cache: installed Lean {} from the pool ({:.1} GB unpacked) in {:.1} s",
                            release.tag,
                            bytes as f64 / 1e9,
                            started.elapsed().as_secs_f64()
                        );
                    }
                    Plan::Release => {
                        let url = release.url()?;
                        install(release, |directory| from_release(&url, directory))?;
                        println!(
                            "Lake cache: installed Lean {} from {url} in {:.1} s",
                            release.tag,
                            started.elapsed().as_secs_f64()
                        );
                    }
                }
                release.home.clone()
            }
            None => {
                // Elan selects the toolchain from the project's cwd; an empty
                // Lake directory avoids loading the workspace.
                let empty = tempfile::tempdir().operation("create toolchain query directory")?;
                PathBuf::from(output(
                    "Lean",
                    Command::new(self.program())
                        .current_dir(root)
                        .arg("--dir")
                        .arg(empty.path())
                        .args(["env", "lean", "--print-prefix"]),
                )?)
            }
        };
        if let Some(leantar) = bundled_leantar()? {
            let target = sysroot
                .join("bin/leantar")
                .with_extension(env::consts::EXE_EXTENSION);
            if !overlay::same(&leantar, &target)? {
                overlay::replace(&leantar, &target)?;
            }
        }
        overlay::apply(&sysroot)?;
        if let (Some(release), Some(path)) = (&self.release, env::var_os("GITHUB_PATH")) {
            // Later steps of the job find `lake` and `lean` without elan.
            let mut file = File::options()
                .append(true)
                .open(&path)
                .operation("open GITHUB_PATH")?;
            writeln!(file, "{}", release.home.join("bin").display())
                .operation("write GITHUB_PATH")?;
        }
        Ok(())
    }

    /// Publish the release archive of the toolchain this run used, unless the
    /// pool holds it.
    pub fn publish(&self, kv: &Arc<dyn Kv>) -> Result<()> {
        let Some(release) = &self.release else {
            return Ok(());
        };
        let key = release.key()?;
        let store_error = |operation: &str| {
            let operation = format!("{operation} {key}");
            move |source| Error::Store { operation, source }
        };
        if kv.exists(key.as_bytes()).map_err(store_error("inspect"))? {
            return Ok(());
        }
        let started = Instant::now();
        let version = output(
            "Lean",
            Command::new(release.home.join("bin/lean")).arg("--version"),
        )?;
        let url = release.url()?;
        let archive = tempfile::NamedTempFile::new().operation("stage Lean release archive")?;
        run("curl", curl(&url).arg("--output").arg(archive.path()))?;
        let (tar, bytes) = Blobs::new(kv.clone(), CAS).put_frame(archive.path())?;
        let record = Record {
            url,
            tar: tar.to_string(),
            version,
        };
        // The archive first, so a record never names a missing one.
        kv.put(key.as_bytes(), Bytes::copy_from_slice(&encode(&record)?))
            .map_err(store_error("publish"))?;
        println!(
            "Lake cache: published Lean {} to the pool ({bytes} bytes) in {:.1} s",
            release.tag,
            started.elapsed().as_secs_f64()
        );
        Ok(())
    }
}

/// The toolchain name elan would use in `root`: `ELAN_TOOLCHAIN`, else the
/// nearest `lean-toolchain` in it or above it.
fn name(root: &Path) -> Result<Option<String>> {
    if let Some(name) = env::var_os("ELAN_TOOLCHAIN") {
        return Ok(name.into_string().ok());
    }
    for directory in root.ancestors() {
        match fs::read_to_string(directory.join("lean-toolchain")) {
            Ok(name) => return Ok(Some(name.trim().to_owned())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(Error::Io {
                    operation: "read Lean toolchain".into(),
                    source,
                });
            }
        }
    }
    Ok(None)
}

/// Fill a new directory beside the release's home with `fill`, then move it
/// there, as elan does; another process that got there first wins.
fn install<T>(release: &Release, fill: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
    let toolchains = release
        .home
        .parent()
        .ok_or_else(|| invalid("elan's toolchain directory has no parent"))?;
    fs::create_dir_all(toolchains).operation(format!("create {}", toolchains.display()))?;
    let staging = tempfile::Builder::new()
        .prefix(&format!(".{}.", release.directory()))
        .tempdir_in(toolchains)
        .operation("create toolchain staging directory")?;
    let filled = fill(staging.path())?;
    match fs::rename(staging.path(), &release.home) {
        Ok(()) => {
            // Its path is the home now; keep it from deletion.
            let _home = staging.keep();
            Ok(filled)
        }
        Err(_) if release.home.is_dir() => Ok(filled),
        Err(source) => Err(Error::Io {
            operation: format!("install {}", release.home.display()),
            source,
        }),
    }
}

/// Download, check and unpack the pool's archive into `directory`; the
/// length of the tar it unpacked.
fn from_pool(kv: &Arc<dyn Kv>, record: &Record, directory: &Path) -> Result<u64> {
    let tar: Sha256 = record
        .tar
        .parse()
        .map_err(|_| invalid("invalid toolchain record CID"))?;
    let blobs = Blobs::new(kv.clone(), CAS);
    let (reader, writer) = io::pipe().operation("open toolchain pipe")?;
    thread::scope(|scope| {
        let unpacking = scope.spawn(move || unpack(reader, directory));
        let downloaded = {
            let mut writer = writer;
            blobs.get_to(&tar, &mut writer)
        };
        let unpacked = unpacking.join().unwrap_or(Err(Error::Worker));
        match (downloaded, unpacked) {
            (Ok(Some(bytes)), Ok(())) => Ok(bytes),
            (Ok(None), _) => Err(invalid(format!("the pool lacks Lean archive {tar}"))),
            // Either side failing breaks the other's stream; report both.
            (Err(download), Err(unpack)) => Err(invalid(format!("{download}; {unpack}"))),
            (Err(error), Ok(())) | (Ok(Some(_)), Err(error)) => Err(error),
        }
    })
}

/// Download and unpack the release archive at `url` into `directory`.
fn from_release(url: &str, directory: &Path) -> Result<()> {
    let mut child = curl(url)
        .stdout(Stdio::piped())
        .spawn()
        .operation("start curl")?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| invalid("curl has no output"))?;
    let unpacked = zstd::stream::read::Decoder::new(stdout)
        .operation("start Zstandard")
        .and_then(|decoder| unpack(decoder, directory));
    let status = child.wait().operation("wait for curl")?;
    unpacked?;
    if !status.success() {
        return Err(Error::Process {
            program: "curl",
            status,
        });
    }
    Ok(())
}

/// Write a Lean release's tar stream under `directory` without its top
/// folder, as elan does. Only files, folders and symbolic links to paths
/// within the folder they sit in or below are allowed.
fn unpack(reader: impl Read, directory: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    let inside = |path: &Path| path.components().all(|c| matches!(c, Component::Normal(_)));
    for entry in archive.entries().operation("read Lean archive")? {
        let mut entry = entry.operation("read Lean archive entry")?;
        let path = entry
            .path()
            .operation("read Lean archive path")?
            .into_owned();
        let mut components = path.components();
        components.next();
        let relative = components.as_path();
        if relative.as_os_str().is_empty() {
            continue;
        }
        let allowed = inside(relative)
            && match entry.header().entry_type() {
                tar::EntryType::Regular | tar::EntryType::Directory => true,
                tar::EntryType::Symlink => entry
                    .link_name()
                    .operation("read Lean archive link")?
                    .is_some_and(|target| inside(&target)),
                _ => false,
            };
        if !allowed {
            return Err(invalid(format!(
                "the Lean archive holds an unexpected entry {}",
                path.display()
            )));
        }
        let destination = directory.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).operation(format!("create {}", parent.display()))?;
        }
        entry
            .unpack(&destination)
            .operation(format!("write {}", destination.display()))?;
    }
    // Read the archive's padding too, so the writer never meets a closed pipe.
    io::copy(&mut archive.into_inner(), &mut io::sink()).operation("read Lean archive")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use covalence_kv_memory::MemoryKv;

    #[test]
    fn release_names_map_to_elans_directories() {
        let elan = Path::new("/e/toolchains");
        let nightly = Release::parse("leanprover/lean4:nightly-2026-11-05", elan).unwrap();
        assert_eq!(nightly.origin, "leanprover/lean4-nightly");
        assert_eq!(
            nightly.home,
            elan.join("leanprover--lean4-nightly---nightly-2026-11-05")
        );
        let stable = Release::parse("leanprover/lean4:v4.22.0-rc3", elan).unwrap();
        assert_eq!(stable.home, elan.join("leanprover--lean4---v4.22.0-rc3"));
        assert_eq!(stable.tag, "v4.22.0-rc3");
        for other in [
            "leanprover/lean4:stable",
            "leanprover/lean4:nightly",
            "leanprover/lean4:nightly-2026-11",
            "leanprover/lean4:v4.22",
            "leanprover/lean4:v4.22.0-rc",
            "leanprover/lean4:v4.22.0/../x",
            "v4.22.0",
            "other/lean4:v4.22.0",
            "/opt/lean",
        ] {
            assert_eq!(Release::parse(other, elan), None, "{other}");
        }
    }

    fn release_archive(entries: &[(&str, tar::EntryType, &[u8], Option<&str>)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, kind, bytes, link) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_mode(if path.contains("/bin/") { 0o755 } else { 0o644 });
            header.set_size(bytes.len() as u64);
            if let Some(link) = link {
                header.set_link_name(link).unwrap();
            }
            // Bypass the builder's path checks to write hostile paths too.
            header.as_gnu_mut().unwrap().name[..path.len()].copy_from_slice(path.as_bytes());
            header.set_cksum();
            builder.append(&header, *bytes).unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn fake_release() -> Vec<u8> {
        use tar::EntryType::{Directory, Regular, Symlink};
        release_archive(&[
            ("lean-4-linux/", Directory, b"", None),
            ("lean-4-linux/bin/", Directory, b"", None),
            ("lean-4-linux/bin/lean", Regular, b"lean", None),
            ("lean-4-linux/lib/libz.so.1", Regular, b"z", None),
            ("lean-4-linux/lib/libz.so", Symlink, b"", Some("libz.so.1")),
        ])
    }

    #[cfg(unix)]
    #[test]
    fn a_release_unpacks_without_its_top_folder() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        unpack(&fake_release()[..], directory.path()).unwrap();
        let lean = directory.path().join("bin/lean");
        assert_eq!(fs::read(&lean).unwrap(), b"lean");
        assert_eq!(
            fs::metadata(&lean).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::read_link(directory.path().join("lib/libz.so")).unwrap(),
            Path::new("libz.so.1")
        );
        assert_eq!(
            fs::read(directory.path().join("lib/libz.so")).unwrap(),
            b"z"
        );
    }

    #[test]
    fn entries_that_leave_the_toolchain_are_rejected() {
        use tar::EntryType::{Link, Regular, Symlink};
        for entries in [
            vec![("lean/../escape", Regular, &b"x"[..], None)],
            vec![("lean/lib/a", Symlink, &b""[..], Some("../../escape"))],
            vec![("lean/lib/a", Symlink, &b""[..], Some("/etc/passwd"))],
            vec![("lean/lib/a", Link, &b""[..], Some("lean/lib/b"))],
        ] {
            let directory = tempfile::tempdir().unwrap();
            let error = unpack(&release_archive(&entries)[..], directory.path()).unwrap_err();
            assert!(error.to_string().contains("unexpected entry"), "{error}");
        }
    }

    #[test]
    fn a_pool_install_checks_the_archive_and_never_leaves_a_partial_toolchain() {
        let kv = MemoryKv::default().into_dyn();
        let work = tempfile::tempdir().unwrap();
        let tar = fake_release();
        let archive = work.path().join("lean.tar.zst");
        fs::write(&archive, zstd::stream::encode_all(&tar[..], 19).unwrap()).unwrap();
        let (cid, _) = Blobs::new(kv.clone(), CAS).put_frame(&archive).unwrap();
        let release = Release::parse(
            "leanprover/lean4:nightly-2026-11-05",
            &work.path().join("elan/toolchains"),
        )
        .unwrap();
        let record = Record {
            url: "https://example.invalid/lean.tar.zst".into(),
            tar: cid.to_string(),
            version: "Lean (version 4)".into(),
        };
        let left = || {
            fs::read_dir(release.home.parent().unwrap())
                .unwrap()
                .count()
        };
        // A record naming an archive the pool lacks fails, and so do bytes
        // that fail their check; neither leaves anything behind.
        let missing = Record {
            tar: Sha256::of(b"other").to_string(),
            ..record
        };
        assert!(install(&release, |directory| from_pool(&kv, &missing, directory)).is_err());
        let digest = data_encoding::BASE32_NOPAD
            .encode(cid.digest())
            .to_ascii_lowercase();
        let key = format!("{CAS}/sha2-256/{digest}/bytes.zst");
        // A well-formed archive of other bytes unpacks, then fails the check.
        let corrupt = release_archive(&[(
            "lean-4-linux/bin/lean",
            tar::EntryType::Regular,
            b"evil",
            None,
        )]);
        kv.put(
            key.as_bytes(),
            zstd::stream::encode_all(&corrupt[..], 19).unwrap().into(),
        )
        .unwrap();
        let record = Record {
            tar: cid.to_string(),
            ..missing
        };
        assert!(install(&release, |directory| from_pool(&kv, &record, directory)).is_err());
        assert!(!release.home.exists());
        assert_eq!(left(), 0, "a staging directory was left behind");
        kv.put(key.as_bytes(), fs::read(&archive).unwrap().into())
            .unwrap();
        install(&release, |directory| from_pool(&kv, &record, directory)).unwrap();
        assert_eq!(fs::read(release.home.join("bin/lean")).unwrap(), b"lean");
        // A second install finds the first and keeps it.
        install(&release, |directory| from_pool(&kv, &record, directory)).unwrap();
        assert_eq!(left(), 1);
    }
}
