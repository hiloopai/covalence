# Lake build cache

`cov-lake-cache` shares successful [Lake](https://github.com/leanprover/lean4/tree/master/src/lake)
builds through S3-compatible storage. It uses Covalence's S3 transport for
credentials, pooled connections, retries and bounded multipart transfers.
Lake keeps responsibility for artifact hashes and build dependency validation.

This command targets Lean 4.37's `2026-03-17` output mapping format. Changes
to Lake's download protocol fail explicitly and need compatibility tests.

```sh
cov-lake-cache dependencies -- Some.Required.Target
cov-lake-cache get --repository example/proofs
# Or fetch only what one target needs:
cov-lake-cache get --repository example/proofs -- MyLibrary.Module
cov-lake-cache build --repository example/proofs -- MyLibrary
# Run the repository's remaining correctness checks, then on trusted main CI:
cov-lake-cache put --repository example/proofs
```

- `dependencies` fetches the Mathlib/CSLib snapshot used by a project with
  those dependencies. Targets select mappings first, then a strict Lake build.
- `build` runs `lake build --wfail` once, records outputs, and saves a success
  receipt. It streams each artifact through Zstandard level one into the record
  directory, so publication does not repeat compression. A failed build removes
  the previous receipt. Recording requires a clean checkout; put generated output
  under an ignored directory.
- `put` requires that receipt from the same revision, pins, platform and CI
  run. It never builds. It lists the scope, streams missing or wrong-size
  compressed artifacts, then publishes their length manifest, the full Lake
  mapping and latest revision pointer.
- `get` downloads through Lake, which validates each artifact's content hash.
  A missing pointer is a cache miss. Other failures are errors; callers may
  explicitly report a failed optional restore and run the full check instead.
- `get -- <targets>` fetches only mappings, from this cache and, when the
  project uses Mathlib, from the dependency snapshot. It then runs
  `lake build --wfail` on the targets. Lake takes each artifact from its local
  cache, else downloads it the first time the build needs it, else builds the
  module. Without a published snapshot it still restores the dependencies and
  builds the targets. Later builds use the local cache with no remote requests.

Set `LAKE_CI_BUCKET` and optionally `LAKE_CI_PREFIX`. Credentials, region and
endpoint follow the AWS credential chain (`AWS_REGION`, `AWS_ENDPOINT_URL`,
`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, web identity,
profiles or machine roles). Do not pass credentials on the command line.
GitHub publication requires a main push or manual main run in the named
repository. Bucket permissions remain the actual write authorization boundary.

Scopes separate repositories, packages, platform, toolchain, archive executable, wire format and dependency
pins. Lake hashes are 64-bit artifact IDs, not Covalence CIDs. Objects therefore
use the raw key-value store. Size inventories detect missing and truncated
objects; they are not remote content audits. Readers check content hashes.
Use one writer per scope; concurrent listing is not a consistent snapshot.
Only trusted writers may publish mappings and artifacts.

While Lake runs under `get`, this command stands in for `curl`. It serves
its own cache URLs and passes requests for other services, such as Mathlib's
cache, to `curl` unchanged. A target build therefore also uses the dependency
services named in Mathlib's Lake configuration.

Sixteen file transfers run at once, with at most two 8 MiB parts each. Upload
payload buffers are limited to 256 MiB. Native Lake can use additional memory
while validating and unpacking. Inventories retain only referenced filenames,
and publication reports bytes, object counts and phase times as JSON.

Compression uses the workspace's existing Zstandard dependency. Downloads stream
directly into temporary Lake artifacts; each decoder has an 8 MiB window limit
and enforces the manifest's exact decoded length. Lake checks the decoded content
hash before installing it. The `zstd-v1` scope separates this representation from
raw artifacts. Prepared bodies occupy extra disk space in the record directory;
the next successful preparation replaces them. `preparation.json` reports their
byte count and compression time separately from `publication.json`.

`--root`, `--package` and `--record` select a project, dependency package and
record directory. `--package` changes which outputs are recorded, preserving
Lake's default build targets unless targets follow `--`. To build a dependency's
own defaults, pass `-- @dependency` explicitly. Packages outside the selected
targets' dependency closure produce an empty snapshot: they have no cached checks.
`LAKE` selects the Lake executable. Installation should use
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

The cache scope hashes the actual `leantar` executable: readers with a different
archive tool get a cache miss, and replacing the tool invalidates old publication
receipts. Existing snapshots remain in their old scopes. Source installations
must supply a compatible archiver themselves. Thomas owns the bundled upstream
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
