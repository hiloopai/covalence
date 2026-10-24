"""Application directories and TOML configuration, backed by Rust."""

from datetime import date, datetime, time
from os import PathLike
from pathlib import Path
from typing import TypeAlias

_TomlValue: TypeAlias = str | bool | int | float | datetime | date | time | list["_TomlValue"] | dict[str, "_TomlValue"]

__all__ = [
    "config_dir", "cache_dir", "data_dir", "state_dir", "runtime_dir",
    "parse", "read", "read_required",
]

def config_dir(name: str, *, override_dir: str | PathLike[str] | None = None) -> Path:
    """Return the application's config directory without creating it.

    An explicit override_dir is returned unchanged. Otherwise name must be a
    single non-empty path component and the platform's XDG-style directory is
    used. Invalid names raise ValueError; an unavailable home raises OSError.
    """

def cache_dir(name: str, *, override_dir: str | PathLike[str] | None = None) -> Path:
    """Return the application's cache directory without creating it.

    An explicit override_dir is returned unchanged. Otherwise name must be a
    single non-empty path component and the platform's XDG-style directory is
    used. Invalid names raise ValueError; an unavailable home raises OSError.
    """

def data_dir(name: str, *, override_dir: str | PathLike[str] | None = None) -> Path:
    """Return the application's data directory without creating it.

    An explicit override_dir is returned unchanged. Otherwise name must be a
    single non-empty path component and the platform's XDG-style directory is
    used. Invalid names raise ValueError; an unavailable home raises OSError.
    """

def state_dir(name: str, *, override_dir: str | PathLike[str] | None = None) -> Path:
    """Return the application's state directory without creating it.

    An explicit override_dir is returned unchanged. Otherwise name must be a
    single non-empty path component. Windows falls back to its data directory.
    Invalid names raise ValueError; an unavailable home raises OSError.
    """

def runtime_dir(name: str, *, override_dir: str | PathLike[str] | None = None) -> Path:
    """Return the application's runtime directory without creating it.

    An explicit override_dir is returned unchanged. Otherwise name must be a
    single non-empty path component. When no runtime directory is configured,
    this falls back to the state directory. Permissions and session lifetime
    are not enforced. Invalid names raise ValueError; an unavailable home
    raises OSError.
    """

def parse(text: str) -> dict[str, _TomlValue]:
    """Parse TOML text into a dict of Python values.

    Tables and arrays become dicts and lists. Other values become str, bool,
    signed 64-bit int, float, datetime.datetime, datetime.date or datetime.time.
    Date/time fractions are truncated to microseconds. Invalid TOML, integers
    outside the signed 64-bit range and dates Python cannot represent raise
    ValueError. This does not interpolate variables or merge configuration.

    >>> from covalence.config import parse
    >>> parse('enabled = true\\nworkers = 2')
    {'enabled': True, 'workers': 2}
    """

def read(path: str | PathLike[str]) -> dict[str, _TomlValue]:
    """Read a UTF-8 TOML file into a dict, returning an empty dict if missing.

    Values follow parse's conversion rules. Invalid TOML raises ValueError;
    other read failures raise OSError. No files or directories are created.
    """

def read_required(path: str | PathLike[str]) -> dict[str, _TomlValue]:
    """Read a required UTF-8 TOML file into a dict.

    Values follow parse's conversion rules. A missing file raises
    FileNotFoundError, invalid TOML raises ValueError and other read failures
    raise OSError. No files or directories are created.
    """

