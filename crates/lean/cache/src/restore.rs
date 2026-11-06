//! Adapt Lake's three read-only curl protocols to the S3 transport.
//!
//! Lake, not this adapter, verifies artifact hashes and installs mappings.
//! Requests for other cache services go to `curl` unchanged.

use std::{
    env,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

use covalence_kv_s3::S3Kv;
use covalence_kv_trait::Kv;
use serde_json::json;

use crate::{
    artifact::{self, Manifest},
    dependencies,
    error::{Error, IoResult, Result, encode, invalid, json},
    mapping,
    project::{Project, output, run},
    storage,
};

const BASE: &str = "https://covalence-lake.invalid";
const SERVICE: &str = "covalence";
const FORMAT: &str = "%{stderr}%{json}\n";

/// Restores the latest published snapshot; with targets, only what they need.
pub(crate) fn restore(project: &Project, store: &S3Kv, targets: &[String]) -> Result<()> {
    let identity = project.identity()?;
    let revision = latest(store, &identity.scope)?;
    if revision.is_none() {
        println!("Lake cache: no published build for these pins and platform");
        if targets.is_empty() {
            return Ok(());
        }
    }
    // This also materializes the workspace's packages, including Mathlib.
    let directory = output(
        "Lake",
        project.lake().args(["env", "printenv", "LAKE_CACHE_DIR"]),
    )?;
    fs::create_dir_all(PathBuf::from(&directory).join("artifacts"))
        .operation("create Lake artifact directory")?;
    let artifact_dir = PathBuf::from(directory)
        .join("artifacts")
        .canonicalize()
        .operation("locate artifact directory")?;
    let temporary = tempfile::tempdir().operation("create Lake cache configuration")?;
    let mut settings = String::new();
    if !targets.is_empty() && dependencies::config(project).exists() {
        // The target build downloads dependency artifacts through the
        // services that Mathlib's configuration names.
        dependencies::mappings(project, true)?;
        settings = fs::read_to_string(dependencies::config(project))
            .operation("read dependency cache configuration")?
            + "\n";
    }
    settings.push_str(&format!("[[cache.service]]\nname = \"{SERVICE}\"\nkind = \"s3\"\nartifactEndpoint = \"{BASE}/a\"\nrevisionEndpoint = \"{BASE}/r\"\n"));
    let config = temporary.path().join("config.toml");
    fs::write(&config, settings).operation("write Lake cache configuration")?;
    let manifest_path = temporary.path().join(artifact::MANIFEST);
    let curl = env::current_exe().operation("locate cache executable")?;
    let lake = || {
        let mut command = project.lake();
        command
            .env("LAKE_CONFIG", &config)
            .env("CURL", &curl)
            .env("COVALENCE_LAKE_SCOPE", &identity.scope)
            .env("COVALENCE_LAKE_ARTIFACTS", &artifact_dir)
            .env("COVALENCE_LAKE_MANIFEST", &manifest_path);
        command
    };
    if let Some(revision) = &revision {
        let mut manifest = Limited {
            bytes: Vec::new(),
            limit: 64 * 1024 * 1024,
        };
        let manifest_key = format!("r/{}/{revision}.artifacts.json", identity.scope);
        let found = store
            .get_to(manifest_key.as_bytes(), &mut manifest)
            .map_err(|source| Error::Store {
                operation: "download artifact manifest".into(),
                source,
            })?;
        if found.is_none() {
            return Err(invalid("published revision has no artifact manifest"));
        }
        Manifest::parse(&manifest.bytes)?;
        fs::write(&manifest_path, &manifest.bytes).operation("save artifact manifest")?;
        let mut command = lake();
        command.args([
            "cache",
            "get",
            "--service",
            SERVICE,
            "--scope",
            &identity.scope,
            "--rev",
            revision,
        ]);
        if !project.package.is_empty() {
            command.args(["--package", &project.package]);
        }
        if targets.is_empty() {
            return run("Lake restore", &mut command);
        }
        run("Lake restore", command.arg("--mappings-only"))?;
    }
    // Lake takes each artifact from its local cache, else downloads it the
    // first time the build needs it, else builds the module.
    run(
        "Lake target restore",
        lake().args(["build", "--wfail"]).args(targets),
    )
}

fn latest(store: &S3Kv, scope: &str) -> Result<Option<String>> {
    let key = format!("r/{scope}/main-latest");
    let mut bytes = Limited {
        bytes: Vec::new(),
        limit: 128,
    };
    let found = store
        .get_to(key.as_bytes(), &mut bytes)
        .map_err(|source| Error::Store {
            operation: "read latest cache revision".into(),
            source,
        })?;
    if found.is_none() {
        return Ok(None);
    }
    let revision = str::from_utf8(&bytes.bytes)
        .map_err(|_| invalid("invalid revision pointer"))?
        .trim();
    if !mapping::hex(revision, 40) {
        return Err(invalid("invalid cache revision pointer"));
    }
    Ok(Some(revision.to_owned()))
}

struct Limited {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for Limited {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("cache metadata exceeds size limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn key(url: &str, scope: &str, kind: &str) -> Result<String> {
    let prefix = format!("{BASE}/{kind}/{scope}/");
    let name = url
        .strip_prefix(&prefix)
        .ok_or_else(|| invalid("Lake requested a URL outside the configured scope"))?;
    let valid = if kind == "a" {
        name.strip_suffix(".art")
            .is_some_and(|s| mapping::hex(s, 16))
    } else {
        name.strip_suffix(".jsonl")
            .is_some_and(|s| mapping::hex(s, 40))
    };
    if !valid {
        return Err(invalid("Lake requested an invalid cache object"));
    }
    Ok(format!("{kind}/{scope}/{name}"))
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

fn published_manifest() -> Result<Manifest> {
    let manifest = env::var_os("COVALENCE_LAKE_MANIFEST")
        .ok_or_else(|| invalid("missing artifact manifest"))?;
    Manifest::parse(&fs::read(manifest).operation("read artifact manifest")?)
}

/// Serves one artifact a build finds missing; a miss makes Lake build it.
fn download_file(scope: &str, url: &str, path: &Path) -> Result<ExitCode> {
    let object = key(url, scope, "a")?;
    let name = &object[object.len() - 20..];
    let parent = path
        .parent()
        .ok_or_else(|| invalid("output has no parent"))?
        .canonicalize()
        .operation("locate download parent")?;
    if parent != artifact_directory()?
        || !path
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix(&name[..16]))
            .is_some_and(|extension| extension.starts_with('.') && !extension.ends_with(".tmp"))
    {
        return Err(invalid("unsafe artifact destination"));
    }
    let Some(info) = published_manifest()?.artifacts.remove(name) else {
        eprintln!("{name} is not in the published snapshot");
        // curl's exit status for an HTTP error under `--fail`.
        return Ok(ExitCode::from(22));
    };
    let store = storage::store()?;
    let mut file = File::create_new(path).operation("create Lake artifact")?;
    let found = artifact::decode_to(&info, &mut file, |decoding| {
        store
            .get_to(format!("{object}.zst").as_bytes(), decoding)
            .map_err(|source| Error::Store {
                operation: format!("download {object}"),
                source,
            })
    })
    .and_then(|found| {
        file.flush().operation("flush Lake artifact")?;
        Ok(found)
    });
    match found {
        Ok(Some(_)) => Ok(ExitCode::SUCCESS),
        Ok(None) => {
            fs::remove_file(path).operation("remove missing artifact")?;
            eprintln!("{object} is missing from the cache bucket");
            Ok(ExitCode::from(22))
        }
        Err(error) => {
            fs::remove_file(path).operation("remove incomplete artifact")?;
            Err(error)
        }
    }
}

pub(crate) fn download(args: &[String]) -> Result<ExitCode> {
    let scope =
        env::var("COVALENCE_LAKE_SCOPE").map_err(|_| invalid("missing Lake transfer scope"))?;
    mapping::segment_path(&scope)?;
    let single = ["-s", "-L", "-w", FORMAT, "--retry", "3"];
    let file = ["-s", "-S", "-f", "-o"];
    let bulk = [
        "-Z", "-X", "GET", "-L", "--retry", "3", "-s", "-w", FORMAT, "--config",
    ];
    let url = args.last().map(String::as_str).unwrap_or_default();
    if !(args.starts_with(&bulk.map(String::from)) || url.starts_with(&format!("{BASE}/"))) {
        // Another service's request, such as a dependency artifact. Curl gets
        // Lake's arguments but none of the cache's AWS credentials.
        let status = Command::new("curl")
            .args(args)
            .env_remove("AWS_ACCESS_KEY_ID")
            .env_remove("AWS_SECRET_ACCESS_KEY")
            .env_remove("AWS_SESSION_TOKEN")
            .status()
            .operation("start curl")?;
        return Ok(status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .map_or(ExitCode::FAILURE, ExitCode::from));
    }
    if args.len() == file.len() + 3 && args[..file.len()] == file && args[file.len() + 1] == "-L" {
        return download_file(&scope, url, Path::new(&args[file.len()]));
    }
    if args.len() == single.len() + 1 && args[..single.len()] == single {
        let object = key(&args[single.len()], &scope, "r")?;
        let store = storage::store()?;
        let mut body = Limited {
            bytes: Vec::new(),
            limit: 64 * 1024 * 1024,
        };
        let found = store
            .get_to(object.as_bytes(), &mut body)
            .map_err(|source| Error::Store {
                operation: "download Lake mappings".into(),
                source,
            })?;
        io::stdout()
            .lock()
            .write_all(&body.bytes)
            .operation("write Lake mappings")?;
        report(0, found)?;
        return Ok(ExitCode::SUCCESS);
    }
    if args.len() != bulk.len() + 1 || args[..bulk.len()] != bulk {
        return Err(invalid("unsupported Lake download protocol"));
    }
    let config =
        fs::read_to_string(&args[bulk.len()]).operation("read Lake transfer configuration")?;
    let directory = artifact_directory()?;
    let lines = config.lines().collect::<Vec<_>>();
    if lines.is_empty() || !lines.len().is_multiple_of(2) {
        return Err(invalid("incomplete Lake transfer configuration"));
    }
    let mut destinations = std::collections::BTreeSet::new();
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
            let object = key(&url, &scope, "a")?;
            let parent = path
                .parent()
                .ok_or_else(|| invalid("output has no parent"))?
                .canonicalize()
                .operation("locate download parent")?;
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| invalid("invalid download filename"))?;
            let hash = &object[object.len() - 20..object.len() - 4];
            if parent != directory
                || !name.starts_with(hash)
                || !name.ends_with(".tmp")
                || !destinations.insert(path.clone())
            {
                return Err(invalid("unsafe or duplicate artifact destination"));
            }
            Ok((object, path))
        })
        .collect::<Result<Vec<_>>>()?;
    let store = storage::store()?;
    let manifest = published_manifest()?;
    // Validate every requested artifact before starting any downloads.
    let transfers = transfers
        .iter()
        .map(|(object, path)| {
            let name = object
                .rsplit('/')
                .next()
                .ok_or_else(|| invalid("artifact key has no name"))?;
            let info = manifest.artifacts.get(name).ok_or_else(|| {
                invalid("requested artifact is absent from the published manifest")
            })?;
            Ok((object, path, info))
        })
        .collect::<Result<Vec<_>>>()?;
    storage::parallel(&transfers, |index, (object, path, info)| {
        let mut file = File::create_new(path).operation("create temporary Lake artifact")?;
        let found = artifact::decode_to(info, &mut file, |decoding| {
            store
                .get_to(format!("{object}.zst").as_bytes(), decoding)
                .map_err(|source| Error::Store {
                    operation: format!("download {object}"),
                    source,
                })
        })?;
        file.flush().operation("flush Lake artifact")?;
        report(index, found)
    })?;
    Ok(ExitCode::SUCCESS)
}
