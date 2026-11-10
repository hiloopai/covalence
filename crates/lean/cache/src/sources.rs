//! Check out Git dependencies from host-wide mirrors instead of cloning them.
//!
//! Lake clones each Git dependency into every new checkout's packages
//! directory, about 46 s for a Mathlib workspace. This keeps one shallow bare
//! mirror per repository under `<Lake cache>/covalence/git/`, fetches only
//! pinned commits it lacks, and checks each missing package out from it
//! through Git alternates, without copying objects. Lake then finds the package at its pinned revision
//! with the manifest's remote URL and fetches nothing.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Seek, Write},
    path::Path,
    process::{Command, Stdio},
    time::Instant,
};

use covalence_kv_trait::Kv;

use crate::{
    error::{IoResult, Result, invalid, json},
    mapping,
    project::{Project, host_cache_dir, output, package_name, repository_of, run},
    storage,
};

fn git(directory: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(directory);
    command
}

/// Check out every Git dependency the packages directory lacks.
pub(crate) fn materialize(project: &Project, kv: &dyn Kv) -> Result<()> {
    match host_cache_dir() {
        Some(cache) => materialize_in(project, kv, &cache),
        None => Ok(()),
    }
}

/// A Git dependency the manifest pins: its name, URL, commit and checkout.
struct Pin {
    name: String,
    url: String,
    commit: String,
    destination: std::path::PathBuf,
}

/// The manifest's Git dependencies from remote URLs. Lake clones local
/// repositories itself without the network.
fn pins(project: &Project) -> Result<Vec<Pin>> {
    let manifest_path = project.root.join("lake-manifest.json");
    if !manifest_path.exists() {
        return Ok(Vec::new());
    }
    let manifest: serde_json::Value =
        json(&fs::read(&manifest_path).operation("read Lake manifest")?)?;
    let packages = project
        .root
        .join(manifest["packagesDir"].as_str().unwrap_or(".lake/packages"));
    let mut pins = Vec::new();
    for package in manifest["packages"].as_array().into_iter().flatten() {
        if package["type"].as_str() != Some("git") {
            continue;
        }
        let (Some(name), Some(url), Some(commit)) = (
            package["name"].as_str(),
            package["url"].as_str(),
            package["rev"].as_str(),
        ) else {
            return Err(invalid(
                "Lake manifest Git package lacks a name, URL or revision",
            ));
        };
        let name = package_name(name);
        mapping::segment_path(name)?;
        if !mapping::hex(commit, 40) {
            return Err(invalid("Lake manifest pins a non-commit revision"));
        }
        if url.contains("://") {
            pins.push(Pin {
                name: name.to_owned(),
                url: url.to_owned(),
                commit: commit.to_owned(),
                destination: packages.join(name),
            });
        }
    }
    Ok(pins)
}

/// Where the pool keeps a pinned commit as one Git pack: its commit, tree
/// and blobs, so a host checks the pin out without asking the Git host.
fn pack_key(url: &str, commit: &str) -> Result<String> {
    Ok(format!("lake/git/{}/{commit}.pack", repository_of(url)?))
}

fn store_error(
    operation: &'static str,
) -> impl FnOnce(covalence_kv_trait::Error) -> crate::error::Error {
    move |source| crate::error::Error::Store {
        operation: operation.into(),
        source,
    }
}

/// Publish each pinned commit the pool lacks as one pack, from its checkout.
pub(crate) fn publish_packs(project: &Project, kv: &dyn Kv) -> Result<usize> {
    let pins = pins(project)?
        .into_iter()
        .filter(|pin| pin.destination.join(".git").exists())
        .collect::<Vec<_>>();
    let published = storage::parallel(&pins, |_, pin| {
        let key = pack_key(&pin.url, &pin.commit)?;
        if kv
            .exists(key.as_bytes())
            .map_err(store_error("check Git pack"))?
        {
            return Ok(false);
        }
        // The commit and everything its tree holds, without its history.
        let objects = git(&pin.destination)
            .args(["rev-list", "--objects", "--no-walk", &pin.commit])
            .output()
            .operation("start git rev-list")?;
        if !objects.status.success() {
            return Err(crate::error::Error::Process {
                program: "git rev-list",
                status: objects.status,
            });
        }
        let mut pack = tempfile::tempfile().operation("stage Git pack")?;
        let mut child = git(&pin.destination)
            .args(["pack-objects", "-q", "--stdout"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .operation("start git pack-objects")?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| invalid("git pack-objects has no input"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| invalid("git pack-objects has no output"))?;
        let writer = std::thread::spawn(move || stdin.write_all(&objects.stdout));
        let bytes = io::copy(&mut stdout, &mut pack).operation("read Git pack")?;
        writer
            .join()
            .map_err(|_| crate::error::Error::Worker)?
            .operation("write Git object list")?;
        let status = child.wait().operation("wait for git pack-objects")?;
        if !status.success() {
            return Err(crate::error::Error::Process {
                program: "git pack-objects",
                status,
            });
        }
        pack.rewind().operation("rewind Git pack")?;
        kv.put_from(key.as_bytes(), &mut pack, bytes)
            .map_err(store_error("publish Git pack"))?;
        Ok(true)
    })?;
    Ok(published.into_iter().filter(|published| *published).count())
}

fn materialize_in(project: &Project, kv: &dyn Kv, cache: &Path) -> Result<()> {
    let mirrors = cache.join("covalence/git");
    let mut checkouts = Vec::new();
    for pin in pins(project)? {
        if pin.destination.exists() {
            continue;
        }
        let mirror = mirrors.join(format!("{}.git", repository_of(&pin.url)?));
        checkouts.push((pin.name, pin.url, pin.commit, pin.destination, mirror));
    }
    if checkouts.is_empty() {
        return Ok(());
    }
    let started = Instant::now();
    // Several packages can share a mirror, so each mirror's fetches run one
    // at a time; different mirrors fetch at once.
    let mut by_mirror = BTreeMap::<&Path, BTreeMap<&str, &str>>::new();
    for (_, url, commit, _, mirror) in &checkouts {
        by_mirror
            .entry(mirror.as_path())
            .or_default()
            .insert(commit.as_str(), url.as_str());
    }
    let by_mirror = by_mirror.into_iter().collect::<Vec<_>>();
    let sources = storage::parallel(&by_mirror, |_, (mirror, commits)| {
        commits
            .iter()
            .map(|(commit, url)| fetch(kv, mirror, url, commit))
            .collect::<Result<Vec<_>>>()
    })?;
    let fetched = sources
        .iter()
        .flatten()
        .filter(|source| **source != Source::Mirror)
        .count();
    let pooled = sources
        .iter()
        .flatten()
        .filter(|source| **source == Source::Pool)
        .count();
    let mirrored = started.elapsed();
    // Checkouts write disjoint directories and only read their mirrors.
    storage::parallel(&checkouts, |_, (name, url, commit, destination, mirror)| {
        check_out(name, url, commit, destination, mirror)
    })?;
    println!(
        "Lake cache: checked out {} Git dependencies in {:.1} s, after fetching {fetched} commits ({pooled} from the pool) into {} host mirrors in {:.1} s",
        checkouts.len(),
        started.elapsed().as_secs_f64(),
        by_mirror.len(),
        mirrored.as_secs_f64(),
    );
    Ok(())
}

/// Check out `commit` at `destination` from `mirror`, which holds it.
fn check_out(name: &str, url: &str, commit: &str, destination: &Path, mirror: &Path) -> Result<()> {
    // A repository borrowing the mirror's objects: no copy, no network.
    fs::create_dir_all(destination).operation("create Lake package directory")?;
    run("git init", git(destination).args(["init", "-q"]))?;
    let objects = mirror
        .join("objects")
        .canonicalize()
        .operation("locate mirror objects")?;
    let git_dir = destination.join(".git");
    fs::write(
        git_dir.join("objects/info/alternates"),
        format!("{}\n", objects.display()),
    )
    .operation("link mirror objects")?;
    // Lake and Git must see the mirror's shallow boundary as their own.
    fs::copy(mirror.join("shallow"), git_dir.join("shallow")).operation("copy shallow boundary")?;
    run(
        "git remote",
        git(destination).args(["remote", "add", "origin", url]),
    )?;
    run(
        "git checkout",
        git(destination).args(["checkout", "-q", "--detach", commit]),
    )?;
    println!("Lake cache: checked out {name} at {commit} from the host mirror");
    Ok(())
}

/// Where a mirror's commit came from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    /// It was already there.
    Mirror,
    /// The pool's pack.
    Pool,
    /// The Git host.
    Remote,
}

/// Make `mirror` hold `commit`: from the pool's pack of it if there is one,
/// which Git checks object by object as it indexes it, else fetching only
/// that commit from `url`.
fn fetch(kv: &dyn Kv, mirror: &Path, url: &str, commit: &str) -> Result<Source> {
    fs::create_dir_all(mirror).operation("create Git mirror")?;
    // Worktrees on one host may restore at once; Git's own ref and shallow
    // locks fail rather than wait.
    let lock = File::create(mirror.join("cov-lake-cache.lock")).operation("create mirror lock")?;
    lock.lock().operation("lock Git mirror")?;
    if !mirror.join("HEAD").exists() {
        run("git init", git(mirror).args(["init", "-q", "--bare"]))?;
    }
    let present = git(mirror)
        .args(["cat-file", "-e", &format!("{commit}^{{commit}}")])
        .stderr(std::process::Stdio::null())
        .status()
        .operation("start git")?
        .success();
    if present {
        return Ok(Source::Mirror);
    }
    let mut pack = tempfile::tempfile().operation("stage Git pack")?;
    let found = kv
        .get_to(pack_key(url, commit)?.as_bytes(), &mut pack)
        .map_err(store_error("read Git pack"))?;
    let source = if found.is_some() {
        pack.rewind().operation("rewind Git pack")?;
        // `index-pack` hashes every object, so a pack can only add the
        // objects it names; the shallow boundary is the commit itself.
        let status = git(mirror)
            .args(["index-pack", "--stdin"])
            .stdin(pack)
            .stdout(Stdio::null())
            .status()
            .operation("start git index-pack")?;
        if !status.success() {
            return Err(crate::error::Error::Process {
                program: "git index-pack",
                status,
            });
        }
        let shallow = mirror.join("shallow");
        let mut boundary = fs::read_to_string(&shallow).unwrap_or_default();
        if !boundary.lines().any(|line| line == commit) {
            boundary.push_str(&format!("{commit}\n"));
            fs::write(&shallow, boundary).operation("record shallow boundary")?;
        }
        Source::Pool
    } else {
        run(
            "git fetch",
            git(mirror).args(["fetch", "-q", "--depth", "1", url, commit]),
        )?;
        Source::Remote
    };
    // The commit must now be present: a fetch or pack that names a different
    // object would leave the checkout wrong.
    output(
        "git",
        git(mirror).args(["rev-parse", "--verify", &format!("{commit}^{{commit}}")]),
    )?;
    Ok(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_ok(directory: &Path, args: &[&str]) -> String {
        output(
            "git",
            Command::new("git")
                .current_dir(directory)
                .args(["-c", "user.name=T", "-c", "user.email=t@example.invalid"])
                .args(args),
        )
        .unwrap()
    }

    #[test]
    fn missing_packages_come_from_one_shallow_host_mirror() {
        let temporary = tempfile::tempdir().unwrap();
        let upstream = temporary.path().join("upstream");
        fs::create_dir(&upstream).unwrap();
        git_ok(&upstream, &["init", "-q"]);
        fs::write(upstream.join("A.lean"), "-- one\n").unwrap();
        git_ok(&upstream, &["add", "."]);
        git_ok(
            &upstream,
            &["-c", "commit.gpgsign=false", "commit", "-qm", "one"],
        );
        fs::write(upstream.join("A.lean"), "-- two\n").unwrap();
        git_ok(
            &upstream,
            &["-c", "commit.gpgsign=false", "commit", "-qam", "two"],
        );
        let commit = git_ok(&upstream, &["rev-parse", "HEAD"]);
        let url = format!("file://{}", upstream.display());
        let cache = temporary.path().join("cache");
        let kv = covalence_kv_memory::MemoryKv::default();
        let checkout = |name: &str, cache: &Path| {
            let root = temporary.path().join(name);
            fs::create_dir(&root).unwrap();
            fs::write(
                root.join("lake-manifest.json"),
                format!(
                    r#"{{"name": "root", "packagesDir": ".lake/packages", "packages": [
                    {{"type": "git", "name": "«dep-a»", "url": "{url}", "rev": "{commit}"}},
                    {{"type": "path", "name": "local", "dir": "local"}}]}}"#
                ),
            )
            .unwrap();
            let project = Project {
                root: root.clone(),
                workspace: String::new(),
                repository: "o/r".into(),
                record: root.join("record"),
            };
            materialize_in(&project, &kv, cache).unwrap();
            (project, root.join(".lake/packages/dep-a"))
        };
        let (project, first) = checkout("first", &cache);
        assert_eq!(git_ok(&first, &["rev-parse", "HEAD"]), commit);
        assert_eq!(git_ok(&first, &["remote", "get-url", "origin"]), url);
        assert_eq!(
            fs::read_to_string(first.join("A.lean")).unwrap(),
            "-- two\n"
        );
        let mirror = cache
            .join("covalence/git")
            .join(format!("{}.git", repository_of(&url).unwrap()));
        // Only the pinned commit was fetched.
        assert_eq!(git_ok(&mirror, &["rev-list", "--count", &commit]), "1");
        // The pin goes to the pool as one pack of its tree.
        assert_eq!(publish_packs(&project, &kv).unwrap(), 1);
        assert_eq!(publish_packs(&project, &kv).unwrap(), 0);
        // A second checkout needs no upstream at all.
        fs::remove_dir_all(&upstream).unwrap();
        let (_, second) = checkout("second", &cache);
        assert_eq!(git_ok(&second, &["rev-parse", "HEAD"]), commit);
        // Nor does a fresh host: the pool's pack fills its mirror.
        let (_, third) = checkout("third", &temporary.path().join("fresh-cache"));
        assert_eq!(git_ok(&third, &["rev-parse", "HEAD"]), commit);
        assert_eq!(
            fs::read_to_string(third.join("A.lean")).unwrap(),
            "-- two\n"
        );
        assert_eq!(git_ok(&third, &["status", "--porcelain"]), "");
    }
}
