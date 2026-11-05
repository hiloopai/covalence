//! Adapt Lake's two read-only curl protocols to the S3 transport.
//!
//! Lake, not this adapter, verifies artifact hashes and installs mappings.

use std::{
    env,
    fs::{self, File},
    io::{self, Write},
    path::PathBuf,
};

use covalence_kv_s3::S3Kv;
use covalence_kv_trait::Kv;
use serde_json::json;

use crate::{
    artifact::{self, Manifest},
    error::{Error, IoResult, Result, encode, invalid, json},
    mapping,
    project::{Project, output, run},
    storage,
};

const BASE: &str = "https://covalence-lake.invalid";
const SERVICE: &str = "covalence";
const FORMAT: &str = "%{stderr}%{json}\n";

pub(crate) fn restore(project: &Project, store: &S3Kv) -> Result<()> {
    let identity = project.identity()?;
    let key = format!("r/{}/main-latest", identity.scope);
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
        println!("Lake cache: no published build for these pins and platform");
        return Ok(());
    }
    let revision = str::from_utf8(&bytes.bytes)
        .map_err(|_| invalid("invalid revision pointer"))?
        .trim();
    if !mapping::hex(revision, 40) {
        return Err(invalid("invalid cache revision pointer"));
    }
    let temporary = tempfile::tempdir().operation("create Lake cache configuration")?;
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
    let manifest_path = temporary.path().join(artifact::MANIFEST);
    fs::write(&manifest_path, &manifest.bytes).operation("save artifact manifest")?;
    let config = temporary.path().join("config.toml");
    fs::write(&config, format!("[[cache.service]]\nname = \"{SERVICE}\"\nkind = \"s3\"\nartifactEndpoint = \"{BASE}/a\"\nrevisionEndpoint = \"{BASE}/r\"\n")).operation("write Lake cache configuration")?;
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
    let mut command = project.lake();
    command
        .env("LAKE_CONFIG", config)
        .env(
            "CURL",
            env::current_exe().operation("locate cache executable")?,
        )
        .env("COVALENCE_LAKE_SCOPE", &identity.scope)
        .env("COVALENCE_LAKE_ARTIFACTS", artifact_dir)
        .env("COVALENCE_LAKE_MANIFEST", manifest_path)
        .args([
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
    run("Lake restore", &mut command)
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

pub(crate) fn download(args: &[String]) -> Result<()> {
    let scope =
        env::var("COVALENCE_LAKE_SCOPE").map_err(|_| invalid("missing Lake transfer scope"))?;
    mapping::segment_path(&scope)?;
    let single = ["-s", "-L", "-w", FORMAT, "--retry", "3"];
    let bulk = [
        "-Z", "-X", "GET", "-L", "--retry", "3", "-s", "-w", FORMAT, "--config",
    ];
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
        return report(0, found);
    }
    if args.len() != bulk.len() + 1 || args[..bulk.len()] != bulk {
        return Err(invalid("unsupported Lake download protocol"));
    }
    let config =
        fs::read_to_string(&args[bulk.len()]).operation("read Lake transfer configuration")?;
    let directory = PathBuf::from(
        env::var_os("COVALENCE_LAKE_ARTIFACTS")
            .ok_or_else(|| invalid("missing Lake artifact directory"))?,
    )
    .canonicalize()
    .operation("locate artifact directory")?;
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
    let manifest = env::var_os("COVALENCE_LAKE_MANIFEST")
        .ok_or_else(|| invalid("missing artifact manifest"))?;
    let manifest = Manifest::parse(&fs::read(manifest).operation("read artifact manifest")?)?;
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
    Ok(())
}
