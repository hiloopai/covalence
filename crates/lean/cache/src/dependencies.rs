use std::fs;

use crate::{
    error::{IoResult, Result},
    project::{Project, run},
};

pub(crate) fn fetch(project: &Project, targets: &[String]) -> Result<()> {
    run("Lake", project.lake().args(["env", "true"]))?;
    let snapshot = project.git(&["-C", ".lake/packages/mathlib", "rev-parse", "HEAD"])?;
    let toolchain =
        fs::read_to_string(project.root.join("lean-toolchain")).operation("read Lean toolchain")?;
    let config = project
        .root
        .join(".lake/packages/mathlib/.meta/lake-config.toml");
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
        if !targets.is_empty() {
            command.arg("--mappings-only");
        }
        run("Lake dependency cache", &mut command)?;
    }
    if !targets.is_empty() {
        run(
            "Lake dependency build",
            project
                .lake()
                .env("LAKE_CONFIG", config)
                .args(["build", "--wfail"])
                .args(targets),
        )?;
    }
    Ok(())
}
