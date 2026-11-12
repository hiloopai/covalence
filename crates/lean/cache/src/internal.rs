//! The internal scope: Lake outputs any internal build made, keyed by Lake's
//! own input hash, for every other internal build to take.
//!
//! Prove workers, pull-request CI and people's machines are internal: they
//! write what they build, committed or not, with `put --internal`, and read
//! with `get --internal` and `build --internal`, through Lake (an overlay
//! hook asks `cov-lake-cache` for the outputs of each input its cache lacks;
//! stock Lake never asks). `main` keeps its own pool and never reads this
//! scope, except through adoption: `build --adopt` lets Lake take internal
//! outputs, and `put` then publishes them only with a passing kernel replay
//! of every adopted module (see [`Adoption`]).
//!
//! Layout, under `lake-int/<os>-<arch>/<toolchain scope>/`:
//!
//! - `out/<package>/<input>.json`: one input's [`Entry`]: Lake's output record
//!   and the content ID of each artifact it names.
//! - `cas/sha2-256/…`: artifacts `main`'s store lacks.
//!
//! [`InternalKv`] presents that layout under the pool's own keys: reads of the
//! store fall back to `main`'s, which a content ID names alone, and every
//! write lands under `lake-int/`; any other write fails, so the internal scope
//! feeds no revision map, seed, build manifest, source tree, Git pack or
//! toolchain. A lifecycle rule deletes it 14 days after each write.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use covalence_kv_trait::{Error, Kv, KvFuture, ready};
use covalence_lib_cid::Sha256;
use covalence_lib_data::Bytes;
use covalence_lib_serde::{Deserialize, Serialize};

use crate::{
    error::{IoResult, Result, encode, invalid, json},
    mapping,
    project::{Lookups, Package, Project},
};

/// The root of the internal scope.
pub(crate) const ROOT: &str = "lake-int";

/// The internal scope over the pool `inner`.
pub(crate) struct InternalKv {
    inner: Arc<dyn Kv>,
}

/// Where a pool key goes in the internal scope.
enum Route {
    /// Output entries: the internal scope's alone.
    Outputs(String),
    /// The store: the internal scope's, else `main`'s for reads.
    Store(String),
    /// Anything else: read from `main`'s pool, never written.
    Main,
}

impl InternalKv {
    pub fn new(inner: Arc<dyn Kv>) -> Self {
        Self { inner }
    }

    /// `lake/<os-arch>/<hash>/<kind>/<rest>` as
    /// `lake-int/<os-arch>/<hash>/<kind>/<rest>`, for the internal kinds.
    fn route(key: &[u8]) -> Route {
        let Ok(key) = std::str::from_utf8(key) else {
            return Route::Main;
        };
        let mut parts = key.splitn(5, '/');
        let (Some("lake"), Some(platform), Some(hash), Some(kind), Some(rest)) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return Route::Main;
        };
        let internal = format!("{ROOT}/{platform}/{hash}/{kind}/{rest}");
        match kind {
            "out" => Route::Outputs(internal),
            "cas" => Route::Store(internal),
            _ => Route::Main,
        }
    }

    /// The key a write of `key` goes to.
    fn written(key: &[u8]) -> std::result::Result<String, Error> {
        match Self::route(key) {
            Route::Outputs(internal) | Route::Store(internal) => Ok(internal),
            Route::Main => Err(Error::Backend {
                source: format!(
                    "an internal build writes only under {ROOT}/, not {}",
                    String::from_utf8_lossy(key)
                )
                .into(),
            }),
        }
    }
}

impl Kv for InternalKv {
    fn get(&self, key: &[u8]) -> std::result::Result<Option<Bytes>, Error> {
        match Self::route(key) {
            Route::Outputs(internal) => self.inner.get(internal.as_bytes()),
            Route::Store(internal) => match self.inner.get(internal.as_bytes())? {
                Some(value) => Ok(Some(value)),
                None => self.inner.get(key),
            },
            Route::Main => self.inner.get(key),
        }
    }

    fn size(&self, key: &[u8]) -> std::result::Result<Option<u64>, Error> {
        match Self::route(key) {
            Route::Outputs(internal) => self.inner.size(internal.as_bytes()),
            Route::Store(internal) => match self.inner.size(internal.as_bytes())? {
                Some(size) => Ok(Some(size)),
                None => self.inner.size(key),
            },
            Route::Main => self.inner.size(key),
        }
    }

    fn get_range(
        &self,
        key: &[u8],
        start: u64,
        len: u64,
    ) -> std::result::Result<Option<Bytes>, Error> {
        match Self::route(key) {
            Route::Outputs(internal) => self.inner.get_range(internal.as_bytes(), start, len),
            Route::Store(internal) => {
                match self.inner.get_range(internal.as_bytes(), start, len)? {
                    Some(value) => Ok(Some(value)),
                    None => self.inner.get_range(key, start, len),
                }
            }
            Route::Main => self.inner.get_range(key, start, len),
        }
    }

    fn get_to(
        &self,
        key: &[u8],
        writer: &mut dyn Write,
    ) -> std::result::Result<Option<u64>, Error> {
        match Self::route(key) {
            Route::Outputs(internal) => self.inner.get_to(internal.as_bytes(), writer),
            // A miss writes nothing, so the fallback starts clean.
            Route::Store(internal) => match self.inner.get_to(internal.as_bytes(), writer)? {
                Some(len) => Ok(Some(len)),
                None => self.inner.get_to(key, writer),
            },
            Route::Main => self.inner.get_to(key, writer),
        }
    }

    fn exists(&self, key: &[u8]) -> std::result::Result<bool, Error> {
        match Self::route(key) {
            Route::Outputs(internal) => self.inner.exists(internal.as_bytes()),
            Route::Store(internal) => {
                Ok(self.inner.exists(internal.as_bytes())? || self.inner.exists(key)?)
            }
            Route::Main => self.inner.exists(key),
        }
    }

    fn put(&self, key: &[u8], value: Bytes) -> std::result::Result<(), Error> {
        self.inner.put(Self::written(key)?.as_bytes(), value)
    }

    fn put_from(
        &self,
        key: &[u8],
        reader: &mut dyn Read,
        len: u64,
    ) -> std::result::Result<(), Error> {
        self.inner
            .put_from(Self::written(key)?.as_bytes(), reader, len)
    }

    fn put_new(&self, key: &[u8], value: Bytes) -> std::result::Result<bool, Error> {
        self.inner.put_new(Self::written(key)?.as_bytes(), value)
    }

    fn put_new_from(
        &self,
        key: &[u8],
        reader: &mut dyn Read,
        len: u64,
    ) -> std::result::Result<bool, Error> {
        self.inner
            .put_new_from(Self::written(key)?.as_bytes(), reader, len)
    }

    fn aget<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
        ready(self.get(key))
    }

    fn aput<'a>(&'a self, key: &'a [u8], value: Bytes) -> KvFuture<'a, ()> {
        ready(self.put(key, value))
    }
}

/// The pool key of one input's [`Entry`], which [`InternalKv`] places in the
/// internal scope.
pub(crate) fn key(scope: &str, package: &str, input: &str) -> Result<String> {
    for name in [scope, package] {
        mapping::segment_path(name)?;
    }
    if !mapping::hex(input, 16) {
        return Err(invalid(
            "an internal output is keyed by a 16-digit input hash",
        ));
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

/// Publish every output this host's Lake built itself for the packages'
/// build folders, which a restore did not bring: each input's record and
/// the artifacts `main`'s and the internal store lack. Records a restore
/// installed name the service they came from; ones Lake wrote after
/// building do not.
pub(crate) fn publish(
    project: &Project,
    kv: Arc<dyn Kv>,
    scope: &str,
    packages: &[Package],
) -> Result<()> {
    let started = Instant::now();
    let cache = project.cache_dir()?;
    let blobs = crate::pool::blobs(kv.clone(), scope)?;
    let mut found = Vec::new();
    for package in packages {
        let build = project.root.join(&package.dir).join(".lake/build");
        let mut inputs = BTreeSet::new();
        for trace in crate::project::files(&build, "trace")? {
            let bytes = fs::read(&trace).operation(format!("read {}", trace.display()))?;
            // Old trace stubs name no input.
            if let Some(input) = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|trace| trace["depHash"].as_str().map(str::to_owned))
            {
                inputs.insert(input);
            }
        }
        for input in inputs {
            let path = cache
                .join("outputs")
                .join(&package.name)
                .join(format!("{input}.json"));
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            let record: serde_json::Value = json(&bytes)?;
            // Lake writes `"service": null` for what it built itself.
            if record["service"].is_string() {
                continue;
            }
            found.push((package.name.clone(), input, record));
        }
    }
    let published = crate::storage::parallel(&found, |_, (package, input, record)| {
        let key = key(scope, package, input)?;
        if kv
            .exists(key.as_bytes())
            .map_err(store_error("check internal output"))?
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
            .map_err(store_error("publish internal output"))?;
        Ok(true)
    })?;
    println!(
        "Lake cache: published {} of the {} outputs this host built to the internal scope in {:.1} s",
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

/// Serve Lake's request for the outputs of `input`: from the internal scope,
/// with each artifact the entry names placed in Lake's `artifacts` folder
/// first, written to `path`, Lake's own outputs file for it. `false` on a
/// miss, which writes nothing. With `adopted`, `main`'s adoption list, the
/// input joins it once its outputs are in place.
pub(crate) fn serve(
    kv: Arc<dyn Kv>,
    scope: &str,
    package: &str,
    input: &str,
    path: &Path,
    artifacts: &Path,
    adopted: Option<&Path>,
) -> Result<bool> {
    let key = key(scope, package, input)?;
    let Some(bytes) = kv
        .get(key.as_bytes())
        .map_err(store_error("read internal output"))?
    else {
        return Ok(false);
    };
    let entry: Entry = json(&bytes)?;
    let names = mapping::names(&entry.record["data"])?;
    if names.iter().collect::<BTreeSet<_>>() != entry.artifacts.keys().collect::<BTreeSet<_>>() {
        return Err(invalid(format!(
            "internal output {key} names other artifacts than it lists"
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
            .map_err(|_| invalid(format!("internal output {key} names an invalid CID")))?;
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
    pending
        .persist(path)
        .map_err(|error| error.error)
        .operation("place Lake outputs")?;
    if let Some(adopted) = adopted {
        let mut list = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(adopted)
            .operation("open adoption list")?;
        // One short append, whole, however many downloads run at once.
        list.write_all(format!("{package}\t{input}\n").as_bytes())
            .operation("record adopted output")?;
    }
    Ok(true)
}

/// `main`'s adoption of internal outputs, checked before `put` publishes
/// them. Lake took each listed input's outputs from the internal scope
/// during `build --adopt`; `put` publishes them only when a kernel replay
/// that ran after the build lists every module they belong to as replayed.
///
/// What this proves, and what it does not, as Thomas accepted for the
/// internal scope's trust:
///
/// - A replay checks every declaration of each adopted module's `.olean`
///   in the Lean kernel against its imports: its proofs are sound.
/// - It does not prove that the `.olean` is what `main`'s source elaborates
///   to. An internal writer could have published other statements, with
///   valid proofs, for the same input.
/// - It checks no native code. An adopted input's `.c`, `.ir` and objects
///   go into `main`'s pool, and into executables linked from them, as an
///   internal writer made them.
pub(crate) struct Adoption {
    /// Each adopted input, by package.
    pub inputs: BTreeSet<(String, String)>,
}

impl Adoption {
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(crate::error::Error::Io {
                    operation: format!("read {}", path.display()),
                    source,
                });
            }
        };
        let inputs = text
            .lines()
            .map(|line| {
                line.split_once('\t')
                    .map(|(package, input)| (package.to_owned(), input.to_owned()))
                    .ok_or_else(|| invalid("invalid adoption list"))
            })
            .collect::<Result<BTreeSet<_>>>()?;
        Ok((!inputs.is_empty()).then_some(Self { inputs }))
    }

    /// The module each adopted input belongs to: the one whose trace in its
    /// package's build folder, `lib/lean/<A/B>.trace` or a facet's
    /// `ir/<A/B>.<facet>.trace`, names that input.
    pub fn modules(&self, project: &Project, packages: &[Package]) -> Result<BTreeSet<String>> {
        let mut owners = BTreeMap::new();
        for package in packages {
            let build = project.root.join(&package.dir).join(".lake/build");
            for (folder, facets) in [("lib/lean", false), ("ir", true)] {
                let root = build.join(folder);
                for trace in crate::project::files(&root, "trace")? {
                    let Some(relative) = trace.strip_prefix(&root).ok().and_then(|relative| {
                        relative.with_extension("").to_str().map(str::to_owned)
                    }) else {
                        continue;
                    };
                    let module = match relative.split_once('.') {
                        Some((path, _)) if facets => path.replace('/', "."),
                        None if !facets => relative.replace('/', "."),
                        _ => continue,
                    };
                    let bytes = fs::read(&trace).operation(format!("read {}", trace.display()))?;
                    if let Some(input) = serde_json::from_slice::<serde_json::Value>(&bytes)
                        .ok()
                        .and_then(|trace| trace["depHash"].as_str().map(str::to_owned))
                    {
                        owners.insert((package.name.clone(), input), module);
                    }
                }
            }
        }
        self.inputs
            .iter()
            .map(|input| {
                owners.get(input).cloned().ok_or_else(|| {
                    invalid(format!(
                        "adopted input {} of {} belongs to no module the build folder traces",
                        input.1, input.0
                    ))
                })
            })
            .collect()
    }

    /// Check that `report`, the `last-run.json` of
    /// `cov-kernel-checker lean-check replay-built`, replayed every module of
    /// this adoption after `since`, the adoption list's last change.
    pub fn check(&self, modules: &BTreeSet<String>, report: &Path, since: &Path) -> Result<()> {
        let written = |path: &Path| {
            fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .operation(format!("read the time of {}", path.display()))
        };
        if written(report)? < written(since)? {
            return Err(invalid(
                "the kernel replay report predates the adopted outputs; replay after the build",
            ));
        }
        let report: serde_json::Value =
            json(&fs::read(report).operation("read kernel replay report")?)?;
        if report["status"] != "kernel_replayed" {
            return Err(invalid("the kernel replay did not pass"));
        }
        let replayed = report["results"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|result| result["status"] == "kernel_replayed")
            .filter_map(|result| result["module"].as_str())
            .collect::<BTreeSet<_>>();
        let missing = modules
            .iter()
            .filter(|module| !replayed.contains(module.as_str()))
            .take(10)
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(invalid(format!(
                "the kernel replay lacks adopted modules: {}",
                missing.join(", ")
            )));
        }
        Ok(())
    }
}

/// Names this executable's service for Lake's output lookups, and with
/// adoption, the list it keeps.
const OUTPUTS: &str = "COVALENCE_LAKE_OUTPUTS";
const ADOPTED: &str = "COVALENCE_LAKE_ADOPTED";

/// Point Lake's lookups of the outputs its cache lacks at the internal
/// scope, through this executable, as `project.lookups` allows; the overlay's
/// Lake asks `LAKE_OUTPUTS_ENDPOINT`, and stock Lake never does.
pub(crate) fn lookups(
    command: &mut std::process::Command,
    project: &Project,
    scope: &str,
) -> Result<()> {
    if project.lookups == Lookups::None {
        return Ok(());
    }
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
        .env(OUTPUTS, "1");
    if project.lookups == Lookups::Adopt {
        command.env(ADOPTED, adoption_list(project));
    }
    Ok(())
}

/// Serve Lake's `curl` of `<endpoint>/<os-arch>/<hash>/<package>/<input>.json`
/// into `path`, which must be Lake's own outputs file for that input beside
/// `artifacts`. Curl's exit status: 22 on a miss.
pub(crate) fn serve_request(
    rest: &str,
    path: &Path,
    artifacts: &Path,
) -> Result<std::process::ExitCode> {
    if std::env::var_os(OUTPUTS).is_none() {
        return Err(invalid("this run takes no internal outputs"));
    }
    let parts = rest.split('/').collect::<Vec<_>>();
    let [platform, hash, package, file] = parts[..] else {
        return Err(invalid("invalid internal output request"));
    };
    let Some(input) = file.strip_suffix(".json") else {
        return Err(invalid("invalid internal output request"));
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
        != cache.join("outputs").join(package)
        || path.file_name().and_then(|name| name.to_str()) != Some(file)
    {
        return Err(invalid("unsafe Lake outputs destination"));
    }
    let kv: Arc<dyn Kv> = Arc::new(InternalKv::new(crate::storage::store()?));
    let adopted = std::env::var_os(ADOPTED).map(PathBuf::from);
    Ok(
        if serve(
            kv,
            &scope,
            package,
            input,
            path,
            artifacts,
            adopted.as_deref(),
        )? {
            std::process::ExitCode::SUCCESS
        } else {
            std::process::ExitCode::from(22)
        },
    )
}

/// Where `build --adopt` lists the inputs Lake took from the internal scope.
pub(crate) fn adoption_list(project: &Project) -> PathBuf {
    project.record.join("adopted.list")
}

#[cfg(test)]
mod tests {
    use covalence_kv_memory::MemoryKv;

    use super::*;

    const CAS: &str = "lake/linux-x86_64/s/cas/sha2-256/abc/bytes.zst";

    #[test]
    fn the_internal_scope_reads_mains_store_and_writes_only_its_own() {
        let main = Arc::new(MemoryKv::default());
        main.put(CAS.as_bytes(), Bytes::from_static(b"main"))
            .unwrap();
        let rev = "lake/linux-x86_64/s/rev/o/r/c/root";
        main.put(rev.as_bytes(), Bytes::from_static(b"main root"))
            .unwrap();
        let internal = InternalKv::new(main.clone());
        assert_eq!(internal.get(CAS.as_bytes()).unwrap().unwrap(), &b"main"[..]);
        assert_eq!(
            internal.get(rev.as_bytes()).unwrap().unwrap(),
            &b"main root"[..]
        );
        let out = key("linux-x86_64/s", "demo", "0123456789abcdef").unwrap();
        assert_eq!(out, "lake/linux-x86_64/s/out/demo/0123456789abcdef.json");
        internal
            .put(out.as_bytes(), Bytes::from_static(b"entry"))
            .unwrap();
        assert_eq!(
            internal.get(out.as_bytes()).unwrap().unwrap(),
            &b"entry"[..]
        );
        assert_eq!(main.get(out.as_bytes()).unwrap(), None);
        assert_eq!(
            main.get(b"lake-int/linux-x86_64/s/out/demo/0123456789abcdef.json")
                .unwrap()
                .unwrap(),
            &b"entry"[..]
        );
        // Revision maps, seeds, manifests, sources, packs, toolchains: refused.
        for refused in [
            rev,
            "lake/linux-x86_64/s/seed/o/r/a/manifest.json",
            "lake/linux-x86_64/s/builds/o/r/l/t",
            "lake/linux-x86_64/s/modules/o/r/c/demo",
            "lake/src/rev/o/r/c",
            "lake/git/o/d/c.pack",
            "lake/toolchains/linux-x86_64/t",
            "other",
        ] {
            assert!(
                internal.put(refused.as_bytes(), Bytes::new()).is_err(),
                "{refused}"
            );
        }
        assert_eq!(
            main.get(rev.as_bytes()).unwrap().unwrap(),
            &b"main root"[..]
        );
        assert!(key("s", "../x", "0123456789abcdef").is_err());
        assert!(key("s", "demo", "../../x").is_err());
    }

    #[test]
    fn an_adoption_needs_a_later_replay_of_every_module() {
        let temporary = tempfile::tempdir().unwrap();
        let list = temporary.path().join("adopted.list");
        fs::write(&list, "demo\t00000000000000aa\n").unwrap();
        let adoption = Adoption::load(&list).unwrap().unwrap();
        let modules = BTreeSet::from(["Demo.A".to_owned()]);
        let report = temporary.path().join("last-run.json");
        let replayed = |modules: &[&str]| {
            let results = modules
                .iter()
                .map(|module| serde_json::json!({"status": "kernel_replayed", "module": module}))
                .collect::<Vec<_>>();
            fs::write(
                &report,
                serde_json::to_vec(
                    &serde_json::json!({"status": "kernel_replayed", "results": results}),
                )
                .unwrap(),
            )
            .unwrap();
        };
        replayed(&["Demo.A"]);
        adoption.check(&modules, &report, &list).unwrap();
        replayed(&["Demo.B"]);
        assert!(adoption.check(&modules, &report, &list).is_err());
        // A replay older than the adopted outputs proves nothing about them.
        replayed(&["Demo.A"]);
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&list, "demo\t00000000000000aa\n").unwrap();
        assert!(adoption.check(&modules, &report, &list).is_err());
        assert!(
            Adoption::load(&temporary.path().join("none"))
                .unwrap()
                .is_none()
        );
    }
}
