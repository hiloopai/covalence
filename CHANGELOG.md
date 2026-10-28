# Changelog

All notable changes to Covalence are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/2.0.0/).
Versions cover the Rust libraries and Python bindings and follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). These pre-alpha
development releases may contain incompatible API changes.
Rust versions such as `0.1.0-dev.7` appear as `0.1.0.dev7` on PyPI.

Dates are the first public release dates in UTC; registry publication may
follow later. Source tags and distribution artifacts for dev.2 onward are
listed in the [GitHub releases]. Dev.1 was an earlier Python-only preview.
The dev.1–dev.7 history was reconstructed on 2026-10-28 from source tags and
published artifacts.

## [Unreleased]

### Added

- Release notes and automated changelog format checks in `glu check` and CI.

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

## [0.1.0-dev.6] - 2026-10-22

### Fixed

- S3 create-only multipart writes now report an existing object correctly
  when S3 embeds `PreconditionFailed` in an HTTP 200 completion response,
  rather than raising an error. Other embedded completion errors remain
  errors.

## [0.1.0-dev.5] - 2026-10-18

### Added

- S3 upload planning through Rust `S3Kv::upload_plan` and `UploadPlan`.
- Optional shared upload payload budgets through Rust
  `S3Kv::upload_buffer_bytes` and Python
  `Kv.s3(..., upload_buffer_bytes=...)`. Insufficient capacity fails
  immediately; the default remains unlimited. The budget bounds upload
  payloads, not total process memory.

### Fixed

- Validate upload lengths, multipart geometry and allocation limits before
  reading upload sources or making upload requests. Respect the single-PUT
  size limit even with large configured parts, and return allocation
  failures as errors.

## [0.1.0-dev.4] - 2026-10-17

### Added

- Python `Cid.of_fileobj`, `Cas.put_fileobj` and `Cas.get_to` for caller-owned
  binary file objects, with required byte limits and optional exact lengths.
  Uploads verify both passes, including when reusing an existing block;
  downloads check the CID before returning successfully.
- File-object callbacks of at most 1 MiB, supporting short reads and writes
  and leaving caller-owned files open. The byte limits bound accepted
  input/output, not backend memory. Downloads can leave partial or unchecked
  bytes on failure, so callers should publish output only after success.

## [0.1.0-dev.3] - 2026-10-17

### Added

- Python `Cas.put_from(cid, chunks, length)` for ingesting iterables of byte
  chunks, checking the CID and length before publishing a new block.
- Python `Kv.s3(..., concurrency=n)` to control concurrent S3 parts and
  streaming memory use.
- Atomic create-only key-value writes through Rust `Kv::put_new`,
  `Kv::put_new_from` and `Kv::aput_new`, implemented by memory and S3 stores,
  and the corresponding `Op::PutNew` variant.

### Changed

- **Breaking:** `KvCas` writes now preserve existing blocks, including when
  writers race. Custom key-value backends must implement the new atomic create-only
  methods; their default implementations return `Unsupported`. Exhaustive
  matches on `Op` must handle the new `PutNew` variant.
- A streaming CAS write may reuse an existing block without consuming or
  validating the remaining input.

### Fixed

- Preserve Python source exceptions during streaming ingestion. If storage
  cleanup also fails, report `StoreError` with the source exception as its
  cause.

## [0.1.0-dev.2] - 2026-10-14

### Added

- The current tagged Rust/Python source line: CIDv1 identifiers, DASL values
  and their DRISL encoding, byte-range blobs and checked content-addressed
  storage traits.
- Memory, SQLite and key-value CAS implementations, with memory and S3
  key-value backends.
- Python top-level `Cid`, `Hash`, `Codec`, `Value`, `Blob`, `BlobFact`, `Fact`,
  `Cas` and `Kv` APIs, including streaming file uploads and checked downloads.
- macOS ARM64 and Linux ARM64 wheels and a source distribution, alongside
  Linux x86-64 wheels. Wheels support CPython 3.11 and later through the
  stable ABI.

### Removed

- **Breaking:** The dev.1 Python API and its `covalence.cas`, `covalence.data`,
  `covalence.lib` and `covalence.logic` modules. Applications using them need
  to migrate to the new top-level APIs.

## [0.1.0-dev.1] - 2026-09-02

This Python-only preview has no corresponding source tag in the current
public repository. These notes are based on its published wheel.

### Added

- Python hashing, CBOR, S-expression, CAS fact and logic APIs through
  `covalence.cas`, `covalence.data`, `covalence.lib` and `covalence.logic`.
- A Linux x86-64 wheel using CPython 3.11's stable ABI. No source distribution
  was published.

[GitHub releases]: https://github.com/hiloopai/covalence/releases
[Unreleased]: https://github.com/hiloopai/covalence/compare/v0.1.0-dev.7...HEAD
[0.1.0-dev.7]: https://github.com/hiloopai/covalence/compare/v0.1.0-dev.6...v0.1.0-dev.7
[0.1.0-dev.6]: https://github.com/hiloopai/covalence/compare/v0.1.0-dev.5...v0.1.0-dev.6
[0.1.0-dev.5]: https://github.com/hiloopai/covalence/compare/v0.1.0-dev.4...v0.1.0-dev.5
[0.1.0-dev.4]: https://github.com/hiloopai/covalence/compare/v0.1.0-dev.3...v0.1.0-dev.4
[0.1.0-dev.3]: https://github.com/hiloopai/covalence/compare/v0.1.0-dev.2...v0.1.0-dev.3
[0.1.0-dev.2]: https://github.com/hiloopai/covalence/releases/tag/v0.1.0-dev.2
[0.1.0-dev.1]: https://pypi.org/project/covalence/0.1.0.dev1/
