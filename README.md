# Covalence

Shared libraries for content-addressed data, in Rust with Python bindings.

- Content identifiers (CIDs) and DASL values, encoded as DRISL.
- Content-addressed stores in memory, in SQLite and over key-value stores such
  as S3.
- Blobs: bytes read whole or by range, and checked against a CID.

Status: pre-alpha. APIs may change without notice.

## Getting started

Install [Nix](https://nixos.org/download/) with flakes enabled, then:

```sh
nix develop --no-update-lock-file
glu ci
```

For automatic shell activation, install [direnv](https://direnv.net/), enable
its shell hook, and run `direnv allow` in this checkout. The development shell
adds `bin/` to `PATH`; outside it, invoke `./bin/glu`.

Alternatively, use **Reopen in Container** in VS Code with Docker and the Dev
Containers extension. Initial setup installs Nix and builds the project. Open
a new terminal after setup to activate the development environment.

`flake.lock` pins the tools used locally, in the container, and in Linux CI.
Without Nix, install the Rust version in `rust-toolchain.toml` and a native
linker; ordinary `cargo build` and `cargo test` work
with `python3` on `PATH` or `PYO3_PYTHON` set. The Nix shell checks that
its Rust version matches. macOS and ARM shells are defined but not yet tested
in CI.

## Commands

| Command | Purpose |
| --- | --- |
| `glu build [--release]` | Build all project artifacts |
| `glu test` | Run all project tests |
| `glu check` | Check formatting and run Clippy |
| `glu fmt` | Format Rust sources |
| `glu doc` | Build the Rust documentation, denying rustdoc warnings |
| `glu package` | Package and verify the crates for crates.io, without uploading |
| `glu ci` | Check, document, package, build, and test |
| `glu cargo <arguments...>` | Run Cargo from the checkout root |

Cargo owns Rust compilation and caching. `bin/glu` compiles only the Rust
dispatcher and its dependencies on launch. `glu build` builds the Rust workspace
and the Python wheel.

## Layout

Content addressing:

- [`crates/lib/cid`](crates/lib/cid/README.md): content identifiers.
- [`crates/lib/dasl`](crates/lib/dasl/README.md): DASL values and the DRISL codec.
- `crates/blob/trait`: the blob trait: bytes read by range.

Stores:

- `crates/cas/trait`: the content-addressed storage traits.
- `crates/cas/memory`: an in-memory store.
- `crates/cas/sqlite`: a SQLite store.
- `crates/cas/kv`: a content-addressed store over a key-value store.
- `crates/kv/trait`: the key-value store trait.
- `crates/kv/memory`: an in-memory key-value store.
- `crates/kv/directory`: a key-value store in a filesystem directory.
- `crates/kv/s3`: a key-value store on S3.

Python:

- [`crates/python`](crates/python/README.md): the `covalence` Python package.

Internal facades, which re-export other crates for the workspace's own use:

- `crates/lib/error`: SNAFU and miette facade and `BoxError`; terminal rendering is opt-in.
- `crates/lib/cli`: Clap facade.
- `crates/lib/data`: shared byte buffers and containers.
- `crates/lib/python`: PyO3 facade.
- `crates/lib/rand`: rand facade.
- `crates/lib/serde`: serialization traits and derives.
- `crates/lib/sqlite`: rusqlite facade, with SQLite bundled.

Tools:

- `tools/glu`: Rust development command dispatcher.
- `.context/`: local design notes, ignored by Git.

## License

Covalence is available under [CC0 1.0](LICENSE-CC0), the [MIT License](LICENSE-MIT)
or the [Apache License 2.0](LICENSE-APACHE), at your option
(`CC0-1.0 OR MIT OR Apache-2.0`).
