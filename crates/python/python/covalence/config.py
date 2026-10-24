"""Application directories and TOML configuration, backed by Rust."""

from ._native import config as _native_config

config_dir = _native_config.config_dir
cache_dir = _native_config.cache_dir
data_dir = _native_config.data_dir
state_dir = _native_config.state_dir
runtime_dir = _native_config.runtime_dir
parse = _native_config.parse
read = _native_config.read
read_required = _native_config.read_required

__all__ = [
    "config_dir", "cache_dir", "data_dir", "state_dir", "runtime_dir",
    "parse", "read", "read_required",
]
