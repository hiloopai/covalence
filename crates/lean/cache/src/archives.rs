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

/// The archive an entry's output is, if it is only one: its name in Lake's
/// cache and its content ID.
fn archive(entry: &Entry) -> Option<(String, &Sha256)> {
    let Ok(Value::String(name)) = serde_json::from_str(&entry.output) else {
        return None;
    };
    let cid = entry.artifacts.get(&name)?;
    name.ends_with(".ltar").then_some((name, cid))
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
    let mut inputs = BTreeMap::<_, Vec<_>>::new();
    for (input, entry) in entries {
        match archive(entry) {
            Some((_, cid)) => {
                jobs.insert(Job::Archive(cid));
                inputs.entry(cid).or_default().push(input.as_str());
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
        Job::Archive(cid) => unpack_one(blobs, artifacts, cid, &inputs[cid]).map(Some),
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
                .and_then(|(_, cid)| descriptors.get(cid))
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
fn unpack_one(blobs: &Blobs, artifacts: &Path, cid: &Sha256, inputs: &[&str]) -> Result<String> {
    let mut bytes = Vec::new();
    blobs
        .get_to(cid, &mut bytes)?
        .ok_or_else(|| invalid(format!("{cid} is missing from the pool")))?;
    place(artifacts, &bytes, &cid.to_string(), inputs)
}

/// Lake's output descriptor for each row of `rows`, such as an upstream
/// snapshot's mapping, whose output is an archive Lake already downloaded
/// into `artifacts`, after placing that archive's files there. Other rows
/// are left out.
///
/// Lake unpacks such an archive on every build that finds only it, and its
/// `cache get` downloads only archives, so the snapshot is decoded here.
pub(crate) fn expand(
    artifacts: &Path,
    rows: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let started = Instant::now();
    let mut inputs = BTreeMap::<_, Vec<_>>::new();
    for (input, output) in rows {
        if let Some(name) = local_archive(artifacts, output) {
            inputs.entry(name).or_default().push(input.as_str());
        }
    }
    let archives: Vec<_> = inputs.keys().copied().collect();
    let descriptors = storage::parallel_with(storage::ARCHIVE_WORKERS, &archives, |_, name| {
        let path = artifacts.join(name);
        let bytes = fs::read(&path).operation(format!("read {}", path.display()))?;
        place(artifacts, &bytes, name, &inputs[name])
    })?;
    let descriptors: BTreeMap<_, _> = archives.into_iter().zip(descriptors).collect();
    if !descriptors.is_empty() {
        println!(
            "Lake cache: placed the files of {} archives in {:.1} s",
            descriptors.len(),
            started.elapsed().as_secs_f64()
        );
    }
    Ok(rows
        .iter()
        .filter_map(|(input, output)| {
            let descriptor = descriptors.get(local_archive(artifacts, output)?)?;
            Some((input.clone(), descriptor.clone()))
        })
        .collect())
}

/// The archive file in `artifacts` that a Lake output descriptor is, if it
/// is only one archive and Lake downloaded it.
fn local_archive<'a>(artifacts: &Path, output: &'a str) -> Option<&'a str> {
    let name = output.strip_prefix('"')?.strip_suffix('"')?;
    (name.ends_with(".ltar") && mapping::artifact_name(name) && artifacts.join(name).is_file())
        .then_some(name)
}

/// Writes an archive's files, which `name` names in errors, and returns the
/// outputs its trace names.
fn place(artifacts: &Path, bytes: &[u8], name: &str, inputs: &[&str]) -> Result<String> {
    // Check every mapping before sharing this archive's decoded files.
    for input in inputs {
        validate_input(bytes, name, input)?;
    }
    // The input hash goes only into the trace's `depHash`, which Lake does
    // not read here, so one decode serves every input naming the archive.
    let archive = covalence_lean_ltar::unpack(bytes, 0).map_err(|source| Error::Archive {
        archive: name.to_owned(),
        source,
    })?;
    let stem = archive
        .trace
        .path
        .strip_suffix(".trace")
        .ok_or_else(|| invalid(format!("archive {name} has no module trace")))?;
    let trace: Value = json(&archive.trace.bytes)?;
    let Some(Value::Object(mut outputs)) = trace.get("outputs").cloned() else {
        return Err(invalid(format!("archive {name} names no module outputs")));
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
                        return Err(invalid(format!("archive {name} names an invalid output")));
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
            .ok_or_else(|| invalid(format!("archive {name} names an invalid output")))?;
        let path = format!("{stem}.{extension}");
        let file = archive
            .files
            .iter()
            .find(|file| file.path == path)
            .ok_or_else(|| invalid(format!("archive {name} lacks its {extension} output")))?;
        save(&artifacts.join(name), &file.bytes)?;
    }
    serde_json::to_string(&Value::Object(outputs)).map_err(|source| Error::Json {
        operation: "write Lake output descriptor".into(),
        source,
    })
}

/// Reject a retained archive-header input that contradicts the requested key,
/// before decoding can replace the trace's hash or any output is installed.
/// A stripped header remains unverified; a match is not producer provenance.
pub(crate) fn validate_input(bytes: &[u8], name: &str, input: &str) -> Result<()> {
    let retained =
        covalence_lean_ltar::header_input_hash(bytes).map_err(|source| Error::Archive {
            archive: name.to_owned(),
            source,
        })?;
    if let Some(retained) = retained {
        if !mapping::hex(input, 16) {
            return Err(invalid("an archive input must be a 16-digit hash"));
        }
        if format!("{retained:016x}") != input {
            return Err(invalid(format!(
                "archive {name} retains input {retained:016x}, contradicting requested input {input}"
            )));
        }
    }
    Ok(())
}

/// Writes a content-addressed file once, read-only as Lake leaves the
/// artifacts it downloads; an existing file has the same bytes. The bytes
/// must be what the file's name says.
fn save(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("artifact has no name"))?;
    crate::lake_hash::check(name, bytes)?;
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

    /// A module archive Lake packed, its files named by their bytes.
    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cd9cef913c0321f4.ltar")
    }

    /// The outputs the fixture's trace names, without its archive.
    fn fixture_outputs() -> Value {
        let archive = covalence_lean_ltar::unpack(&fs::read(fixture()).unwrap(), 0).unwrap();
        let mut outputs: Value =
            serde_json::from_slice::<Value>(&archive.trace.bytes).unwrap()["outputs"].clone();
        outputs.as_object_mut().unwrap().remove("l");
        outputs
    }

    /// An object file and its name, as Lake names it.
    fn object_code() -> (String, &'static [u8]) {
        let bytes = b"object code".as_slice();
        (format!("{:016x}.o", crate::lake_hash::of(bytes)), bytes)
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

    fn retained_fixture(input: u64) -> Vec<u8> {
        let stripped = fs::read(fixture()).unwrap();
        assert_eq!(stripped[4], 0);
        let mut bytes = b"LTR4\x01".to_vec();
        bytes.extend_from_slice(&input.to_le_bytes());
        bytes.extend_from_slice(&stripped[5..]);
        bytes
    }

    #[test]
    fn deduplicated_archive_checks_every_input_before_placing_files() {
        let staging = tempfile::tempdir().unwrap();
        let path = staging.path().join("archive");
        fs::write(&path, retained_fixture(0x0123_4567_89ab_cdef)).unwrap();
        let blobs = Blobs::new(MemoryKv::default().into_dyn(), "lake/test/cas");
        let (cid, _) = blobs::hash_file(&path).unwrap();
        blobs.put_file(&cid, &path).unwrap();
        let entry = Entry {
            output: "\"89abcdef01234567.ltar\"".to_owned(),
            artifacts: BTreeMap::from([("89abcdef01234567.ltar".to_owned(), cid)]),
        };
        let mut entries = BTreeMap::from([("0123456789abcdef".to_owned(), entry.clone())]);
        let artifacts = tempfile::tempdir().unwrap();
        entries.insert("0123456789abcde0".to_owned(), entry);
        let error = install(&blobs, artifacts.path(), &entries).unwrap_err();
        assert!(error.to_string().contains("contradicting requested input"));
        assert!(listing(artifacts.path()).is_empty());
        entries.remove("0123456789abcde0");
        assert!(install(&blobs, artifacts.path(), &entries).is_ok());
        assert!(!listing(artifacts.path()).is_empty());
    }

    #[test]
    fn local_archive_checks_all_inputs_even_with_warm_outputs() {
        let artifacts = tempfile::tempdir().unwrap();
        let name = "89abcdef01234567.ltar";
        fs::write(
            artifacts.path().join(name),
            retained_fixture(0x0123_4567_89ab_cdef),
        )
        .unwrap();
        let mut rows = BTreeMap::from([("0123456789abcdef".to_owned(), format!("\"{name}\""))]);
        let before = listing(artifacts.path());
        rows.insert("0123456789abcde0".to_owned(), format!("\"{name}\""));
        assert!(expand(artifacts.path(), &rows).is_err());
        assert_eq!(listing(artifacts.path()), before);
        rows.remove("0123456789abcde0");
        expand(artifacts.path(), &rows).unwrap();
        let warm = listing(artifacts.path());
        rows.insert("0123456789abcde0".to_owned(), format!("\"{name}\""));
        assert!(expand(artifacts.path(), &rows).is_err());
        assert_eq!(listing(artifacts.path()), warm);
    }

    #[test]
    fn restores_place_each_artifact_once_and_never_rewrite_a_warm_cache() {
        let blobs = Blobs::new(MemoryKv::default().into_dyn(), "lake/test/cas");
        let (archive, _) = blobs::hash_file(&fixture()).unwrap();
        blobs.put_file(&archive, &fixture()).unwrap();
        let staging = tempfile::tempdir().unwrap();
        let object = staging.path().join("object");
        let (o, code_bytes) = object_code();
        fs::write(&object, code_bytes).unwrap();
        let (code, _) = blobs::hash_file(&object).unwrap();
        blobs.put_file(&code, &object).unwrap();
        let ltar = "89abcdef01234567.ltar".to_owned();
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
        assert_eq!(outputs, fixture_outputs());
        let first = listing(artifacts.path());
        let names: Vec<_> = first.iter().map(|(name, _)| name.clone()).collect();
        let mut expected = mapping::names(&outputs).unwrap();
        expected.push(o.clone());
        expected.sort();
        assert_eq!(names, expected);
        for (name, _) in &first {
            let path = artifacts.path().join(name);
            assert!(
                fs::metadata(&path).unwrap().permissions().readonly(),
                "{name}"
            );
        }
        assert_eq!(fs::read(artifacts.path().join(&o)).unwrap(), code_bytes);
        // A warm cache: the same restore again succeeds and writes nothing.
        assert_eq!(install(&blobs, artifacts.path(), &entries).unwrap(), rows);
        assert_eq!(listing(artifacts.path()), first);
    }

    #[test]
    fn upstream_archives_lake_downloaded_become_their_outputs() {
        let artifacts = tempfile::tempdir().unwrap();
        let archive = "89abcdef01234567.ltar";
        fs::copy(fixture(), artifacts.path().join(archive)).unwrap();
        let rows = BTreeMap::from([
            ("0123456789abcdef".to_owned(), format!("\"{archive}\"")),
            // Another input with the same archive, an object file, and an
            // archive Lake did not download: only archives on disk expand.
            ("0123456789abcde0".to_owned(), format!("\"{archive}\"")),
            (
                "1123456789abcdef".to_owned(),
                "\"0d9deac573f269f3.o\"".to_owned(),
            ),
            (
                "2123456789abcdef".to_owned(),
                "\"aaaaaaaaaaaaaaaa.ltar\"".to_owned(),
            ),
        ]);
        let expanded = expand(artifacts.path(), &rows).unwrap();
        assert_eq!(
            expanded.keys().collect::<Vec<_>>(),
            ["0123456789abcde0", "0123456789abcdef"]
        );
        let outputs: Value = serde_json::from_str(&expanded["0123456789abcdef"]).unwrap();
        assert_eq!(outputs, fixture_outputs());
        for name in mapping::names(&outputs).unwrap() {
            assert!(artifacts.path().join(&name).is_file(), "{name}");
        }
        let first = listing(artifacts.path());
        assert_eq!(expand(artifacts.path(), &rows).unwrap(), expanded);
        assert_eq!(listing(artifacts.path()), first);
    }
}
