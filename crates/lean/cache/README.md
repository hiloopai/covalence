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
# On a fresh host that will keep its cache, start from the published seed:
cov-lake-cache get --seed --repository example/proofs -- MyLibrary
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

- `cas/sha2-256/<digest>/bytes.zst` holds each artifact as a Zstandard frame,
  under the SHA-256 CID of its raw bytes, the hash of Covalence receipts and of
  Lake's planned content hashes. Every read checks it.
- `cas/blake3/<digest>/bytes.zst` holds map nodes in Covalence's key-value CAS
  with Zstandard; Prolly names its nodes by BLAKE3.
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
  then replays the build once per other package, path and Git
  dependencies alike, to record its outputs too. Lake's records leave out
  the native objects of a package that sets `platformIndependent`, such as
  Mathlib and Aesop, so `build` adds each one the package's build folder
  traces, and executables that link them need not compile them again. A failed build removes the previous receipt. Recording
  requires a clean checkout; put generated output under an ignored directory.
- `put` requires that receipt from the same revision, toolchain, platform and
  CI run. It never builds. For each recorded package it builds the revision
  map locally, diffs it against the nearest published ancestor's map, uploads
  the artifacts the diff names that the pool lacks, then the map nodes the
  pool lacks, children first, and only then the revision pointer. A Git
  dependency's map is the union of every publisher's entries for its pin,
  this publisher's winning, plus the upstream snapshot Lake fetched for a
  pinned commit, for the artifacts on disk. A pin's map can still lack
  modules no publisher built.
- `put` then publishes the commit's source tree under `lake/src/`: each
  tracked Git blob, uncompressed, in a key-value CAS under its raw BLAKE3 CID;
  the DRISL tree `{"files": {<path>: <link>}}`; the record `{"tree": <link>,
  "targets": [...]}` naming the targets `build` checked; then the pointer
  `lake/src/rev/<owner>/<repo>/<commit>`, in that order. Blobs the nearest
  published first-parent ancestor's tree holds are not checked again.
- `put` then publishes the commit's seed (below), unless one of the
  repository's two seed slots already holds it.
- `get` restores this repository's packages from `HEAD`, else from the
  nearest of its 64 first-parent ancestors with a published build, and
  prints which commit it used. `COVALENCE_LAKE_COMMITS` (full revisions,
  comma- or newline-separated, nearest first) names the commits to search
  instead, for a checkout without the repository's history, such as a
  remote build's copy of a client's tree.
- `dependencies` restores the Mathlib/CSLib snapshot from upstream, then
  each Git dependency's build of its pin from the pool on top of it, so a
  partial pool map never hides upstream's artifacts.
- With targets, `get` and `dependencies` install only mappings (`get` also
  those of the dependencies). `get` then runs `lake build --wfail` on the
  targets, which fetches only the artifacts they need. Lake takes each artifact from its local
  cache, else downloads it the first time the build needs it, else builds the
  module.

A restore diffs the published map against the root this host last installed
and hands Lake only the changed entries; Lake skips artifacts it
already has. [`pool::transfer`] is that root-to-root diff for other callers,
such as a remote builder computing a minimal upload.

Lake restores a module whose output is only its `.ltar` archive by running
`leantar` once per module. An eager restore (without targets) instead
downloads each archive itself, 64 at a time, decodes it in process with
`covalence-lean-ltar`, writes its files into Lake's artifact
directory under the names its trace gives them, and installs the trace's
outputs as the mapping. It downloads every other artifact too, each once, and
never rewrites a file already in the cache: a cache entry is immutable once
written. Lake records only the mappings, then finds every file in its cache
and runs no archiver. An eager restore of the upstream Mathlib/CSLib snapshot
decodes the archives Lake downloaded the same way. Each host fetches an
upstream snapshot once per pin; `covalence/upstream/` records which it has.
Targeted restores still leave archives to Lake. With `LAKE_ARTIFACT_CACHE=true`, a cache hit
leaves a module's files in Lake's cache and links only its `.ilean` into the
package's `.lake/build`, unless Lake restores all artifacts. Tools that run
`lean` on `.lake/build`, such as `lake env lean`, need every file there, so
every Lake run of this command sets `LAKE_RESTORE_ARTIFACTS=true`, and so does
the GitHub action for later steps. Elsewhere, export it beside
`LAKE_ARTIFACT_CACHE=true`, or set `restoreAllArtifacts = true` in the root
lakefile; packages that do not set it themselves inherit it. Lake hard-links
the files, so this costs no space.

## Seeds

A fresh host otherwise has Lake fetch and unpack each module on its own: about
27,000 requests and archive decodes for one large repository. `put` also
publishes the recorded build as a seed: for every input `build` checked, Lake's
local output file, without its archive, and the unpacked artifacts and native
objects it names, as 16 Zstandard tar streams. An output Lake kept as its
archive alone is decoded first, so no host the seed fills unpacks it. Each repository keeps two seeds
per scope under `lake/<scope>/seed/<owner>/<repo>/{a,b}/`; a publish overwrites
the older slot, its chunks first and its `manifest.json` (commit, publication
time, each chunk's SHA-256, length and file count) last. Storage stays at two
seeds per repository and scope, with nothing to delete; a toolchain change
leaves its scope's seeds behind with the rest of that scope.

`get --seed` on a host that never installed the repository's mappings
downloads the seed of the commit it restores, if a slot holds it, while the
upstream snapshots install. A seed holds the whole recorded build, so pass
`--seed` where the host keeps its cache or builds most of it, such as a
developer machine or a full-library check, and not for a one-shot runner that
needs a few targets. It unpacks the 16 streams at once beside Lake's cache,
checks each against the manifest, then moves the files in: artifacts the cache
lacks, and every output file, after the upstream snapshots that would rewrite
them. The pool's maps then install only the inputs the seed does not map. Lake
still checks every trace against the sources. A seed that is missing, or fails
its check because a publish overwrote it mid-read, is reported and the restore
continues from the pool.

## Host-wide local layer

Before any Lake run, `dependencies`, `build` and `get` check out each Git
dependency missing from the packages directory from a shallow bare mirror in
`covalence/git/` under Lake's cache directory. The mirror fetches only pinned
commits it lacks, at depth one; checkouts borrow its objects through Git
alternates. Lake then finds each package at its pinned revision and clones
nothing. Local-path Git URLs are left to Lake.

Lake's cache directory is the local layer. It is `LAKE_CACHE_DIR`, which this
command sets to `$XDG_CACHE_HOME/lake` or `~/.cache/lake` when unset, so CI can
save one directory: every worktree and checkout on
a host shares its artifacts and mappings. Beside them, `covalence/nodes.db`
holds map nodes, `covalence/mapped/` the root whose mappings were last
installed per scope and package, and `covalence/installed/` the root whose
artifacts were too. Restores read Lake's artifact cache without writing to it,
so a one-shot runner does not copy every unpacked artifact. On a host that keeps
its cache, export the same `LAKE_CACHE_DIR`, `LAKE_ARTIFACT_CACHE=true` and
`LAKE_RESTORE_ARTIFACTS=true` for every Lake run, this command's included: Lake then keeps each archive unpacked
in the cache after its first use, and later worktrees restore without unpacking. Nothing
evicts old entries yet.

Set `LAKE_CI_BUCKET`. Credentials, region and endpoint follow the AWS
credential chain (`AWS_REGION`, `AWS_ENDPOINT_URL`, `AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, web identity, profiles or machine
roles). Do not pass credentials on the command line.

Outside AWS, transfers go through the bucket's
[Transfer Acceleration](https://docs.aws.amazon.com/AmazonS3/latest/userguide/transfer-acceleration.html)
endpoint, `<bucket>.s3-accelerate.amazonaws.com`, which must be enabled on the
bucket. Inside AWS they keep the regional endpoint, where acceleration would
only add its fee. `COVALENCE_S3_ACCELERATE` is `auto` (the default), `on` or
`off`. `auto` counts a host as in AWS when an AWS runtime's variables are set
(`AWS_EXECUTION_ENV`, ECS task metadata or roles, EKS Pod Identity) or when
instance metadata answers as EC2's does within 40 ms. Web identity alone does
not count, since CI and other clouds use it too. An `AWS_ENDPOINT_URL` or
profile `endpoint_url` overrides acceleration. GitHub publication
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
