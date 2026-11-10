use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, File},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Instant,
};

use covalence_cas_prolly::Map;
use covalence_cas_trait::Cas;
use covalence_kv_s3::{AwsOptions, S3Kv, S3Location};
use covalence_kv_trait::Kv;
use covalence_lib_cid::{Bdasl, Blake3, Cid, Format};
use covalence_lib_data::Bytes;
use covalence_lib_serde::Serialize;

use crate::{
    error::{Error, IoResult, Result, encode, invalid},
    local::Local,
    mapping,
    pool::{self, Entry},
    project::{Package, Project, Source},
};

const WORKERS: usize = 16;

/// The bucket holding the pool, from `LAKE_CI_BUCKET`.
pub(crate) fn store() -> Result<Arc<dyn Kv>> {
    let bucket = env::var("LAKE_CI_BUCKET").map_err(|_| invalid("LAKE_CI_BUCKET is required"))?;
    let location = S3Location::new(&bucket, "").map_err(|source| Error::S3 { source })?;
    // Sixteen files, each with up to two 8 MiB parts, bound payload buffers
    // to 256 MiB. The transport supplies retries and connection reuse.
    S3Kv::from_aws(location, &AwsOptions::default())
        .map(|s| {
            Arc::new(
                s.concurrency(2)
                    .max_idle_connections(WORKERS * 2)
                    .part_size(8 * 1024 * 1024)
                    .upload_buffer_bytes(256 * 1024 * 1024),
            ) as Arc<dyn Kv>
        })
        .map_err(|source| Error::S3 { source })
}

/// `work` on every item, sixteen at a time; results keep the items' order.
/// The first failure stops further work.
pub(crate) fn parallel<T: Sync, R: Send>(
    items: &[T],
    work: impl Fn(usize, &T) -> Result<R> + Sync,
) -> Result<Vec<R>> {
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        let mut workers = Vec::new();
        for _ in 0..WORKERS.min(items.len()) {
            workers.push(
                thread::Builder::new()
                    .name("lake-cache-transfer".into())
                    .spawn_scoped(scope, || {
                        let mut results = Vec::new();
                        while !stop.load(Ordering::Relaxed) {
                            let index = next.fetch_add(1, Ordering::Relaxed);
                            let Some(item) = items.get(index) else {
                                break;
                            };
                            match work(index, item) {
                                Ok(result) => results.push((index, result)),
                                Err(error) => {
                                    stop.store(true, Ordering::Relaxed);
                                    return Err(error);
                                }
                            }
                        }
                        Ok(results)
                    })
                    .operation("start cache transfer worker")?,
            );
        }
        let mut results = Vec::new();
        let mut failure = None;
        for worker in workers {
            match worker.join().unwrap_or(Err(Error::Worker)) {
                Ok(done) => results.extend(done),
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        results.sort_unstable_by_key(|(index, _)| *index);
        Ok(results.into_iter().map(|(_, result)| result).collect())
    })
}

fn store_error(operation: impl Into<String>) -> impl FnOnce(covalence_kv_trait::Error) -> Error {
    move |source| Error::Store {
        operation: operation.into(),
        source,
    }
}

fn cas_error(operation: impl Into<String>) -> impl FnOnce(covalence_cas_trait::Error) -> Error {
    move |source| Error::Cas {
        operation: operation.into(),
        source,
    }
}

/// The root a revision pointer names, or `None` if it is unpublished.
pub(crate) fn pointer(kv: &dyn Kv, key: &str) -> Result<Option<Cid<Bdasl>>> {
    let Some(bytes) = kv
        .get(key.as_bytes())
        .map_err(store_error("read revision pointer"))?
    else {
        return Ok(None);
    };
    str::from_utf8(&bytes)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .map(Some)
        .ok_or_else(|| invalid(format!("invalid revision pointer {key}")))
}

/// The nearest of `commits` with a published build of `package`, and its
/// root: one parallel round of pointer reads.
pub(crate) fn nearest(
    kv: &dyn Kv,
    project: &Project,
    scope: &str,
    package: &Package,
    commits: &[String],
) -> Result<Option<(String, Cid<Bdasl>)>> {
    let roots = parallel(commits, |_, commit| {
        pointer(kv, &project.revision_key(scope, package, commit)?)
    })?;
    Ok(commits
        .iter()
        .cloned()
        .zip(roots)
        .find_map(|(commit, root)| root.map(|root| (commit, root))))
}

/// The CID of each file, hashed in parallel.
fn hash_files(files: &[(String, std::path::PathBuf)]) -> Result<BTreeMap<String, (Blake3, u64)>> {
    Ok(files
        .iter()
        .map(|(name, _)| name.clone())
        .zip(parallel(files, |_, (_, path)| {
            let mut file = File::open(path).operation(format!("open {}", path.display()))?;
            let mut hash = blake3::Hasher::new();
            let bytes = std::io::copy(&mut file, &mut hash).operation("hash Lake artifact")?;
            let cid =
                Blake3::new(Format::BLAKE3, hash.finalize().as_bytes()).map_err(|source| {
                    Error::Cid {
                        operation: "address Lake artifact".into(),
                        source,
                    }
                })?;
            Ok((cid, bytes))
        })?)
        .collect())
}

/// The entries of a Lake mapping, with each artifact's CID, from files in
/// `directory`. With `complete`, entries naming a missing artifact fail;
/// without, they are left out.
fn mapped_entries(
    mapping_path: &Path,
    directory: &Path,
    complete: bool,
) -> Result<BTreeMap<String, Entry>> {
    let rows = mapping::rows(mapping_path)?;
    let mut files = BTreeMap::new();
    for (_, names) in rows.values() {
        for name in names {
            let path = directory.join(name);
            if path.is_file() {
                files.insert(name.clone(), path);
            } else if complete {
                return Err(invalid(format!("artifact {} is missing", path.display())));
            }
        }
    }
    let files = files.into_iter().collect::<Vec<_>>();
    let hashes = hash_files(&files)?;
    Ok(rows
        .into_iter()
        .filter_map(|(input, (output, names))| {
            let artifacts = names
                .iter()
                .map(|name| hashes.get(name).map(|(cid, _)| (name.clone(), *cid)))
                .collect::<Option<BTreeMap<_, _>>>()?;
            Some((input, Entry { output, artifacts }))
        })
        .collect())
}

#[derive(Serialize)]
#[serde(crate = "covalence_lib_serde::serde")]
struct Publication {
    package: String,
    commit: String,
    root: String,
    parent: Option<String>,
    entries: usize,
    changed: usize,
    artifacts: usize,
    uploaded: usize,
    artifact_bytes: u64,
    nodes: usize,
    seconds: f64,
}

struct Publisher<'a> {
    project: &'a Project,
    kv: &'a dyn Kv,
    remote: &'a dyn Cas,
    local: &'a Local,
    scope: &'a str,
    /// Every commit the manifest pins.
    pins: BTreeSet<String>,
    artifacts: &'a Path,
}

/// Publish one recorded package; `None` if there is nothing to publish.
fn publish_package(publisher: &Publisher, package: &Package) -> Result<Option<Publication>> {
    let Publisher {
        project,
        kv,
        remote,
        local,
        scope,
        ref pins,
        artifacts,
    } = *publisher;
    let started = Instant::now();
    let commit = match &package.source {
        Source::Repository => project.git(&["rev-parse", "HEAD"])?,
        Source::Git { commit, .. } => commit.clone(),
    };
    let key = project.revision_key(scope, package, &commit)?;
    let (parent, mut entries) = match &package.source {
        Source::Repository => (
            nearest(kv, project, scope, package, &project.ancestors()?)?,
            BTreeMap::new(),
        ),
        Source::Git { .. } => {
            if pointer(kv, &key)?.is_some() {
                return Ok(None);
            }
            // Fold in the upstream snapshots Lake fetched for this package, so
            // the pool covers at least what upstream does. Upstream services
            // name snapshots by a pinned commit, not always the package's own.
            let mut upstream = BTreeMap::new();
            for pin in pins {
                let path = local
                    .directory()
                    .join("revisions")
                    .join(&package.name)
                    .join(format!("{pin}.jsonl"));
                if path.is_file() {
                    upstream.extend(mapped_entries(&path, artifacts, false)?);
                }
            }
            (None, upstream)
        }
    };
    entries.extend(mapped_entries(
        &project.mapping(&package.name),
        artifacts,
        true,
    )?);
    if entries.is_empty() {
        return Ok(None);
    }
    let map = pool::build(local.nodes(), &entries)?;
    let parent_map = match &parent {
        Some((_, root)) => {
            pool::copy_nodes(remote, &*local.nodes(), *root)?;
            Some(Map::open(local.nodes(), *root))
        }
        None => None,
    };
    let transfer = pool::transfer(parent_map.as_ref(), &map)?;
    let wanted = transfer
        .artifacts
        .iter()
        .map(|(name, cid)| (name.clone(), *cid))
        .collect::<Vec<_>>();
    let present = parallel(&wanted, |_, (_, cid)| {
        remote
            .has(&(*cid).into())
            .map_err(cas_error("inspect pool artifact"))
    })?;
    let mut missing = wanted
        .into_iter()
        .zip(present)
        .filter(|(_, present)| !present)
        .map(|((name, cid), _)| {
            let path = artifacts.join(&name);
            let bytes = path.metadata().operation("read artifact length")?.len();
            Ok((path, cid, bytes))
        })
        .collect::<Result<Vec<_>>>()?;
    // Start large bodies before small files can leave them at the tail.
    missing.sort_unstable_by_key(|(_, _, bytes)| Reverse(*bytes));
    let artifact_bytes = parallel(&missing, |_, (path, cid, bytes)| {
        let mut file = File::open(path).operation(format!("open {}", path.display()))?;
        remote
            .put_from(*cid, &mut file, *bytes)
            .map_err(cas_error(format!("upload {}", path.display())))?;
        Ok(*bytes)
    })?
    .into_iter()
    .sum();
    // Artifacts, then nodes children first, then the pointer: a pointer
    // never names an incomplete map.
    let nodes = pool::copy_nodes(&*local.nodes(), remote, map.root())?;
    kv.put(
        key.as_bytes(),
        Bytes::copy_from_slice(format!("{}\n", map.root()).as_bytes()),
    )
    .map_err(store_error("publish revision pointer"))?;
    Ok(Some(Publication {
        package: package.name.clone(),
        commit,
        root: map.root().to_string(),
        parent: parent.map(|(commit, _)| commit),
        entries: entries.len(),
        changed: transfer.entries.len(),
        artifacts: transfer.artifacts.len(),
        uploaded: missing.len(),
        artifact_bytes,
        nodes,
        seconds: started.elapsed().as_secs_f64(),
    }))
}

pub(crate) fn publish(project: &Project, kv: &Arc<dyn Kv>) -> Result<()> {
    let started = Instant::now();
    let receipt = project.receipt()?;
    let scope = &receipt.identity.scope;
    let remote = pool::store(kv.clone(), scope)?;
    let local = Local::open(project)?;
    let packages = project.packages()?;
    let pins = packages
        .iter()
        .filter_map(|package| match &package.source {
            Source::Git { commit, .. } => Some(commit.clone()),
            Source::Repository => None,
        })
        .collect();
    let publisher = Publisher {
        project,
        kv: &**kv,
        remote: &remote,
        local: &local,
        scope,
        pins,
        artifacts: &receipt.artifacts,
    };
    let mut published = Vec::new();
    for package in packages {
        if receipt.packages.contains_key(&package.name)
            && let Some(publication) = publish_package(&publisher, &package)?
        {
            published.push(publication);
        }
    }
    let report = encode(&serde_json::json!({
        "scope": scope,
        "packages": published,
        "seconds": started.elapsed().as_secs_f64(),
    }))?;
    println!(
        "Lake cache publication: {}",
        String::from_utf8_lossy(&report)
    );
    fs::write(project.record.join("publication.json"), report).operation("write publication report")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallel_results_keep_input_order() {
        let items = (0..1000u64).collect::<Vec<_>>();
        let results = parallel(&items, |index, item| {
            // Uneven work so workers finish out of order.
            thread::sleep(std::time::Duration::from_micros((item * 7919) % 50));
            Ok((index, *item))
        })
        .unwrap();
        assert!(
            results
                .iter()
                .enumerate()
                .all(|(i, (index, item))| *index == i && *item == i as u64)
        );
    }
}
