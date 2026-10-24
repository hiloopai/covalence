# Application directories and TOML configuration

`covalence-lib-config` selects application directories and deserializes TOML
configuration into Rust types. It does not create directories or write files.

```rust
use std::path::{Path, PathBuf};
use covalence_lib_config::{config_dir, parse};
use covalence_lib_serde::Deserialize;

#[derive(Debug, PartialEq, Deserialize)]
#[serde(crate = "covalence_lib_serde::serde", deny_unknown_fields)]
struct Config {
    name: String,
}

let dir = config_dir("example", Some(PathBuf::from("relative/config")))?;
assert_eq!(dir, Path::new("relative/config"));
let config: Config = parse("name = \"example\"")?;
assert_eq!(config.name, "example");
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Directories

`config_dir`, `data_dir`, `cache_dir`, `state_dir` and `runtime_dir` return an explicit
`override_dir` unchanged, including relative paths. No name or home directory is
needed in that case. Otherwise, `name` must be one normal directory component:
empty names, `.`, `..`, separators, colons and NUL are rejected with `Error::Name`.
This keeps application names from replacing or traversing the selected base.
Names are not checked against platform-specific reserved filenames.

On Unix, including macOS, the functions use the [XDG Base Directory] variables.
Unset, empty and relative variables are ignored:

| Function | Environment variable | Default |
| --- | --- | --- |
| `config_dir` | `XDG_CONFIG_HOME` | `~/.config/<name>` |
| `data_dir` | `XDG_DATA_HOME` | `~/.local/share/<name>` |
| `cache_dir` | `XDG_CACHE_HOME` | `~/.cache/<name>` |
| `state_dir` | `XDG_STATE_HOME` | `~/.local/state/<name>` |
| `runtime_dir` | `XDG_RUNTIME_DIR` | the state directory |

On Windows config, data, state and runtime use the roaming application-data
directory: a nonempty `APPDATA`, then the Windows known-folder API, then
`AppData/Roaming` under the home directory. The cache uses nonempty
`LOCALAPPDATA`, then its known-folder API, then `AppData/Local` under the home
directory. All append `name`.
The functions report `Error::Home` when the home directory cannot be found.

Path selection does not check existence, create directories, canonicalize
symlinks, enforce permissions or provide an isolation boundary. In particular,
the runtime fallback does not have the lifetime or permission guarantees of an
XDG runtime directory; callers creating sockets or sensitive files must enforce
their own requirements.

## TOML

`parse<T>` parses TOML text into any Serde-deserializable type. `read<T>` reads a
UTF-8 TOML file; a missing file (including a missing parent) returns `T::default()`.
`read_required<T>` reports missing files as errors, suitable for an explicit
configuration path from the user.
Other I/O errors return `Error::Read` with the path and underlying error. Invalid
TOML or a type mismatch returns `Error::Parse` with the path and parser error.
`parse` returns the TOML parser error directly.

The target Rust type controls defaults, accepted fields and validation. There is
no interpolation, environment-variable expansion, layer merging, schema
registry or size limit added by this crate. Reading follows ordinary filesystem
symlink semantics.

[XDG Base Directory]: https://specifications.freedesktop.org/basedir-spec/latest/
