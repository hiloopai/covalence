use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
};

use covalence_kv_trait::Kv;
use covalence_lib_serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    error::{Error, IoResult, Result, encode, invalid, json},
    gc, mapping,
    modules::Index,
    pool,
    toolchain::Lean,
};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(crate = "covalence_lib_serde::serde", deny_unknown_fields)]
pub(crate) struct Identity {
    pub scope: String,
    pub revision: String,
    pub run: [String; 2],
}

#[derive(Deserialize, Serialize)]
#[serde(crate = "covalence_lib_serde::serde", deny_unknown_fields)]
pub(crate) struct Receipt {
    pub identity: Identity,
    /// The SHA-256 of each recorded package's Lake mapping.
    pub packages: BTreeMap<String, String>,
    pub artifacts: PathBuf,
    /// The Lake targets the build checked.
    pub targets: Vec<String>,
    /// Git dependencies whose mapping is the pool's map of their pin, which
    /// already held everything the build used; `put` leaves them as they are.
    #[serde(default)]
    pub pooled: BTreeSet<String>,
}

/// Where a package's sources, and so its revision pointer, come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// The root, or a path dependency: this repository at `HEAD`.
    Repository,
    /// A Git dependency, pinned by the manifest.
    Git { repository: String, commit: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Package {
    pub name: String,
    pub source: Source,
    /// The package's directory, relative to the root.
    pub dir: PathBuf,
}

pub(crate) struct Project {
    pub root: PathBuf,
    pub repository: String,
    /// The workspace's folder in the repository, empty for its root. A
    /// repository can hold several Lake workspaces; each keeps its own
    /// revision maps, module indexes and seeds.
    pub workspace: String,
    pub record: PathBuf,
    /// The Lake this command runs, which waits for its toolchain.
    pub lean: Lean,
    /// Whether Lake may take outputs from the internal scope.
    pub lookups: Lookups,
}

/// Where Lake looks for the outputs of an input its cache lacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lookups {
    /// Nowhere: it builds them.
    None,
    /// The internal scope (see `internal`).
    Internal,
    /// The internal scope, listing what it takes for `put`'s adoption check.
    Adopt,
}

/// `LAKE_CACHE_DIR`, else `$XDG_CACHE_HOME/lake`, else `~/.cache/lake`.
pub(crate) fn host_cache_dir() -> Option<PathBuf> {
    env::var_os("LAKE_CACHE_DIR")
        .filter(|directory| !directory.is_empty())
        .map(PathBuf::from)
        .or_else(default_cache_dir)
}

fn default_cache_dir() -> Option<PathBuf> {
    env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .map(|home| home.join("lake"))
}

/// A manifest package name as Lake's `--package` and cache directories spell
/// it: without the `«»` quotes Lean puts around names that are not identifiers.
pub(crate) fn package_name(name: &str) -> &str {
    name.strip_prefix('«')
        .and_then(|name| name.strip_suffix('»'))
        .unwrap_or(name)
}

/// The repository path of a Git URL: `owner/name` on GitHub, else the host
/// followed by the path.
pub(crate) fn repository_of(url: &str) -> Result<String> {
    let rest = url
        .split_once("://")
        .map_or(url, |(_, rest)| rest)
        .trim_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let path = rest.strip_prefix("github.com/").unwrap_or(rest);
    mapping::segment_path(path)?;
    Ok(path.to_owned())
}

pub(crate) fn run(program: &'static str, command: &mut Command) -> Result<()> {
    let status = command.status().operation(format!("start {program}"))?;
    if !status.success() {
        return Err(Error::Process { program, status });
    }
    Ok(())
}

pub(crate) fn output(program: &'static str, command: &mut Command) -> Result<String> {
    let result = command
        .stderr(Stdio::inherit())
        .output()
        .operation(format!("start {program}"))?;
    if !result.status.success() {
        return Err(Error::Process {
            program,
            status: result.status,
        });
    }
    String::from_utf8(result.stdout)
        .map(|s| s.trim().to_owned())
        .map_err(|_| invalid(format!("{program} returned non-UTF-8 output")))
}

pub(crate) fn digest(path: &Path) -> Result<String> {
    let mut file = File::open(path).operation(format!("open {}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).operation("hash cache metadata")?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(data_encoding::HEXLOWER.encode(&hash.finalize()))
}

fn remove(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Io {
            operation: format!("remove {}", path.display()),
            source,
        }),
    }
}

impl Project {
    /// Lake in this project, once its toolchain is ready, with
    /// `LAKE_CACHE_DIR` defaulting to `$XDG_CACHE_HOME/lake`, else
    /// `~/.cache/lake`, instead of Lake's per-toolchain directory, so CI can
    /// save one host-wide cache.
    pub fn lake(&self) -> Result<Command> {
        self.lean.ready(&self.root)?;
        let mut command = Command::new(self.lean.program());
        command.current_dir(&self.root);
        if let Some(directory) = host_cache_dir() {
            command.env("LAKE_CACHE_DIR", directory);
        }
        // A restored module's files live in the artifact cache. Lake links
        // only its `.ilean` into the package's `.lake/build` unless it
        // restores all artifacts, and tools that run `lean` on that folder
        // need the rest. Every package that does not set
        // `restoreAllArtifacts` itself inherits this.
        if env::var_os("LAKE_RESTORE_ARTIFACTS").is_none() {
            command.env("LAKE_RESTORE_ARTIFACTS", "true");
        }
        Ok(command)
    }

    pub fn git(&self, args: &[&str]) -> Result<String> {
        output(
            "git",
            Command::new("git").current_dir(&self.root).args(args),
        )
    }

    pub fn clean(&self) -> Result<()> {
        if !self.git(&["status", "--porcelain"])?.is_empty() {
            return Err(invalid(
                "cache recording/publication requires a clean source tree",
            ));
        }
        Ok(())
    }

    /// The workspace's packages from its manifest: the root first.
    pub fn packages(&self) -> Result<Vec<Package>> {
        let manifest: serde_json::Value =
            json(&fs::read(self.root.join("lake-manifest.json")).operation("read Lake manifest")?)?;
        let name_of = |value: &serde_json::Value| {
            let name = package_name(
                value["name"]
                    .as_str()
                    .ok_or_else(|| invalid("Lake manifest package has no name"))?,
            );
            mapping::segment_path(name)?;
            Ok(name.to_owned())
        };
        let mut packages = vec![Package {
            name: name_of(&manifest)?,
            source: Source::Repository,
            dir: PathBuf::new(),
        }];
        let packages_dir = Path::new(manifest["packagesDir"].as_str().unwrap_or(".lake/packages"));
        for package in manifest["packages"]
            .as_array()
            .ok_or_else(|| invalid("Lake manifest has no package list"))?
        {
            let name = name_of(package)?;
            let (source, dir) = match package["type"].as_str() {
                Some("path") => (
                    Source::Repository,
                    PathBuf::from(
                        package["dir"]
                            .as_str()
                            .ok_or_else(|| invalid("Lake manifest path package has no dir"))?,
                    ),
                ),
                Some("git") => {
                    let commit = package["rev"].as_str().unwrap_or_default();
                    if !mapping::hex(commit, 40) {
                        return Err(invalid("Lake manifest pins a non-commit revision"));
                    }
                    let dir = packages_dir.join(&name);
                    (
                        Source::Git {
                            repository: repository_of(package["url"].as_str().unwrap_or_default())?,
                            commit: commit.to_owned(),
                        },
                        match package["subDir"].as_str() {
                            Some(sub) => dir.join(sub),
                            None => dir,
                        },
                    )
                }
                _ => return Err(invalid("unsupported Lake manifest package type")),
            };
            packages.push(Package { name, source, dir });
        }
        Ok(packages)
    }

    /// The repository, followed by the workspace's folder unless it is the
    /// root: the name this workspace's own builds and seeds are kept under.
    /// A second workspace's path dependency on the root, such as
    /// `../..`, then never overwrites the root workspace's map of it.
    pub fn namespace(&self) -> String {
        if self.workspace.is_empty() {
            self.repository.clone()
        } else {
            format!("{}/{}", self.repository, self.workspace)
        }
    }

    /// The workspace folder of `root` in its Git repository, empty at the
    /// root.
    pub fn workspace_of(root: &Path) -> Result<String> {
        let prefix = output(
            "git",
            Command::new("git")
                .current_dir(root)
                .args(["rev-parse", "--show-prefix"]),
        )?;
        let workspace = prefix.trim_end_matches('/').to_owned();
        if !workspace.is_empty() {
            mapping::segment_path(&workspace)?;
        }
        Ok(workspace)
    }

    /// The pool key of `package`'s revision pointer at `commit` of this
    /// repository, or at its own pin.
    pub fn revision_key(&self, scope: &str, package: &Package, commit: &str) -> Result<String> {
        match &package.source {
            Source::Repository => {
                pool::revision_key(scope, &self.namespace(), commit, &package.name)
            }
            Source::Git { repository, commit } => {
                pool::revision_key(scope, repository, commit, &package.name)
            }
        }
    }

    /// The pool key of `package`'s module index at `commit` of this
    /// repository, or at its own pin.
    pub fn modules_key(&self, scope: &str, package: &Package, commit: &str) -> Result<String> {
        match &package.source {
            Source::Repository => {
                pool::modules_key(scope, &self.namespace(), commit, &package.name)
            }
            Source::Git { repository, commit } => {
                pool::modules_key(scope, repository, commit, &package.name)
            }
        }
    }

    /// Where `build` records `package`'s module index.
    pub fn modules(&self, package: &str) -> PathBuf {
        self.record.join("modules").join(format!("{package}.json"))
    }

    /// Lake's cache directory, which every worktree on this host shares
    /// unless `LAKE_CACHE_DIR` says otherwise.
    pub fn cache_dir(&self) -> Result<PathBuf> {
        // Every Lake this tool runs gets this directory (see `lake`), so
        // there is no need to ask Lake.
        let directory = host_cache_dir().ok_or_else(|| invalid("Lake has no cache directory"))?;
        let artifacts = directory.join("artifacts");
        fs::create_dir_all(&artifacts).operation("create Lake artifact directory")?;
        artifacts
            .parent()
            .ok_or_else(|| invalid("Lake cache directory has no parent"))?
            .canonicalize()
            .operation("locate Lake cache directory")
    }

    /// The commits to try for this repository's published builds: those
    /// [`COMMITS`] names, else `HEAD` then its first-parent ancestors,
    /// nearest first.
    pub fn ancestors(&self) -> Result<Vec<String>> {
        if let Some(named) = env::var_os(COMMITS) {
            let named = named
                .into_string()
                .map_err(|_| invalid(format!("{COMMITS} is not UTF-8")))?;
            return named_commits(&named);
        }
        let list = self.git(&[
            "rev-list",
            "--first-parent",
            &format!("--max-count={ANCESTORS}"),
            "HEAD",
        ])?;
        list.lines()
            .map(|commit| {
                if !mapping::hex(commit, 40) {
                    return Err(invalid("expected 40-digit Git revisions"));
                }
                Ok(commit.to_owned())
            })
            .collect()
    }

    pub fn identity(&self) -> Result<Identity> {
        if !self.repository.is_empty() {
            mapping::segment_path(&self.repository)?;
        }
        let mut hash = Sha256::new();
        hash.update(pool::FORMAT.as_bytes());
        hash.update([0]);
        // A toolchain still arriving from the pool needs no Lean to name.
        let (version, leantar) = match self.lean.predicted()? {
            Some(predicted) => predicted,
            None => {
                // Elan selects the toolchain from the project's cwd. An empty
                // Lake directory avoids loading the entire workspace just to
                // read its version.
                let empty = tempfile::tempdir().operation("create toolchain query directory")?;
                let version = output(
                    "Lean",
                    self.lake()?
                        .arg("--dir")
                        .arg(empty.path())
                        .args(["env", "lean", "--version"]),
                )?;
                let sysroot = output(
                    "Lean",
                    self.lake()?.arg("--dir").arg(empty.path()).args([
                        "env",
                        "lean",
                        "--print-prefix",
                    ]),
                )?;
                let leantar = Path::new(&sysroot)
                    .join("bin/leantar")
                    .with_extension(env::consts::EXE_EXTENSION);
                (version, leantar)
            }
        };
        hash.update(version.as_bytes());
        // Patched and stock leantar both write `LTR4` and report the same
        // version, and stock leantar decodes the patched opcodes 0xa0 and
        // 0xb0 as constructors instead of rejecting them. Only the
        // executable identifies the codec.
        hash.update(b"\0leantar\0");
        hash.update(digest(&leantar)?);
        let revision = self.git(&["rev-parse", "HEAD"])?;
        if !mapping::hex(&revision, 40) {
            return Err(invalid("expected a 40-digit Git revision"));
        }
        let arch = env::consts::ARCH;
        let os = match env::consts::OS {
            "macos" => "darwin",
            os => os,
        };
        let scope = format!(
            "{os}-{arch}/{}",
            data_encoding::HEXLOWER.encode(&hash.finalize())
        );
        let toolchain = match fs::read_to_string(self.root.join("lean-toolchain")) {
            Ok(toolchain) => Some(toolchain),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(Error::Io {
                    operation: "read Lean toolchain".into(),
                    source,
                });
            }
        };
        gc::record_use(
            &self.cache_dir()?,
            &scope,
            toolchain.as_deref().map(str::trim),
        )?;
        Ok(Identity {
            scope,
            revision,
            run: [
                env::var("GITHUB_RUN_ID").unwrap_or_else(|_| "local".into()),
                env::var("GITHUB_RUN_ATTEMPT").unwrap_or_else(|_| "local".into()),
            ],
        })
    }

    pub fn mapping(&self, package: &str) -> PathBuf {
        self.record
            .join("packages")
            .join(format!("{package}.jsonl"))
    }

    /// Run one strict build, then record each package's outputs: the
    /// repository's own packages always, and Git dependencies that `skip`
    /// does not exclude.
    pub fn build(&self, kv: &Arc<dyn Kv>, targets: &[String]) -> Result<()> {
        let receipt = self.record.join("success.json");
        remove(&receipt)?;
        remove(&crate::internal::adoption_list(self))?;
        remove(&self.build_manifest())?;
        remove(&self.published())?;
        let packages_dir = self.record.join("packages");
        for directory in [&packages_dir, &self.record.join("modules")] {
            if directory.exists() {
                fs::remove_dir_all(directory).operation("remove prior build records")?;
            }
        }
        fs::create_dir_all(&packages_dir).operation("create build record directory")?;
        self.clean()?;
        let identity = self.identity()?;
        let packages = self.packages()?;
        // A commit published with this tree's build needs no second record.
        let root = self.revision_key(&identity.scope, &packages[0], &identity.revision)?;
        if crate::storage::pointer(&**kv, &root)?.is_some()
            && let Some(manifest) = crate::builds::find(
                self,
                &**kv,
                &identity.scope,
                &crate::builds::lake(self)?,
                targets,
            )?
        {
            let artifacts = self.cache_dir()?.join("artifacts");
            if manifest.present(&artifacts)? && manifest.place(self, &packages, &artifacts)? {
                println!(
                    "Lake cache: {} is published with this build already; nothing to record",
                    identity.revision
                );
                return fs::write(self.published(), &identity.revision)
                    .operation("record the published commit");
            }
        }
        crate::builds::forget(self)?;
        let nearest = crate::storage::nearest(
            &**kv,
            self,
            &identity.scope,
            &packages[0],
            &self.ancestors()?,
        )?;
        crate::modules::reuse_archives(
            self,
            kv,
            &identity.scope,
            &packages,
            nearest.as_ref().map(|(commit, _)| commit.as_str()),
        )?;
        let mut recorded = BTreeMap::new();
        let mut pooled = BTreeSet::new();
        for (index, package) in packages.iter().enumerate() {
            let mapping = self.mapping(&package.name);
            let build = self.root.join(&package.dir).join(".lake/build");
            // The root build checked every package; a dependency's replay
            // only records its outputs, which the pool may hold already.
            if index > 0
                && let Some(rows) =
                    crate::modules::pooled_mapping(self, kv, &identity.scope, package)?
            {
                mapping::write(&mapping, &rows)?;
                Index::record(&build, &mapping)?.save(&self.modules(&package.name))?;
                recorded.insert(package.name.clone(), digest(&mapping)?);
                pooled.insert(package.name.clone());
                continue;
            }
            let mut command = self.lake()?;
            crate::internal::lookups(&mut command, self, &identity.scope)?;
            command
                .args(["build", "--wfail", "-o"])
                .arg(&mapping)
                .env("LAKE_ARTIFACT_CACHE", "true");
            // The first build checks everything; later ones only replay
            // traces to record another package's outputs.
            if index > 0 {
                command.args(["--package", &package.name]);
            }
            run("Lake build", command.args(targets))?;
            add_traced_outputs(&mapping, &build, &self.cache_dir()?.join("artifacts"))?;
            Index::record(&build, &mapping)?.save(&self.modules(&package.name))?;
            recorded.insert(package.name.clone(), digest(&mapping)?);
        }
        self.clean()?;
        if self.identity()? != identity || self.packages()? != packages {
            return Err(invalid("build identity changed while checking"));
        }
        let artifacts = self.cache_dir()?.join("artifacts");
        for package in recorded.keys().filter(|package| !pooled.contains(*package)) {
            mapping::artifacts(&self.mapping(package), &artifacts)?;
        }
        if let Some(change) = crate::builds::changed(self)? {
            println!("Lake cache: no build manifest: {change} differs from the commit");
        } else if let Some(tree) = crate::builds::tree(self)?
            && let Some(manifest) = crate::builds::Manifest::record(
                self,
                &packages,
                &artifacts,
                crate::builds::lake(self)?,
                tree,
                targets,
            )?
        {
            manifest.save(&self.build_manifest())?;
        }
        if !pooled.is_empty() {
            println!(
                "Lake cache: recorded {} dependencies from the pool's maps of their pins without replaying them",
                pooled.len()
            );
        }
        let record = Receipt {
            identity,
            packages: recorded,
            artifacts,
            targets: targets.to_vec(),
            pooled,
        };
        fs::write(receipt, encode(&record)?).operation("save successful build receipt")
    }

    /// Where `build` names the commit it found published already, which
    /// leaves `put` nothing to do.
    pub fn published(&self) -> PathBuf {
        self.record.join("published")
    }

    /// Where `build` records the manifest of the build folders it checked.
    pub fn build_manifest(&self) -> PathBuf {
        self.record.join("build.json")
    }

    pub fn receipt(&self) -> Result<Receipt> {
        self.clean()?;
        let receipt: Receipt = json(
            &fs::read(self.record.join("success.json"))
                .operation("read successful build receipt")?,
        )?;
        if receipt.identity != self.identity()? {
            return Err(invalid(
                "cache receipt does not match this revision, platform, toolchain and run",
            ));
        }
        for (package, sha256) in &receipt.packages {
            if *sha256 != digest(&self.mapping(package))? {
                return Err(invalid("cache mapping changed after successful build"));
            }
        }
        Ok(receipt)
    }
}

/// Add to the Lake mapping at `mapping` the single-artifact outputs that the
/// traces under a package's `build` folder record and the mapping lacks.
///
/// Lake's `-o` leaves out the platform-dependent outputs, such as the native
/// objects an executable links, of a package that sets `platformIndependent`,
/// as Mathlib and Aesop do; every later build then compiles them again. The
/// pool is per platform, so they belong in its maps. Each output's trace,
/// beside it, names its input hash and artifact; only artifacts in Lake's
/// cache in `artifacts` are added, as `-o` adds only cached outputs.
fn add_traced_outputs(mapping: &Path, build: &Path, artifacts: &Path) -> Result<usize> {
    let mut rows = mapping::rows(mapping)?
        .into_iter()
        .map(|(input, (output, _))| (input, output))
        .collect::<BTreeMap<_, _>>();
    let mut added = 0;
    for trace in files(build, "trace")? {
        let bytes = fs::read(&trace).operation(format!("read {}", trace.display()))?;
        // Old trace stubs and outputs of several files carry no single artifact.
        let Ok(trace) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        let (Some(input), Some(name)) = (trace["depHash"].as_str(), trace["outputs"].as_str())
        else {
            continue;
        };
        if !mapping::hex(input, 16)
            || !mapping::artifact_name(name)
            || rows.contains_key(input)
            || !artifacts.join(name).is_file()
        {
            continue;
        }
        rows.insert(input.to_owned(), serde_json::Value::from(name).to_string());
        added += 1;
    }
    if added > 0 {
        mapping::write(mapping, &rows)?;
    }
    Ok(added)
}

/// The files with `extension` under `directory`, if it exists.
pub(crate) fn files(directory: &Path, extension: &str) -> Result<Vec<PathBuf>> {
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
            let path = entry.path();
            if entry
                .file_type()
                .operation("read build entry type")?
                .is_dir()
            {
                pending.push(path);
            } else if path.extension().is_some_and(|found| found == extension) {
                found.push(path);
            }
        }
    }
    Ok(found)
}

/// How many first-parent commits a restore searches for a published build.
const ANCESTORS: usize = 64;

/// The commits a restore searches instead of the checkout's history,
/// nearest first: for a checkout that has none of its own, such as a remote
/// build's copy of a client's tree.
const COMMITS: &str = "COVALENCE_LAKE_COMMITS";

/// The full Git revisions in `text`, separated by commas or newlines, at
/// most [`ANCESTORS`] of them.
fn named_commits(text: &str) -> Result<Vec<String>> {
    let commits = text
        .split([',', '\n'])
        .map(str::trim)
        .filter(|commit| !commit.is_empty())
        .take(ANCESTORS)
        .map(|commit| {
            if !mapping::hex(commit, 40) {
                return Err(invalid(format!(
                    "{COMMITS} names 40-digit lowercase Git revisions, not {commit:?}"
                )));
            }
            Ok(commit.to_owned())
        })
        .collect::<Result<Vec<_>>>()?;
    if commits.is_empty() {
        return Err(invalid(format!("{COMMITS} names no commits")));
    }
    Ok(commits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_commits_are_full_revisions_nearest_first() {
        let (a, b) = ("a".repeat(40), "0123456789abcdef0123456789abcdef01234567");
        assert_eq!(
            named_commits(&format!("{a},\n {b}\n")).unwrap(),
            [a.clone(), b.to_owned()]
        );
        assert_eq!(
            named_commits(&vec![a; 70].join(",")).unwrap().len(),
            ANCESTORS
        );
        for bad in ["", " ,\n", "HEAD", &"A".repeat(40), &"a".repeat(39)] {
            assert!(named_commits(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn traced_single_artifact_outputs_the_mapping_lacks_are_added() {
        let dir = tempfile::tempdir().unwrap();
        let (build, artifacts) = (dir.path().join("build"), dir.path().join("artifacts"));
        fs::create_dir_all(build.join("ir/M")).unwrap();
        fs::create_dir_all(build.join("lib/lean/M")).unwrap();
        fs::create_dir_all(&artifacts).unwrap();
        for name in ["00000000000000a1.ltar", "00000000000000c1.o"] {
            fs::write(artifacts.join(name), name).unwrap();
        }
        let mapping = dir.path().join("p.jsonl");
        fs::write(
            &mapping,
            "\"2026-03-17\"\n[\"000000000000000a\",\"00000000000000a1.ltar\"]\n",
        )
        .unwrap();
        let trace = |path: &str, input: &str, outputs: &str| {
            fs::write(
                build.join(path),
                format!(r#"{{"schemaVersion": "2025-09-10", "depHash": "{input}", "inputs": [], "outputs": {outputs}, "log": [], "synthetic": false}}"#),
            )
            .unwrap();
        };
        // A native object of a platform-independent package: added.
        trace(
            "ir/M/A.c.o.export.trace",
            "000000000000000c",
            r#""00000000000000c1.o""#,
        );
        // A module, whose -o row is already there, and several files: skipped.
        trace(
            "lib/lean/M/A.trace",
            "000000000000000a",
            r#"{"o": "00000000000000a2.olean"}"#,
        );
        // Not in Lake's cache, so not published: skipped.
        trace(
            "ir/M/B.c.o.export.trace",
            "000000000000000d",
            r#""00000000000000d1.o""#,
        );
        fs::write(build.join("ir/M/C.c.o.export.trace"), "12345").unwrap();
        assert_eq!(add_traced_outputs(&mapping, &build, &artifacts).unwrap(), 1);
        let rows = mapping::rows(&mapping).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows["000000000000000c"],
            (
                "\"00000000000000c1.o\"".to_owned(),
                vec!["00000000000000c1.o".to_owned()]
            )
        );
        assert_eq!(add_traced_outputs(&mapping, &build, &artifacts).unwrap(), 0);
        assert_eq!(
            add_traced_outputs(&mapping, &dir.path().join("absent"), &artifacts).unwrap(),
            0
        );
    }

    #[test]
    fn a_workspace_below_the_repository_root_has_its_own_namespace() {
        let root = tempfile::tempdir().unwrap();
        output(
            "git",
            Command::new("git")
                .current_dir(root.path())
                .args(["init", "-q"]),
        )
        .unwrap();
        let nested = root.path().join("projects/smt-axioms");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(Project::workspace_of(root.path()).unwrap(), "");
        assert_eq!(
            Project::workspace_of(&nested).unwrap(),
            "projects/smt-axioms"
        );
        let project = |workspace: &str| Project {
            root: root.path().to_owned(),
            workspace: workspace.into(),
            repository: "o/r".into(),
            record: root.path().join("record"),
            lean: Lean::default(),
            lookups: crate::project::Lookups::None,
        };
        assert_eq!(project("").namespace(), "o/r");
        let nested = project("projects/smt-axioms");
        assert_eq!(nested.namespace(), "o/r/projects/smt-axioms");
        let path = Package {
            name: "silmaril".into(),
            source: Source::Repository,
            dir: "../..".into(),
        };
        let commit = "a".repeat(40);
        assert_eq!(
            nested.revision_key("s/t", &path, &commit).unwrap(),
            format!("lake/s/t/rev/o/r/projects/smt-axioms/{commit}/silmaril")
        );
        let pin = Package {
            name: "mathlib".into(),
            source: Source::Git {
                repository: "o/m".into(),
                commit: commit.clone(),
            },
            dir: ".lake/packages/mathlib".into(),
        };
        // Dependency pins stay shared.
        assert_eq!(
            nested.revision_key("s/t", &pin, &commit).unwrap(),
            project("").revision_key("s/t", &pin, &commit).unwrap()
        );
    }

    #[test]
    fn quoted_manifest_names_match_lake_package_names() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("lake-manifest.json"),
            r#"{"version": "1.1.0", "name": "«moria-lean»", "packagesDir": ".lake/packages",
            "packages": [
              {"type": "path", "scope": "", "name": "«riscv-zkvm»", "manifestFile": "lake-manifest.json",
               "inherited": false, "dir": "vendor/riscv-zkvm", "configFile": "lakefile.toml"},
              {"type": "git", "scope": "", "name": "«lean-ctrees»", "url": "https://github.com/o/lean-ctrees.git",
               "rev": "0123456789abcdef0123456789abcdef01234567", "inputRev": "main",
               "inherited": true, "manifestFile": "lake-manifest.json", "configFile": "lakefile.lean"},
              {"type": "git", "scope": "", "name": "mathlib", "url": "https://github.com/o/m",
               "rev": "89abcdef0123456789abcdef0123456789abcdef", "inputRev": "main", "subDir": "mathlib4",
               "inherited": false, "manifestFile": "lake-manifest.json", "configFile": "lakefile.lean"}
            ]}"#,
        )
        .unwrap();
        let project = Project {
            root: root.path().to_owned(),
            workspace: String::new(),
            repository: "o/moria-lean".into(),
            record: root.path().join("record"),
            lean: Lean::default(),
            lookups: crate::project::Lookups::None,
        };
        let names = project
            .packages()
            .unwrap()
            .into_iter()
            .map(|package| package.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            ["moria-lean", "riscv-zkvm", "lean-ctrees", "mathlib"]
        );
        let dirs = project
            .packages()
            .unwrap()
            .into_iter()
            .map(|package| package.dir)
            .collect::<Vec<_>>();
        assert_eq!(
            dirs,
            [
                "",
                "vendor/riscv-zkvm",
                ".lake/packages/lean-ctrees",
                ".lake/packages/mathlib/mathlib4"
            ]
            .map(PathBuf::from)
        );
        assert_eq!(package_name("«a»"), "a");
        assert_eq!(package_name("«a"), "«a");
    }
}
