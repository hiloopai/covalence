# covalence-lib-cid

Content identifiers (CIDs): names for bytes computed from the bytes.

## Terms

- A [CID](https://github.com/multiformats/cid) names content by its hash. This
  crate handles CIDv1 only: a version, a codec and a multihash.
- A [multicodec](https://github.com/multiformats/multicodec) is a number from a
  shared table that names a format or a hash function, such as `raw` (0x55),
  `dag-cbor` (0x71) or `sha2-256` (0x12).
- The codec says how to read the content, such as `raw` for plain bytes.
- A [multihash](https://github.com/multiformats/multihash) is a hash
  function's code, the digest length and the digest. It says which bytes the
  CID names; the codec only says how to read them.
- [DASL CIDs](https://dasl.ing/cid.html) are a strict subset: the `raw` or
  `dag-cbor` codec, SHA-256 and a 32-byte digest.
  [BDASL](https://dasl.ing/bdasl.html) also allows BLAKE3.
- [DRISL](https://dasl.ing/drisl.html) is DASL's deterministic profile of CBOR.
  DASL CIDs use the `dag-cbor` code for it.
- A fact is bytes together with a claim, such as a CID, that was checked to
  hold of them.

## CIDs

- `Cid<S>` is a CID stored in the layout `S`, a storage. Each storage holds
  some CIDs and rejects the rest with `Error::Unsupported`:
  - `Cid32` (the default `Cid`) and `Cid64`: any CIDv1 with a digest of at
    most 32 or 64 bytes.
  - `Bdasl`: BDASL CIDs. `Dasl`: DASL CIDs.
  - `Blake3` and `Sha256`: raw CIDs of one hash function, stored as just the
    32-byte digest.
  - `GitSha1`, `GitSha256` and `Git` (either): Git object IDs, with the
    `git-raw` codec.
- Every storage converts to `Cid64` with `From`, and every storage but
  `Cid64` converts to `Cid32` with `From`. `Cid32` and `Cid64` convert to
  each storage with `TryFrom`. `Cid::cast` converts between any two storages,
  where the target holds the CID.
- `cid.raw_blake3()` and `cid.raw_sha256()` give the raw CID with the same
  hash function and 32-byte digest, discarding the codec, or `None` for other
  hashes.
- `cid.same_multihash(&other)` says whether two CIDs have the same hash
  function and digest, whatever their codecs.
- Codes must be below 2^63, the most a 9-byte varint holds.
- Text is `b` and lowercase, unpadded base32 of the binary form. Parsing
  accepts exactly what `Display` writes.
- `Ord` is the order of the binary forms.

## Facts

- `Fact::new` checks that bytes hash to a CID's digest. It does not check the
  codec, so the bytes need not be valid for it.
- `Fact::of` hashes bytes into a fact for a storage with one codec and hash
  function, such as `Blake3`.
- `Fact::with_codec`, `cast`, `widen` and `raw_blake3` relabel a fact
  without hashing again. They keep its hash function, digest and bytes.
- This crate computes SHA-1, SHA-256, SHA-512 and 32-byte BLAKE3. Checking any
  other hash, or a digest of another length, fails with `Error::Hash`.
- SHA-1 is not collision resistant: two different byte strings can be facts
  for one SHA-1 CID.

## Other identifiers

`O256` is a 32-byte identifier with no CID form: a BLAKE3 hash (plain, keyed
or derived) or a random GUID. It is also the key of keyed BLAKE3, and
`Blake3::into_o256` converts a raw BLAKE3 CID to one.

```rust
use covalence_lib_cid::{Blake3, Cid, O256};

let cid = Blake3::of(b"hello");
let any: Cid = cid.into();
assert_eq!(any.raw_blake3(), Some(cid));
assert_eq!(cid.into_o256(), O256::from_bytes(b"hello"));
```
