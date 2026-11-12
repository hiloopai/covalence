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

- `modules/<owner>/<repo>/<commit>/<package>` holds, as Zstandard JSON
  `{"modules": {<module>: <input>}}`, the Lake input each module of that
  package revision had in the recorded build, read from the module traces
  Lake left in the build folder. It is written before the revision pointer.
  A targeted `get` uses it to prefetch (below); nothing else reads it.

A repository can hold several Lake workspaces. One below the repository's
root, such as `projects/smt-axioms`, keeps its revision maps, module indexes
and seeds under `<owner>/<repo>/<folder>` instead of `<owner>/<repo>`, so a
path dependency on the root (`../..`) never overwrites the root workspace's
map of it and its seeds take their own two slots. Git dependency pins stay
shared. Only the root workspace publishes the commit's source tree.

Lake artifact IDs are 64-bit hashes over outputs, so artifacts from different
repositories coincide when their bytes do. Input hashes include no checkout
path. A revision map, not the pool, says which output an input has: two
builds of one input can differ, for example in logged messages.

Patched and stock `leantar` both write `LTR4` archives and report the same
version, and stock `leantar` decodes the patched opcodes `0xa0` and `0xb0` as
constructors instead of rejecting them. The executable is therefore part of
the scope.

## Commands

Before `build`, and before `get` or `dependencies` with targets, each
command fails if a module belongs to more than one Lake library of a
`lakefile.toml` package, and names each such module and its libraries. Lake
counts a module as a library's own when a glob matches it, or when a root is
a prefix of it and a glob matches that root, so a library's default root,
its name, claims every module under it. A build reaches a shared module
through either library and takes that library's `needs` and options, so the
module's trace, and the input hash the cache is keyed by, differ between
runs.

- `build` runs `lake build --wfail` once on the given targets (pass every
  target CI builds, such as executables, so they are published), records the root package's outputs,
  then replays the build once per other package, path and Git
  dependencies alike, to record its outputs too. A Git dependency whose
  pin's map in the pool already holds the input of every trace in its build
  folder is recorded from that map instead of replayed, and `put` leaves it
  as it is. Lake's records leave out
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
- `gc` frees the host's cache of toolchain scopes it stopped using (below).
- `scope --base <rev> -- <targets>` prints the part of `targets` that the
  change from `rev` to the working tree can affect, for CI that checks only
  what a pull request changed. A changed module reaches every module that
  imports it, through this repository's packages. A changed file that a
  module reads with `include_str` or `include_bytes` reaches that module, and
  a changed `needs` input reaches its library's modules. A library prints as
  its affected modules (`+Module`), less those another one imports. An
  executable or module prints when its import closure is affected. A change
  to a `lakefile`, `lake-manifest.json` or `lean-toolchain`, a package
  without `lakefile.toml`, or a changed file none of these rules places
  prints every target. `--exclude <glob>` (Git glob syntax, from the
  repository's root) names files Lake never reads, such as `docs/**`.
- With targets, `get` and `dependencies` install only mappings (`get` also
  those of the dependencies). `get` then runs `lake build --wfail` on the
  targets, which fetches only the artifacts they need. Before the build, `get`
  reads the targets' import closure from the sources (the `import` lines of
  each file, resolved in the packages' source folders and the root
  `lakefile.toml`'s libraries and executables), looks each module up in the
  restored revision's module index and places those entries' files as an
  eager restore does. A module whose input changed since misses, and Lake
  fetches or builds it as before. Lake takes each artifact from its local
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
objects it names, as 16 Zstandard tar streams. Executables stay in the pool:
a library restore needs none, and a build that does fetches each in one
request. An output Lake kept as its
archive alone is decoded first, so no host the seed fills unpacks it. The seed
also holds every mapping of each upstream Mathlib/CSLib snapshot the
publisher fetched, with the artifacts it has, and its slot's `upstream.json`
names those snapshots; a targeted restore it fills does not fetch them from
upstream. A snapshot with any mapping missing is left out. Each repository keeps two seeds
per scope under `lake/<scope>/seed/<owner>/<repo>/{a,b}/`; a publish overwrites
the older slot, its chunks first and its `manifest.json` (commit, publication
time, each chunk's SHA-256, length and file count) last. Storage stays at two
seeds per repository and scope, with nothing to delete; a toolchain change
leaves its scope's seeds behind with the rest of that scope.

`get --seed` on a host that never installed the repository's mappings
downloads a seed while the Git dependencies check out and the upstream
snapshots install: the restored commit's,
else the nearest seeded commit it searched, else the newest seed, since the
slots turn over with every publish and an older checkout rarely has a seeded
ancestor. Any seed is sound: Lake checks every trace, and the pool's maps add
the inputs it lacks. Without one, it says so and restores from the pool. A seed holds the whole recorded build, so pass
`--seed` where the host keeps its cache or builds most of it, such as a
developer machine or a full-library check, and not for a one-shot runner that
needs a few targets. It unpacks the 16 streams at once beside Lake's cache,
checks each against the manifest, then moves the files in: artifacts the cache
lacks, and every output file, after the upstream snapshots that would rewrite
them. The pool's maps then install only the inputs the seed does not map. Lake
still checks every trace against the sources. A seed that is missing, or fails
its check because a publish overwrote it mid-read, is reported and the restore
continues from the pool.

## Internal scope

Prove workers, pull requests and people's machines are internal hosts. Each
publishes the outputs its Lake built itself, committed or not, keyed by Lake's
own input hash, and takes any other internal host's:

```sh
lake build Demo                          # any build, with LAKE_ARTIFACT_CACHE=true
cov-lake-cache put --internal            # publish what this host built
cov-lake-cache get --internal -- Demo    # elsewhere: Lake takes them instead of building
```

They live under `lake-int/<os>-<arch>/<toolchain scope>/`: `out/<package>/<input>.json`
holds Lake's output record and each artifact's content ID, and `cas/` the
artifacts `main`'s store lacks. A lifecycle rule deletes them 14 days after they
are written; a missing entry or artifact only makes Lake build. `--internal`
writes nothing else: no revision map, seed, build manifest, source tree, Git
pack or toolchain, and it is refused in `main`'s CI.

Lake asks for the outputs of each input its cache lacks through the bundled
overlay, which reads `LAKE_OUTPUTS_ENDPOINT`; stock Lake never asks, and builds.

### Adoption on `main`

`main`'s CI reads the internal scope only by adopting outputs:

```sh
cov-lake-cache build --adopt -- Demo     # Lake takes internal outputs; lists them
cov-lake-cache adopted > modules.txt     # the modules those outputs belong to
cov-kernel-checker lean-check replay-built --project . --modules modules.txt
cov-lake-cache put --replayed .lake/declaration-replay/last-run.json
```

`put` publishes a build that adopted outputs only when that replay ran after the
build and passed for every module an adopted input belongs to. The replay proves
each adopted `.olean`'s declarations sound in the Lean kernel. It does not prove:

- that the `.olean` is what `main`'s source elaborates to: an internal host could
  publish other statements, with valid proofs, for the same input;
- anything about native code: an adopted input's `.c`, `.ir` and objects reach
  `main`'s pool, and executables linked from them, as an internal host built them.

Both are accepted under the internal hosts' trust.

## Toolchain

A `lean-toolchain` naming a Lean release or nightly
(`leanprover/lean4:v4.22.0`, `leanprover/lean4:nightly-2026-11-05`) runs from
elan's directory for it, `$ELAN_HOME/toolchains/<elan's name>` (`~/.elan`
without `ELAN_HOME`), without elan. `ELAN_TOOLCHAIN` overrides the file, as
for elan. When that directory is missing, `get`, `dependencies` and `build`
install it while the Git dependencies check out and the seed downloads, and
only Lake runs wait for it: from the pool, else from the release archive elan
downloads (found through `https://release.lean-lang.org`). A toolchain
already there is used as it is. Under GitHub Actions they then add the
toolchain's `bin` to `GITHUB_PATH`, so later steps find `lake` and `lean`
without elan or `leanprover/lean-action`. `LAKE`, or any other toolchain name,
runs that Lake as found and installs nothing.

`put` publishes the release archive the first time a toolchain is used,
under `lake/toolchains/`:

- `cas/sha2-256/<digest>/bytes.zst`: the release's `.tar.zst`, byte for byte,
  which is a Zstandard frame of the tar under whose SHA-256 CID it is kept.
  Every read checks it before the toolchain is moved into place.
- `<os>-<arch>/<elan's name>`: the JSON record `{"url", "tar", "version"}`:
  the archive's URL, the tar's CID and `lean --version`, written after the
  archive.

The record's `lean --version` and the bundled `leantar` name the pool scope,
so a restore looks up its build and downloads its seed before the toolchain
arrives; once it has, its `lean --version` must match the record. The pool
holds the release's bytes only: the bundled `leantar` and the Lake overlay
are installed into the unpacked toolchain on each host.

A host keeps each pool archive it installs beside Lake's cache, at
`covalence/toolchains/<elan's name>/<tar CID>.tar.zst`, one per toolchain.
When the toolchain directory is missing but that copy is there, as on a
runner whose Lake cache sits on a sticky disk, the toolchain is only unpacked
from it, with the same check; a copy that fails it is reported, deleted and
fetched from the pool again. `gc` deletes the copies of toolchains no kept
scope uses.

Install the toolchain itself on the runner's local disk. A toolchain on lazily
loaded network storage, such as a Blacksmith sticky disk, costs seconds on its
first reads inside Lake; keep `ELAN_HOME` off it. One archive read in order
costs far less there than thousands of files.

## Host-wide local layer

Before any Lake run, `dependencies`, `build` and `get` check out each Git
dependency missing from the packages directory from a shallow bare mirror in
`covalence/git/` under Lake's cache directory. The mirror fetches only pinned
commits it lacks, at depth one, and different mirrors fetch at once;
checkouts borrow its objects through Git alternates. A mirror takes a pinned
commit from the pool first: `put` publishes each pin it lacks as one Git pack
of the commit and its tree, at `lake/git/<owner>/<repo>/<commit>.pack`, and
`git index-pack` hashes every object as the mirror takes it in. Only a pin the
pool lacks is fetched from its Git host. Lake then finds each package at its pinned revision and clones
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
in the cache after its first use, and later worktrees restore without unpacking.

Every toolchain bump starts a new scope, and Lake keeps every scope's files in
the one cache directory. Each command records the scope it uses, with the
project's `lean-toolchain`, under `covalence/scopes/`. `cov-lake-cache gc`
keeps the scopes used within `COVALENCE_LAKE_GC_DAYS` days (14 unless set),
the most recently used one, and every other scope of their toolchains. For
the rest it deletes the output records that only their installed maps, or the
upstream snapshots of their toolchains, name. It also deletes the artifacts
those name that no remaining record, kept map or kept snapshot names, along
with the scopes' own state. Records of local builds belong to no map and stay.
`gc` runs only while no other `cov-lake-cache` command holds the cache, so it
never deletes the scope in use. After every `get`, `gc` runs by itself when
the cache's file system has less than `COVALENCE_LAKE_GC_FREE_PERCENT` percent
free (10 unless set; 0 turns this off). A scope that a host used before these
records existed counts as used at its first `gc`.

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
instance metadata answers as EC2's does within 40 ms. A web identity token
counts only when an EKS cluster issued it (`https://oidc.eks.<region>.amazonaws.com`),
since CI and clusters elsewhere federate into AWS with their own issuers. An `AWS_ENDPOINT_URL` or
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
It needs no Lean: `get`, `dependencies` and `build` install the toolchain
(below). Every command that runs Lake first installs the bundled `leantar`,
which sits beside the executable, into the toolchain. The archive records the upstream
commits in `LEANTAR-REVISIONS`, their combined source tree in `LEANTAR-TREE`,
and all license notices. It combines upstream master with the pinned, unmerged
[explicit-stack fix](https://github.com/digama0/leangz/pull/22). The codec supports
embedded NUL strings, scalar arrays, and deeply nested data that older Lean
distributions cannot safely archive. It changes no Lean sources or kernel checks.

The archive also carries Lake overlays under `lake-overlay/<githash>/`: the
Lake files of a toolchain, rebuilt from a Lake that walks a no-op build faster
([lean4-lake-parallel](https://github.com/hiloopai/lean4-lake-parallel),
branches `lake-overlay/<nightly>`), at their paths under `lean --print-prefix`.
`LAKE-OVERLAYS` names each one's release and SHA-256. Every command that runs Lake
first installs the overlay whose `<githash>` is the project
toolchain's `lean --githash`; other toolchains keep their own Lake. The first
install keeps each replaced file under `lib/lean/.lake-overlay-stock/`, and
`LAKE_POOL_STOCK_LAKE=1` puts those back instead of installing. The overlay
changes how Lake walks a build, not its traces or outputs, so it is not part of
the scope: patched and stock hosts share the pool. A toolchain that already
holds the overlay's files is left unwritten, so an image can unpack the overlay
into a read-only toolchain.

Replacing the archive tool changes the scope and invalidates publication
receipts. Source installations must supply a compatible archiver themselves. Thomas owns the bundled upstream
pin; remove it once the supported Lean distribution includes
[leangz#12](https://github.com/digama0/leangz/pull/12),
[leangz#15](https://github.com/digama0/leangz/pull/15), and
[leangz#22](https://github.com/digama0/leangz/pull/22).

Run `cargo test -p covalence-lean-cache --features lake-tests` with the pinned
Lean release on `PATH` for the signed S3 server and real Lake integration
tests. They run that release through `LAKE`, except one, which installs it
from its release archive and then from the test pool. The umbrella CI calls a separate workflow for these tests so library
developers do not need to install Lean to run `glu ci`.

Maintainer: Thomas. Retire this command when Lake natively provides bounded S3
transfers and publication from a successful recorded build without rebuilding.
