# Lake build cache

`cov-lake-cache` shares successful [Lake](https://github.com/leanprover/lean4/tree/master/src/lake)
builds through one S3 pool that every repository, worktree and host reads.
It uses Covalence's S3 transport for credentials, pooled connections, retries
and bounded multipart transfers. Lake keeps responsibility for artifact
hashes and build dependency validation.

This command targets Lean 4.37's `2026-03-17` output mapping format. Changes
to Lake's download protocol fail explicitly and need compatibility tests.

```sh
cov-lake-cache dependencies
cov-lake-cache get --repository example/proofs
# Or fetch only what one target needs:
cov-lake-cache get --repository example/proofs -- MyLibrary.Module
cov-lake-cache build --repository example/proofs -- MyLibrary
# Run the repository's remaining correctness checks, then on trusted main CI:
cov-lake-cache put --repository example/proofs
```

## Pool

The pool is shared by platform and toolchain, never by repository. Its scope
is `<os>-<arch>/<toolchain>`, where `<toolchain>` is the SHA-256 of this
layout's version (`cov-lake/v1`), `lean --version` and the toolchain's
`leantar` executable. Under `lake/<scope>/`:

- `cas/blake3/<digest>/bytes.zst` holds artifacts and map nodes in
  Covalence's key-value CAS with Zstandard. Each CID names the raw bytes, and
  every read checks them.
- `rev/<owner>/<repo>/<commit>/<package>` holds, as text, the root CID of a
  Prolly map from Lake input hash to that input's output descriptor and the
  CID of each artifact it names. That map is the only place Lake artifact IDs
  meet CIDs. The root and path packages use this repository and commit; a Git
  dependency uses its own pinned repository and commit.

Lake artifact IDs are 64-bit hashes over outputs, so artifacts from different
repositories coincide when their bytes do. Input hashes include no checkout
path. A revision map, not the pool, says which output an input has: two
builds of one input can differ, for example in logged messages.

Patched and stock `leantar` both write `LTR4` archives and report the same
version, and stock `leantar` decodes the patched opcodes `0xa0` and `0xb0` as
constructors instead of rejecting them. The executable is therefore part of
the scope.

## Commands

- `build` runs `lake build --wfail` once on the given targets (pass every
  target CI builds, such as executables, so they are published), records the root package's outputs,
  then replays the build once per other package to record its outputs too:
  path dependencies always, Git dependencies only when the pool has no build
  of their pin. A failed build removes the previous receipt. Recording
  requires a clean checkout; put generated output under an ignored directory.
- `put` requires that receipt from the same revision, toolchain, platform and
  CI run. It never builds. For each recorded package it builds the revision
  map locally, diffs it against the nearest published ancestor's map, uploads
  the artifacts the diff names that the pool lacks, then the map nodes the
  pool lacks, children first, and only then the revision pointer. A Git
  dependency's map also takes in the upstream snapshot Lake fetched for a
  pinned commit, for the artifacts on disk.
- `get` restores this repository's packages from `HEAD`, else from the
  nearest of its 64 first-parent ancestors with a published build, and
  prints which commit it used.
- `dependencies` restores each Git dependency's build of its pin from the
  pool, and the Mathlib/CSLib snapshot from upstream only for packages the
  pool has no build of.
- With targets, `get` and `dependencies` install only mappings (`get` also
  those of the dependencies). `get` then fetches this repository's changed
  artifacts in parallel, and runs `lake build --wfail` on the targets. Lake takes each artifact from its local
  cache, else downloads it the first time the build needs it, else builds the
  module.

A restore diffs the published map against the root this host last installed
and hands Lake only the changed entries; Lake skips artifacts it
already has. [`pool::transfer`] is that root-to-root diff for other callers,
such as a remote builder computing a minimal upload.

## Host-wide local layer

Lake's cache directory is the local layer. It is `LAKE_CACHE_DIR`, which this
command sets to `$XDG_CACHE_HOME/lake` or `~/.cache/lake` when unset, so CI can
save one directory: every worktree and checkout on
a host shares its artifacts and mappings. Beside them, `covalence/nodes.db`
holds map nodes, `covalence/mapped/` the root whose mappings were last
installed per scope and package, and `covalence/installed/` the root whose
artifacts were too. This command enables Lake's artifact cache for its own
Lake runs. Export the same `LAKE_CACHE_DIR` and `LAKE_ARTIFACT_CACHE=true` for
every other Lake build on the host: Lake then keeps each archive unpacked in the
cache after its first use, and later worktrees restore without unpacking. Nothing
evicts old entries yet.

Set `LAKE_CI_BUCKET`. Credentials, region and endpoint follow the AWS
credential chain (`AWS_REGION`, `AWS_ENDPOINT_URL`, `AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, web identity, profiles or machine
roles). Do not pass credentials on the command line. GitHub publication
requires a main push or manual main run in the named repository. Bucket
permissions remain the actual write authorization boundary. Only trusted
writers may publish maps and artifacts.

While Lake runs under this command, it stands in for `curl`. It serves pool
artifacts named by the maps it installed and passes requests for other
services, such as Mathlib's cache, to `curl` unchanged.

Sixteen file transfers run at once, with at most two 8 MiB parts each. Upload
payload buffers are limited to 256 MiB. Native Lake can use additional memory
while validating and unpacking.

`--root` and `--record` select a project and record directory. `LAKE` selects the Lake executable. Installation should use
a pinned prebuilt binary; compiling the storage SDK in every proof CI run
would defeat the setup-time goal.

The manual `Release Lake cache` workflow builds and tests public `main`, then
publishes a Linux x86-64 archive at the `lake-cache-<commit>` GitHub release.
Pin both that commit and the archive SHA-256 in consumers. The archive targets
Ubuntu 24.04 or newer. Source installs can use
`cargo install --locked --git https://github.com/hiloopai/covalence --rev <commit> covalence-lean-cache`.
GitHub workflows can use the `.github/actions/lake-cache` composite action
pinned to a full public commit, with `revision` and `sha256` inputs. It verifies
the archive even when restored from disk and puts `cov-lake-cache` on `PATH`.
It also installs the bundled `leantar` in the selected Lean toolchain, which
must already be installed and writable. The archive records the upstream
commits in `LEANTAR-REVISIONS`, their combined source tree in `LEANTAR-TREE`,
and all license notices. It combines upstream master with the pinned, unmerged
[explicit-stack fix](https://github.com/digama0/leangz/pull/22). The codec supports
embedded NUL strings, scalar arrays, and deeply nested data that older Lean
distributions cannot safely archive. It changes no Lean sources or kernel checks.

Replacing the archive tool changes the scope and invalidates publication
receipts. Source installations must supply a compatible archiver themselves. Thomas owns the bundled upstream
pin; remove it once the supported Lean distribution includes
[leangz#12](https://github.com/digama0/leangz/pull/12),
[leangz#15](https://github.com/digama0/leangz/pull/15), and
[leangz#22](https://github.com/digama0/leangz/pull/22).

Run `cargo test -p covalence-lean-cache --features lake-tests` with the pinned
Lean release on `PATH` for the signed S3 server and real Lake integration
tests. The umbrella CI calls a separate workflow for these tests so library
developers do not need to install Lean to run `glu ci`.

Maintainer: Thomas. Retire this command when Lake natively provides bounded S3
transfers and publication from a successful recorded build without rebuilding.
