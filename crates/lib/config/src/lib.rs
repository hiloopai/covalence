#![doc = include_str!("../README.md")]
#![deny(missing_docs)]

use std::{
    io,
    path::{Component, Path, PathBuf},
};

use covalence_lib_error::snafu::{self, ResultExt, Snafu};
use covalence_lib_serde::serde::de::DeserializeOwned;
use etcetera::BaseStrategy;

/// Errors from finding directories and reading config files.
#[derive(Debug, Snafu)]
#[snafu(crate_root(covalence_lib_error::snafu))]
#[non_exhaustive]
pub enum Error {
    /// The application name is not a single directory component.
    #[snafu(display("application name {name:?} must be a single directory component"))]
    Name {
        /// The rejected name.
        name: String,
    },
    /// No directory was given, and the home directory could not be found.
    #[snafu(display("could not find the home directory"))]
    Home,
    /// The file could not be read, including a missing required file.
    #[snafu(display("could not read {}: {source}", path.display()))]
    Read {
        /// The file.
        path: PathBuf,
        /// Why reading failed.
        source: io::Error,
    },
    /// The file is not TOML of the requested shape.
    #[snafu(display("could not parse {}: {source}", path.display()))]
    Parse {
        /// The file.
        path: PathBuf,
        /// Why parsing failed.
        source: toml::de::Error,
    },
}

/// The config directory for `name`: `override_dir` if given, else
/// `$XDG_CONFIG_HOME/<name>` or `~/.config/<name>` (`%APPDATA%\<name>` on
/// Windows).
/// See the [crate documentation](crate) for name, override and error rules.
pub fn config_dir(name: &str, override_dir: Option<PathBuf>) -> Result<PathBuf, Error> {
    dir(name, override_dir, |base| base.config_dir())
}

/// The data directory for `name`: `override_dir` if given, else
/// `$XDG_DATA_HOME/<name>` or `~/.local/share/<name>` (`%APPDATA%\<name>` on
/// Windows).
/// See the [crate documentation](crate) for name, override and error rules.
pub fn data_dir(name: &str, override_dir: Option<PathBuf>) -> Result<PathBuf, Error> {
    dir(name, override_dir, |base| base.data_dir())
}

/// The cache directory for `name`: `override_dir` if given, else
/// `$XDG_CACHE_HOME/<name>` or `~/.cache/<name>` (`%LOCALAPPDATA%\<name>` on
/// Windows). See the [crate documentation](crate) for name, override and error rules.
pub fn cache_dir(name: &str, override_dir: Option<PathBuf>) -> Result<PathBuf, Error> {
    dir(name, override_dir, |base| base.cache_dir())
}

/// The state directory for `name`: `override_dir` if given, else
/// `$XDG_STATE_HOME/<name>` or `~/.local/state/<name>`. Windows has none, so
/// there it is the data directory.
/// See the [crate documentation](crate) for name, override and error rules.
pub fn state_dir(name: &str, override_dir: Option<PathBuf>) -> Result<PathBuf, Error> {
    dir(name, override_dir, |base| {
        base.state_dir().unwrap_or_else(|| base.data_dir())
    })
}

/// The runtime directory for `name`, for sockets and other files that last
/// one session: `override_dir` if given, else `$XDG_RUNTIME_DIR/<name>`.
/// Without that variable (as is usual on macOS, and always on Windows), it is
/// the state directory.
/// This only selects a path; it does not enforce socket directory permissions
/// or session lifetime. See the [crate documentation](crate) for other rules.
pub fn runtime_dir(name: &str, override_dir: Option<PathBuf>) -> Result<PathBuf, Error> {
    dir(name, override_dir, |base| {
        base.runtime_dir()
            .unwrap_or_else(|| base.state_dir().unwrap_or_else(|| base.data_dir()))
    })
}

fn dir(
    name: &str,
    override_dir: Option<PathBuf>,
    default: impl FnOnce(&dyn BaseStrategy) -> PathBuf,
) -> Result<PathBuf, Error> {
    match override_dir {
        Some(dir) => Ok(dir),
        None => {
            let mut components = Path::new(name).components();
            snafu::ensure!(
                !name.contains(['/', '\\', ':', '\0'])
                    && matches!(components.next(), Some(Component::Normal(_)))
                    && components.next().is_none(),
                NameSnafu { name }
            );
            let base = etcetera::choose_base_strategy().map_err(|_| Error::Home)?;
            Ok(default(&base).join(name))
        }
    }
}

/// Parse TOML text.
pub fn parse<T: DeserializeOwned>(text: &str) -> Result<T, toml::de::Error> {
    toml::from_str(text)
}

/// Read and parse a TOML file. A missing file gives `T::default()`.
pub fn read<T: DeserializeOwned + Default>(path: &Path) -> Result<T, Error> {
    match read_required(path) {
        Err(Error::Read { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            Ok(T::default())
        }
        result => result,
    }
}

/// Read and parse a required UTF-8 TOML file, reporting missing files as errors.
pub fn read_required<T: DeserializeOwned>(path: &Path) -> Result<T, Error> {
    let text = std::fs::read_to_string(path).context(ReadSnafu { path })?;
    parse(&text).context(ParseSnafu { path })
}
