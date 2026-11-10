use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use covalence_cas_prolly::Map;
use covalence_cas_trait::Cas;
use covalence_kv_s3::{AwsOptions, Credentials, S3Kv, S3Location};
use covalence_kv_trait::Kv;
use covalence_lib_cid::{Bdasl, Cid, Sha256};
use covalence_lib_data::Bytes;
use covalence_lib_serde::Serialize;

use crate::{
    accelerate,
    blobs::{self, Blobs},
    error::{Error, IoResult, Result, encode, invalid},
    local::Local,
    mapping,
    pool::{self, Entry},
    project::{Package, Project, Source},
    seed, sources_tree,
};

const WORKERS: usize = 16;
/// Archive downloads are single small GETs that a restore decodes as they
/// arrive, so more of them run at once than transfers do.
pub(crate) const ARCHIVE_WORKERS: usize = 64;

/// The bucket holding the pool, from `LAKE_CI_BUCKET`.
pub(crate) fn store() -> Result<Arc<dyn Kv>> {
    // Sixteen files, each with up to two 8 MiB parts, bound payload buffers
    // to 256 MiB. The transport supplies retries and connection reuse.
    Ok(Arc::new(
        s3()?
            .concurrency(2)
            .max_idle_connections(ARCHIVE_WORKERS)
            .part_size(8 * 1024 * 1024)
            .upload_buffer_bytes(256 * 1024 * 1024),
    ))
}

/// The environment that hands resolved credentials to a download process.
const LEASE: [&str; 4] = [
    "COVALENCE_LAKE_AWS_ACCESS_KEY_ID",
    "COVALENCE_LAKE_AWS_SECRET_ACCESS_KEY",
    "COVALENCE_LAKE_AWS_SESSION_TOKEN",
    "COVALENCE_LAKE_AWS_EXPIRY",
];

/// The pool bucket, signing with leased credentials while they last.
fn s3() -> Result<S3Kv> {
    let bucket = env::var("LAKE_CI_BUCKET").map_err(|_| invalid("LAKE_CI_BUCKET is required"))?;
    let location = S3Location::new(&bucket, "").map_err(|source| Error::S3 { source })?;
    let options = AwsOptions {
        accelerate: accelerate::enabled()?,
        ..AwsOptions::default()
    };
    let lease = LEASE.map(|name| env::var(name).ok());
    match lease {
        [Some(key), Some(secret), Some(token), Some(expiry)] => {
            let expiry = expiry
                .parse::<u64>()
                .map(|seconds| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds))
                .map_err(|_| invalid("invalid leased credential expiry"))?;
            S3Kv::from_aws_with(
                location,
                &options,
                Credentials::new_with_token(key, secret, token),
                expiry,
            )
        }
        _ => S3Kv::from_aws(location, &options),
    }
    .map_err(|source| Error::S3 { source })
}

/// The environment to hand to download processes, resolved once, so each
/// neither probes for acceleration nor asks the credential chain again. It
/// carries no credentials that do not expire, which the chain finds cheaply.
pub(crate) fn lease() -> Result<Vec<(&'static str, String)>> {
    let accelerate = (
        accelerate::VARIABLE,
        if accelerate::enabled()? { "on" } else { "off" }.to_owned(),
    );
    let (credentials, expiry) = s3()?
        .credential_lease()
        .map_err(|source| Error::S3 { source })?;
    let (Some(expiry), Some(token)) = (expiry, credentials.token()) else {
        return Ok(vec![accelerate]);
    };
    let expiry = expiry
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| invalid("credential expiry precedes the epoch"))?
        .as_secs();
    Ok(LEASE
        .into_iter()
        .zip([
            credentials.key().to_owned(),
            credentials.secret().to_owned(),
            token.to_owned(),
            expiry.to_string(),
        ])
        .chain([accelerate])
        .collect())
}

/// The leased-credential variables, which other services must not see.
pub(crate) const LEASED: [&str; 4] = LEASE;

/// `work` on every item, sixteen at a time; results keep the items' order.
/// The first failure stops further work.
pub(crate) fn parallel<T: Sync, R: Send>(
    items: &[T],
    work: impl Fn(usize, &T) -> Result<R> + Sync,
) -> Result<Vec<R>> {
    parallel_with(WORKERS, items, work)
}

/// [`parallel`] with `width` at a time.
pub(crate) fn parallel_with<T: Sync, R: Send>(
    width: usize,
    items: &[T],
    work: impl Fn(usize, &T) -> Result<R> + Sync,
) -> Result<Vec<R>> {
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        let mut workers = Vec::new();
        for _ in 0..width.min(items.len()) {
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
fn hash_files(files: &[(String, std::path::PathBuf)]) -> Result<BTreeMap<String, (Sha256, u64)>> {
    Ok(files
        .iter()
        .map(|(name, _)| name.clone())
        .zip(parallel(files, |_, (_, path)| blobs::hash_file(path))?)
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
    blobs: &'a Blobs,
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
        blobs,
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
            // Publishers of one pin record different closures; the pool keeps
            // their union, this publisher's outputs winning.
            let existing = pointer(kv, &key)?;
            let mut upstream = BTreeMap::new();
            if let Some(root) = existing {
                pool::copy_nodes(remote, &*local.nodes(), root)?;
                upstream = pool::entries(&Map::open(local.nodes(), root))?;
            }
            // Fold in the upstream snapshots Lake fetched for this package, so
            // the pool covers at least what upstream does. Upstream services
            // name snapshots by a pinned commit, not always the package's own.
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
            (existing.map(|root| (commit.clone(), root)), upstream)
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
    if parent.as_ref().is_some_and(|(_, root)| *root == map.root())
        && package.source != Source::Repository
    {
        return Ok(None);
    }
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
    let present = parallel(&wanted, |_, (_, cid)| blobs.has(cid))?;
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
        blobs.put_file(cid, path)?;
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
        blobs: &pool::blobs(kv.clone(), scope)?,
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
    // A seed only shortcuts the maps just published.
    let seed = seed::publish(project, &**kv, &receipt, local.directory())?;
    // The source tree goes last: a record says its targets built and were
    // published at that tree.
    let sources = sources_tree::publish(project, kv, &receipt.identity.revision, &receipt.targets)?;
    let report = encode(&serde_json::json!({
        "scope": scope,
        "packages": published,
        "sources": sources,
        "seed": seed,
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
