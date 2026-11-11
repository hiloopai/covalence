//! Eviction of the toolchain scopes a host stopped using.
//!
//! Lake keeps every scope's artifacts and output records in one cache
//! directory, so a scope is not a directory to delete. Each command records
//! the scope it uses, with the project's `lean-toolchain`, under
//! `covalence/scopes/`. `gc` keeps the scopes used within [`DAYS`] days, the
//! most recently used one, and every scope of a toolchain one of those uses.
//! For the rest it deletes:
//!
//! - the output records (`outputs/<package>/<input>.json`) that only their
//!   installed maps, or the upstream snapshots of their toolchains, name;
//! - the artifacts those records and entries name that no remaining record
//!   and no kept map or snapshot names;
//! - their installed roots, their snapshots' markers and mappings, and their
//!   use records;
//! - the Lean release archives kept for toolchains none of the kept scopes
//!   uses (`covalence/toolchains/<elan's name>/`).
//!
//! Records of local builds belong to no map, so they and their artifacts
//! stay. Every other command holds the cache lock shared while it runs, and
//! `gc` runs only when it can take the lock exclusively, so it never deletes
//! what a running command uses.

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, File, OpenOptions, TryLockError},
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use covalence_cas_prolly::Map;
use covalence_lib_cid::{Bdasl, Cid};
use covalence_lib_serde::Serialize;
use serde_json::Value;

use crate::{
    error::{Error, IoResult, Result, encode, invalid, json},
    local::Local,
    mapping, pool,
    project::package_name,
};

/// Days a scope stays after its last use: 14 unless set.
pub(crate) const DAYS: &str = "COVALENCE_LAKE_GC_DAYS";
/// `get` collects afterwards when the cache's file system has less than this
/// percentage free: 10 unless set; 0 never.
pub(crate) const FREE: &str = "COVALENCE_LAKE_GC_FREE_PERCENT";

/// The local layer's per-scope root directories.
const LEVELS: [&str; 2] = ["mapped", "installed"];

/// Held shared by a command for its whole run.
pub(crate) struct InUse {
    _lock: File,
}

fn lock_file(directory: &Path) -> Result<File> {
    let state = directory.join("covalence");
    fs::create_dir_all(&state).operation("create local pool directory")?;
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(state.join("lock"))
        .operation("open the Lake cache lock")
}

/// Marks the cache in `directory` in use until the result drops.
pub(crate) fn in_use(directory: &Path) -> Result<InUse> {
    let lock = lock_file(directory)?;
    lock.lock_shared().operation("lock the Lake cache")?;
    Ok(InUse { _lock: lock })
}

/// Records that this host uses `scope`, for a project pinning `toolchain`.
pub(crate) fn record_use(directory: &Path, scope: &str, toolchain: Option<&str>) -> Result<()> {
    mapping::segment_path(scope)?;
    let path = directory.join("covalence").join("scopes").join(scope);
    let parent = path
        .parent()
        .ok_or_else(|| invalid("scope record has no parent"))?;
    fs::create_dir_all(parent).operation("create scope record directory")?;
    fs::write(&path, toolchain.unwrap_or_default()).operation("record the scope's use")
}

fn setting(name: &str, default: u64) -> Result<u64> {
    match env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|_| invalid(format!("{name} must be a whole number, not {value:?}"))),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(env::VarError::NotUnicode(_)) => Err(invalid(format!("{name} is not UTF-8"))),
    }
}

/// `cov-lake-cache gc`: collect the scopes unused for [`DAYS`] days.
pub(crate) fn collect(directory: &Path) -> Result<()> {
    let days = setting(DAYS, 14)?;
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(days.saturating_mul(86_400)))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    match sweep(directory, cutoff)? {
        Some(report) => println!(
            "Lake cache gc: {}",
            String::from_utf8_lossy(&encode(&report)?)
        ),
        None => println!("Lake cache gc: skipped while another cov-lake-cache command runs"),
    }
    Ok(())
}

/// After a restore, [`collect`] if the disk is fuller than [`FREE`] allows.
pub(crate) fn after_restore(directory: &Path) -> Result<()> {
    let percent = setting(FREE, 10)?;
    if percent == 0 {
        return Ok(());
    }
    let disk = rustix::fs::statvfs(directory).map_err(|error| Error::Io {
        operation: "read the cache's free space".into(),
        source: error.into(),
    })?;
    if disk.f_bavail.saturating_mul(100) >= disk.f_blocks.saturating_mul(percent) {
        return Ok(());
    }
    collect(directory)
}

#[derive(Debug, Serialize)]
#[serde(crate = "covalence_lib_serde::serde")]
struct Report {
    kept: Vec<String>,
    deleted: Vec<String>,
    snapshots: usize,
    records: usize,
    artifacts: usize,
    /// Bytes of the artifacts unlinked; worktrees that hard-link them keep
    /// theirs until they go too.
    bytes: u64,
    seconds: f64,
}

struct Scope {
    used: SystemTime,
    toolchain: Option<String>,
}

/// An upstream snapshot a restore installed.
struct Snapshot {
    marker: PathBuf,
    used: SystemTime,
    package: String,
    toolchain: String,
    /// Lake's mapping file for the snapshot.
    mapping: PathBuf,
}

/// The output records, artifacts and snapshot mappings some scopes reach.
#[derive(Default)]
struct Reach {
    inputs: BTreeSet<(String, String)>,
    names: BTreeSet<String>,
    mappings: BTreeSet<PathBuf>,
}

impl Reach {
    fn scope(&mut self, local: &Local, scope: &str) -> Result<()> {
        let mut roots = BTreeSet::new();
        for level in LEVELS {
            let directory = local.directory().join("covalence").join(level).join(scope);
            for (path, package) in children(&directory)? {
                let text = fs::read_to_string(&path).operation("read installed root")?;
                let root: Cid<Bdasl> = text.trim().parse().map_err(|_| {
                    invalid(format!("invalid installed root in {}", path.display()))
                })?;
                roots.insert((package, root));
            }
        }
        for (package, root) in roots {
            for (input, entry) in pool::entries(&Map::open(local.nodes(), root))? {
                self.names.extend(entry.artifacts.into_keys());
                self.inputs.insert((package.clone(), input));
            }
        }
        Ok(())
    }

    fn snapshot(&mut self, snapshot: &Snapshot) -> Result<()> {
        if !snapshot.mapping.is_file() {
            return Ok(());
        }
        for (input, (_, names)) in mapping::rows(&snapshot.mapping)? {
            self.inputs.insert((snapshot.package.clone(), input));
            self.names.extend(names);
        }
        self.mappings.insert(snapshot.mapping.clone());
        Ok(())
    }
}

/// The entries of `directory` with their names; none if it is missing.
pub(crate) fn children(directory: &Path) -> Result<Vec<(PathBuf, String)>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(Error::Io {
                operation: format!("list {}", directory.display()),
                source,
            });
        }
    };
    entries
        .map(|entry| {
            let entry = entry.operation(format!("list {}", directory.display()))?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| invalid(format!("non-UTF-8 name in {}", directory.display())))?;
            Ok((entry.path(), name))
        })
        .collect()
}

fn modified(path: &Path) -> Result<SystemTime> {
    path.metadata()
        .and_then(|metadata| metadata.modified())
        .operation(format!("read the age of {}", path.display()))
}

/// The scopes this host used, by name, with when it last did. A scope with
/// installed roots but no use record, from a host that predates the records,
/// gets one now: its clock starts at its first collection.
fn scopes(directory: &Path) -> Result<BTreeMap<String, Scope>> {
    let state = directory.join("covalence");
    let records = state.join("scopes");
    for level in LEVELS {
        for (platform, platform_name) in children(&state.join(level))? {
            for (_, hash) in children(&platform)? {
                let scope = format!("{platform_name}/{hash}");
                if !records.join(&scope).exists() {
                    record_use(directory, &scope, None)?;
                }
            }
        }
    }
    let mut scopes = BTreeMap::new();
    for (platform, platform_name) in children(&records)? {
        for (path, hash) in children(&platform)? {
            let toolchain = fs::read_to_string(&path).operation("read scope record")?;
            let toolchain = Some(toolchain.trim().to_owned()).filter(|t| !t.is_empty());
            scopes.insert(
                format!("{platform_name}/{hash}"),
                Scope {
                    used: modified(&path)?,
                    toolchain,
                },
            );
        }
    }
    Ok(scopes)
}

/// The upstream snapshots this host installed, from their markers.
fn snapshots(directory: &Path) -> Result<Vec<Snapshot>> {
    let mut snapshots = Vec::new();
    for level in LEVELS {
        let markers = directory.join("covalence").join("upstream").join(level);
        for (marker, _) in children(&markers)? {
            let text = fs::read_to_string(&marker).operation("read upstream marker")?;
            let malformed = || invalid(format!("malformed upstream marker {}", marker.display()));
            let [name, revision, scope] = text.lines().collect::<Vec<_>>()[..] else {
                return Err(malformed());
            };
            // The request's scope is `<lean-toolchain>/<upstream repository>`.
            let (toolchain, _) = scope.rsplit_once('/').ok_or_else(malformed)?;
            let package = package_name(name).to_owned();
            mapping::segment_path(&package)?;
            if !mapping::hex(revision, 40) {
                return Err(malformed());
            }
            snapshots.push(Snapshot {
                used: modified(&marker)?,
                mapping: directory
                    .join("revisions")
                    .join(&package)
                    .join(format!("{revision}.jsonl")),
                marker,
                package,
                toolchain: toolchain.to_owned(),
            });
        }
    }
    Ok(snapshots)
}

/// The artifact names an output record names.
fn record_names(bytes: &[u8], path: &Path) -> Result<Vec<String>> {
    let record: Value = json(bytes)?;
    let data = record
        .get("data")
        .ok_or_else(|| invalid(format!("Lake output record {} has no data", path.display())))?;
    mapping::names(data)
}

/// Every artifact name the output records under `outputs` name.
fn live_names(outputs: &Path) -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    for (package, _) in children(outputs)? {
        for (path, _) in children(&package)? {
            let bytes = fs::read(&path).operation("read Lake output record")?;
            names.extend(record_names(&bytes, &path)?);
        }
    }
    Ok(names)
}

/// Delete `path`, if it is there; its size, if it was.
fn remove(path: &Path) -> Result<Option<u64>> {
    let size = match path.symlink_metadata() {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(Error::Io {
                operation: format!("read {}", path.display()),
                source,
            });
        }
    };
    match fs::remove_file(path) {
        Ok(()) => Ok(Some(size)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Io {
            operation: format!("delete {}", path.display()),
            source,
        }),
    }
}

fn remove_dir(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(Error::Io {
            operation: format!("delete {}", path.display()),
            source: error,
        }),
        _ => Ok(()),
    }
}

/// Collect the scopes last used before `cutoff`; `None` while another
/// command holds the cache.
fn sweep(directory: &Path, cutoff: SystemTime) -> Result<Option<Report>> {
    let lock = lock_file(directory)?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(None),
        Err(TryLockError::Error(source)) => {
            return Err(Error::Io {
                operation: "lock the Lake cache".into(),
                source,
            });
        }
    }
    let started = Instant::now();
    let local = Local::at(directory.to_owned())?;
    let state = directory.join("covalence");
    let scopes = scopes(directory)?;
    let newest = scopes
        .iter()
        .max_by_key(|(_, scope)| scope.used)
        .map(|(name, _)| name.clone());
    let recent =
        |name: &String, scope: &Scope| scope.used >= cutoff || Some(name) == newest.as_ref();
    let toolchains = scopes
        .iter()
        .filter(|(name, scope)| recent(name, scope))
        .filter_map(|(_, scope)| scope.toolchain.clone())
        .collect::<BTreeSet<_>>();
    let (kept, deleted): (BTreeMap<_, _>, BTreeMap<_, _>) =
        scopes.into_iter().partition(|(name, scope)| {
            recent(name, scope)
                || scope
                    .toolchain
                    .as_ref()
                    .is_some_and(|toolchain| toolchains.contains(toolchain))
        });
    // A kept scope whose toolchain is unknown may use any snapshot.
    let unknown = kept.values().any(|scope| scope.toolchain.is_none());
    let (kept_snapshots, dead_snapshots): (Vec<_>, Vec<_>) =
        snapshots(directory)?.into_iter().partition(|snapshot| {
            unknown || snapshot.used >= cutoff || toolchains.contains(&snapshot.toolchain)
        });

    // Kept Lean archives of toolchains no kept scope uses.
    if !unknown {
        let wanted = toolchains
            .iter()
            .filter_map(|toolchain| crate::toolchain::elan_directory(toolchain))
            .collect::<BTreeSet<_>>();
        for (path, name) in children(&crate::toolchain::archives(directory))? {
            if !wanted.contains(&name) {
                remove_dir(&path)?;
            }
        }
    }

    let mut keep = Reach::default();
    for scope in kept.keys() {
        keep.scope(&local, scope)?;
    }
    for snapshot in &kept_snapshots {
        keep.snapshot(snapshot)?;
    }
    let mut lose = Reach::default();
    for scope in deleted.keys() {
        lose.scope(&local, scope)?;
    }
    for snapshot in &dead_snapshots {
        lose.snapshot(snapshot)?;
    }

    // Records first, so an artifact goes only when no record names it.
    let outputs = directory.join("outputs");
    let mut candidates = lose.names;
    let mut records = 0;
    for (package, input) in lose.inputs.difference(&keep.inputs) {
        let path = outputs.join(package).join(format!("{input}.json"));
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(Error::Io {
                    operation: format!("read {}", path.display()),
                    source,
                });
            }
        };
        candidates.extend(record_names(&bytes, &path)?);
        if remove(&path)?.is_some() {
            records += 1;
        }
    }
    let live = live_names(&outputs)?;
    let artifacts_directory = directory.join("artifacts");
    let (mut artifacts, mut bytes) = (0, 0);
    for name in candidates
        .iter()
        .filter(|name| !live.contains(*name) && !keep.names.contains(*name))
    {
        if let Some(size) = remove(&artifacts_directory.join(name))? {
            artifacts += 1;
            bytes += size;
        }
    }
    for snapshot in &dead_snapshots {
        if !keep.mappings.contains(&snapshot.mapping) {
            remove(&snapshot.mapping)?;
        }
        remove(&snapshot.marker)?;
    }
    for scope in deleted.keys() {
        for level in LEVELS {
            remove_dir(&state.join(level).join(scope))?;
        }
        remove(&state.join("scopes").join(scope))?;
    }
    Ok(Some(Report {
        kept: kept.into_keys().collect(),
        deleted: deleted.into_keys().collect(),
        snapshots: dead_snapshots.len(),
        records,
        artifacts,
        bytes,
        seconds: started.elapsed().as_secs_f64(),
    }))
}

#[cfg(test)]
mod tests {
    use std::fs::FileTimes;

    use covalence_lib_cid::Sha256;

    use super::*;
    use crate::{local::Installed, pool::Entry};

    const OLD: &str =
        "linux-x86_64/1111111111111111111111111111111111111111111111111111111111111111";
    const NEW: &str =
        "linux-x86_64/2222222222222222222222222222222222222222222222222222222222222222";
    const DAY: Duration = Duration::from_secs(86_400);

    struct Cache {
        _directory: tempfile::TempDir,
        path: PathBuf,
        local: Local,
    }

    impl Cache {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().to_owned();
            let local = Local::at(path.clone()).unwrap();
            Self {
                _directory: directory,
                path,
                local,
            }
        }

        fn age(&self, path: impl AsRef<Path>, days: u32) {
            let time = SystemTime::now() - DAY * days;
            File::options()
                .write(true)
                .open(self.path.join(path))
                .unwrap()
                .set_times(FileTimes::new().set_modified(time))
                .unwrap();
        }

        /// A scope used `days` ago whose map gives `package` the record
        /// `input`, naming `artifacts`, all written to the cache.
        fn scope(&self, scope: &str, toolchain: &str, days: u32, inputs: &[(&str, &[&str])]) {
            record_use(&self.path, scope, Some(toolchain)).unwrap();
            self.age(format!("covalence/scopes/{scope}"), days);
            let mut entries = BTreeMap::new();
            for (input, artifacts) in inputs {
                entries.insert(input.to_string(), self.record("pkg", input, artifacts));
            }
            let root = pool::build(self.local.nodes(), &entries).unwrap().root();
            for level in [Installed::Mappings, Installed::Artifacts] {
                self.local.set_installed(level, scope, "pkg", root).unwrap();
            }
            for level in LEVELS {
                self.age(format!("covalence/{level}/{scope}/pkg"), days);
            }
        }

        /// The output record of `input` and its artifacts, as an entry.
        fn record(&self, package: &str, input: &str, artifacts: &[&str]) -> Entry {
            let descriptor = serde_json::json!({ "o": artifacts }).to_string();
            let records = self.path.join("outputs").join(package);
            fs::create_dir_all(&records).unwrap();
            fs::write(
                records.join(format!("{input}.json")),
                format!("{{\"service\": null, \"schemaVersion\": \"2026-02-25\", \"data\": {descriptor}}}"),
            )
            .unwrap();
            for artifact in artifacts {
                fs::create_dir_all(self.path.join("artifacts")).unwrap();
                fs::write(self.path.join("artifacts").join(artifact), artifact).unwrap();
            }
            Entry {
                output: descriptor,
                artifacts: artifacts
                    .iter()
                    .map(|name| (name.to_string(), Sha256::of(name.as_bytes())))
                    .collect(),
            }
        }

        /// An upstream snapshot of `package` for `toolchain`, installed
        /// `days` ago, mapping `input` to `artifact`.
        fn snapshot(&self, package: &str, toolchain: &str, days: u32, input: &str, artifact: &str) {
            let revision = format!("{:0>40}", days);
            let request = [package, &revision, &format!("{toolchain}/upstream")];
            self.local
                .set_upstream_installed(Installed::Artifacts, &request)
                .unwrap();
            let markers = self.path.join("covalence/upstream/installed");
            for (marker, _) in children(&markers).unwrap() {
                if fs::read_to_string(&marker).unwrap().contains(&revision) {
                    self.age(&marker, days);
                }
            }
            let mappings = self.path.join("revisions").join(package);
            fs::create_dir_all(&mappings).unwrap();
            fs::write(
                mappings.join(format!("{revision}.jsonl")),
                format!("\"{}\"\n[\"{input}\",\"{artifact}\"]\n", mapping::SCHEMA),
            )
            .unwrap();
            self.record(package, input, &[artifact]);
        }

        fn has(&self, path: &str) -> bool {
            self.path.join(path).exists()
        }
    }

    const OLD_TOOLCHAIN: &str = "leanprover/lean4:v4.1.0";
    const NEW_TOOLCHAIN: &str = "leanprover/lean4:v4.2.0";

    fn cutoff() -> SystemTime {
        SystemTime::now() - DAY * 14
    }

    #[test]
    fn collects_only_what_stale_scopes_alone_reach() {
        let cache = Cache::new();
        cache.scope(
            OLD,
            OLD_TOOLCHAIN,
            30,
            &[
                (
                    "000000000000000a",
                    &["000000000000000a.olean", "00000000000000ff.c"],
                ),
                ("000000000000000b", &["000000000000000b.olean"]),
            ],
        );
        cache.scope(
            NEW,
            NEW_TOOLCHAIN,
            1,
            // Shares an artifact with the old scope.
            &[(
                "000000000000000c",
                &["000000000000000c.olean", "00000000000000ff.c"],
            )],
        );
        // A local build's record, in no map.
        cache.record("pkg", "000000000000000d", &["000000000000000d.olean"]);
        cache.snapshot(
            "mathlib",
            OLD_TOOLCHAIN,
            30,
            "00000000000000e1",
            "00000000000000e1.ltar",
        );
        cache.snapshot(
            "mathlib",
            NEW_TOOLCHAIN,
            20,
            "00000000000000e2",
            "00000000000000e2.ltar",
        );

        let report = sweep(&cache.path, cutoff()).unwrap().unwrap();

        assert_eq!(report.kept, [NEW]);
        assert_eq!(report.deleted, [OLD]);
        assert_eq!(
            (report.snapshots, report.records, report.artifacts),
            (1, 3, 3)
        );
        for gone in [
            "outputs/pkg/000000000000000a.json",
            "outputs/pkg/000000000000000b.json",
            "outputs/mathlib/00000000000000e1.json",
            "artifacts/000000000000000a.olean",
            "artifacts/000000000000000b.olean",
            "artifacts/00000000000000e1.ltar",
            &format!("covalence/mapped/{OLD}"),
            &format!("covalence/installed/{OLD}"),
            &format!("covalence/scopes/{OLD}"),
            &format!("revisions/mathlib/{:0>40}.jsonl", 30),
        ] {
            assert!(!cache.has(gone), "{gone} remains");
        }
        for kept in [
            "outputs/pkg/000000000000000c.json",
            "outputs/pkg/000000000000000d.json",
            "outputs/mathlib/00000000000000e2.json",
            "artifacts/000000000000000c.olean",
            "artifacts/000000000000000d.olean",
            "artifacts/00000000000000ff.c",
            "artifacts/00000000000000e2.ltar",
            &format!("covalence/installed/{NEW}/pkg"),
            &format!("covalence/scopes/{NEW}"),
            &format!("revisions/mathlib/{:0>40}.jsonl", 20),
        ] {
            assert!(cache.has(kept), "{kept} is gone");
        }
        assert!(
            cache
                .local
                .installed(Installed::Mappings, NEW, "pkg")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn the_last_scope_used_stays_however_old() {
        let cache = Cache::new();
        cache.scope(
            OLD,
            OLD_TOOLCHAIN,
            60,
            &[("000000000000000a", &["000000000000000a.olean"])],
        );
        cache.scope(
            NEW,
            NEW_TOOLCHAIN,
            30,
            &[("000000000000000c", &["000000000000000c.olean"])],
        );
        let report = sweep(&cache.path, cutoff()).unwrap().unwrap();
        assert_eq!(
            (report.kept, report.deleted),
            (vec![NEW.to_owned()], vec![OLD.to_owned()])
        );
        assert!(cache.has("artifacts/000000000000000c.olean"));
        assert!(!cache.has("artifacts/000000000000000a.olean"));
    }

    #[test]
    fn kept_lean_archives_go_with_their_toolchains() {
        let cache = Cache::new();
        cache.scope(OLD, OLD_TOOLCHAIN, 30, &[]);
        cache.scope(NEW, NEW_TOOLCHAIN, 1, &[]);
        for name in ["leanprover--lean4---v4.1.0", "leanprover--lean4---v4.2.0"] {
            let kept = cache.path.join("covalence/toolchains").join(name);
            fs::create_dir_all(&kept).unwrap();
            fs::write(kept.join("archive.tar.zst"), name).unwrap();
        }
        sweep(&cache.path, cutoff()).unwrap().unwrap();
        assert!(!cache.has("covalence/toolchains/leanprover--lean4---v4.1.0"));
        assert!(cache.has("covalence/toolchains/leanprover--lean4---v4.2.0/archive.tar.zst"));
    }

    #[test]
    fn a_stale_scope_of_a_toolchain_in_use_stays() {
        let cache = Cache::new();
        cache.scope(
            OLD,
            NEW_TOOLCHAIN,
            30,
            &[("000000000000000a", &["000000000000000a.olean"])],
        );
        cache.scope(
            NEW,
            NEW_TOOLCHAIN,
            1,
            &[("000000000000000c", &["000000000000000c.olean"])],
        );
        let report = sweep(&cache.path, cutoff()).unwrap().unwrap();
        assert!(report.deleted.is_empty());
        assert!(cache.has("artifacts/000000000000000a.olean"));
    }

    #[test]
    fn a_scope_without_a_use_record_starts_its_clock() {
        let cache = Cache::new();
        cache.scope(
            OLD,
            OLD_TOOLCHAIN,
            30,
            &[("000000000000000a", &["000000000000000a.olean"])],
        );
        cache.scope(
            NEW,
            NEW_TOOLCHAIN,
            1,
            &[("000000000000000c", &["000000000000000c.olean"])],
        );
        cache.snapshot(
            "mathlib",
            OLD_TOOLCHAIN,
            30,
            "00000000000000e1",
            "00000000000000e1.ltar",
        );
        fs::remove_file(cache.path.join("covalence/scopes").join(OLD)).unwrap();
        let report = sweep(&cache.path, cutoff()).unwrap().unwrap();
        assert!(report.deleted.is_empty());
        assert_eq!(report.snapshots, 0);
        assert!(cache.has("artifacts/000000000000000a.olean"));
        assert!(cache.has("artifacts/00000000000000e1.ltar"));
        assert!(cache.has(&format!("covalence/scopes/{OLD}")));
    }

    #[test]
    fn a_running_command_blocks_collection() {
        let cache = Cache::new();
        cache.scope(
            OLD,
            OLD_TOOLCHAIN,
            30,
            &[("000000000000000a", &["000000000000000a.olean"])],
        );
        cache.scope(
            NEW,
            NEW_TOOLCHAIN,
            1,
            &[("000000000000000c", &["000000000000000c.olean"])],
        );
        let running = in_use(&cache.path).unwrap();
        assert!(sweep(&cache.path, cutoff()).unwrap().is_none());
        assert!(cache.has("artifacts/000000000000000a.olean"));
        drop(running);
        assert!(sweep(&cache.path, cutoff()).unwrap().is_some());
        assert!(!cache.has("artifacts/000000000000000a.olean"));
    }
}
