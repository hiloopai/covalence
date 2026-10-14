use std::{fmt::Debug, hash::Hash};

use crate::{Format, Multicodec};

pub use crate::dasl::{Bdasl, Blake3, Dasl, Sha256};

/// How an identifier is stored: bytes that contain a digest. A storage is a
/// unit marker type.
pub trait Storage: Copy + Ord + Hash {
    /// The stored bytes, such as `[u8; 36]`.
    type Bytes: Clone + Debug + Ord + Hash + AsRef<[u8]>;
    /// The digest, such as `[u8; 32]`, or `[u8]` for any length.
    type Digest: ?Sized + Debug + Ord + Hash + AsRef<[u8]>;

    /// The digest in valid stored bytes.
    fn digest(bytes: &Self::Bytes) -> &Self::Digest;
}

/// A [`Storage`] of CIDv1s: the binary CID is the header, then the digest.
/// The stored bytes compare in the order of the binary CIDs.
pub trait CidFormat: Storage + sealed::Sealed {
    /// The CID header in valid stored bytes.
    fn header(bytes: &Self::Bytes) -> &[u8];

    /// Whether this storage holds CIDs of this format and digest length.
    fn holds(format: Format, len: usize) -> bool;

    /// The stored bytes of a CID this storage holds.
    fn store(header: &[u8], digest: &[u8]) -> Self::Bytes;
}

/// A [`CidFormat`] of one codec and hash function, so content has one CID.
pub trait Hashed: CidFormat<Digest: Sized> {
    /// The format.
    const FORMAT: Format;

    /// The digest of `content`.
    fn digest_of(content: &[u8]) -> Self::Digest;
}

pub(crate) mod sealed {
    pub trait Sealed {}
}

/// A minimal unsigned varint of at most 9 bytes, and the rest of `bytes`.
fn varint(bytes: &[u8]) -> Option<(u64, &[u8])> {
    let (value, rest) = unsigned_varint::decode::u64(bytes).ok()?;
    (value < 1 << 63).then_some((value, rest))
}

/// The codec and hash code of a CIDv1, its digest length, and the rest.
pub(crate) fn parse_header(bytes: &[u8]) -> Option<(Format, u64, &[u8])> {
    let (1, rest) = varint(bytes)? else {
        return None;
    };
    let (codec, rest) = varint(rest)?;
    let (hash, rest) = varint(rest)?;
    let (len, rest) = varint(rest)?;
    let format = Format {
        codec: Multicodec(codec),
        hash: Multicodec(hash),
    };
    Some((format, len, rest))
}

/// The codec, hash code and digest of exactly one binary CIDv1.
pub(crate) fn parse(bytes: &[u8]) -> Option<(Format, &[u8])> {
    let (format, len, digest) = parse_header(bytes)?;
    (len == digest.len() as u64).then_some((format, digest))
}

/// The header of a CIDv1, in a buffer of the longest header.
pub(crate) fn header(format: Format, len: usize) -> ([u8; 28], usize) {
    let mut header = [0; 28];
    let mut header_len = 0;
    for code in [1, format.codec.0, format.hash.0, len as u64] {
        let mut buffer = unsigned_varint::encode::u64_buffer();
        let encoded = unsigned_varint::encode::u64(code, &mut buffer);
        header[header_len..header_len + encoded.len()].copy_from_slice(encoded);
        header_len += encoded.len();
    }
    (header, header_len)
}

/// The header of a CIDv1 with one-byte codes and digest length.
pub(crate) const fn short_header(format: Format, len: usize) -> [u8; 4] {
    let (codec, hash) = (format.codec.0, format.hash.0);
    assert!(codec < 0x80 && hash < 0x80 && len < 0x80);
    [1, codec as u8, hash as u8, len as u8]
}

/// `parts` in order, zero-padded to `N` bytes.
pub(crate) fn pad<const N: usize>(parts: &[&[u8]]) -> [u8; N] {
    let mut bytes = [0; N];
    let mut len = 0;
    for part in parts {
        bytes[len..len + part.len()].copy_from_slice(part);
        len += part.len();
    }
    bytes
}

/// A CID storage whose stored bytes are only the digest: one codec, one hash
/// and one digest length.
macro_rules! digest_only {
    ($(#[$doc:meta])* $name:ident, $format:expr, $len:literal) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name;

        impl $crate::Storage for $name {
            type Bytes = [u8; $len];
            type Digest = [u8; $len];

            fn digest(bytes: &[u8; $len]) -> &[u8; $len] {
                bytes
            }
        }

        impl $crate::storage::sealed::Sealed for $name {}

        impl $crate::CidFormat for $name {
            fn header(_: &[u8; $len]) -> &[u8] {
                &const { $crate::storage::short_header($format, $len) }
            }

            fn holds(format: $crate::Format, len: usize) -> bool {
                (format, len) == ($format, $len)
            }

            fn store(_: &[u8], digest: &[u8]) -> [u8; $len] {
                $crate::storage::pad(&[digest])
            }
        }
    };
}

/// A CID storage whose stored bytes are the binary CID, zero-padded to `$len`
/// bytes, for the CIDs that `$holds` accepts.
macro_rules! cid_header {
    ($(#[$doc:meta])* $name:ident, $len:expr, |$codec:pat_param, $hash:pat_param, $digest:pat_param| $holds:expr) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name;

        impl $crate::Storage for $name {
            type Bytes = [u8; $len];
            type Digest = [u8];

            fn digest(bytes: &[u8; $len]) -> &[u8] {
                let (.., len, rest) = $crate::storage::parse_header(bytes).expect("a valid header");
                &rest[..len as usize]
            }
        }

        impl $crate::storage::sealed::Sealed for $name {}

        impl $crate::CidFormat for $name {
            fn header(bytes: &[u8; $len]) -> &[u8] {
                let (.., rest) = $crate::storage::parse_header(bytes).expect("a valid header");
                &bytes[..bytes.len() - rest.len()]
            }

            fn holds(format: $crate::Format, len: usize) -> bool {
                let ($codec, $hash, $digest) = (format.codec, format.hash, len);
                $holds
            }

            fn store(header: &[u8], digest: &[u8]) -> [u8; $len] {
                $crate::storage::pad(&[header, digest])
            }
        }
    };
}

/// A CID storage whose stored bytes are a `$header`-byte CID header and a
/// `$digest`-byte digest, for the codes that `$holds` accepts.
macro_rules! fixed_header {
    ($(#[$doc:meta])* $name:ident, $header:literal + $digest:literal, |$codec:pat_param, $hash:pat_param| $holds:expr) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name;

        impl $crate::Storage for $name {
            type Bytes = [u8; $header + $digest];
            type Digest = [u8; $digest];

            fn digest(bytes: &[u8; $header + $digest]) -> &[u8; $digest] {
                bytes[$header..].try_into().expect("a fixed-length digest")
            }
        }

        impl $crate::storage::sealed::Sealed for $name {}

        impl $crate::CidFormat for $name {
            fn header(bytes: &[u8; $header + $digest]) -> &[u8] {
                &bytes[..$header]
            }

            fn holds(format: $crate::Format, len: usize) -> bool {
                let ($codec, $hash) = (format.codec, format.hash);
                len == $digest && $holds
            }

            fn store(header: &[u8], digest: &[u8]) -> [u8; $header + $digest] {
                assert_eq!(header.len(), $header, "a {}-byte header", $header);
                $crate::storage::pad(&[header, digest])
            }
        }
    };
}

pub(crate) use {cid_header, digest_only, fixed_header};

/// The longest header of a digest under 128 bytes: version, two 9-byte
/// codes, and the length.
const MAX_HEADER: usize = 20;

cid_header!(
    /// Any CIDv1 with a digest of at most 32 bytes, stored inline.
    Packed32,
    MAX_HEADER + 32,
    |_, _, len| len <= 32
);
cid_header!(
    /// Any CIDv1 with a digest of at most 64 bytes, stored inline.
    Packed64,
    MAX_HEADER + 64,
    |_, _, len| len <= 64
);
