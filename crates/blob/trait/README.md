# covalence-blob-trait

The blob trait: bytes read whole or by range.

## Terms

- A blob is a sequence of bytes that can be read whole or by range, such as
  bytes in memory, a file or an object in remote storage.
- A [CID](https://github.com/multiformats/cid) names bytes by their hash. Its
  multihash is the hash function and the digest; its codec says how to read
  the bytes. `covalence-lib-cid` defines both.
- A fact (`Fact` from `covalence-lib-cid`) is bytes together with a CID whose
  digest they were checked to hash to. Facts check the multihash, not the
  codec.

## Blobs

- `Blob` has `size`, `read_range` and `read_all`. Only `size` and
  `read_range` must be written; `read_all` reads the range from 0 to `size`.
- Blobs are untrusted: a backend may report any size and return any bytes.
  Callers that need particular content check it, for example with
  `BlobExt::fact`.
- `Bytes` is a blob whose ranges share its storage.
- `Slice` is a range of another blob. Slices of slices point at the blob under
  them, so they do not nest.
- `Arc<dyn Blob>` is a blob that forwards every method, overrides included.
- Other blobs, such as files, live in their own crates.

## Ranges

`read_range(start, len)` returns:

- `len` bytes from `start`, or fewer only where the blob ends;
- nothing at or past the end;
- bytes up to the end if `start + len` is beyond `u64::MAX`.

Implementations must not return fewer bytes before the end: one that reads in
parts loops until done.

## Checked and unchecked facts

A blob turns its bytes into a fact for a CID in two steps:

- `Blob::fact_unchecked` is the step a blob may override, for example to
  return a fact it already has without hashing again. By default it reads the
  blob once with `read_all`, copies the bytes, and checks the copy against the
  CID. "Unchecked" means no one checks that an override returns a fact for the
  CID asked for.
- `BlobExt::fact` is the checked step. It calls `fact_unchecked` and then
  checks the result. It is defined once for every blob, so no blob can
  override it.

`BlobExt::fact(cid)` returns:

- `Some(fact)`, with `fact.claim() == cid`, when the blob's bytes hash to
  `cid`'s digest. A fact with the same multihash but another codec is
  relabelled to `cid`.
- `None` when `fact_unchecked` returns `None`. By default that means the bytes
  do not hash to `cid`. A blob claims no CID, so other content is not an
  error.
- `Error::Mismatch` when `fact_unchecked` returns a fact with another
  multihash.
- `Error::Check` when the bytes must be checked against `cid` but
  `covalence-lib-cid` cannot compute its digest, such as for an unknown hash
  function or a truncated digest.
- The read's error when the default `fact_unchecked` fails to read. An
  override may handle a failed read differently.

What this does and does not guarantee:

- Every fact is true: only `covalence-lib-cid` builds facts, and it hashes or
  checks the bytes first.
- A fact from `BlobExt::fact` is for exactly the CID asked for.
- The default copies what it reads before hashing, so later writes under the
  blob cannot change the fact.
- An override may still return `None` for content that matches, or a fact
  whose bytes differ from what `read_range` returns. Facts built from
  storage that can change, such as a memory map, are only as stable as that
  storage.

A `Fact` is itself a blob. Asked for a CID with its own multihash, it returns
itself, relabelled, without hashing again; for any other CID it hashes its
bytes and checks them.
