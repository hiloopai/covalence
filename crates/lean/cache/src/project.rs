use std::{
    env,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use covalence_lib_serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    artifact,
    error::{Error, IoResult, Result, encode, invalid, json},
    mapping,
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
    pub mapping_sha256: String,
    pub artifacts_sha256: String,
    pub artifacts: PathBuf,
}

pub(crate) struct Project {
    pub root: PathBuf,
    pub repository: String,
    pub package: String,
    pub record: PathBuf,
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
    pub fn lake(&self) -> Command {
        let mut command = Command::new(env::var_os("LAKE").unwrap_or_else(|| "lake".into()));
        command.current_dir(&self.root);
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

    pub fn identity(&self) -> Result<Identity> {
        mapping::segment_path(&self.repository)?;
        let package = if self.package.is_empty() {
            "root"
        } else {
            &self.package
        };
        mapping::segment_path(package)?;
        let mut hash = Sha256::new();
        for name in [
            "lean-toolchain",
            "lake-manifest.json",
            "lakefile.toml",
            "lakefile.lean",
        ] {
            match fs::read(self.root.join(name)) {
                Ok(bytes) => {
                    hash.update(name.as_bytes());
                    hash.update([0]);
                    hash.update(bytes);
                    hash.update([0]);
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(Error::Io {
                        operation: format!("read {name}"),
                        source,
                    });
                }
            }
        }
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
                "{}/{os}-{arch}/{}/{package}/zstd-v1",
                self.repository,
                data_encoding::HEXLOWER.encode(&hash.finalize())
            ),
            revision,
            run: [
                env::var("GITHUB_RUN_ID").unwrap_or_else(|_| "local".into()),
                env::var("GITHUB_RUN_ATTEMPT").unwrap_or_else(|_| "local".into()),
            ],
        })
    }

    pub fn build(&self, targets: &[String]) -> Result<()> {
        fs::create_dir_all(&self.record).operation("create build record directory")?;
        let receipt = self.record.join("success.json");
        let mapping = self.record.join("outputs.jsonl");
        remove(&receipt)?;
        remove(&mapping)?;
        self.clean()?;
        let identity = self.identity()?;
        let mut command = self.lake();
        command
            .args(["build", "--wfail", "-o"])
            .arg(&mapping)
            .env("LAKE_ARTIFACT_CACHE", "true");
        if !self.package.is_empty() {
            command.args(["--package", &self.package]);
        }
        // --package selects recorded mappings, not Lake's build targets.
        if targets.is_empty() && !self.package.is_empty() {
            command.arg(format!("@{}", self.package));
        }
        run("Lake build", command.args(targets))?;
        self.clean()?;
        if self.identity()? != identity {
            return Err(invalid("build identity changed while checking"));
        }
        let directory = output(
            "Lake",
            self.lake().args(["env", "printenv", "LAKE_CACHE_DIR"]),
        )?;
        let artifacts = Path::new(&directory)
            .join("artifacts")
            .canonicalize()
            .operation("locate Lake artifacts")?;
        let files = mapping::artifacts(&mapping, &artifacts)?;
        artifact::prepare(&files, &self.record)?;
        let record = Receipt {
            identity,
            mapping_sha256: digest(&mapping)?,
            artifacts_sha256: digest(&self.record.join(artifact::MANIFEST))?,
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
                "cache receipt does not match this revision, platform, pins and run",
            ));
        }
        if receipt.mapping_sha256 != digest(&self.record.join("outputs.jsonl"))? {
            return Err(invalid("cache mapping changed after successful build"));
        }
        if receipt.artifacts_sha256 != digest(&self.record.join(artifact::MANIFEST))? {
            return Err(invalid(
                "compressed artifact manifest changed after successful build",
            ));
        }
        Ok(receipt)
    }
}
