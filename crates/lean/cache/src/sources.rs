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
    path::Path,
    process::Command,
    time::Instant,
};

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
pub(crate) fn materialize(project: &Project) -> Result<()> {
    match host_cache_dir() {
        Some(cache) => materialize_in(project, &cache),
        None => Ok(()),
    }
}

fn materialize_in(project: &Project, cache: &Path) -> Result<()> {
    let manifest_path = project.root.join("lake-manifest.json");
    if !manifest_path.exists() {
        return Ok(());
    }
    let manifest: serde_json::Value =
        json(&fs::read(&manifest_path).operation("read Lake manifest")?)?;
    let packages = project
        .root
        .join(manifest["packagesDir"].as_str().unwrap_or(".lake/packages"));
    let mirrors = cache.join("covalence/git");
    let mut checkouts = Vec::new();
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
        let destination = packages.join(name);
        // Lake already clones local repositories without the network.
        if destination.exists() || !url.contains("://") {
            continue;
        }
        let mirror = mirrors.join(format!("{}.git", repository_of(url)?));
        checkouts.push((name, url, commit, destination, mirror));
    }
    if checkouts.is_empty() {
        return Ok(());
    }
    let started = Instant::now();
    // Several packages can share a mirror, so each mirror's fetches run one
    // at a time; different mirrors fetch at once.
    let mut by_mirror = BTreeMap::<&Path, BTreeMap<&str, &str>>::new();
    for (_, url, commit, _, mirror) in &checkouts {
        by_mirror.entry(mirror).or_default().insert(commit, url);
    }
    let by_mirror = by_mirror.into_iter().collect::<Vec<_>>();
    let fetched = storage::parallel(&by_mirror, |_, (mirror, commits)| {
        let mut fetched = 0;
        for (commit, url) in commits {
            fetched += usize::from(fetch(mirror, url, commit)?);
        }
        Ok(fetched)
    })?
    .into_iter()
    .sum::<usize>();
    let mirrored = started.elapsed();
    // Checkouts write disjoint directories and only read their mirrors.
    storage::parallel(&checkouts, |_, (name, url, commit, destination, mirror)| {
        check_out(name, url, commit, destination, mirror)
    })?;
    println!(
        "Lake cache: checked out {} Git dependencies in {:.1} s, after fetching {fetched} commits into {} host mirrors in {:.1} s",
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

/// Make `mirror` hold `commit`, fetching only that commit from `url`;
/// whether it fetched.
fn fetch(mirror: &Path, url: &str, commit: &str) -> Result<bool> {
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
    if !present {
        run(
            "git fetch",
            git(mirror).args(["fetch", "-q", "--depth", "1", url, commit]),
        )?;
        // The commit must now be present: a fetch that names a different
        // object would leave the checkout wrong.
        output(
            "git",
            git(mirror).args(["rev-parse", "--verify", &format!("{commit}^{{commit}}")]),
        )?;
    }
    Ok(!present)
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
        let checkout = |name: &str| {
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
                repository: "o/r".into(),
                record: root.join("record"),
            };
            materialize_in(&project, &cache).unwrap();
            root.join(".lake/packages/dep-a")
        };
        let first = checkout("first");
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
        // A second checkout needs no upstream at all.
        fs::remove_dir_all(&upstream).unwrap();
        let second = checkout("second");
        assert_eq!(git_ok(&second, &["rev-parse", "HEAD"]), commit);
    }
}
