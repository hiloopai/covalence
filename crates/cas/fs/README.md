# covalence-cas-fs

Build immutable filesystem directories from named child links. Every directory
is one block. Build bottom-up by passing the CIDs of already named children.

- A [CID](https://dasl.ing/cid.html) identifies bytes by codec, hash function
  and digest. [BDASL](https://dasl.ing/bdasl.html) extends DASL CIDs to BLAKE3
  as well as SHA-256.
- [DASL](https://dasl.ing/) specifies content addressing and a data model that
  includes byte strings, lists and links.
- [DRISL](https://dasl.ing/drisl.html) is DASL's deterministic encoding of
  values. Its CIDs use the `dag-cbor` codec.
- A content-addressed store (CAS) stores blocks by content hash. The
  [`Cas`](https://docs.rs/covalence-cas-trait/latest/covalence_cas_trait/trait.Cas.html)
  trait supplies writes and checked reads by CID.

## Build and read

`Directory::new` sorts and validates one list of entries. `Directory::cid`
computes its CID without a CAS. `Trees::store` writes and checks readback of
that one block. `Trees::insert` does the same and adds a stable root index to
the table on success. A failed insert leaves the table unchanged and can
leave the block in the CAS. Root indices live only in their issuing table.

`Trees::load` checks the requested CID and decodes one directory. It returns
`None` for a missing block and an error for invalid bytes or a failed read.
None of these operations follows links. A known or retained root gives no
guarantee that its descendants exist, that links point to blocks of the
declared kind, or that the CAS provides durable storage. Use stored child CIDs
at each step when the whole tree needs to be available.

```rust
use std::sync::Arc;
use covalence_cas_fs::{Directory, Entry, EntryKind, LeafMode, Trees};
use covalence_cas_memory::MemoryCas;
use covalence_lib_cid::Blake3;

let subdir = Directory::new(LeafMode::Bytes, vec![Entry {
    name: b"message\xff".as_slice().into(),
    kind: EntryKind::Leaf,
    cid: Blake3::of(b"hello").into(),
}])?;
let root = Directory::new(LeafMode::Bytes, vec![Entry {
    name: "docs".into(),
    kind: EntryKind::Directory,
    cid: subdir.cid()?,
}])?;

let mut trees = Trees::new(Arc::new(MemoryCas::default()));
let id = trees.insert(&root)?;
assert_eq!(trees.root(id), Some(root.cid()?));
assert_eq!(trees.load(root.cid()?)?, Some(root));
assert_eq!(trees.load(subdir.cid()?)?, None);
trees.store(&subdir)?;
assert_eq!(trees.load(subdir.cid()?)?, Some(subdir));
# Ok::<(), covalence_cas_fs::Error>(())
```

## Directory format v1

The exact DRISL value is `["covalence.fs.dir.v1", mode, children]`. `children`
is a list of `[name, kind, link]` entries. A directory's CID is CIDv1 with the
`dag-cbor` codec, BLAKE3 and a 32-byte digest (`01 71 1e 20` in hex followed
by the digest).

- `name` is a byte string. Names are unique and sorted in unsigned
  lexicographic byte order. Empty names, `.` and `..`, slash and NUL are
  invalid. Other bytes are preserved, with no Unicode or host-specific
  normalization. These rules describe one component in the format.
- `kind` is `0` for a leaf or `1` for a directory. A directory link requires
  the same codec and hash as the enclosing directory's CID, in either mode.
  Each directory declares its own leaf mode.
- `mode` is `0` for raw bytes (`LeafMode::Bytes`, codec `0x55`) or `1` for a
  DASL value (`LeafMode::Dasl`, codec `0x71`). Leaf CIDs use BLAKE3 or SHA-256
  with a 32-byte digest. For example, `Blake3::of(b"hello")` names raw bytes
  and `Value::Bytes("hello".into()).cid()` names the canonical DRISL byte
  string. The latter hashes its encoded bytes, including the DRISL header.
  A DRISL leaf can hold any value supported by `covalence-lib-dasl`. The
  caller supplies leaf CIDs and is responsible for their bytes. The CAS may
  support a smaller set of hashes than this link format does.
- `link` is a DRISL link (CBOR tag 42 on a byte string containing `00` and
  the binary CIDv1). The declared kind and codec describe how to interpret a
  target when it is read. A CID by itself cannot prove a target's schema or
  distinguish a directory from another DRISL value.

This is a Covalence-specific flat layout with only name, kind and CID on
each edge. The public Silmaril [Git filesystem profile] uses Git modes and
width-32 Prolly blocks. It shares the byte-component rules and BLAKE3
directory CID envelope but has a different wire format and different root
CIDs. Silmaril's [readback contract] also covers a complete published tree,
whereas this crate checks one directory at a time.

[Git filesystem profile]: https://github.com/hiloopai/silmaril-lean/blob/686835ad7d9abb6158d8fff0d3e759ce1ca704d6/docs/git-filesystem.md
[readback contract]: https://github.com/hiloopai/silmaril-lean/blob/686835ad7d9abb6158d8fff0d3e759ce1ca704d6/docs/git-filesystem-readback.md
