use std::{collections::BTreeSet, fs, path::PathBuf, sync::Arc};

use covalence_kv_trait::Kv;
use covalence_lib_cid::{Bdasl, Cid};

use crate::{
    archives,
    error::{IoResult, Result, invalid},
    local::{Installed, Local},
    mapping,
    project::{Package, Project, Source, package_name, run},
    restore::Session,
    seed::Seeded,
    storage,
};

/// Mathlib's Lake configuration naming the cache services of its snapshot.
///
/// Mappings fetched with `--mappings-only` keep naming these services, so any
/// later build that may download those artifacts needs this configuration.
pub(crate) fn config(project: &Project) -> PathBuf {
    project
        .root
        .join(".lake/packages/mathlib/.meta/lake-config.toml")
}

/// Restores each Git dependency's published build of its pin; with targets,
/// only mappings, then a build of what the targets need.
pub(crate) fn fetch(project: &Project, kv: &Arc<dyn Kv>, targets: &[String]) -> Result<()> {
    run("Lake", project.lake().args(["env", "true"]))?;
    let scope = project.identity()?.scope;
    let lazy = !targets.is_empty();
    let mut session = Session::new(project, kv, &scope)?;
    upstream(project, session.local(), lazy)?;
    let installs = pins(project, kv, &session)?;
    session.install(project, &installs, lazy, &Seeded::new())?;
    if lazy {
        session.build(project, targets)?;
    }
    Ok(())
}

/// Install the Mathlib/CSLib snapshot from upstream. A pool map holds only
/// the closures its publishers built, so upstream's snapshot comes first and
/// the pool's maps of the [`pins`] go on top of it.
pub(crate) fn upstream(project: &Project, local: &Local, lazy: bool) -> Result<()> {
    if !config(project).exists() {
        return Ok(());
    }
    let names = project
        .packages()?
        .into_iter()
        .filter(|package| matches!(package.source, Source::Git { .. }))
        .map(|package| package.name)
        .collect();
    mappings(project, local, lazy, &names)
}

/// Each Git dependency with a published build of its pin, and that build's
/// map root.
pub(crate) fn pins(
    project: &Project,
    kv: &Arc<dyn Kv>,
    session: &Session,
) -> Result<Vec<(Package, bool, Cid<Bdasl>)>> {
    let mut installs = Vec::new();
    for package in project.packages()? {
        let Source::Git { commit, .. } = &package.source else {
            continue;
        };
        let key = project.revision_key(session.scope(), &package, commit)?;
        if let Some(root) = storage::pointer(&**kv, &key)? {
            installs.push((package, false, root));
        }
    }
    Ok(installs)
}

/// Fetches the Mathlib/CSLib snapshot's mappings for `packages`, and unless
/// `mappings_only`, all of their artifacts. A snapshot this host already
/// installed is not fetched again: a pin's upstream mappings never change.
fn mappings(
    project: &Project,
    local: &Local,
    mappings_only: bool,
    packages: &BTreeSet<String>,
) -> Result<()> {
    let level = if mappings_only {
        Installed::Mappings
    } else {
        Installed::Artifacts
    };
    let snapshot = project.git(&["-C", ".lake/packages/mathlib", "rev-parse", "HEAD"])?;
    let toolchain =
        fs::read_to_string(project.root.join("lean-toolchain")).operation("read Lean toolchain")?;
    let config = config(project);
    for (name, upstream) in [
        ("mathlib", "mathlib4"),
        ("cslib", "cslib"),
        ("batteries", "batteries"),
        ("Qq", "quote4"),
        ("aesop", "aesop"),
        ("proofwidgets", "ProofWidgets4"),
        ("plausible", "plausible"),
        ("LeanSearchClient", "LeanSearchClient"),
        ("importGraph", "import-graph"),
        ("Cli", "lean4-cli"),
    ] {
        if !packages.contains(name) {
            continue;
        }
        let revision = if name == "cslib" {
            project.git(&["-C", ".lake/packages/cslib", "rev-parse", "HEAD"])?
        } else {
            snapshot.clone()
        };
        let scope = format!("{}/{upstream}", toolchain.trim());
        let request = [name, revision.as_str(), scope.as_str()];
        if local.upstream_installed(level, &request) {
            continue;
        }
        let mut command = project.lake();
        command.env("LAKE_CONFIG", &config).args([
            "cache",
            "get",
            &format!("--package={name}"),
            &format!("--rev={revision}"),
            &format!("--scope={scope}"),
        ]);
        if mappings_only {
            command.arg("--mappings-only");
        }
        run("Lake dependency cache", &mut command)?;
        if !mappings_only {
            unpack_snapshot(project, local, name, &revision)?;
        }
        local.set_upstream_installed(level, &request)?;
    }
    Ok(())
}

/// Decodes the archives of `package`'s upstream snapshot at `revision`, which
/// Lake just downloaded, and installs their outputs as its mappings, so Lake
/// unpacks none of them.
fn unpack_snapshot(project: &Project, local: &Local, package: &str, revision: &str) -> Result<()> {
    // Where Lake keeps the mappings it downloaded for a package revision
    // (lean4 nightly-2026-11-05, `Lake/Config/Cache.lean`, `revisionPath`).
    let revisions = local
        .directory()
        .join("revisions")
        .join(package_name(package))
        .join(format!("{revision}.jsonl"));
    if !revisions.is_file() {
        return Err(invalid(format!(
            "Lake kept no mappings for {package} at {}",
            revisions.display()
        )));
    }
    let outputs = mapping::rows(&revisions)?
        .into_iter()
        .map(|(input, (output, _))| (input, output))
        .collect();
    let unpacked = archives::expand(&local.directory().join("artifacts"), &outputs)?;
    if unpacked.is_empty() {
        return Ok(());
    }
    let file = tempfile::NamedTempFile::new().operation("stage upstream mappings")?;
    mapping::write(file.path(), &unpacked)?;
    run(
        "Lake dependency cache",
        project
            .lake()
            .env("LAKE_CONFIG", config(project))
            .args(["cache", "add", &format!("--package={package}")])
            .arg(file.path()),
    )
}
