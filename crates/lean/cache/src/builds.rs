//! Build manifests: every file under a workspace's build folders for one
//! source tree, so a restore of that tree places them without running Lake.
//!
//! `build` records one from the clean tree it checked, after Lake recorded
//! every package's outputs, and `put` publishes it under
//! `lake/<scope>/builds/<namespace>/<lake>/<tree>` as Zstandard JSON
//! `{"lake": .., "tree": .., "targets": [..], "files": {<path>: {"link": <artifact>} | {"text": <contents>}}}`.
//! `<tree>` is the Git tree of `HEAD` and `<lake>` the first 16 digits of the
//! digest of the Lake that wrote the traces, which the manifest holds whole;
//! the scope covers the platform and toolchain.
//!
//! A restore whose tree, scope and Lake match, and whose host cache holds
//! every artifact the manifest names, places the files and runs no Lake:
//! outputs as hard links into Lake's artifact cache, as Lake's own no-op
//! makes them, and traces and hash files as recorded. Lake traces name
//! artifacts, not paths, so they hold for any checkout of the tree.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs, io,
    path::{Component, Path, PathBuf},
    time::Instant,
};

use covalence_kv_trait::Kv;
use covalence_lib_data::Bytes;
use covalence_lib_serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    error::{Error, IoResult, Result, encode, invalid, json},
    mapping, overlay,
    project::{Package, Project, digest, output},
    storage::parallel_with,
};

/// Names the build a restore placed, until Lake builds anything else.
const PLACED: &str = ".lake/build/.cov-lake-placed";

/// Forget the placed build before Lake changes the build folders.
pub(crate) fn forget(project: &Project) -> Result<()> {
    match fs::remove_file(project.root.join(PLACED)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Io {
            operation: "forget the placed build".into(),
            source,
        }),
    }
}

/// The largest file a manifest carries inline; Lake's traces and hash files
/// are far smaller.
const TEXT_LIMIT: u64 = 1 << 20;
/// Placement is many small metadata operations, which scale past the
/// transfer width.
const WORKERS: usize = 32;

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(crate = "covalence_lib_serde::serde", rename_all = "lowercase")]
pub(crate) enum Content {
    /// A hard link to this file of Lake's artifact cache.
    Link(String),
    /// A file with these contents.
    Text(String),
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(crate = "covalence_lib_serde::serde", deny_unknown_fields)]
pub(crate) struct Manifest {
    pub lake: String,
    pub tree: String,
    /// The Lake targets the build checked.
    pub targets: Vec<String>,
    /// Every file under the build folders, by path from the workspace root.
    pub files: BTreeMap<String, Content>,
}

fn store_error(operation: impl Into<String>) -> impl FnOnce(covalence_kv_trait::Error) -> Error {
    move |source| Error::Store {
        operation: operation.into(),
        source,
    }
}

/// The digest of the Lake this project runs, known before a toolchain still
/// arriving from the pool is installed: the Lean release, whose
/// `lean --version` names its commit, the Lake overlay bundled for it, and a
/// `LAKE` given instead.
pub(crate) fn lake(project: &Project) -> Result<String> {
    let version = match project.lean.predicted()? {
        Some((version, _)) => version,
        None => {
            let empty = tempfile::tempdir().operation("create toolchain query directory")?;
            output(
                "Lean",
                project
                    .lake()?
                    .arg("--dir")
                    .arg(empty.path())
                    .args(["env", "lean", "--version"]),
            )?
        }
    };
    let mut hash = Sha256::new();
    hash.update(version.as_bytes());
    hash.update([0]);
    if let Some(lake) = env::var_os("LAKE") {
        let path = Path::new(&lake);
        if path.is_file() {
            hash.update(digest(path)?.as_bytes());
        } else {
            hash.update(lake.as_encoded_bytes());
        }
    }
    hash.update([0]);
    let githash = version
        .split("commit ")
        .nth(1)
        .and_then(|rest| rest.get(..40))
        .filter(|githash| mapping::hex(githash, 40));
    if let Some(overlay) = githash.map(overlay::bundled).transpose()?.flatten() {
        for file in [
            Path::new("bin/lake").with_extension(env::consts::EXE_EXTENSION),
            Path::new("lib/lean/libLake_shared").with_extension(env::consts::DLL_EXTENSION),
        ] {
            let path = overlay.join(&file);
            hash.update(file.to_string_lossy().as_bytes());
            hash.update([0]);
            if path.exists() {
                hash.update(digest(&path)?.as_bytes());
            }
            hash.update([0]);
        }
    }
    Ok(data_encoding::HEXLOWER.encode(&hash.finalize()))
}

/// The Git tree of `HEAD`, when the worktree is clean; any change, untracked
/// files included, may change what Lake builds. So may an ignored Lean file,
/// such as a generated module, outside the `.lake` folders that hold the
/// dependencies and the builds.
pub(crate) fn tree(project: &Project) -> Result<Option<String>> {
    if changed(project)?.is_some() {
        return Ok(None);
    }
    let tree = project.git(&["rev-parse", "HEAD^{tree}"])?;
    if !mapping::hex(&tree, 40) {
        return Err(invalid("expected a 40-digit Git tree"));
    }
    Ok(Some(tree))
}

/// The first change that leaves the worktree unlike `HEAD`'s tree to Lake.
pub(crate) fn changed(project: &Project) -> Result<Option<String>> {
    if let Some(first) = project.git(&["status", "--porcelain"])?.lines().next() {
        return Ok(Some(first.to_owned()));
    }
    // The pathspec spares Git the build folders; Git still lists a nested
    // repository, such as a dependency's checkout, as a folder, so `.lake`
    // folders are left out here too.
    let ignored = project.git(&[
        "ls-files",
        "--others",
        "--ignored",
        "--exclude-standard",
        "--",
        "*.lean",
        ":(exclude,glob)**/.lake/**",
    ])?;
    Ok(ignored
        .lines()
        .find(|path| !path.split('/').any(|part| part == ".lake"))
        .map(str::to_owned))
}

pub(crate) fn key(project: &Project, scope: &str, lake: &str, tree: &str) -> Result<String> {
    let namespace = project.namespace();
    for name in [scope, &namespace] {
        mapping::segment_path(name)?;
    }
    if !mapping::hex(lake, 64) || !mapping::hex(tree, 40) {
        return Err(invalid(
            "a build manifest is keyed by a Lake digest and a Git tree",
        ));
    }
    // A manifest holds the whole digest, so a reader checks the rest; the
    // prefix keeps keys short.
    Ok(format!(
        "lake/{scope}/builds/{namespace}/{}/{tree}",
        &lake[..16]
    ))
}

/// The published manifest of this clean tree's build by `lake`, if it
/// checked every one of `targets`.
pub(crate) fn find(
    project: &Project,
    kv: &dyn Kv,
    scope: &str,
    lake: &str,
    targets: &[String],
) -> Result<Option<Manifest>> {
    let Some(tree) = tree(project)? else {
        return Ok(None);
    };
    Ok(
        Manifest::get(kv, &key(project, scope, lake, &tree)?)?.filter(|manifest| {
            manifest.tree == tree && manifest.lake == lake && manifest.covers(targets)
        }),
    )
}

/// A package's build folder from the workspace root, its `..` resolved
/// (path dependencies may name each other through their parents); `None`
/// when it lies outside the workspace.
fn folder(package: &Package) -> Option<PathBuf> {
    let mut folder = PathBuf::new();
    for part in package.dir.join(".lake/build").components() {
        match part {
            Component::Normal(name) => folder.push(name),
            Component::CurDir => {}
            Component::ParentDir => {
                if !folder.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(folder)
}

/// Every file under `directory`; `None` if it holds anything else, such as a
/// symbolic link, which a manifest does not describe.
fn walk(directory: &Path) -> Result<Option<Vec<(PathBuf, fs::Metadata)>>> {
    let mut found = Vec::new();
    let mut pending = vec![directory.to_owned()];
    while let Some(directory) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(Error::Io {
                    operation: format!("read {}", directory.display()),
                    source,
                });
            }
        };
        for entry in entries {
            let entry = entry.operation("read build folder")?;
            let metadata = entry.metadata().operation("read build entry metadata")?;
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                found.push((entry.path(), metadata));
            } else {
                return Ok(None);
            }
        }
    }
    Ok(Some(found))
}

impl Manifest {
    /// The build folders of `packages` under the project root, each output a
    /// hard link into `artifacts`, Lake's artifact cache, and every other file
    /// small text. `None`, with the reason printed, for anything else.
    #[cfg(unix)]
    pub fn record(
        project: &Project,
        packages: &[Package],
        artifacts: &Path,
        lake: String,
        tree: String,
        targets: &[String],
    ) -> Result<Option<Self>> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if let Some(package) = packages.iter().find(|package| folder(package).is_none()) {
            println!(
                "Lake cache: no build manifest: {} lies outside the workspace",
                package.name
            );
            return Ok(None);
        }
        let mut cached = BTreeMap::new();
        let mut by_hash: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for entry in fs::read_dir(artifacts).operation("read Lake artifact cache")? {
            let entry = entry.operation("read Lake artifact cache")?;
            let metadata = entry.metadata().operation("read Lake artifact metadata")?;
            if let Some(name) = entry.file_name().to_str() {
                cached.insert((metadata.dev(), metadata.ino()), name.to_owned());
                if let Some((hash, _)) = name.split_once('.') {
                    by_hash
                        .entry(hash.to_owned())
                        .or_default()
                        .push(name.to_owned());
                }
            }
        }
        // Lake links an output it built into its cache only when the cache
        // lacks that artifact. One rebuilt with the bytes of an artifact the
        // cache already holds stays a separate file: it is recorded as that
        // artifact, found by the hash Lake wrote beside it.
        let same_artifact = |path: &Path, len: u64| -> Result<Option<String>> {
            let Ok(hash) = fs::read_to_string(format!("{}.hash", path.display())) else {
                return Ok(None);
            };
            let Some(names) = by_hash.get(hash.trim()) else {
                return Ok(None);
            };
            let mut bytes = None;
            for name in names {
                let candidate = artifacts.join(name);
                if fs::metadata(&candidate).is_ok_and(|metadata| metadata.len() == len) {
                    let built = match &bytes {
                        Some(built) => built,
                        None => bytes
                            .insert(fs::read(path).operation(format!("read {}", path.display()))?),
                    };
                    let cached =
                        fs::read(&candidate).operation(format!("read {}", candidate.display()))?;
                    if cached == *built {
                        return Ok(Some(name.clone()));
                    }
                }
            }
            Ok(None)
        };
        let mut files = BTreeMap::new();
        let mut executables = 0;
        for folder in packages.iter().filter_map(folder) {
            let Some(found) = walk(&project.root.join(&folder))? else {
                println!(
                    "Lake cache: no build manifest: {} holds more than files",
                    folder.display()
                );
                return Ok(None);
            };
            for (path, metadata) in found {
                let relative = path
                    .strip_prefix(&project.root)
                    .ok()
                    .and_then(Path::to_str)
                    .map(|path| path.replace(std::path::MAIN_SEPARATOR, "/"));
                let Some(relative) = relative else {
                    println!(
                        "Lake cache: no build manifest: {} is not UTF-8",
                        path.display()
                    );
                    return Ok(None);
                };
                // Lake's packed archives serve only a recording, which packs
                // or reuses them again; seeds hold unpacked files instead.
                if relative == PLACED
                    || relative.ends_with(".ltar")
                    || relative.ends_with(".ltar.hash")
                {
                    continue;
                }
                // Executables stay out, as seeds leave them to the pool:
                // Lake links one when a target needs it.
                if path
                    .strip_prefix(project.root.join(&folder).join("bin"))
                    .is_ok()
                    && metadata.permissions().mode() & 0o111 != 0
                {
                    executables += 1;
                    continue;
                }
                let linked = match cached.get(&(metadata.dev(), metadata.ino())) {
                    Some(name) => Some(name.clone()),
                    None => same_artifact(&path, metadata.len())?,
                };
                let content = match linked {
                    Some(name) => Content::Link(name),
                    None if metadata.len() <= TEXT_LIMIT => {
                        let bytes = fs::read(&path).operation(format!("read {relative}"))?;
                        match String::from_utf8(bytes) {
                            Ok(text) => Content::Text(text),
                            Err(_) => {
                                println!(
                                    "Lake cache: no build manifest: {relative} is neither a cached artifact nor text"
                                );
                                return Ok(None);
                            }
                        }
                    }
                    None => {
                        println!(
                            "Lake cache: no build manifest: {relative} is neither a cached artifact nor small"
                        );
                        return Ok(None);
                    }
                };
                files.insert(relative, content);
            }
        }
        if executables > 0 {
            println!(
                "Lake cache: the build manifest leaves out {executables} executables, which Lake links when a target needs them"
            );
        }
        Ok(Some(Self {
            lake,
            tree,
            targets: targets.to_vec(),
            files,
        }))
    }

    #[cfg(not(unix))]
    pub fn record(
        _: &Project,
        _: &[Package],
        _: &Path,
        _: String,
        _: String,
        _: &[String],
    ) -> Result<Option<Self>> {
        Ok(None)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        fs::write(path, encode(self)?).operation("save build manifest")
    }

    pub fn load(path: &Path) -> Result<Option<Self>> {
        match fs::read(path) {
            Ok(bytes) => json(&bytes).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(Error::Io {
                operation: format!("read {}", path.display()),
                source,
            }),
        }
    }

    /// The published manifest at `key`, if any.
    pub fn get(kv: &dyn Kv, key: &str) -> Result<Option<Self>> {
        let Some(bytes) = kv
            .get(key.as_bytes())
            .map_err(store_error("read build manifest"))?
        else {
            return Ok(None);
        };
        let raw = zstd::decode_all(&bytes[..]).operation("decode build manifest")?;
        json(&raw).map(Some)
    }

    pub fn put(&self, kv: &dyn Kv, key: &str) -> Result<()> {
        let bytes = zstd::encode_all(&encode(self)?[..], 9).operation("encode build manifest")?;
        kv.put(key.as_bytes(), Bytes::from(bytes))
            .map_err(store_error("publish build manifest"))
    }

    /// Whether the build checked every one of `targets`; none asks for no
    /// particular target.
    pub fn covers(&self, targets: &[String]) -> bool {
        targets.iter().all(|target| self.targets.contains(target))
    }

    /// Whether `artifacts` holds every artifact this manifest links.
    pub fn present(&self, artifacts: &Path) -> Result<bool> {
        let names = self
            .files
            .values()
            .filter_map(|content| match content {
                Content::Link(name) => Some(name),
                Content::Text(_) => None,
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let present = parallel_with(WORKERS, &names, |_, name| {
            Ok(artifacts.join(name).is_file())
        })?;
        Ok(present.into_iter().all(|present| present))
    }

    /// Place this manifest's files into the build folders of `packages`,
    /// when they are empty or hold this build already; `false` leaves a
    /// build of another tree to Lake, which updates only what changed. Only
    /// call it when [`Self::present`] holds.
    pub fn place(&self, project: &Project, packages: &[Package], artifacts: &Path) -> Result<bool> {
        let started = Instant::now();
        let marker = project.root.join(PLACED);
        if fs::read_to_string(&marker).is_ok_and(|placed| placed == self.mark()) {
            println!(
                "Lake cache: the build folders hold the build of tree {} already",
                self.tree
            );
            return Ok(true);
        }
        if fs::read_dir(project.root.join(".lake/build"))
            .is_ok_and(|mut entries| entries.next().is_some())
        {
            return Ok(false);
        }
        let folders = packages.iter().filter_map(folder).collect::<Vec<_>>();
        for path in self.files.keys() {
            if !folders
                .iter()
                .any(|folder| Path::new(path).starts_with(folder))
                || path.split('/').any(|part| matches!(part, "" | "." | ".."))
            {
                return Err(invalid(format!(
                    "build manifest names {path}, outside the build folders"
                )));
            }
        }
        for folder in &folders {
            match fs::remove_dir_all(project.root.join(folder)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(Error::Io {
                        operation: format!("remove {}", folder.display()),
                        source,
                    });
                }
            }
        }
        let directories = self
            .files
            .keys()
            .filter_map(|path| Path::new(path).parent())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        parallel_with(WORKERS, &directories, |_, directory| {
            fs::create_dir_all(project.root.join(directory)).operation("create build folder")
        })?;
        let files = self.files.iter().collect::<Vec<_>>();
        parallel_with(WORKERS, &files, |_, (path, content)| {
            let target = project.root.join(path);
            match content {
                // Lake copies when it cannot link, as across file systems.
                Content::Link(name) => fs::hard_link(artifacts.join(name), &target)
                    .or_else(|_| fs::copy(artifacts.join(name), &target).map(|_| ()))
                    .operation(format!("place {path}")),
                Content::Text(text) => fs::write(&target, text).operation(format!("write {path}")),
            }
        })?;
        if let Some(parent) = marker.parent() {
            fs::create_dir_all(parent).operation("create build folder")?;
        }
        fs::write(&marker, self.mark()).operation("mark the placed build")?;
        println!(
            "Lake cache: placed the build of tree {} ({} files) in {:.1} s; Lake has nothing to rebuild",
            self.tree,
            self.files.len(),
            started.elapsed().as_secs_f64()
        );
        Ok(true)
    }

    fn mark(&self) -> String {
        format!("{} {}", self.lake, self.tree)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use super::*;
    use crate::project::Source;

    fn project(root: &Path) -> Project {
        Project {
            root: root.to_owned(),
            workspace: String::new(),
            repository: "o/r".into(),
            record: root.join("record"),
            lean: crate::toolchain::Lean::default(),
        }
    }

    #[test]
    fn a_recorded_build_places_into_another_checkout() {
        let temporary = tempfile::tempdir().unwrap();
        let artifacts = temporary.path().join("cache/artifacts");
        fs::create_dir_all(&artifacts).unwrap();
        fs::write(artifacts.join("0123456789abcdef.olean"), "olean").unwrap();
        let one = temporary.path().join("one");
        let lib = one.join(".lake/packages/dep/.lake/build/lib/lean");
        fs::create_dir_all(&lib).unwrap();
        fs::hard_link(
            artifacts.join("0123456789abcdef.olean"),
            lib.join("A.olean"),
        )
        .unwrap();
        fs::write(lib.join("A.olean.hash"), "0123456789abcdef").unwrap();
        fs::write(lib.join("A.trace"), r#"{"depHash": "1"}"#).unwrap();
        // Rebuilt with the bytes of an artifact already cached: Lake leaves a
        // copy, not a link. Too large to carry inline.
        let large = vec![0xffu8; (TEXT_LIMIT + 1) as usize];
        fs::write(artifacts.join("fedcba9876543210.c"), &large).unwrap();
        let ir = one.join(".lake/packages/dep/.lake/build/ir");
        fs::create_dir_all(&ir).unwrap();
        fs::write(ir.join("A.c"), &large).unwrap();
        fs::write(ir.join("A.c.hash"), "fedcba9876543210").unwrap();
        // An executable Lake linked, which seeds leave to the pool too.
        let bin = one.join(".lake/build/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("tool"), [0xff, 0xfe, 0x00]).unwrap();
        fs::set_permissions(bin.join("tool"), fs::Permissions::from_mode(0o755)).unwrap();
        let packages = [
            Package {
                name: "root".into(),
                source: Source::Repository,
                dir: "".into(),
            },
            Package {
                name: "dep".into(),
                source: Source::Git {
                    repository: "o/d".into(),
                    commit: "a".repeat(40),
                },
                dir: ".lake/packages/dep".into(),
            },
        ];
        let manifest = Manifest::record(
            &project(&one),
            &packages,
            &artifacts,
            "l".into(),
            "t".into(),
            &["A".into()],
        )
        .unwrap()
        .unwrap();
        let prefix = ".lake/packages/dep/.lake/build/lib/lean";
        let ir = ".lake/packages/dep/.lake/build/ir";
        assert_eq!(
            manifest.files,
            BTreeMap::from([
                (
                    format!("{ir}/A.c"),
                    Content::Link("fedcba9876543210.c".into())
                ),
                (
                    format!("{ir}/A.c.hash"),
                    Content::Text("fedcba9876543210".into())
                ),
                (
                    format!("{prefix}/A.olean"),
                    Content::Link("0123456789abcdef.olean".into())
                ),
                (
                    format!("{prefix}/A.olean.hash"),
                    Content::Text("0123456789abcdef".into())
                ),
                (
                    format!("{prefix}/A.trace"),
                    Content::Text(r#"{"depHash": "1"}"#.into())
                ),
            ])
        );
        assert!(manifest.covers(&[]) && manifest.covers(&["A".into()]));
        assert!(!manifest.covers(&["B".into()]));
        let path = temporary.path().join("manifest.json");
        manifest.save(&path).unwrap();
        assert_eq!(Manifest::load(&path).unwrap().unwrap(), manifest);

        let two = temporary.path().join("two");
        fs::create_dir_all(two.join(".lake/build")).unwrap();
        fs::write(two.join(".lake/build/other"), "").unwrap();
        assert!(manifest.present(&artifacts).unwrap());
        // Another tree's build is left to Lake.
        let place = || {
            manifest
                .place(&project(&two), &packages, &artifacts)
                .unwrap()
        };
        assert!(!place());
        fs::remove_file(two.join(".lake/build/other")).unwrap();
        assert!(place());
        // The placed build stays, until Lake builds anything else.
        assert!(place());
        forget(&project(&two)).unwrap();
        fs::write(two.join(".lake/build/built"), "").unwrap();
        assert!(!place());
        let placed = fs::metadata(two.join(prefix).join("A.olean")).unwrap();
        let cached = fs::metadata(artifacts.join("0123456789abcdef.olean")).unwrap();
        assert_eq!(placed.ino(), cached.ino());
        assert_eq!(
            fs::read_to_string(two.join(prefix).join("A.trace")).unwrap(),
            r#"{"depHash": "1"}"#
        );

        fs::remove_file(artifacts.join("0123456789abcdef.olean")).unwrap();
        assert!(!manifest.present(&artifacts).unwrap());

        // A copy whose bytes differ from the artifact its hash names is not it.
        let mut other = large.clone();
        other[0] = 0;
        fs::remove_file(one.join(ir).join("A.c")).unwrap();
        fs::write(one.join(ir).join("A.c"), &other).unwrap();
        let record = Manifest::record(
            &project(&one),
            &packages,
            &artifacts,
            "l".into(),
            "t".into(),
            &[],
        );
        assert!(record.unwrap().is_none());
    }

    #[test]
    fn placement_stays_inside_the_build_folders() {
        let temporary = tempfile::tempdir().unwrap();
        let packages = [Package {
            name: "root".into(),
            source: Source::Repository,
            dir: "".into(),
        }];
        for path in ["src/A.lean", ".lake/build/../../escape"] {
            let manifest = Manifest {
                lake: "l".into(),
                tree: "t".into(),
                targets: Vec::new(),
                files: BTreeMap::from([(path.to_owned(), Content::Text(String::new()))]),
            };
            assert!(
                manifest
                    .place(&project(temporary.path()), &packages, temporary.path())
                    .is_err(),
                "{path}"
            );
        }
    }

    #[test]
    fn build_folders_resolve_parents_inside_the_workspace() {
        let package = |dir: &str| Package {
            name: "p".into(),
            source: Source::Repository,
            dir: dir.into(),
        };
        assert_eq!(
            folder(&package("vendor/veir/../lean-sail")),
            Some(PathBuf::from("vendor/lean-sail/.lake/build"))
        );
        assert_eq!(folder(&package("")), Some(PathBuf::from(".lake/build")));
        assert_eq!(folder(&package("../..")), None);
    }

    #[test]
    fn keys_name_scope_workspace_lake_and_tree() {
        let temporary = tempfile::tempdir().unwrap();
        let (lake, tree) = ("b".repeat(64), "c".repeat(40));
        assert_eq!(
            key(&project(temporary.path()), "linux-x86_64/s", &lake, &tree).unwrap(),
            format!("lake/linux-x86_64/s/builds/o/r/{}/{tree}", &lake[..16])
        );
        assert!(key(&project(temporary.path()), "s", "l", &tree).is_err());
    }
}
