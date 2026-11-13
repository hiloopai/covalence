//! Lake outputs by input hash, shared by every host: what one host's Lake
//! built, any other takes instead of building.
//!
//! `put` publishes each output Lake built itself, committed or not; `get`
//! and `build` point the bundled overlay's Lake at this executable, which
//! answers each lookup of an input its cache lacks (stock Lake never asks).
//!
//! Layout, under `lake/<os>-<arch>/<toolchain scope>/`:
//!
//! - `out/<package>/<input>.json`: one input's [`Entry`]: Lake's output record
//!   and the content ID of each artifact it names.
//! - `cas/sha2-256/…`: the artifacts, in the pool's one store.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use covalence_kv_trait::Kv;
use covalence_lib_cid::Sha256;
use covalence_lib_data::Bytes;
use covalence_lib_serde::{Deserialize, Serialize};

use crate::{
    error::{IoResult, Result, encode, invalid, json},
    mapping,
    project::Project,
};

pub(crate) fn key(scope: &str, package: &str, input: &str) -> Result<String> {
    for name in [scope, package] {
        mapping::segment_path(name)?;
    }
    if !mapping::hex(input, 16) {
        return Err(invalid("an output is keyed by a 16-digit input hash"));
    }
    Ok(format!("lake/{scope}/out/{package}/{input}.json"))
}

/// What a build made from one input: Lake's output record, as Lake wrote it to
/// its cache's `outputs/<package>/<input>.json`, and the content ID of each
/// artifact it names.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(crate = "covalence_lib_serde::serde", deny_unknown_fields)]
pub(crate) struct Entry {
    pub record: serde_json::Value,
    pub artifacts: BTreeMap<String, String>,
}

/// What this host's Lake built itself since the first `get` or `build`
/// after this checkout's last `put` (all of it before any): each output
/// record in Lake's cache written since, by package and input. Records a
/// restore installed name the service they came from, and ones a seed or a
/// lookup installed are dated before any run ([`installed`]); ones Lake
/// wrote after building name no service.
pub(crate) type Built = Vec<(String, String, serde_json::Value)>;

pub(crate) fn built(project: &Project) -> Result<Built> {
    let outputs = project.cache_dir()?.join("outputs");
    let since = fs::metadata(started_marker(project))
        .and_then(|marker| marker.modified())
        .ok();
    let mut found = Vec::new();
    for path in crate::project::files(&outputs, "json")? {
        let written = fs::metadata(&path)
            .and_then(|record| record.modified())
            .operation(format!("read {}", path.display()))?;
        if since.is_some_and(|since| written < since) {
            continue;
        }
        let (Some(package), Some(input)) = (
            path.parent()
                .and_then(|folder| folder.strip_prefix(&outputs).ok())
                .and_then(Path::to_str),
            path.file_stem().and_then(|stem| stem.to_str()),
        ) else {
            continue;
        };
        if !mapping::hex(input, 16) {
            continue;
        }
        let record: serde_json::Value =
            json(&fs::read(&path).operation(format!("read {}", path.display()))?)?;
        // Lake writes `"service": null` for what it built itself.
        if record["service"].is_string() {
            continue;
        }
        found.push((package.to_owned(), input.to_owned(), record));
    }
    Ok(found)
}

/// Publish what `built` found: each input's record and the artifacts the
/// pool lacks.
pub(crate) fn publish(
    project: &Project,
    kv: Arc<dyn Kv>,
    scope: &str,
    found: &Built,
) -> Result<()> {
    let started = Instant::now();
    let cache = project.cache_dir()?;
    let blobs = crate::pool::blobs(kv.clone(), scope)?;
    let published = crate::storage::parallel(found, |_, (package, input, record)| {
        let key = key(scope, package, input)?;
        if kv
            .exists(key.as_bytes())
            .map_err(store_error("check output"))?
        {
            return Ok(false);
        }
        let mut artifacts = BTreeMap::new();
        for name in mapping::names(&record["data"])? {
            let path = cache.join("artifacts").join(&name);
            if !path.is_file() {
                // Lake keeps only what it restored or built; another build
                // supplies this input.
                return Ok(false);
            }
            let (cid, _) = crate::blobs::hash_file(&path)?;
            if !blobs.has(&cid)? {
                blobs.put_file(&cid, &path)?;
            }
            artifacts.insert(name, cid.to_string());
        }
        let entry = Entry {
            record: record.clone(),
            artifacts,
        };
        kv.put(key.as_bytes(), Bytes::from(encode(&entry)?))
            .map_err(store_error("publish output"))?;
        Ok(true)
    })?;
    println!(
        "Lake cache: published {} of the {} outputs this host built in {:.1} s",
        published.iter().filter(|published| **published).count(),
        found.len(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn store_error(
    operation: &'static str,
) -> impl FnOnce(covalence_kv_trait::Error) -> crate::error::Error {
    move |source| crate::error::Error::Store {
        operation: operation.into(),
        source,
    }
}

/// Serve Lake's request for the outputs of `input`: from the pool, with each
/// artifact the entry names placed in Lake's `artifacts` folder first,
/// written to `path`, Lake's own outputs file for it. `false` on a miss,
/// which writes nothing.
pub(crate) fn serve(
    kv: Arc<dyn Kv>,
    scope: &str,
    package: &str,
    input: &str,
    path: &Path,
    artifacts: &Path,
) -> Result<bool> {
    let key = key(scope, package, input)?;
    let Some(bytes) = kv.get(key.as_bytes()).map_err(store_error("read output"))? else {
        return Ok(false);
    };
    let entry: Entry = json(&bytes)?;
    let names = mapping::names(&entry.record["data"])?;
    if names.iter().collect::<BTreeSet<_>>() != entry.artifacts.keys().collect::<BTreeSet<_>>() {
        return Err(invalid(format!(
            "output {key} names other artifacts than it lists"
        )));
    }
    let blobs = crate::pool::blobs(kv, scope)?;
    for (name, cid) in &entry.artifacts {
        let target = artifacts.join(name);
        if target.is_file() {
            continue;
        }
        let cid: Sha256 = cid
            .parse()
            .map_err(|_| invalid(format!("output {key} names an invalid CID")))?;
        let mut pending = tempfile::NamedTempFile::new_in(artifacts).operation("stage artifact")?;
        if blobs.get_to(&cid, &mut pending)?.is_none() {
            // Expired before its entry: a miss, and Lake builds.
            return Ok(false);
        }
        let mut permissions = pending
            .as_file()
            .metadata()
            .operation("read artifact permissions")?
            .permissions();
        permissions.set_readonly(true);
        pending
            .as_file()
            .set_permissions(permissions)
            .operation("mark artifact read-only")?;
        if let Err(error) = pending.persist_noclobber(&target)
            && error.error.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(error.error).operation("place artifact");
        }
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid("outputs file has no folder"))?;
    fs::create_dir_all(parent).operation("create Lake outputs folder")?;
    let mut pending = tempfile::NamedTempFile::new_in(parent).operation("stage Lake outputs")?;
    pending
        .write_all(&encode(&entry.record)?)
        .operation("write Lake outputs")?;
    installed(pending.as_file())?;
    pending
        .persist(path)
        .map_err(|error| error.error)
        .operation("place Lake outputs")?;
    Ok(true)
}

/// Date a record this tool installs, from a seed or a lookup, before any
/// run: `put` takes a record written since the run started for one this
/// host's Lake built, and Lake writes its own with the current time.
pub(crate) fn installed(file: &fs::File) -> Result<()> {
    file.set_modified(std::time::SystemTime::UNIX_EPOCH)
        .operation("date an installed Lake record")
}

/// The socket of this process's [`Service`], for the `curl` stand-ins Lake
/// runs.
const SOCKET: &str = "COVALENCE_LAKE_OUTPUTS_SOCKET";

/// Answers every outputs lookup of this process's Lake runs from one warm
/// store. Lake asks once per input its cache lacks, through a `curl`
/// stand-in process each time, and a module's lookup waits for its imports'
/// builds, so lookups add up along the build's longest chain: a stand-in
/// that opened its own store paid a client start, credential load and TLS
/// handshake every time. Each one now costs a local socket round trip and
/// one pooled request.
struct Service {
    socket: PathBuf,
    fetched: AtomicUsize,
    // Holds the socket's folder for the process's life.
    _folder: tempfile::TempDir,
}

static SERVICE: Mutex<Option<&'static Service>> = Mutex::new(None);

impl Service {
    /// This process's service, started on first use.
    fn get(project: &Project) -> Result<&'static Service> {
        let mut service = SERVICE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(service) = *service {
            return Ok(service);
        }
        let folder = tempfile::Builder::new()
            .prefix("cov-lake-")
            .tempdir()
            .operation("create outputs service folder")?;
        let socket = folder.path().join("outputs.sock");
        let listener = UnixListener::bind(&socket).operation("open outputs service socket")?;
        let started: &'static Service = Box::leak(Box::new(Service {
            socket,
            fetched: AtomicUsize::new(0),
            _folder: folder,
        }));
        let kv = crate::storage::store()?;
        let artifacts = project.cache_dir()?.join("artifacts");
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (kv, artifacts) = (kv.clone(), artifacts.clone());
                std::thread::spawn(move || started.answer(stream, kv, &artifacts));
            }
        });
        *service = Some(started);
        Ok(started)
    }

    /// One lookup: `<request>\t<outputs file>\n` in, curl's exit status out.
    fn answer(&self, mut stream: UnixStream, kv: Arc<dyn Kv>, artifacts: &Path) {
        let mut line = String::new();
        let answer = BufReader::new(&stream)
            .read_line(&mut line)
            .operation("read an outputs lookup")
            .and_then(|_| {
                let (rest, path) = line
                    .trim_end_matches('\n')
                    .split_once('\t')
                    .ok_or_else(|| invalid("invalid output request"))?;
                lookup(kv, rest, Path::new(path), artifacts)
            });
        let code = match answer {
            Ok(true) => {
                self.fetched.fetch_add(1, Ordering::Relaxed);
                0
            }
            Ok(false) => 22,
            Err(error) => {
                // Lake takes any failure as a miss and says nothing.
                eprintln!("Lake cache: an outputs lookup failed: {error}");
                1
            }
        };
        // Lake's stand-in gone means Lake no longer waits for the answer.
        let _ = writeln!(stream, "{code}");
    }
}

/// Point Lake's lookups of the outputs its cache lacks at the pool, through
/// this process's [`Service`]; the overlay's Lake asks
/// `LAKE_OUTPUTS_ENDPOINT`, and stock Lake never does.
pub(crate) fn lookups(
    command: &mut std::process::Command,
    project: &Project,
    scope: &str,
) -> Result<()> {
    command
        .env(
            "CURL",
            std::env::current_exe().operation("locate cache executable")?,
        )
        .env("COVALENCE_LAKE_SCOPE", scope)
        .env(
            "COVALENCE_LAKE_ARTIFACTS",
            project.cache_dir()?.join("artifacts"),
        )
        .env(
            "LAKE_OUTPUTS_ENDPOINT",
            format!("{}/o/{scope}", crate::restore::BASE),
        )
        .env(SOCKET, &Service::get(project)?.socket);
    Ok(())
}

/// Lake's `curl` of `<endpoint>/<os-arch>/<hash>/<package>/<input>.json`
/// into `path`, sent to the [`Service`] that ran Lake. Curl's exit status:
/// 22 on a miss.
pub(crate) fn serve_request(rest: &str, path: &Path) -> Result<std::process::ExitCode> {
    let socket = std::env::var_os(SOCKET).ok_or_else(|| invalid("no outputs service"))?;
    let path = path.to_str().filter(|path| !path.contains(['\t', '\n']));
    let path = path.ok_or_else(|| invalid("unsupported Lake outputs path"))?;
    if rest.contains(['\t', '\n']) {
        return Err(invalid("invalid output request"));
    }
    let mut stream = UnixStream::connect(socket).operation("reach the outputs service")?;
    writeln!(stream, "{rest}\t{path}").operation("send an outputs lookup")?;
    let mut code = String::new();
    stream
        .read_to_string(&mut code)
        .operation("read the outputs service's answer")?;
    match code.trim() {
        "0" => Ok(std::process::ExitCode::SUCCESS),
        "22" => Ok(std::process::ExitCode::from(22)),
        _ => Ok(std::process::ExitCode::FAILURE),
    }
}

/// Serve one request of `<os-arch>/<hash>/<package>/<input>.json` into
/// `path`, which must be Lake's own outputs file for that input beside
/// `artifacts`. Whether it found the outputs.
fn lookup(kv: Arc<dyn Kv>, rest: &str, path: &Path, artifacts: &Path) -> Result<bool> {
    let parts = rest.split('/').collect::<Vec<_>>();
    let [platform, hash, package, file] = parts[..] else {
        return Err(invalid("invalid output request"));
    };
    let Some(input) = file.strip_suffix(".json") else {
        return Err(invalid("invalid output request"));
    };
    let scope = format!("{platform}/{hash}");
    // The key checks the package and input names before any path uses them.
    key(&scope, package, input)?;
    let cache = artifacts
        .parent()
        .ok_or_else(|| invalid("Lake artifact folder has no parent"))?;
    let folder = path
        .parent()
        .ok_or_else(|| invalid("outputs file has no folder"))?;
    if folder
        .canonicalize()
        .operation("locate Lake outputs folder")?
        != cache
            .canonicalize()
            .operation("locate Lake cache")?
            .join("outputs")
            .join(package)
        || path.file_name().and_then(|name| name.to_str()) != Some(file)
    {
        return Err(invalid("unsafe Lake outputs destination"));
    }
    serve(kv, &scope, package, input, path, artifacts)
}

/// Marks when the first `get` or `build` since this checkout's last `put`
/// started Lake.
fn started_marker(project: &Project) -> PathBuf {
    project.record.join("unpublished-since")
}

/// Mark where the outputs the next `put` publishes start, unless an earlier
/// run left them unpublished.
pub(crate) fn start(project: &Project) -> Result<()> {
    fs::create_dir_all(&project.record).operation("create build record folder")?;
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(started_marker(project))
    {
        Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => {
            Err(error).operation("mark the run's start")
        }
        _ => Ok(()),
    }
}

/// Everything Lake built here so far is published.
pub(crate) fn published(project: &Project) -> Result<()> {
    match fs::remove_file(started_marker(project)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).operation("clear the run's start")
        }
        _ => Ok(()),
    }
}

/// Say how many outputs this run's Lake fetched by input hash.
pub(crate) fn report_fetches() {
    let fetched = SERVICE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .map_or(0, |service| service.fetched.load(Ordering::Relaxed));
    println!("Lake cache: fetched {fetched} outputs by input hash");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outputs_are_keyed_by_package_and_input_hash() {
        assert_eq!(
            key("linux-x86_64/s", "demo", "0123456789abcdef").unwrap(),
            "lake/linux-x86_64/s/out/demo/0123456789abcdef.json"
        );
        assert!(key("linux-x86_64/s", "demo", "HEAD").is_err());
        assert!(key("linux-x86_64/s", "../demo", "0123456789abcdef").is_err());
    }
}
