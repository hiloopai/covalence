use std::{
    collections::BTreeSet,
    env,
    fs::{self, File},
    path::Path,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    thread,
    time::Instant,
};

use covalence_kv_s3::{AwsOptions, S3Kv, S3Location};
use covalence_kv_trait::Kv;
use covalence_lib_data::Bytes;
use covalence_lib_serde::Serialize;

use crate::{
    error::{Error, IoResult, Result, encode, invalid},
    mapping,
    project::Project,
};

const WORKERS: usize = 16;

pub(crate) fn store() -> Result<S3Kv> {
    let bucket = env::var("LAKE_CI_BUCKET").map_err(|_| invalid("LAKE_CI_BUCKET is required"))?;
    let prefix = env::var("LAKE_CI_PREFIX").unwrap_or_default();
    let location = S3Location::new(&bucket, &prefix).map_err(|source| Error::S3 { source })?;
    // Sixteen files, each with up to two 8 MiB parts, bound payload buffers
    // to 256 MiB. The transport supplies retries and connection reuse.
    S3Kv::from_aws(location, &AwsOptions::default())
        .map(|s| {
            s.concurrency(2)
                .part_size(8 * 1024 * 1024)
                .upload_buffer_bytes(256 * 1024 * 1024)
        })
        .map_err(|source| Error::S3 { source })
}

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
                                Ok(result) => results.push(result),
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
        failure.map_or(Ok(results), Err)
    })
}

pub(crate) fn put_file(store: &S3Kv, key: &str, path: &Path) -> Result<u64> {
    let mut file = File::open(path).operation(format!("open upload {}", path.display()))?;
    let size = file.metadata().operation("read upload size")?.len();
    store
        .put_from(key.as_bytes(), &mut file, size)
        .map_err(|source| Error::Store {
            operation: format!("upload {key}"),
            source,
        })?;
    Ok(size)
}

#[derive(Serialize)]
#[serde(crate = "covalence_lib_serde::serde")]
struct Publication {
    artifacts: usize,
    reused: usize,
    uploaded: usize,
    artifact_bytes: u64,
    metadata_bytes: u64,
    inventory_seconds: f64,
    transfer_seconds: f64,
    seconds: f64,
}

pub(crate) fn publish(project: &Project, store: &S3Kv) -> Result<()> {
    let started = Instant::now();
    let receipt = project.receipt()?;
    let mapping = project.record.join("outputs.jsonl");
    let files = mapping::artifacts(&mapping, &receipt.artifacts)?;
    let prefix = format!("a/{}/", receipt.identity.scope);
    let inventory_started = Instant::now();
    let mut present = BTreeSet::new();
    let mut tokens = BTreeSet::new();
    let mut continuation = None;
    loop {
        let page = store
            .list(&prefix, continuation.as_deref())
            .map_err(|source| Error::S3 { source })?;
        for object in page.objects {
            let name = object
                .key
                .strip_prefix(&prefix)
                .ok_or_else(|| invalid("inventory escaped artifact scope"))?;
            if let Some(path) = files.get(name)
                && path.metadata().operation("read local artifact size")?.len() == object.size
                && !present.insert(name.to_owned())
            {
                return Err(invalid("inventory repeated an artifact"));
            }
        }
        let Some(next) = page.next else {
            break;
        };
        if !tokens.insert(next.clone()) {
            return Err(invalid("inventory repeated a continuation token"));
        }
        continuation = Some(next);
    }
    let inventory_seconds = inventory_started.elapsed().as_secs_f64();
    let missing = files
        .iter()
        .filter(|(key, _)| !present.contains(*key))
        .collect::<Vec<_>>();
    let transfer_started = Instant::now();
    let artifact_bytes = parallel(&missing, |_, (key, path)| {
        put_file(store, &format!("{prefix}{key}"), path)
    })?
    .into_iter()
    .sum();
    let transfer_seconds = transfer_started.elapsed().as_secs_f64();
    // The mapping and pointer become visible only after every body succeeds.
    let metadata_bytes = put_file(
        store,
        &format!(
            "r/{}/{}.jsonl",
            receipt.identity.scope, receipt.identity.revision
        ),
        &mapping,
    )?;
    let pointer = format!("{}\n", receipt.identity.revision);
    let key = format!("r/{}/main-latest", receipt.identity.scope);
    store
        .put(key.as_bytes(), Bytes::copy_from_slice(pointer.as_bytes()))
        .map_err(|source| Error::Store {
            operation: "publish revision pointer".into(),
            source,
        })?;
    let report = Publication {
        artifacts: files.len(),
        reused: present.len(),
        uploaded: missing.len(),
        artifact_bytes,
        metadata_bytes: metadata_bytes + pointer.len() as u64,
        inventory_seconds,
        transfer_seconds,
        seconds: started.elapsed().as_secs_f64(),
    };
    let report = encode(&report)?;
    println!(
        "Lake cache publication: {}",
        String::from_utf8_lossy(&report)
    );
    fs::write(project.record.join("publication.json"), report).operation("write publication report")
}
