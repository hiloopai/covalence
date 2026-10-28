# Changelog

User-visible changes in Covalence's tagged source releases. Rust versions such
as `0.1.0-dev.7` correspond to Python versions such as `0.1.0.dev7`.
Distribution artifacts are listed in the [GitHub releases].

This changelog starts with `0.1.0-dev.7`; see [earlier releases] for older versions.

## [Unreleased]

## [0.1.0-dev.7] - 2026-10-24

### Added

- The Rust [`covalence-lib-config`](crates/lib/config/README.md) crate for
  application directories and TOML configuration. Its `config_dir`, `data_dir`,
  `cache_dir`, `state_dir` and `runtime_dir` helpers select paths with explicit
  overrides and platform defaults, without creating directories.
- Typed TOML loading in Rust: `parse<T>` parses text, `read<T>` returns
  `T::default()` for a missing file, and `read_required<T>` reports a missing
  file as an error.
- The Python `covalence.config` module, exposing the same directory helpers as
  `pathlib.Path` values and TOML `parse`, `read` and `read_required` functions
  returning dictionaries. `read` returns an empty dictionary for a missing
  file; `read_required` raises `FileNotFoundError`.
- Python type stubs and Rust/Python usage documentation for configuration
  helpers, including path override, platform fallback and error behavior.

[GitHub releases]: https://github.com/hiloopai/covalence/releases
[earlier releases]: https://github.com/hiloopai/covalence/releases
[Unreleased]: https://github.com/hiloopai/covalence/compare/v0.1.0-dev.7...main
[0.1.0-dev.7]: https://github.com/hiloopai/covalence/compare/v0.1.0-dev.6...v0.1.0-dev.7
