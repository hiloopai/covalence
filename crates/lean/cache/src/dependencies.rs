use std::{fs, path::PathBuf};

use crate::{
    error::{IoResult, Result},
    project::{Project, run},
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

pub(crate) fn fetch(project: &Project, targets: &[String]) -> Result<()> {
    run("Lake", project.lake().args(["env", "true"]))?;
    mappings(project, !targets.is_empty())?;
    if !targets.is_empty() {
        run(
            "Lake dependency build",
            project
                .lake()
                .env("LAKE_CONFIG", config(project))
                .args(["build", "--wfail"])
                .args(targets),
        )?;
    }
    Ok(())
}

/// Fetches the Mathlib/CSLib snapshot's mappings, and unless `mappings_only`,
/// all of its artifacts.
pub(crate) fn mappings(project: &Project, mappings_only: bool) -> Result<()> {
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
