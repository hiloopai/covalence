//! Module archives unpacked in this process, into Lake's artifact cache.
//!
//! Lake restores a module whose cached output is only its `.ltar` by running
//! `leantar` once per module. For the archives an eager restore installs,
//! this tool instead downloads and decodes each one here, 64 at a time, writes
//! its files into Lake's artifact directory under the names its trace gives
//! them, and hands Lake the trace's outputs as the mapping. Other artifacts
//! it downloads as they are. Lake then gets only mappings, finds every file in
//! its cache and runs no archiver.
//!
//! Lake's own `cache get` must not place these files: when one mapping names
//! an artifact twice and the file is already in a warm cache, it rewrites the
//! read-only file and fails (lean4 nightly-2026-11-05,
//! `Lake/Config/Cache.lean`, `downloadArtifacts`).

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Write},
    path::Path,
    time::Instant,
};

use covalence_lib_cid::Sha256;
use serde_json::Value;

use crate::{
    blobs::Blobs,
    error::{Error, IoResult, Result, invalid, json},
    mapping,
    pool::Entry,
    storage,
};

/// The archive an entry's output is, if it is only one.
fn archive(entry: &Entry) -> Option<&Sha256> {
    let Ok(Value::String(name)) = serde_json::from_str(&entry.output) else {
        return None;
    };
    name.ends_with(".ltar")
        .then(|| entry.artifacts.get(&name))
        .flatten()
}

/// What an eager restore puts in the artifact directory: an archive's files,
/// or one artifact as it is.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Job<'a> {
    Archive(&'a Sha256),
    File(&'a str, &'a Sha256),
}

/// Places every artifact `entries` name in `artifacts`, and returns Lake's
/// output descriptor for each entry, by input hash: for an archive, the
/// outputs its trace names; else the entry's own.
///
/// Each archive and each artifact is fetched once, however many entries name
/// it, and a file already in `artifacts` is never rewritten: a cache entry is
/// immutable once written. Lake then needs only the mappings.
pub(crate) fn install(
    blobs: &Blobs,
    artifacts: &Path,
    entries: &BTreeMap<String, Entry>,
) -> Result<BTreeMap<String, String>> {
    let started = Instant::now();
    let mut jobs = BTreeSet::new();
    for entry in entries.values() {
        match archive(entry) {
            Some(cid) => {
                jobs.insert(Job::Archive(cid));
            }
            None => jobs.extend(
                entry
                    .artifacts
                    .iter()
                    .map(|(name, cid)| Job::File(name.as_str(), cid)),
            ),
        }
    }
    let jobs: Vec<_> = jobs.into_iter().collect();
    if !jobs.is_empty() {
        fs::create_dir_all(artifacts).operation("create Lake artifact directory")?;
    }
    let done = storage::parallel_with(storage::ARCHIVE_WORKERS, &jobs, |_, job| match job {
        Job::Archive(cid) => unpack_one(blobs, artifacts, cid).map(Some),
        Job::File(name, cid) => fetch(blobs, &artifacts.join(name), cid).map(|()| None),
    })?;
    let descriptors: BTreeMap<_, _> = jobs
        .iter()
        .zip(done)
        .filter_map(|(job, descriptor)| match job {
            Job::Archive(cid) => Some((*cid, descriptor?)),
            Job::File(..) => None,
        })
        .collect();
    if !jobs.is_empty() {
        println!(
            "Lake cache: placed {} archives and {} artifacts in {:.1} s",
            descriptors.len(),
            jobs.len() - descriptors.len(),
            started.elapsed().as_secs_f64()
        );
    }
    Ok(entries
        .iter()
        .map(|(input, entry)| {
            let output = archive(entry)
                .and_then(|cid| descriptors.get(cid))
                .unwrap_or(&entry.output);
            (input.clone(), output.clone())
        })
        .collect())
}

/// Downloads one artifact to `path` unless it is already there.
fn fetch(blobs: &Blobs, path: &Path, cid: &Sha256) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let mut bytes = Vec::new();
    blobs
        .get_to(cid, &mut bytes)?
        .ok_or_else(|| invalid(format!("{cid} is missing from the pool")))?;
    save(path, &bytes)
}

/// Writes an archive's files and returns the outputs its trace names.
fn unpack_one(blobs: &Blobs, artifacts: &Path, cid: &Sha256) -> Result<String> {
    let mut bytes = Vec::new();
    blobs
        .get_to(cid, &mut bytes)?
        .ok_or_else(|| invalid(format!("{cid} is missing from the pool")))?;
    // The input hash goes only into the trace's `depHash`, which Lake does
    // not read here, so one decode serves every input naming the archive.
    let archive = covalence_lean_ltar::unpack(&bytes, 0).map_err(|source| Error::Archive {
        cid: cid.to_string(),
        source,
    })?;
    let stem = archive
        .trace
        .path
        .strip_suffix(".trace")
        .ok_or_else(|| invalid(format!("archive {cid} has no module trace")))?;
    let trace: Value = json(&archive.trace.bytes)?;
    let Some(Value::Object(mut outputs)) = trace.get("outputs").cloned() else {
        return Err(invalid(format!("archive {cid} names no module outputs")));
    };
    // Lake would resolve the archive too; the files are all it needs.
    outputs.remove("l");
    let mut names = Vec::new();
    for value in outputs.values() {
        match value {
            Value::String(name) => names.push(name),
            Value::Array(items) => {
                for item in items {
                    let Value::String(name) = item else {
                        return Err(invalid(format!("archive {cid} names an invalid output")));
                    };
                    names.push(name);
                }
            }
            // `m`, whether the module uses the module system.
            _ => {}
        }
    }
    for name in names {
        let extension = name
            .split_once('.')
            .filter(|_| mapping::artifact_name(name))
            .map(|(_, extension)| extension)
            .ok_or_else(|| invalid(format!("archive {cid} names an invalid output")))?;
        let path = format!("{stem}.{extension}");
        let file = archive
            .files
            .iter()
            .find(|file| file.path == path)
            .ok_or_else(|| invalid(format!("archive {cid} lacks its {extension} output")))?;
        save(&artifacts.join(name), &file.bytes)?;
    }
    serde_json::to_string(&Value::Object(outputs)).map_err(|source| Error::Json {
        operation: "write Lake output descriptor".into(),
        source,
    })
}

/// Writes a content-addressed file once, read-only as Lake leaves the
/// artifacts it downloads; an existing file has the same bytes.
fn save(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid("artifact has no parent"))?;
    let mut pending = tempfile::NamedTempFile::new_in(parent).operation("stage Lake artifact")?;
    pending.write_all(bytes).operation("write Lake artifact")?;
    let mut permissions = pending
        .as_file()
        .metadata()
        .operation("read Lake artifact permissions")?
        .permissions();
    permissions.set_readonly(true);
    pending
        .as_file()
        .set_permissions(permissions)
        .operation("mark Lake artifact read-only")?;
    match pending.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.error).operation("save Lake artifact"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use covalence_kv_memory::MemoryKv;
    use covalence_kv_trait::Kv;

    use super::*;
    use crate::blobs;

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../ltar/tests/fixtures/module.ltar")
    }

    /// Each file in `directory` with its modification time.
    fn listing(directory: &Path) -> Vec<(String, std::time::SystemTime)> {
        let mut files: Vec<_> = fs::read_dir(directory)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let modified = entry.metadata().unwrap().modified().unwrap();
                (entry.file_name().into_string().unwrap(), modified)
            })
            .collect();
        files.sort();
        files
    }

    #[test]
    fn restores_place_each_artifact_once_and_never_rewrite_a_warm_cache() {
        let blobs = Blobs::new(MemoryKv::default().into_dyn(), "lake/test/cas");
        let (archive, _) = blobs::hash_file(&fixture()).unwrap();
        blobs.put_file(&archive, &fixture()).unwrap();
        let staging = tempfile::tempdir().unwrap();
        let object = staging.path().join("object");
        fs::write(&object, b"object code").unwrap();
        let (code, _) = blobs::hash_file(&object).unwrap();
        blobs.put_file(&code, &object).unwrap();
        let ltar = "89abcdef01234567.ltar".to_owned();
        let o = "0d9deac573f269f3.o".to_owned();
        // Two inputs per output, as a union of publishers' maps has them.
        let entry = |name: &String, cid| Entry {
            output: format!("\"{name}\""),
            artifacts: BTreeMap::from([(name.clone(), cid)]),
        };
        let entries = BTreeMap::from([
            ("0123456789abcdef".to_owned(), entry(&ltar, archive)),
            ("0123456789abcde0".to_owned(), entry(&ltar, archive)),
            ("1123456789abcdef".to_owned(), entry(&o, code)),
            ("1123456789abcde0".to_owned(), entry(&o, code)),
        ]);
        let artifacts = tempfile::tempdir().unwrap();
        let rows = install(&blobs, artifacts.path(), &entries).unwrap();
        assert_eq!(rows["1123456789abcdef"], format!("\"{o}\""));
        assert_eq!(rows["1123456789abcde0"], format!("\"{o}\""));
        assert_eq!(rows["0123456789abcdef"], rows["0123456789abcde0"]);
        let outputs: Value = serde_json::from_str(&rows["0123456789abcdef"]).unwrap();
        assert_eq!(
            outputs,
            serde_json::json!({
                "m": true,
                "o": ["1111111111111111.olean", "2222222222222222.olean.server",
                      "3333333333333333.olean.private"],
                "i": "4444444444444444.ilean",
                "rs": "5555555555555555.ir.sig",
                "r": "6666666666666666.ir",
                "c": "7777777777777777.c",
            })
        );
        let first = listing(artifacts.path());
        let names: Vec<_> = first.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "0d9deac573f269f3.o",
                "1111111111111111.olean",
                "2222222222222222.olean.server",
                "3333333333333333.olean.private",
                "4444444444444444.ilean",
                "5555555555555555.ir.sig",
                "6666666666666666.ir",
                "7777777777777777.c",
            ]
        );
        for (name, _) in &first {
            let path = artifacts.path().join(name);
            assert!(
                fs::metadata(&path).unwrap().permissions().readonly(),
                "{name}"
            );
        }
        assert_eq!(fs::read(artifacts.path().join(&o)).unwrap(), b"object code");
        // A warm cache: the same restore again succeeds and writes nothing.
        assert_eq!(install(&blobs, artifacts.path(), &entries).unwrap(), rows);
        assert_eq!(listing(artifacts.path()), first);
    }
}
