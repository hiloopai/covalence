use std::{collections::BTreeSet, fs, path::PathBuf, sync::Arc};

use covalence_kv_trait::Kv;
use covalence_lib_cid::{Bdasl, Cid};

use crate::{
    error::{IoResult, Result},
    project::{Package, Project, Source, run},
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
    upstream(project, lazy)?;
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
pub(crate) fn upstream(project: &Project, lazy: bool) -> Result<()> {
    if !config(project).exists() {
        return Ok(());
    }
    let names = project
        .packages()?
        .into_iter()
        .filter(|package| matches!(package.source, Source::Git { .. }))
        .map(|package| package.name)
        .collect();
    mappings(project, lazy, &names)
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
/// `mappings_only`, all of their artifacts.
fn mappings(project: &Project, mappings_only: bool, packages: &BTreeSet<String>) -> Result<()> {
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
        let mut command = project.lake();
        command.env("LAKE_CONFIG", &config).args([
            "cache",
            "get",
            &format!("--package={name}"),
            &format!("--rev={revision}"),
            &format!("--scope={}/{upstream}", toolchain.trim()),
        ]);
        if mappings_only {
            command.arg("--mappings-only");
        }
        run("Lake dependency cache", &mut command)?;
    }
    Ok(())
}
