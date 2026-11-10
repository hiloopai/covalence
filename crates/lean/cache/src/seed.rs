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
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
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
    dependencies::{self, Snapshot},
    error::{Error, IoResult, Result, encode, invalid, json},
    mapping,
    project::{Project, Receipt, package_name},
    storage::parallel,
};

/// Seeds kept per repository and scope.
const SLOTS: [&str; 2] = ["a", "b"];
/// Streams per seed, which a restore downloads and unpacks at once.
const CHUNKS: usize = 16;
/// Higher levels and long-distance windows shrink a seed by under 1% more,
/// and a 2 GiB window takes 25 GB of memory to decode.
const LEVEL: i32 = 9;

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
/// them exist. Other inputs are left to the pool, and so are executables: a
/// library restore needs none, and each is one pool request for a build that
/// does. As in an eager restore, an output drops its archive: the unpacked
/// files are all Lake needs.
///
/// A module Lake restored from its archive alone can keep that archive as
/// its local output, as on a publisher whose build folder was already up to
/// date; every host the seed fills would then unpack it again. Such an
/// archive is decoded here and its files stand in its place.
///
/// The seed also holds every mapping of each upstream snapshot Lake fetched
/// here, its artifacts when they are local, and returns those snapshots: a
/// restore it fills need not ask upstream for them. A snapshot one of whose
/// mappings is not local is left out, as an incomplete map must never stand
/// in for upstream's.
fn contents(
    project: &Project,
    receipt: &Receipt,
    snapshots: Vec<Snapshot>,
    cache: &Path,
) -> Result<(Vec<Member>, Vec<Snapshot>)> {
    let mut seed = Contents {
        cache,
        members: Vec::new(),
        outputs: BTreeSet::new(),
        artifacts: BTreeSet::new(),
        archived: BTreeMap::new(),
    };
    for package in receipt.packages.keys() {
        for input in mapping::rows(&project.mapping(package))?.keys() {
            seed.add(format!("outputs/{package}/{input}.json"), false)?;
        }
    }
    let mut covered = Vec::new();
    for snapshot in snapshots {
        let path = snapshot.revisions(cache);
        if !path.is_file() {
            continue;
        }
        let package = package_name(&snapshot.package);
        let mut complete = true;
        for input in mapping::rows(&path)?.keys() {
            complete &= seed.add(format!("outputs/{package}/{input}.json"), true)?;
        }
        if complete {
            covered.push(snapshot);
        }
    }
    Ok((seed.finish()?, covered))
}

/// A seed's members as [`contents`] collects them.
struct Contents<'a> {
    cache: &'a Path,
    members: Vec<Member>,
    /// The output files added or awaiting their archive's files.
    outputs: BTreeSet<String>,
    artifacts: BTreeSet<String>,
    /// Outputs that are only an archive in the cache, by output file.
    archived: BTreeMap<String, Value>,
}

impl Contents<'_> {
    /// Add the output file `name` and the artifacts it names, if they are
    /// all in the cache; with `mapping`, the output file alone otherwise, as
    /// Lake fetches its artifacts from the service it names. Whether the
    /// seed holds it.
    fn add(&mut self, name: String, mapping: bool) -> Result<bool> {
        if self.outputs.contains(&name) {
            return Ok(true);
        }
        let bytes = match fs::read(self.cache.join(&name)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
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
        let names = mapping::names(&output["data"])?;
        let local = !names.iter().any(|artifact| executable(artifact))
            && names
                .iter()
                .all(|artifact| self.cache.join("artifacts").join(artifact).is_file());
        if local
            && output["data"]
                .as_str()
                .is_some_and(|data| data.ends_with(".ltar"))
        {
            self.outputs.insert(name.clone());
            self.archived.insert(name, output);
            return Ok(true);
        }
        if !(local || mapping) {
            return Ok(false);
        }
        if local {
            self.artifacts.extend(
                names
                    .into_iter()
                    .map(|artifact| format!("artifacts/{artifact}")),
            );
        }
        self.push(name, &output)?;
        Ok(true)
    }

    fn push(&mut self, name: String, output: &Value) -> Result<()> {
        let bytes = encode(output)?;
        self.outputs.insert(name.clone());
        self.members.push(Member {
            name,
            bytes: bytes.len() as u64,
            inline: Some(bytes),
        });
        Ok(())
    }

    /// The members, once every archived output's files are in the cache.
    fn finish(mut self) -> Result<Vec<Member>> {
        let archived = std::mem::take(&mut self.archived);
        let rows = archived
            .iter()
            .map(|(name, output)| Ok((name.clone(), encode(&output["data"]).and_then(utf8)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let unpacked = archives::expand(&self.cache.join("artifacts"), &rows)?;
        for (name, mut output) in archived {
            let descriptor = unpacked
                .get(&name)
                .ok_or_else(|| invalid(format!("{name} names an archive that did not unpack")))?;
            output["data"] = json(descriptor.as_bytes())?;
            self.artifacts.extend(
                mapping::names(&output["data"])?
                    .into_iter()
                    .map(|artifact| format!("artifacts/{artifact}")),
            );
            self.push(name, &output)?;
        }
        for name in std::mem::take(&mut self.artifacts) {
            let bytes = self
                .cache
                .join(&name)
                .metadata()
                .operation(format!("read {name}"))?
                .len();
            self.members.push(Member {
                name,
                bytes,
                inline: None,
            });
        }
        Ok(self.members)
    }
}

/// Whether Lake named the artifact `name` as an executable: it gives them
/// the platform's executable extension, none on Unix, and nothing else.
fn executable(name: &str) -> bool {
    Path::new(name)
        .extension()
        .map_or("", |extension| extension.to_str().unwrap_or_default())
        == std::env::consts::EXE_EXTENSION
}

fn utf8(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes).map_err(|_| invalid("output descriptor is not UTF-8"))
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
    let snapshots = dependencies::snapshots(project)?;
    let (files, upstream) = contents(project, receipt, snapshots, cache)?;
    put(
        kv,
        &identity.scope,
        &project.namespace(),
        &identity.revision,
        files,
        &upstream,
        cache,
    )
}

/// The upstream snapshots a seed holds every mapping of. It is written
/// before its slot's manifest and names the commit, so a reader that races
/// a publish finds another commit and asks upstream as before.
#[derive(Deserialize, Serialize)]
#[serde(crate = "covalence_lib_serde::serde", deny_unknown_fields)]
struct Upstream {
    commit: String,
    snapshots: Vec<Snapshot>,
}

/// The mapping lists Lake kept for a seed's upstream snapshots
/// (`revisions/<package>/<revision>.jsonl`), so a host the seed fills keeps
/// them as `lake cache get` would, and a seed it publishes in turn still holds
/// those snapshots. A separate object, which older readers never read.
#[derive(Deserialize, Serialize)]
#[serde(crate = "covalence_lib_serde::serde", deny_unknown_fields)]
struct Revisions {
    commit: String,
    /// Each file's text, by its path under `revisions/`.
    files: BTreeMap<String, String>,
}

/// Publish `members` of `cache` as the seed of `commit`, holding every
/// mapping of the `upstream` snapshots.
fn put(
    kv: &dyn Kv,
    scope: &str,
    repository: &str,
    commit: &str,
    members: Vec<Member>,
    upstream: &[Snapshot],
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
    let files = upstream
        .iter()
        .map(|snapshot| {
            let path = snapshot.revisions(cache);
            let relative = path
                .strip_prefix(cache.join("revisions"))
                .ok()
                .and_then(Path::to_str)
                .ok_or_else(|| invalid("upstream mapping list outside the cache"))?
                .to_owned();
            let text = fs::read_to_string(&path).operation(format!("read {}", path.display()))?;
            Ok((relative, text))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let revisions = Revisions {
        commit: commit.to_owned(),
        files,
    };
    kv.put(
        format!("{prefix}/revisions.json.zst").as_bytes(),
        Bytes::from(
            zstd::encode_all(&encode(&revisions)?[..], LEVEL)
                .operation("compress upstream mapping lists")?,
        ),
    )
    .map_err(store_error("publish seed upstream mapping lists"))?;
    let record = Upstream {
        commit: commit.to_owned(),
        snapshots: upstream.to_vec(),
    };
    kv.put(
        format!("{prefix}/upstream.json").as_bytes(),
        Bytes::from(encode(&record)?),
    )
    .map_err(store_error("publish seed upstream snapshots"))?;
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
        "upstream": upstream.len(),
        "seconds": started.elapsed().as_secs_f64(),
    }))
}

/// The inputs a seed mapped, by package; restores leave them alone.
pub(crate) type Seeded = BTreeMap<String, BTreeSet<String>>;

/// A seed unpacked beside Lake's cache.
pub(crate) struct Staged {
    directory: tempfile::TempDir,
    /// The seed's upstream mapping lists, by path under `revisions/`.
    revisions: BTreeMap<String, String>,
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

/// The seed for a restore that searched `commits`, nearest first: the
/// nearest of them that has one, else the newest. A seed of any commit is
/// sound, since Lake checks every trace and the pool's maps add each input
/// it lacks; the nearer, the fewer they are. Slots turn over on every
/// publish, so an older checkout often has no seeded commit in its history.
fn choose(
    slots: Vec<(&'static str, Option<Manifest>)>,
    commits: &[String],
) -> Option<(&'static str, Manifest)> {
    slots
        .into_iter()
        .filter_map(|(slot, manifest)| manifest.map(|manifest| (slot, manifest)))
        .min_by_key(|(_, manifest)| {
            (
                commits
                    .iter()
                    .position(|commit| *commit == manifest.commit)
                    .unwrap_or(usize::MAX),
                Reverse(manifest.published),
            )
        })
}

/// A published seed a restore chose.
pub(crate) struct Remote {
    slot: &'static str,
    manifest: Manifest,
    /// The upstream snapshots it holds every mapping of.
    pub upstream: BTreeSet<Snapshot>,
}

/// The seed for restoring `commit`, searching `commits`: its own, else
/// another's (see [`choose`]). Says which, or that the repository has none.
pub(crate) fn find(
    kv: &dyn Kv,
    scope: &str,
    repository: &str,
    commit: &str,
    commits: &[String],
) -> Result<Option<Remote>> {
    let Some((slot, manifest)) = choose(manifests(kv, scope, repository)?, commits) else {
        println!("Lake cache: no seed for {commit}; falling back to per-artifact restore");
        return Ok(None);
    };
    if manifest.commit != commit {
        let distance = commits
            .iter()
            .position(|c| *c == manifest.commit)
            .map_or_else(
                || "not an ancestor".to_owned(),
                |n| format!("{n} commits behind HEAD"),
            );
        println!(
            "Lake cache: no seed for {commit}; seeding from {} ({distance}), then the pool's maps",
            manifest.commit
        );
    }
    let key = format!("{}/upstream.json", prefix(scope, repository, slot)?);
    let upstream = kv
        .get(key.as_bytes())
        .map_err(store_error("read seed upstream snapshots"))?
        .map(|bytes| json::<Upstream>(&bytes))
        .transpose()?
        .filter(|record| record.commit == manifest.commit)
        .map(|record| record.snapshots.into_iter().collect())
        .unwrap_or_default();
    Ok(Some(Remote {
        slot,
        manifest,
        upstream,
    }))
}

impl Remote {
    /// Download and unpack the seed beside Lake's cache in `cache`.
    pub fn fetch(
        &self,
        kv: &dyn Kv,
        scope: &str,
        repository: &str,
        cache: &Path,
    ) -> Result<Staged> {
        let started = Instant::now();
        let state = cache.join("covalence");
        fs::create_dir_all(&state).operation("create local pool directory")?;
        let directory = tempfile::tempdir_in(&state).operation("create seed directory")?;
        let prefix = prefix(scope, repository, self.slot)?;
        let files = parallel(&self.manifest.chunks, |index, chunk| {
            fetch_chunk(
                kv,
                &format!("{prefix}/{index}.tar.zst"),
                chunk,
                directory.path(),
            )
        })?;
        let revisions = if self.upstream.is_empty() {
            BTreeMap::new()
        } else {
            let key = format!("{prefix}/revisions.json.zst");
            kv.get(key.as_bytes())
                .map_err(store_error("read seed upstream mapping lists"))?
                .map(|bytes| {
                    let raw =
                        zstd::decode_all(&bytes[..]).operation("decode upstream mapping lists")?;
                    json::<Revisions>(&raw)
                })
                .transpose()?
                // Another commit's lists raced the reader: leave them out.
                .filter(|lists| lists.commit == self.manifest.commit)
                .map(|lists| lists.files)
                .unwrap_or_default()
        };
        Ok(Staged {
            directory,
            revisions,
            files: files.into_iter().sum(),
            bytes: self.manifest.chunks.iter().map(|c| c.bytes).sum(),
            seconds: started.elapsed().as_secs_f64(),
        })
    }
}

impl Staged {
    /// Move the seed into Lake's cache: the artifacts it lacks, then every
    /// output file. Returns the inputs it mapped.
    pub fn install(self, cache: &Path) -> Result<Seeded> {
        let started = Instant::now();
        for (relative, text) in &self.revisions {
            let mut parts = relative.split('/');
            let (Some(package), Some(file), None) = (parts.next(), parts.next(), parts.next())
            else {
                return Err(invalid("invalid upstream mapping list path"));
            };
            mapping::segment_path(package)?;
            if !file
                .strip_suffix(".jsonl")
                .is_some_and(|revision| mapping::hex(revision, 40))
            {
                return Err(invalid("invalid upstream mapping list name"));
            }
            let path = cache.join("revisions").join(package).join(file);
            if !path.exists() {
                fs::create_dir_all(cache.join("revisions").join(package))
                    .operation("create upstream mapping directory")?;
                fs::write(&path, text).operation("write upstream mapping list")?;
            }
        }
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

    use std::sync::Arc;

    const SCOPE: &str = "linux-x86_64/t";

    fn fetch(
        kv: &Arc<dyn Kv>,
        scope: &str,
        repository: &str,
        commit: &str,
        commits: &[String],
        cache: &Path,
    ) -> Result<Option<Staged>> {
        find(&**kv, scope, repository, commit, commits)?
            .map(|remote| remote.fetch(&**kv, scope, repository, cache))
            .transpose()
    }
    const REPOSITORY: &str = "o/r";
    const OUTPUT: &str = "outputs/p/00000000000000aa.json";

    fn commit(n: u8) -> String {
        format!("{n:040}")
    }

    /// A publisher's Lake cache with one unpacked module, its archive and a
    /// native object, and the receipt of the build that checked them.
    fn publisher() -> (tempfile::TempDir, Project, Receipt) {
        let dir = tempfile::tempdir().unwrap();
        let files: [(&str, &[u8]); 9] = [
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
            ("cache/artifacts/0000000000000005", b"executable"),
            (
                "cache/outputs/p/00000000000000cc.json",
                br#"{"service": null, "data": "0000000000000005"}"#,
            ),
            (
                "record/packages/p.jsonl",
                b"\"2026-03-17\"\n[\"00000000000000aa\",\"0000000000000003.ltar\"]\n[\"00000000000000bb\",\"0000000000000004.o\"]\n[\"00000000000000cc\",\"0000000000000005\"]\n",
            ),
        ];
        for (name, bytes) in files {
            let path = dir.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        let project = Project {
            root: dir.path().to_owned(),
            workspace: String::new(),
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
            pooled: BTreeSet::new(),
        };
        (dir, project, receipt)
    }

    #[test]
    fn a_seed_holds_every_mapping_of_a_complete_upstream_snapshot() {
        let (dir, project, receipt) = publisher();
        let cache = dir.path().join("cache");
        let snapshot = |package: &str| Snapshot {
            package: package.into(),
            revision: "1".repeat(40),
            scope: "t/upstream".into(),
        };
        let files: [(&str, &[u8]); 6] = [
            // Lake fetched only one artifact of the snapshot.
            ("artifacts/00000000000000f2.o", b"object"),
            (
                "outputs/mathlib/00000000000000e1.json",
                br#"{"service": "upstream", "data": "00000000000000f1.ltar"}"#,
            ),
            (
                "outputs/mathlib/00000000000000e2.json",
                br#"{"service": "upstream", "data": "00000000000000f2.o"}"#,
            ),
            (
                "revisions/mathlib/1111111111111111111111111111111111111111.jsonl",
                b"\"2026-03-17\"\n[\"00000000000000e1\",\"00000000000000f1.ltar\"]\n[\"00000000000000e2\",\"00000000000000f2.o\"]\n",
            ),
            // A snapshot one of whose mappings is not here is not a seed's.
            (
                "outputs/cslib/00000000000000e3.json",
                br#"{"service": "upstream", "data": "00000000000000f3.ltar"}"#,
            ),
            (
                "revisions/cslib/1111111111111111111111111111111111111111.jsonl",
                b"\"2026-03-17\"\n[\"00000000000000e3\",\"00000000000000f3.ltar\"]\n[\"00000000000000e4\",\"00000000000000f4.ltar\"]\n",
            ),
        ];
        for (name, bytes) in files {
            let path = cache.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        let (members, covered) = contents(
            &project,
            &receipt,
            vec![snapshot("mathlib"), snapshot("cslib")],
            &cache,
        )
        .unwrap();
        assert_eq!(covered, [snapshot("mathlib")]);
        let names = members
            .iter()
            .map(|m| m.name.as_str())
            .collect::<BTreeSet<_>>();
        for name in [
            "outputs/mathlib/00000000000000e1.json",
            "outputs/mathlib/00000000000000e2.json",
            "artifacts/00000000000000f2.o",
        ] {
            assert!(names.contains(name), "{name}");
        }
        assert!(!names.contains("artifacts/00000000000000f1.ltar"));
        let kv = MemoryKv::default().into_dyn();
        put(
            &*kv,
            SCOPE,
            REPOSITORY,
            &commit(1),
            members,
            &covered,
            &cache,
        )
        .unwrap();
        let remote = find(&*kv, SCOPE, REPOSITORY, &commit(1), &[commit(1)])
            .unwrap()
            .unwrap();
        assert_eq!(remote.upstream, BTreeSet::from([snapshot("mathlib")]));
        let host = tempfile::tempdir().unwrap();
        let seeded = remote
            .fetch(&*kv, SCOPE, REPOSITORY, host.path())
            .unwrap()
            .install(host.path())
            .unwrap();
        assert_eq!(seeded["mathlib"].len(), 2);
        // The host keeps the snapshot's mapping list, so a seed it publishes
        // still holds the snapshot.
        let list = "revisions/mathlib/1111111111111111111111111111111111111111.jsonl";
        assert_eq!(
            fs::read(host.path().join(list)).unwrap(),
            fs::read(cache.join(list)).unwrap()
        );
        assert!(!host.path().join("revisions/cslib").exists());
        // A publish racing the reader leaves another commit's record.
        let key = format!(
            "{}/upstream.json",
            prefix(SCOPE, REPOSITORY, remote.slot).unwrap()
        );
        let record = Upstream {
            commit: commit(2),
            snapshots: vec![snapshot("mathlib")],
        };
        kv.put(key.as_bytes(), Bytes::from(encode(&record).unwrap()))
            .unwrap();
        let raced = find(&*kv, SCOPE, REPOSITORY, &commit(1), &[commit(1)])
            .unwrap()
            .unwrap();
        assert!(raced.upstream.is_empty());
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
        let seeded = fetch(
            &kv,
            SCOPE,
            REPOSITORY,
            &commit(1),
            &[commit(1)],
            host.path(),
        )
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
        // Executables stay in the pool.
        assert!(!host.path().join("artifacts/0000000000000005").exists());
        assert!(!host.path().join("outputs/p/00000000000000cc.json").exists());
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
        let seeded = fetch(
            &kv,
            SCOPE,
            REPOSITORY,
            &commit(1),
            &[commit(1)],
            host.path(),
        )
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
        assert!(
            fetch(
                &kv,
                SCOPE,
                REPOSITORY,
                &commit(3),
                &[commit(3)],
                host.path()
            )
            .is_err()
        );
    }

    #[test]
    fn a_restore_takes_the_nearest_seed_in_its_history_else_the_newest() {
        let slots = |seeds: &[(u8, u64)]| {
            seeds
                .iter()
                .zip(SLOTS)
                .map(|((n, published), slot)| {
                    let manifest = Manifest {
                        commit: commit(*n),
                        published: *published,
                        chunks: Vec::new(),
                    };
                    (slot, Some(manifest))
                })
                .collect::<Vec<_>>()
        };
        let chosen = |seeds: &[(u8, u64)], commits: &[u8]| {
            let commits = commits.iter().map(|n| commit(*n)).collect::<Vec<_>>();
            choose(slots(seeds), &commits).map(|(_, manifest)| manifest.commit)
        };
        let seeds = [(3, 30), (5, 50)];
        // Its own seed, else the nearest seeded ancestor, though older.
        assert_eq!(chosen(&seeds, &[5, 4, 3]), Some(commit(5)));
        assert_eq!(chosen(&seeds, &[4, 3]), Some(commit(3)));
        assert_eq!(chosen(&seeds, &[6, 5, 3]), Some(commit(5)));
        // No seeded ancestor: the newest seed, of a later commit.
        assert_eq!(chosen(&seeds, &[2, 1]), Some(commit(5)));
        assert_eq!(chosen(&[(5, 50), (3, 30)], &[2, 1]), Some(commit(5)));
        assert_eq!(chosen(&[], &[2, 1]), None);

        let kv = MemoryKv::default().into_dyn();
        let host = tempfile::tempdir().unwrap();
        assert!(
            fetch(
                &kv,
                SCOPE,
                REPOSITORY,
                &commit(2),
                &[commit(2)],
                host.path()
            )
            .unwrap()
            .is_none()
        );
        publish_at(&*kv, 3);
        let seeded = fetch(
            &kv,
            SCOPE,
            REPOSITORY,
            &commit(2),
            &[commit(2)],
            host.path(),
        )
        .unwrap()
        .unwrap()
        .install(host.path())
        .unwrap();
        assert_eq!(seeded["p"].len(), 2);
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
