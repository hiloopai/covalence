use std::{collections::BTreeSet, fs, path::PathBuf, sync::Arc};

use covalence_kv_trait::Kv;

use crate::{
    error::{IoResult, Result},
    project::{Project, Source, run},
    restore::Session,
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
    let mut session = Session::new(project, kv, &scope)?;
    install(project, kv, &mut session, !targets.is_empty())?;
    if !targets.is_empty() {
        session.build(project, targets)?;
    }
    Ok(())
}

/// Install the Mathlib/CSLib snapshot from upstream, then each Git
/// dependency's map from the pool on top of it.
pub(crate) fn install(
    project: &Project,
    kv: &Arc<dyn Kv>,
    session: &mut Session,
    lazy: bool,
) -> Result<()> {
    let packages = project.packages()?;
    // A pool map holds only the closures its publishers built, so upstream's
    // snapshot comes first and the pool adds to it.
    if config(project).exists() {
        let names = packages
            .iter()
            .filter(|package| matches!(package.source, Source::Git { .. }))
            .map(|package| package.name.clone())
            .collect();
        mappings(project, lazy, &names)?;
    }
    for package in packages {
        let Source::Git { commit, .. } = &package.source else {
            continue;
        };
        let key = project.revision_key(session.scope(), &package, commit)?;
        if let Some(root) = storage::pointer(&**kv, &key)? {
            session.install(project, &package, false, root, lazy)?;
        }
    }
    Ok(())
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
