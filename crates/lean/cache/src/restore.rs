//! Install published maps through Lake, and serve Lake's artifact downloads
//! from the pool.
//!
//! Lake, not this adapter, installs mappings and checks artifact hashes; the
//! pool store checks each artifact's CID first. Requests for other cache
//! services go to `curl` unchanged.

use std::{
    collections::BTreeMap,
    env,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    sync::Arc,
};

use covalence_cas_kv::KvCas;
use covalence_cas_prolly::Map;
use covalence_kv_trait::Kv;
use covalence_lib_cid::{Bdasl, Cid, Sha256};
use serde_json::json;

use crate::{
    archives,
    blobs::Blobs,
    dependencies,
    error::{IoResult, Result, encode, invalid, json},
    local::{Installed, Local},
    mapping, pool,
    project::{Package, Project, Source, run},
    storage,
};

const BASE: &str = "https://covalence-lake.invalid";
const SERVICE: &str = "covalence";
const FORMAT: &str = "%{stderr}%{json}\n";

/// One run of Lake against the pool: its configuration, and the artifacts
/// this tool may serve to it.
pub(crate) struct Session {
    directory: tempfile::TempDir,
    scope: String,
    local: Local,
    remote: KvCas,
    blobs: Blobs,
    index: BTreeMap<String, String>,
    /// Credentials resolved once for every download process.
    lease: Vec<(&'static str, String)>,
}

impl Session {
    pub fn new(project: &Project, kv: &Arc<dyn Kv>, scope: &str) -> Result<Self> {
        let local = Local::open(project)?;
        let directory = tempfile::tempdir().operation("create Lake cache configuration")?;
        let mut settings = String::new();
        if dependencies::config(project).exists() {
            // Mappings fetched from Mathlib's services keep naming them, so
            // any build that may download those artifacts needs them.
            settings = fs::read_to_string(dependencies::config(project))
                .operation("read dependency cache configuration")?
                + "\n";
        }
        settings.push_str(&format!("[[cache.service]]\nname = \"{SERVICE}\"\nkind = \"s3\"\nartifactEndpoint = \"{BASE}/a\"\nrevisionEndpoint = \"{BASE}/r\"\n"));
        fs::write(directory.path().join("config.toml"), settings)
            .operation("write Lake cache configuration")?;
        let session = Self {
            directory,
            scope: scope.to_owned(),
            local,
            remote: pool::store(kv.clone(), scope)?,
            blobs: pool::blobs(kv.clone(), scope)?,
            index: BTreeMap::new(),
            lease: storage::lease()?,
        };
        session.save_index()?;
        Ok(session)
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }

    fn save_index(&self) -> Result<()> {
        fs::write(
            self.directory.path().join("index.json"),
            encode(&self.index)?,
        )
        .operation("save artifact index")
    }

    /// Lake, configured to fetch through this session.
    pub fn lake(&self, project: &Project) -> Result<Command> {
        let mut command = project.lake();
        command
            .env("LAKE_CONFIG", self.directory.path().join("config.toml"))
            .env(
                "CURL",
                env::current_exe().operation("locate cache executable")?,
            )
            .envs(self.lease.iter().map(|(name, value)| (*name, value)))
            .env("COVALENCE_LAKE_SCOPE", &self.scope)
            .env(
                "COVALENCE_LAKE_ARTIFACTS",
                self.local.directory().join("artifacts"),
            )
            .env(
                "COVALENCE_LAKE_INDEX",
                self.directory.path().join("index.json"),
            );
        Ok(command)
    }

    /// Install the map at `root` for `package`, transferring only what
    /// differs from the root this host last installed in full. With `lazy`,
    /// install only mappings; Lake downloads artifacts when a build needs
    /// them.
    pub fn install(
        &mut self,
        project: &Project,
        package: &Package,
        root_package: bool,
        root: Cid<Bdasl>,
        lazy: bool,
    ) -> Result<()> {
        let local = &self.local;
        let nodes = pool::copy_nodes(&self.remote, &*local.nodes(), root)?;
        let target = Map::open(local.nodes(), root);
        let level = if lazy {
            Installed::Mappings
        } else {
            Installed::Artifacts
        };
        let installed = local.installed(level, &self.scope, &package.name)?;
        let base = installed.map(|root| target.at(root));
        let transfer = pool::transfer(base.as_ref(), &target)?;
        println!(
            "Lake cache: {}: {} changed entries naming {} new artifacts, {nodes} map nodes fetched",
            package.name,
            transfer.entries.len(),
            transfer.artifacts.len(),
        );
        // A lazy build may fetch any artifact whose mapping an earlier lazy
        // restore installed, so it may be served the whole map.
        let served = if lazy {
            pool::entries(&target)?
        } else {
            transfer.entries.clone()
        };
        for entry in served.values() {
            for (name, cid) in &entry.artifacts {
                self.index.insert(name[..16].to_owned(), cid.to_string());
            }
        }
        self.save_index()?;
        if !transfer.entries.is_empty() {
            let file = self
                .directory
                .path()
                .join(format!("{}.jsonl", package.name));
            let outputs = if lazy {
                transfer
                    .entries
                    .iter()
                    .map(|(input, entry)| (input.clone(), entry.output.clone()))
                    .collect()
            } else {
                archives::unpack(
                    &self.blobs,
                    &self.local.directory().join("artifacts"),
                    &transfer.entries,
                )?
            };
            mapping::write(&file, &outputs)?;
            let mut command = self.lake(project)?;
            command
                .args(["cache", if lazy { "add" } else { "get" }])
                .args(["--service", SERVICE, "--scope", &self.scope]);
            if !root_package {
                command.args(["--package", &package.name]);
            }
            run("Lake restore", command.arg(&file))?;
        }
        let levels: &[Installed] = if lazy {
            &[Installed::Mappings]
        } else {
            &[Installed::Mappings, Installed::Artifacts]
        };
        for level in levels {
            self.local
                .set_installed(*level, &self.scope, &package.name, root)?;
        }
        Ok(())
    }

    /// Build `targets` strictly. Lake takes each artifact from its local
    /// cache, else downloads it the first time the build needs it, else
    /// builds the module.
    pub fn build(&self, project: &Project, targets: &[String]) -> Result<()> {
        run(
            "Lake target restore",
            self.lake(project)?.args(["build", "--wfail"]).args(targets),
        )
    }
}

/// Restores this repository's packages from the nearest published commit.
/// With targets, installs only mappings, for dependencies too, then builds
/// what the targets need.
pub(crate) fn restore(project: &Project, kv: &Arc<dyn Kv>, targets: &[String]) -> Result<()> {
    let identity = project.identity()?;
    let scope = &identity.scope;
    let packages = project.packages()?;
    let commits = project.ancestors()?;
    let mut session = Session::new(project, kv, scope)?;
    if !targets.is_empty() {
        dependencies::install(project, kv, &mut session, true)?;
    }
    match storage::nearest(&**kv, project, scope, &packages[0], &commits)? {
        None => {
            println!(
                "Lake cache: no published build for this toolchain and platform in {} commits",
                commits.len()
            );
            if targets.is_empty() {
                return Ok(());
            }
        }
        Some((commit, _)) => {
            let behind = commits
                .iter()
                .position(|c| *c == commit)
                .unwrap_or_default();
            println!("Lake cache: restoring {commit}, {behind} commits behind HEAD");
            for (index, package) in packages.iter().enumerate() {
                if package.source != Source::Repository {
                    continue;
                }
                let key = project.revision_key(scope, package, &commit)?;
                if let Some(root) = storage::pointer(&**kv, &key)? {
                    session.install(project, package, index == 0, root, !targets.is_empty())?;
                }
            }
        }
    }
    if targets.is_empty() {
        return Ok(());
    }
    session.build(project, targets)
}

fn key(url: &str, scope: &str) -> Result<String> {
    let prefix = format!("{BASE}/a/{scope}/");
    let name = url
        .strip_prefix(&prefix)
        .ok_or_else(|| invalid("Lake requested a URL outside the configured scope"))?;
    match name.strip_suffix(".art") {
        Some(hash) if mapping::hex(hash, 16) => Ok(hash.to_owned()),
        _ => Err(invalid("Lake requested an invalid cache object")),
    }
}

fn report(index: usize, found: Option<u64>) -> Result<()> {
    let row = encode(
        &json!({ "urlnum": index, "exitcode": 0, "http_code": if found.is_some() { 200 } else { 404 }, "response_code": if found.is_some() { 200 } else { 404 }, "size_download": found.unwrap_or(0), "content_type": "application/octet-stream" }),
    )?;
    let mut stderr = io::stderr().lock();
    stderr
        .write_all(&row)
        .operation("write Lake transfer status")?;
    stderr
        .write_all(b"\n")
        .operation("write Lake transfer status")
}

fn artifact_directory() -> Result<PathBuf> {
    PathBuf::from(
        env::var_os("COVALENCE_LAKE_ARTIFACTS")
            .ok_or_else(|| invalid("missing Lake artifact directory"))?,
    )
    .canonicalize()
    .operation("locate artifact directory")
}

/// The CIDs of the artifacts this session may serve, by Lake hash.
fn index() -> Result<BTreeMap<String, Sha256>> {
    let path =
        env::var_os("COVALENCE_LAKE_INDEX").ok_or_else(|| invalid("missing artifact index"))?;
    let index: BTreeMap<String, String> = json(&fs::read(path).operation("read artifact index")?)?;
    index
        .into_iter()
        .map(|(hash, cid)| {
            Ok((
                hash,
                cid.parse()
                    .map_err(|_| invalid("invalid artifact index CID"))?,
            ))
        })
        .collect()
}

/// Stream one artifact from the pool into a new file at `path`.
fn fetch(store: &Blobs, cid: &Sha256, path: &Path) -> Result<Option<u64>> {
    let mut file = File::create_new(path).operation("create Lake artifact")?;
    let found = store.get_to(cid, &mut file).and_then(|found| {
        file.flush().operation("flush Lake artifact")?;
        Ok(found)
    });
    match found {
        Ok(Some(bytes)) => Ok(Some(bytes)),
        Ok(None) => {
            fs::remove_file(path).operation("remove missing artifact")?;
            eprintln!("{cid} is missing from the pool");
            Ok(None)
        }
        Err(error) => {
            fs::remove_file(path).operation("remove incomplete artifact")?;
            Err(error)
        }
    }
}

fn pool_store(scope: &str) -> Result<Blobs> {
    pool::blobs(storage::store()?, scope)
}

/// Serves one artifact a build finds missing; a miss makes Lake build it.
fn download_file(scope: &str, url: &str, path: &Path) -> Result<ExitCode> {
    let hash = key(url, scope)?;
    let parent = path
        .parent()
        .ok_or_else(|| invalid("output has no parent"))?
        .canonicalize()
        .operation("locate download parent")?;
    if parent != artifact_directory()?
        || !path
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix(hash.as_str()))
            .is_some_and(|extension| extension.starts_with('.') && !extension.ends_with(".tmp"))
    {
        return Err(invalid("unsafe artifact destination"));
    }
    let Some(cid) = index()?.remove(&hash) else {
        eprintln!("{hash} is not in the installed maps");
        // curl's exit status for an HTTP error under `--fail`.
        return Ok(ExitCode::from(22));
    };
    match fetch(&pool_store(scope)?, &cid, path)? {
        Some(_) => Ok(ExitCode::SUCCESS),
        None => Ok(ExitCode::from(22)),
    }
}

/// The configuration file of a bulk download, if `args` are one.
///
/// Lake before nightly-2026-11-05 passed `--retry 3` after `-L`; later Lake
/// retries failed transfers itself and omits it.
fn bulk_config(args: &[String]) -> Option<&str> {
    let args = match args {
        [z, x, get, l, retry, count, rest @ ..] if retry == "--retry" && count == "3" => {
            let head = [z, x, get, l];
            return (head == ["-Z", "-X", "GET", "-L"])
                .then(|| bulk_tail(rest))
                .flatten();
        }
        [z, x, get, l, rest @ ..] if [z, x, get, l] == ["-Z", "-X", "GET", "-L"] => rest,
        _ => return None,
    };
    bulk_tail(args)
}

fn bulk_tail(args: &[String]) -> Option<&str> {
    match args {
        [s, w, format, config, path]
            if [s, w, format, config] == ["-s", "-w", FORMAT, "--config"] =>
        {
            Some(path)
        }
        _ => None,
    }
}

pub(crate) fn download(args: &[String]) -> Result<ExitCode> {
    let scope =
        env::var("COVALENCE_LAKE_SCOPE").map_err(|_| invalid("missing Lake transfer scope"))?;
    mapping::segment_path(&scope)?;
    let file = ["-s", "-S", "-f", "-o"];
    let url = args.last().map(String::as_str).unwrap_or_default();
    let bulk = bulk_config(args);
    if !(bulk.is_some() || url.starts_with(&format!("{BASE}/"))) {
        // Another service's request, such as a dependency artifact. Curl gets
        // Lake's arguments but none of the cache's AWS credentials.
        let mut curl = Command::new("curl");
        curl.args(args)
            .env_remove("AWS_ACCESS_KEY_ID")
            .env_remove("AWS_SECRET_ACCESS_KEY")
            .env_remove("AWS_SESSION_TOKEN");
        for name in storage::LEASED {
            curl.env_remove(name);
        }
        let status = curl.status().operation("start curl")?;
        return Ok(status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .map_or(ExitCode::FAILURE, ExitCode::from));
    }
    if args.len() == file.len() + 3 && args[..file.len()] == file && args[file.len() + 1] == "-L" {
        return download_file(&scope, url, Path::new(&args[file.len()]));
    }
    let Some(config) = bulk else {
        return Err(invalid("unsupported Lake download protocol"));
    };
    let config = fs::read_to_string(config).operation("read Lake transfer configuration")?;
    let directory = artifact_directory()?;
    let lines = config.lines().collect::<Vec<_>>();
    if lines.is_empty() || !lines.len().is_multiple_of(2) {
        return Err(invalid("incomplete Lake transfer configuration"));
    }
    let index = index()?;
    let mut destinations = std::collections::BTreeSet::new();
    // Validate every requested artifact before starting any downloads.
    let transfers = lines
        .as_chunks::<2>()
        .0
        .iter()
        .map(|lines| {
            let url: String = json(
                lines[0]
                    .strip_prefix("url = ")
                    .ok_or_else(|| invalid("expected Lake download URL"))?
                    .as_bytes(),
            )?;
            let path: PathBuf = json(
                lines[1]
                    .strip_prefix("-o ")
                    .ok_or_else(|| invalid("expected Lake output path"))?
                    .as_bytes(),
            )?;
            let hash = key(&url, &scope)?;
            let parent = path
                .parent()
                .ok_or_else(|| invalid("output has no parent"))?
                .canonicalize()
                .operation("locate download parent")?;
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| invalid("invalid download filename"))?;
            if parent != directory
                || !name.starts_with(&hash)
                || !name.ends_with(".tmp")
                || !destinations.insert(path.clone())
            {
                return Err(invalid("unsafe or duplicate artifact destination"));
            }
            let cid = index
                .get(&hash)
                .ok_or_else(|| invalid("requested artifact is absent from the installed maps"))?;
            Ok((*cid, path))
        })
        .collect::<Result<Vec<_>>>()?;
    let store = pool_store(&scope)?;
    storage::parallel(&transfers, |index, (cid, path)| {
        report(index, fetch(&store, cid, path)?)
    })?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn bulk_downloads_match_lake_before_and_after_nightly_2026_11_05() {
        // Lake at nightly-2026-11-05 (lean4#14725) and later.
        let current = argv(&[
            "-Z", "-X", "GET", "-L", "-s", "-w", FORMAT, "--config", "/t/c",
        ]);
        assert_eq!(bulk_config(&current), Some("/t/c"));
        // Lake at nightly-2026-10-30 and earlier.
        let earlier = argv(&[
            "-Z", "-X", "GET", "-L", "--retry", "3", "-s", "-w", FORMAT, "--config", "/t/c",
        ]);
        assert_eq!(bulk_config(&earlier), Some("/t/c"));
        for other in [
            argv(&[
                "-Z", "-X", "PUT", "-L", "-s", "-w", FORMAT, "--config", "/t/c",
            ]),
            argv(&["-Z", "-X", "GET", "-L", "-s", "-w", FORMAT, "--config"]),
            argv(&[
                "-Z", "-X", "GET", "-L", "--retry", "9", "-s", "-w", FORMAT, "--config", "/t/c",
            ]),
            argv(&[
                "-s",
                "-S",
                "-f",
                "-o",
                "/t/a",
                "-L",
                "https://example.invalid/a",
            ]),
        ] {
            assert_eq!(bulk_config(&other), None, "{other:?}");
        }
    }
}
