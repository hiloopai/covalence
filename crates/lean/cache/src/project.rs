use std::{
    collections::BTreeMap,
    env,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use covalence_lib_serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    error::{Error, IoResult, Result, encode, invalid, json},
    mapping, pool,
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
}

pub(crate) struct Project {
    pub root: PathBuf,
    pub repository: String,
    pub record: PathBuf,
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
    /// Lake in this project, with `LAKE_CACHE_DIR` defaulting to
    /// `$XDG_CACHE_HOME/lake`, else `~/.cache/lake`, instead of Lake's
    /// per-toolchain directory, so CI can save one host-wide cache.
    pub fn lake(&self) -> Command {
        let mut command = Command::new(env::var_os("LAKE").unwrap_or_else(|| "lake".into()));
        command.current_dir(&self.root);
        if env::var_os("LAKE_CACHE_DIR").is_none() {
            let home = env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")));
            if let Some(home) = home {
                command.env("LAKE_CACHE_DIR", home.join("lake"));
            }
        }
        command
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
        let name = |value: &serde_json::Value| {
            let name = package_name(
                value["name"]
                    .as_str()
                    .ok_or_else(|| invalid("Lake manifest package has no name"))?,
            );
            mapping::segment_path(name)?;
            Ok(name.to_owned())
        };
        let mut packages = vec![Package {
            name: name(&manifest)?,
            source: Source::Repository,
        }];
        for package in manifest["packages"]
            .as_array()
            .ok_or_else(|| invalid("Lake manifest has no package list"))?
        {
            let source = match package["type"].as_str() {
                Some("path") => Source::Repository,
                Some("git") => {
                    let commit = package["rev"].as_str().unwrap_or_default();
                    if !mapping::hex(commit, 40) {
                        return Err(invalid("Lake manifest pins a non-commit revision"));
                    }
                    Source::Git {
                        repository: repository_of(package["url"].as_str().unwrap_or_default())?,
                        commit: commit.to_owned(),
                    }
                }
                _ => return Err(invalid("unsupported Lake manifest package type")),
            };
            packages.push(Package {
                name: name(package)?,
                source,
            });
        }
        Ok(packages)
    }

    /// The pool key of `package`'s revision pointer at `commit` of this
    /// repository, or at its own pin.
    pub fn revision_key(&self, scope: &str, package: &Package, commit: &str) -> Result<String> {
        match &package.source {
            Source::Repository => {
                pool::revision_key(scope, &self.repository, commit, &package.name)
            }
            Source::Git { repository, commit } => {
                pool::revision_key(scope, repository, commit, &package.name)
            }
        }
    }

    /// Lake's cache directory, which every worktree on this host shares
    /// unless `LAKE_CACHE_DIR` says otherwise.
    pub fn cache_dir(&self) -> Result<PathBuf> {
        let directory = output(
            "Lake",
            self.lake().args(["env", "printenv", "LAKE_CACHE_DIR"]),
        )?;
        if directory.is_empty() {
            return Err(invalid("Lake has no cache directory"));
        }
        let artifacts = Path::new(&directory).join("artifacts");
        fs::create_dir_all(&artifacts).operation("create Lake artifact directory")?;
        artifacts
            .parent()
            .ok_or_else(|| invalid("Lake cache directory has no parent"))?
            .canonicalize()
            .operation("locate Lake cache directory")
    }

    /// The commits to try for this repository's published builds: `HEAD`,
    /// then its first-parent ancestors, nearest first.
    pub fn ancestors(&self) -> Result<Vec<String>> {
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
        // Elan selects the toolchain from the project's cwd. An empty Lake
        // directory avoids loading the entire workspace just to read its version.
        let empty = tempfile::tempdir().operation("create toolchain query directory")?;
        hash.update(
            output(
                "Lean",
                self.lake()
                    .arg("--dir")
                    .arg(empty.path())
                    .args(["env", "lean", "--version"]),
            )?
            .as_bytes(),
        );
        let sysroot = output(
            "Lean",
            self.lake()
                .arg("--dir")
                .arg(empty.path())
                .args(["env", "lean", "--print-prefix"]),
        )?;
        // Patched and stock leantar both write `LTR4` and report the same
        // version, and stock leantar decodes the patched opcodes 0xa0 and
        // 0xb0 as constructors instead of rejecting them. Only the
        // executable identifies the codec.
        hash.update(b"\0leantar\0");
        hash.update(digest(
            &Path::new(&sysroot)
                .join("bin/leantar")
                .with_extension(env::consts::EXE_EXTENSION),
        )?);
        let revision = self.git(&["rev-parse", "HEAD"])?;
        if !mapping::hex(&revision, 40) {
            return Err(invalid("expected a 40-digit Git revision"));
        }
        let arch = env::consts::ARCH;
        let os = match env::consts::OS {
            "macos" => "darwin",
            os => os,
        };
        Ok(Identity {
            scope: format!(
                "{os}-{arch}/{}",
                data_encoding::HEXLOWER.encode(&hash.finalize())
            ),
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
    pub fn build(&self, targets: &[String], skip: impl Fn(&Package) -> Result<bool>) -> Result<()> {
        let receipt = self.record.join("success.json");
        remove(&receipt)?;
        let packages_dir = self.record.join("packages");
        if packages_dir.exists() {
            fs::remove_dir_all(&packages_dir).operation("remove prior build records")?;
        }
        fs::create_dir_all(&packages_dir).operation("create build record directory")?;
        self.clean()?;
        let identity = self.identity()?;
        let packages = self.packages()?;
        let mut recorded = BTreeMap::new();
        for (index, package) in packages.iter().enumerate() {
            if index > 0 && skip(package)? {
                continue;
            }
            let mapping = self.mapping(&package.name);
            let mut command = self.lake();
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
            recorded.insert(package.name.clone(), digest(&mapping)?);
        }
        self.clean()?;
        if self.identity()? != identity || self.packages()? != packages {
            return Err(invalid("build identity changed while checking"));
        }
        let artifacts = self.cache_dir()?.join("artifacts");
        for package in recorded.keys() {
            mapping::artifacts(&self.mapping(package), &artifacts)?;
        }
        let record = Receipt {
            identity,
            packages: recorded,
            artifacts,
        };
        fs::write(receipt, encode(&record)?).operation("save successful build receipt")
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

/// How many first-parent commits a restore searches for a published build.
const ANCESTORS: usize = 64;

#[cfg(test)]
mod tests {
    use super::*;

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
               "rev": "89abcdef0123456789abcdef0123456789abcdef", "inputRev": "main",
               "inherited": false, "manifestFile": "lake-manifest.json", "configFile": "lakefile.lean"}
            ]}"#,
        )
        .unwrap();
        let project = Project {
            root: root.path().to_owned(),
            repository: "o/moria-lean".into(),
            record: root.path().join("record"),
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
        assert_eq!(package_name("«a»"), "a");
        assert_eq!(package_name("«a"), "«a");
    }
}
