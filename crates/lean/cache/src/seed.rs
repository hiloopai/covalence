//! Host seeds: one published revision's unpacked Lake cache entries as a few
//! large objects.
//!
//! A fresh host otherwise asks Lake to fetch and unpack each module's archive
//! on its own, one request and one `leantar` process per module. A seed holds,
//! for every input a recorded build checked, Lake's local output file
//! (`outputs/<package>/<input>.json`, naming the unpacked artifacts) and the
//! artifacts it names, as [`CHUNKS`] Zstandard tar streams that download and
//! unpack in parallel. Lake still checks every trace against the sources; a
//! seed only fills its cache.
//!
//! A repository keeps [`SLOTS`] seeds per scope under
//! `lake/<scope>/seed/<owner>/<repo>/<slot>/`; each publish overwrites the
//! oldest slot, chunks first and its manifest last, so storage stays bounded
//! without deletes. The manifest names each chunk's SHA-256, so a reader that
//! races an overwrite fails the check and restores without the seed.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Instant, SystemTime},
};

use covalence_kv_trait::Kv;
use covalence_lib_data::Bytes;
use covalence_lib_serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    archives,
    blobs::Hashing,
    error::{Error, IoResult, Result, encode, invalid, json},
    mapping,
    project::{Project, Receipt},
    storage::parallel,
};

/// Seeds kept per repository and scope.
const SLOTS: [&str; 2] = ["a", "b"];
/// Streams per seed, which a restore downloads and unpacks at once.
const CHUNKS: usize = 16;
const LEVEL: i32 = 3;

#[derive(Deserialize, Serialize)]
#[serde(crate = "covalence_lib_serde::serde", deny_unknown_fields)]
struct Manifest {
    commit: String,
    /// Seconds since the Unix epoch; a publish replaces the oldest slot.
    published: u64,
    chunks: Vec<Chunk>,
}

#[derive(Deserialize, Serialize)]
#[serde(crate = "covalence_lib_serde::serde", deny_unknown_fields)]
struct Chunk {
    /// Lowercase hex SHA-256 of the stored, compressed bytes.
    sha256: String,
    bytes: u64,
    files: usize,
}

fn store_error(operation: impl Into<String>) -> impl FnOnce(covalence_kv_trait::Error) -> Error {
    move |source| Error::Store {
        operation: operation.into(),
        source,
    }
}

fn prefix(scope: &str, repository: &str, slot: &str) -> Result<String> {
    mapping::segment_path(scope)?;
    mapping::segment_path(repository)?;
    Ok(format!("lake/{scope}/seed/{repository}/{slot}"))
}

fn manifests(
    kv: &dyn Kv,
    scope: &str,
    repository: &str,
) -> Result<Vec<(&'static str, Option<Manifest>)>> {
    SLOTS
        .iter()
        .map(|slot| {
            let key = format!("{}/manifest.json", prefix(scope, repository, slot)?);
            let manifest = kv
                .get(key.as_bytes())
                .map_err(store_error("read seed manifest"))?
                .map(|bytes| json::<Manifest>(&bytes))
                .transpose()?;
            Ok((*slot, manifest))
        })
        .collect()
}

/// A relative path a seed may hold: an artifact, or a package's output file.
fn member(path: &str) -> bool {
    match path.split('/').collect::<Vec<_>>()[..] {
        ["artifacts", name] => mapping::artifact_name(name),
        ["outputs", package, file] => {
            mapping::segment_path(package).is_ok()
                && file
                    .strip_suffix(".json")
                    .is_some_and(|input| mapping::hex(input, 16))
        }
        _ => false,
    }
}

/// One file of a seed, by its path in Lake's cache: an artifact to copy, or
/// an output file's new bytes.
struct Member {
    name: String,
    bytes: u64,
    inline: Option<Vec<u8>>,
}

/// What a seed of `receipt` holds: each recorded input's local output, with
/// the artifacts it names (unpacked modules, native objects), when all of
/// them exist. Other inputs are left to the pool. As in an eager restore, an
/// output drops its archive: the unpacked files are all Lake needs.
///
/// A module Lake restored from its archive alone can keep that archive as
/// its local output, as on a publisher whose build folder was already up to
/// date; every host the seed fills would then unpack it again. Such an
/// archive is decoded here and its files stand in its place.
fn contents(project: &Project, receipt: &Receipt, cache: &Path) -> Result<Vec<Member>> {
    let mut outputs = Vec::new();
    let mut artifacts = BTreeSet::new();
    let mut archived = BTreeMap::new();
    for package in receipt.packages.keys() {
        for input in mapping::rows(&project.mapping(package))?.keys() {
            let name = format!("outputs/{package}/{input}.json");
            let bytes = match fs::read(cache.join(&name)) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(Error::Io {
                        operation: format!("read {name}"),
                        source,
                    });
                }
            };
            let mut output: Value = json(&bytes)?;
            if let Some(data) = output["data"].as_object_mut() {
                data.remove("l");
            }
            if output["data"]
                .as_str()
                .is_some_and(|data| data.ends_with(".ltar"))
            {
                archived.insert(name, output);
                continue;
            }
            add_output(cache, name, &output, &mut outputs, &mut artifacts)?;
        }
    }
    let rows = archived
        .iter()
        .map(|(name, output)| Ok((name.clone(), encode(&output["data"]).and_then(utf8)?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let unpacked = archives::expand(&cache.join("artifacts"), &rows)?;
    for (name, mut output) in archived {
        // An archive missing from the cache leaves its input to the pool.
        if let Some(descriptor) = unpacked.get(&name) {
            output["data"] = json(descriptor.as_bytes())?;
            add_output(cache, name, &output, &mut outputs, &mut artifacts)?;
        }
    }
    for name in artifacts {
        let bytes = cache
            .join(&name)
            .metadata()
            .operation(format!("read {name}"))?
            .len();
        outputs.push(Member {
            name,
            bytes,
            inline: None,
        });
    }
    Ok(outputs)
}

fn utf8(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes).map_err(|_| invalid("output descriptor is not UTF-8"))
}

/// Add the output file `name` holding `output`, and the artifacts it names,
/// when every one is in `cache`.
fn add_output(
    cache: &Path,
    name: String,
    output: &Value,
    outputs: &mut Vec<Member>,
    artifacts: &mut BTreeSet<String>,
) -> Result<()> {
    let names = mapping::names(&output["data"])?
        .into_iter()
        .map(|name| format!("artifacts/{name}"))
        .collect::<Vec<_>>();
    if names.iter().all(|name| cache.join(name).is_file()) {
        let bytes = encode(output)?;
        outputs.push(Member {
            name,
            bytes: bytes.len() as u64,
            inline: Some(bytes),
        });
        artifacts.extend(names);
    }
    Ok(())
}

/// Spread `members` over [`CHUNKS`] by size, largest first.
fn chunks(mut members: Vec<Member>) -> Vec<Vec<Member>> {
    members.sort_unstable_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    let mut bins: Vec<(u64, Vec<Member>)> = (0..CHUNKS.min(members.len()).max(1))
        .map(|_| (0, Vec::new()))
        .collect();
    for member in members {
        if let Some(bin) = bins.iter_mut().min_by_key(|(total, _)| *total) {
            bin.0 += member.bytes;
            bin.1.push(member);
        }
    }
    bins.into_iter().map(|(_, members)| members).collect()
}

fn write_chunk(members: &[Member], cache: &Path) -> Result<(File, Chunk)> {
    let staged = tempfile::tempfile().operation("stage seed chunk")?;
    let hashing = Hashing::new(staged);
    let encoder = zstd::stream::write::Encoder::new(hashing, LEVEL).operation("start Zstandard")?;
    let mut tar = tar::Builder::new(encoder);
    for member in members {
        let mut header = tar::Header::new_ustar();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(member.bytes);
        header.set_mode(0o444);
        header.set_mtime(0);
        match &member.inline {
            Some(bytes) => tar.append_data(&mut header, &member.name, bytes.as_slice()),
            None => tar.append_data(
                &mut header,
                &member.name,
                File::open(cache.join(&member.name)).operation(format!("open {}", member.name))?,
            ),
        }
        .operation("write seed chunk")?;
    }
    let hashing = tar
        .into_inner()
        .operation("finish seed archive")?
        .finish()
        .operation("finish seed chunk")?;
    let (mut staged, sha256, bytes) = hashing.finish();
    staged.rewind().operation("rewind seed chunk")?;
    Ok((
        staged,
        Chunk {
            sha256: data_encoding::HEXLOWER.encode(&sha256),
            bytes,
            files: members.len(),
        },
    ))
}

/// Publish a seed of the recorded build, unless a slot holds one already.
pub(crate) fn publish(
    project: &Project,
    kv: &dyn Kv,
    receipt: &Receipt,
    cache: &Path,
) -> Result<Value> {
    let identity = &receipt.identity;
    let files = contents(project, receipt, cache)?;
    put(
        kv,
        &identity.scope,
        &project.repository,
        &identity.revision,
        files,
        cache,
    )
}

/// Publish `members` of `cache` as the seed of `commit`.
fn put(
    kv: &dyn Kv,
    scope: &str,
    repository: &str,
    commit: &str,
    members: Vec<Member>,
    cache: &Path,
) -> Result<Value> {
    let started = Instant::now();
    let slots = manifests(kv, scope, repository)?;
    if slots
        .iter()
        .any(|(_, manifest)| manifest.as_ref().is_some_and(|m| m.commit == commit))
    {
        return Ok(serde_json::json!({ "skipped": "already seeded" }));
    }
    let slot = slots
        .iter()
        .min_by_key(|(_, manifest)| manifest.as_ref().map(|m| m.published))
        .map(|(slot, _)| *slot)
        .ok_or_else(|| invalid("no seed slots"))?;
    let prefix = prefix(scope, repository, slot)?;
    let bins = chunks(members);
    let chunks = parallel(&bins, |index, files| {
        let (mut staged, chunk) = write_chunk(files, cache)?;
        kv.put_from(
            format!("{prefix}/{index}.tar.zst").as_bytes(),
            &mut staged,
            chunk.bytes,
        )
        .map_err(store_error("upload seed chunk"))?;
        Ok(chunk)
    })?;
    let manifest = Manifest {
        commit: commit.to_owned(),
        published: SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_| invalid("clock precedes the epoch"))?
            .as_secs(),
        chunks,
    };
    kv.put(
        format!("{prefix}/manifest.json").as_bytes(),
        Bytes::from(encode(&manifest)?),
    )
    .map_err(store_error("publish seed manifest"))?;
    Ok(serde_json::json!({
        "slot": slot,
        "files": manifest.chunks.iter().map(|c| c.files).sum::<usize>(),
        "bytes": manifest.chunks.iter().map(|c| c.bytes).sum::<u64>(),
        "seconds": started.elapsed().as_secs_f64(),
    }))
}

/// The inputs a seed mapped, by package; restores leave them alone.
pub(crate) type Seeded = BTreeMap<String, BTreeSet<String>>;

/// A seed unpacked beside Lake's cache.
pub(crate) struct Staged {
    directory: tempfile::TempDir,
    files: usize,
    bytes: u64,
    seconds: f64,
}

/// Write one tar stream's members under `directory`.
fn unpack(reader: impl Read, directory: &Path) -> Result<usize> {
    let mut archive = tar::Archive::new(reader);
    let mut files = 0;
    for entry in archive.entries().operation("read seed chunk")? {
        let mut entry = entry.operation("read seed entry")?;
        let path = entry.path().operation("read seed entry path")?;
        let Some(name) = path.to_str().filter(|name| member(name)) else {
            return Err(invalid("seed holds an unexpected path"));
        };
        if entry.header().entry_type() != tar::EntryType::Regular {
            return Err(invalid("seed holds a non-file entry"));
        }
        let destination = directory.join(name);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).operation("create seed directory")?;
        }
        let mut file = File::create_new(&destination).operation("create seed file")?;
        io::copy(&mut entry, &mut file).operation("write seed file")?;
        files += 1;
    }
    // Read the archive's padding too, so the writer never meets a closed pipe.
    io::copy(&mut archive.into_inner(), &mut io::sink()).operation("read seed chunk")?;
    Ok(files)
}

/// Download, check and unpack one chunk into `directory`.
fn fetch_chunk(kv: &dyn Kv, key: &str, chunk: &Chunk, directory: &Path) -> Result<usize> {
    let (reader, writer) = io::pipe().operation("open seed pipe")?;
    thread::scope(|scope| {
        let unpacking = scope.spawn(move || unpack(reader, directory));
        let downloaded = (|| {
            let decoder = zstd::stream::write::Decoder::new(writer).operation("start Zstandard")?;
            let mut hashing = Hashing::new(decoder);
            let found = kv
                .get_to(key.as_bytes(), &mut hashing)
                .map_err(store_error("download seed chunk"))?;
            let (mut decoder, sha256, bytes) = hashing.finish();
            decoder.flush().operation("finish seed chunk")?;
            Ok::<_, Error>((found, sha256, bytes))
        })();
        let unpacked = unpacking.join().unwrap_or(Err(Error::Worker));
        let (found, sha256, bytes) = match (downloaded, &unpacked) {
            (Ok(downloaded), _) => downloaded,
            // Either side failing breaks the other's stream; report both.
            (Err(download), Err(unpack)) => {
                return Err(invalid(format!("{download}; {unpack}")));
            }
            (Err(download), Ok(_)) => return Err(download),
        };
        if found.is_none() {
            return Err(invalid("seed chunk is missing"));
        }
        if bytes != chunk.bytes || data_encoding::HEXLOWER.encode(&sha256) != chunk.sha256 {
            return Err(invalid("seed chunk failed its hash check"));
        }
        let files = unpacked?;
        if files != chunk.files {
            return Err(invalid("seed chunk holds the wrong number of files"));
        }
        Ok(files)
    })
}

/// Download and unpack the seed of `commit` beside Lake's cache in `cache`,
/// if a slot holds one.
pub(crate) fn fetch(
    kv: &Arc<dyn Kv>,
    scope: &str,
    repository: &str,
    commit: &str,
    cache: &Path,
) -> Result<Option<Staged>> {
    let started = Instant::now();
    let Some((slot, manifest)) = manifests(&**kv, scope, repository)?
        .into_iter()
        .find_map(|(slot, manifest)| manifest.filter(|m| m.commit == commit).map(|m| (slot, m)))
    else {
        return Ok(None);
    };
    let state = cache.join("covalence");
    fs::create_dir_all(&state).operation("create local pool directory")?;
    let directory = tempfile::tempdir_in(&state).operation("create seed directory")?;
    let prefix = prefix(scope, repository, slot)?;
    let files = parallel(&manifest.chunks, |index, chunk| {
        fetch_chunk(
            &**kv,
            &format!("{prefix}/{index}.tar.zst"),
            chunk,
            directory.path(),
        )
    })?;
    Ok(Some(Staged {
        directory,
        files: files.into_iter().sum(),
        bytes: manifest.chunks.iter().map(|c| c.bytes).sum(),
        seconds: started.elapsed().as_secs_f64(),
    }))
}

impl Staged {
    /// Move the seed into Lake's cache: the artifacts it lacks, then every
    /// output file. Returns the inputs it mapped.
    pub fn install(self, cache: &Path) -> Result<Seeded> {
        let started = Instant::now();
        let mut seeded = Seeded::new();
        let mut moved = 0;
        for kind in ["artifacts", "outputs"] {
            let source = self.directory.path().join(kind);
            if !source.exists() {
                continue;
            }
            for from in walk(&source)? {
                let relative = from
                    .strip_prefix(&source)
                    .map_err(|_| invalid("seed file outside its directory"))?;
                let to = cache.join(kind).join(relative);
                if kind == "outputs"
                    && let (Some(package), Some(input)) = (
                        relative.parent().and_then(Path::to_str),
                        relative.file_stem().and_then(|stem| stem.to_str()),
                    )
                {
                    seeded
                        .entry(package.to_owned())
                        .or_default()
                        .insert(input.to_owned());
                } else if to.exists() {
                    continue;
                }
                if let Some(parent) = to.parent() {
                    fs::create_dir_all(parent).operation("create Lake cache directory")?;
                }
                fs::rename(&from, &to).operation("install seed file")?;
                moved += 1;
            }
        }
        println!(
            "Lake cache: seeded {moved} of {} files ({:.1} GB) in {:.1} s, installed in {:.1} s",
            self.files,
            self.bytes as f64 / 1e9,
            self.seconds,
            started.elapsed().as_secs_f64(),
        );
        Ok(seeded)
    }
}

fn walk(directory: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(directory).operation("read seed directory")? {
        let entry = entry.operation("read seed directory")?;
        let path = entry.path();
        if entry
            .file_type()
            .operation("read seed entry type")?
            .is_dir()
        {
            files.extend(walk(&path)?);
        } else {
            files.push(path);
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Identity;
    use covalence_kv_memory::MemoryKv;

    const SCOPE: &str = "linux-x86_64/t";
    const REPOSITORY: &str = "o/r";
    const OUTPUT: &str = "outputs/p/00000000000000aa.json";

    fn commit(n: u8) -> String {
        format!("{n:040}")
    }

    /// A publisher's Lake cache with one unpacked module, its archive and a
    /// native object, and the receipt of the build that checked them.
    fn publisher() -> (tempfile::TempDir, Project, Receipt) {
        let dir = tempfile::tempdir().unwrap();
        let files: [(&str, &[u8]); 7] = [
            ("cache/artifacts/0000000000000001.olean", &[1; 300_000]),
            ("cache/artifacts/0000000000000002.ilean", b"ilean"),
            ("cache/artifacts/0000000000000003.ltar", &[3; 70_000]),
            ("cache/artifacts/0000000000000004.o", b"object"),
            (
                "cache/outputs/p/00000000000000aa.json",
                br#"{"service": null, "data": {"o": ["0000000000000001.olean"], "i": "0000000000000002.ilean", "l": "0000000000000003.ltar"}}"#,
            ),
            (
                "cache/outputs/p/00000000000000bb.json",
                br#"{"service": null, "data": "0000000000000004.o"}"#,
            ),
            (
                "record/packages/p.jsonl",
                b"\"2026-03-17\"\n[\"00000000000000aa\",\"0000000000000003.ltar\"]\n[\"00000000000000bb\",\"0000000000000004.o\"]\n",
            ),
        ];
        for (name, bytes) in files {
            let path = dir.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        let project = Project {
            root: dir.path().to_owned(),
            repository: REPOSITORY.into(),
            record: dir.path().join("record"),
        };
        let receipt = Receipt {
            identity: Identity {
                scope: SCOPE.into(),
                revision: commit(1),
                run: ["1".into(), "1".into()],
            },
            packages: BTreeMap::from([("p".into(), String::new())]),
            artifacts: dir.path().join("cache/artifacts"),
            targets: Vec::new(),
        };
        (dir, project, receipt)
    }

    fn publish_at(kv: &dyn Kv, n: u8) -> Value {
        let (dir, project, mut receipt) = publisher();
        receipt.identity.revision = commit(n);
        publish(&project, kv, &receipt, &dir.path().join("cache")).unwrap()
    }

    #[test]
    fn a_seed_restores_unpacked_outputs_without_their_archives() {
        let kv = MemoryKv::default().into_dyn();
        assert_eq!(publish_at(&*kv, 1)["files"], 5);
        assert_eq!(publish_at(&*kv, 1)["skipped"], "already seeded");
        let host = tempfile::tempdir().unwrap();
        assert!(
            fetch(&kv, SCOPE, REPOSITORY, &commit(2), host.path())
                .unwrap()
                .is_none()
        );
        let seeded = fetch(&kv, SCOPE, REPOSITORY, &commit(1), host.path())
            .unwrap()
            .unwrap()
            .install(host.path())
            .unwrap();
        assert_eq!(
            seeded,
            Seeded::from([(
                "p".into(),
                BTreeSet::from(["00000000000000aa".into(), "00000000000000bb".into()])
            )])
        );
        let (publisher, ..) = publisher();
        for name in [
            "artifacts/0000000000000001.olean",
            "artifacts/0000000000000002.ilean",
            "artifacts/0000000000000004.o",
        ] {
            assert_eq!(
                fs::read(host.path().join(name)).unwrap(),
                fs::read(publisher.path().join("cache").join(name)).unwrap(),
                "{name}"
            );
        }
        assert!(!host.path().join("artifacts/0000000000000003.ltar").exists());
        let output: Value = json(&fs::read(host.path().join(OUTPUT)).unwrap()).unwrap();
        assert_eq!(
            output["data"],
            serde_json::json!({"o": ["0000000000000001.olean"], "i": "0000000000000002.ilean"})
        );
    }

    #[test]
    fn a_seed_holds_the_files_of_an_output_that_is_only_an_archive() {
        let (dir, project, receipt) = publisher();
        let cache = dir.path().join("cache");
        let archive = "89abcdef01234567.ltar";
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../ltar/tests/fixtures/module.ltar"),
            cache.join("artifacts").join(archive),
        )
        .unwrap();
        // As Lake leaves a module it restored from the pool's archive alone.
        fs::write(
            cache.join("outputs/p/00000000000000dd.json"),
            format!(r#"{{"service": "covalence", "data": "{archive}"}}"#),
        )
        .unwrap();
        let mapping = project.mapping("p");
        let mut rows = fs::read_to_string(&mapping).unwrap();
        rows.push_str(&format!("[\"00000000000000dd\",\"{archive}\"]\n"));
        fs::write(&mapping, rows).unwrap();
        let kv = MemoryKv::default().into_dyn();
        publish(&project, &*kv, &receipt, &cache).unwrap();
        let host = tempfile::tempdir().unwrap();
        let seeded = fetch(&kv, SCOPE, REPOSITORY, &commit(1), host.path())
            .unwrap()
            .unwrap()
            .install(host.path())
            .unwrap();
        assert!(seeded["p"].contains("00000000000000dd"));
        let output: Value =
            json(&fs::read(host.path().join("outputs/p/00000000000000dd.json")).unwrap()).unwrap();
        assert_eq!(output["data"]["c"], "7777777777777777.c");
        for name in mapping::names(&output["data"]).unwrap() {
            assert!(
                host.path().join("artifacts").join(&name).is_file(),
                "{name}"
            );
        }
        assert!(!host.path().join("artifacts").join(archive).exists());
    }

    #[test]
    fn publishes_rotate_two_slots_and_a_torn_chunk_fails() {
        let kv = MemoryKv::default().into_dyn();
        for n in 1..=3 {
            publish_at(&*kv, n);
        }
        let kept = manifests(&*kv, SCOPE, REPOSITORY)
            .unwrap()
            .into_iter()
            .map(|(_, manifest)| manifest.unwrap().commit)
            .collect::<BTreeSet<_>>();
        assert_eq!(kept, BTreeSet::from([commit(2), commit(3)]));
        let host = tempfile::tempdir().unwrap();
        assert!(
            fetch(&kv, SCOPE, REPOSITORY, &commit(1), host.path())
                .unwrap()
                .is_none()
        );
        // A chunk overwritten under a reader no longer matches its manifest.
        let (slot, _) = manifests(&*kv, SCOPE, REPOSITORY)
            .unwrap()
            .into_iter()
            .find(|(_, m)| m.as_ref().unwrap().commit == commit(3))
            .unwrap();
        let prefix = prefix(SCOPE, REPOSITORY, slot).unwrap();
        let other = kv
            .get(format!("{prefix}/1.tar.zst").as_bytes())
            .unwrap()
            .unwrap();
        kv.put(format!("{prefix}/0.tar.zst").as_bytes(), other)
            .unwrap();
        assert!(fetch(&kv, SCOPE, REPOSITORY, &commit(3), host.path()).is_err());
    }

    #[test]
    fn members_are_flat_artifacts_and_package_outputs() {
        assert!(member("artifacts/0123456789abcdef.olean.private"));
        assert!(member("outputs/riscv-zkvm/0123456789abcdef.json"));
        for path in [
            "artifacts/../x",
            "artifacts/0123456789abcdef/x",
            "outputs/../0123456789abcdef.json",
            "outputs/a/0123456789abcdef.txt",
            "covalence/nodes.db",
            "/etc/passwd",
        ] {
            assert!(!member(path), "{path}");
        }
    }
}
