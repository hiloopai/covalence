use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use covalence_lib_serde::{Deserialize, Serialize};

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
    run("Lake", project.lake()?.args(["env", "true"]))?;
    let scope = project.identity()?.scope;
    let lazy = !targets.is_empty();
    let mut session = Session::new(project, kv, &scope, Local::open(project)?)?;
    upstream(project, session.local(), lazy, &BTreeSet::new())?;
    let installs = pins(project, kv, &session)?;
    session.install(
        project,
        &installs,
        lazy,
        &Seeded::new(),
        &Default::default(),
    )?;
    if lazy {
        session.build(project, targets)?;
    }
    Ok(())
}

/// Install the Mathlib/CSLib snapshot from upstream, except the snapshots in
/// `seeded`, whose every mapping a seed holds. A pool map holds only the
/// closures its publishers built, so upstream's snapshot comes first and the
/// pool's maps of the [`pins`] go on top of it.
pub(crate) fn upstream(
    project: &Project,
    local: &Local,
    lazy: bool,
    seeded: &BTreeSet<Snapshot>,
) -> Result<()> {
    let level = if lazy {
        Installed::Mappings
    } else {
        Installed::Artifacts
    };
    for snapshot in snapshots(project)? {
        if seeded.contains(&snapshot) || local.upstream_installed(level, &snapshot.request()) {
            continue;
        }
        let mut command = project.lake()?;
        command.env("LAKE_CONFIG", config(project)).args([
            "cache",
            "get",
            &format!("--package={}", snapshot.package),
            &format!("--rev={}", snapshot.revision),
            &format!("--scope={}", snapshot.scope),
        ]);
        if lazy {
            command.arg("--mappings-only");
        }
        run("Lake dependency cache", &mut command)?;
        if !lazy {
            unpack_snapshot(project, local, &snapshot.package, &snapshot.revision)?;
        }
        local.set_upstream_installed(level, &snapshot.request())?;
    }
    Ok(())
}

/// One upstream Mathlib/CSLib snapshot: a package's mappings at a revision
/// in a cache scope of the upstream service.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(crate = "covalence_lib_serde::serde", deny_unknown_fields)]
pub(crate) struct Snapshot {
    pub package: String,
    pub revision: String,
    pub scope: String,
}

impl Snapshot {
    pub fn request(&self) -> [&str; 3] {
        [&self.package, &self.revision, &self.scope]
    }

    /// Where Lake keeps the mappings it downloaded for this snapshot
    /// (lean4 nightly-2026-11-05, `Lake/Config/Cache.lean`, `revisionPath`).
    pub fn revisions(&self, cache: &Path) -> PathBuf {
        cache
            .join("revisions")
            .join(package_name(&self.package))
            .join(format!("{}.jsonl", self.revision))
    }
}

/// Each Git dependency with a published build of its pin, and that build's
/// map root.
pub(crate) fn pins(
    project: &Project,
    kv: &Arc<dyn Kv>,
    session: &Session,
) -> Result<Vec<(Package, bool, Cid<Bdasl>)>> {
    let pinned = project
        .packages()?
        .into_iter()
        .filter_map(|package| match &package.source {
            Source::Git { commit, .. } => Some(
                project
                    .revision_key(session.scope(), &package, commit)
                    .map(|key| (package, key)),
            ),
            Source::Repository => None,
        })
        .collect::<Result<Vec<_>>>()?;
    let roots = storage::parallel(&pinned, |_, (_, key)| storage::pointer(&**kv, key))?;
    Ok(pinned
        .into_iter()
        .zip(roots)
        .filter_map(|((package, _), root)| root.map(|root| (package, false, root)))
        .collect())
}

/// The upstream snapshots of this workspace's Git dependencies, which
/// Mathlib's checkout configures.
pub(crate) fn snapshots(project: &Project) -> Result<Vec<Snapshot>> {
    if !config(project).exists() {
        return Ok(Vec::new());
    }
    let names = project
        .packages()?
        .into_iter()
        .filter(|package| matches!(package.source, Source::Git { .. }))
        .map(|package| package.name)
        .collect::<BTreeSet<_>>();
    let snapshot = project.git(&["-C", ".lake/packages/mathlib", "rev-parse", "HEAD"])?;
    let toolchain =
        fs::read_to_string(project.root.join("lean-toolchain")).operation("read Lean toolchain")?;
    let mut snapshots = Vec::new();
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
        if !names.contains(name) {
            continue;
        }
        let revision = if name == "cslib" {
            project.git(&["-C", ".lake/packages/cslib", "rev-parse", "HEAD"])?
        } else {
            snapshot.clone()
        };
        snapshots.push(Snapshot {
            package: name.to_owned(),
            revision,
            scope: format!("{}/{upstream}", toolchain.trim()),
        });
    }
    Ok(snapshots)
}

/// Decodes the archives of `package`'s upstream snapshot at `revision`, which
/// Lake just downloaded, and installs their outputs as its mappings, so Lake
/// unpacks none of them.
fn unpack_snapshot(project: &Project, local: &Local, package: &str, revision: &str) -> Result<()> {
    let revisions = Snapshot {
        package: package.to_owned(),
        revision: revision.to_owned(),
        scope: String::new(),
    }
    .revisions(local.directory());
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
            .lake()?
            .env("LAKE_CONFIG", config(project))
            .args(["cache", "add", &format!("--package={package}")])
            .arg(file.path()),
    )
}
