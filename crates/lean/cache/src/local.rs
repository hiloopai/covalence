//! The host-wide layer in front of the pool, inside Lake's own cache.
//!
//! Lake keeps artifacts and input-to-output mappings in its cache directory,
//! which every worktree and checkout on a host shares. Beside them this keeps
//! the map nodes it has read or built, and, per scope and package, the root
//! whose mappings a restore last installed and the root whose artifacts it
//! last installed too. A restore then transfers only the difference.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use covalence_cas_sqlite::SqliteCas;
use covalence_cas_trait::Cas;
use covalence_lib_cid::{Bdasl, Cid};
use sha2::{Digest, Sha256};

use crate::{
    error::{Error, IoResult, Result, invalid},
    mapping,
    project::Project,
};

/// What a restore installed from a root.
#[derive(Clone, Copy)]
pub(crate) enum Installed {
    /// Its mappings; Lake fetches artifacts when a build needs them.
    Mappings,
    /// Its mappings and every artifact they name.
    Artifacts,
}

impl Installed {
    fn directory(self) -> &'static str {
        match self {
            Self::Mappings => "mapped",
            Self::Artifacts => "installed",
        }
    }
}

pub(crate) struct Local {
    directory: PathBuf,
    nodes: Arc<dyn Cas>,
}

impl Local {
    pub fn open(project: &Project) -> Result<Self> {
        Self::at(project.cache_dir()?)
    }

    /// The layer in the Lake cache `directory`.
    pub fn at(directory: PathBuf) -> Result<Self> {
        let state = directory.join("covalence");
        fs::create_dir_all(&state).operation("create local pool directory")?;
        let nodes = SqliteCas::open(state.join("nodes.db")).map_err(|source| Error::Cas {
            operation: "open local map nodes".into(),
            source,
        })?;
        Ok(Self {
            directory,
            nodes: Arc::new(nodes),
        })
    }

    /// Lake's cache directory.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn nodes(&self) -> Arc<dyn Cas> {
        self.nodes.clone()
    }

    fn installed_path(&self, level: Installed, scope: &str, package: &str) -> Result<PathBuf> {
        mapping::segment_path(scope)?;
        mapping::segment_path(package)?;
        Ok(self
            .directory
            .join("covalence")
            .join(level.directory())
            .join(scope)
            .join(package))
    }

    /// Where this host records that it installed `level` of an upstream
    /// snapshot, named by a digest of the request because its scope holds
    /// characters no path segment may.
    fn upstream_path(&self, level: Installed, request: &[&str]) -> PathBuf {
        let mut hash = Sha256::new();
        for part in request {
            hash.update(part.as_bytes());
            hash.update([0]);
        }
        self.directory
            .join("covalence")
            .join("upstream")
            .join(level.directory())
            .join(data_encoding::HEXLOWER.encode(&hash.finalize()))
    }

    /// Whether this host installed at least `level` of the upstream snapshot
    /// that `request` names. Installed artifacts include the mappings.
    pub fn upstream_installed(&self, level: Installed, request: &[&str]) -> bool {
        [level, Installed::Artifacts]
            .into_iter()
            .any(|level| self.upstream_path(level, request).exists())
    }

    /// Records that this host installed `level` of an upstream snapshot.
    pub fn set_upstream_installed(&self, level: Installed, request: &[&str]) -> Result<()> {
        let path = self.upstream_path(level, request);
        let parent = path
            .parent()
            .ok_or_else(|| invalid("upstream marker has no parent"))?;
        fs::create_dir_all(parent).operation("create upstream marker directory")?;
        fs::write(&path, request.join("\n") + "\n").operation("write upstream marker")
    }

    /// The root whose `level` was last installed for `package` in `scope`.
    pub fn installed(
        &self,
        level: Installed,
        scope: &str,
        package: &str,
    ) -> Result<Option<Cid<Bdasl>>> {
        let path = self.installed_path(level, scope, package)?;
        match fs::read_to_string(&path) {
            Ok(text) => text
                .trim()
                .parse()
                .map(Some)
                .map_err(|_| invalid(format!("invalid installed root in {}", path.display()))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(Error::Io {
                operation: format!("read {}", path.display()),
                source,
            }),
        }
    }

    pub fn set_installed(
        &self,
        level: Installed,
        scope: &str,
        package: &str,
        root: Cid<Bdasl>,
    ) -> Result<()> {
        let path = self.installed_path(level, scope, package)?;
        let parent = path
            .parent()
            .ok_or_else(|| invalid("installed root has no parent"))?;
        fs::create_dir_all(parent).operation("create installed root directory")?;
        let mut pending =
            tempfile::NamedTempFile::new_in(parent).operation("stage installed root")?;
        io::Write::write_all(&mut pending, format!("{root}\n").as_bytes())
            .operation("write installed root")?;
        pending
            .persist(&path)
            .map_err(|error| error.error)
            .operation("save installed root")?;
        Ok(())
    }
}
