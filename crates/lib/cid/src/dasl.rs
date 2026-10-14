use sha2::Digest;

use crate::{
    Cid, Format, Hashed, Multicodec,
    storage::{digest_only, fixed_header, pad, short_header},
};

/// A DASL codec. Variants are in code order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Codec {
    /// Raw bytes.
    Raw,
    /// DRISL, under the `dag-cbor` code.
    Drisl,
}

impl Codec {
    /// The multicodec code.
    pub const fn code(self) -> Multicodec {
        match self {
            Codec::Raw => Multicodec::RAW,
            Codec::Drisl => Multicodec::DAG_CBOR,
        }
    }

    /// The codec with this code.
    pub const fn from_code(code: Multicodec) -> Option<Self> {
        match code {
            Multicodec::RAW => Some(Codec::Raw),
            Multicodec::DAG_CBOR => Some(Codec::Drisl),
            _ => None,
        }
    }
}

/// A DASL hash function, with a 32-byte digest. Variants are in code order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HashFn {
    /// SHA-256.
    Sha256,
    /// BLAKE3, with a 32-byte output.
    Blake3,
}

impl HashFn {
    /// The multicodec code.
    pub const fn code(self) -> Multicodec {
        match self {
            HashFn::Sha256 => Multicodec::SHA2_256,
            HashFn::Blake3 => Multicodec::BLAKE3,
        }
    }

    /// The hash function with this code.
    pub const fn from_code(code: Multicodec) -> Option<Self> {
        match code {
            Multicodec::SHA2_256 => Some(HashFn::Sha256),
            Multicodec::BLAKE3 => Some(HashFn::Blake3),
            _ => None,
        }
    }

    /// The digest of `bytes`.
    pub fn digest(self, bytes: &[u8]) -> [u8; 32] {
        match self {
            HashFn::Sha256 => sha2::Sha256::digest(bytes).into(),
            HashFn::Blake3 => blake3::hash(bytes).into(),
        }
    }
}

fixed_header!(
    /// A strict DASL CID: a [`Codec`], SHA-256 and a 32-byte digest, stored as
    /// the 36-byte binary CID.
    Dasl,
    4 + 32,
    |codec, hash| Codec::from_code(codec).is_some() && hash == Multicodec::SHA2_256
);

fixed_header!(
    /// A BDASL CID: a [`Codec`], a [`HashFn`] and a 32-byte digest, stored as
    /// the 36-byte binary CID.
    Bdasl,
    4 + 32,
    |codec, hash| Codec::from_code(codec).is_some() && HashFn::from_code(hash).is_some()
);

digest_only!(
    /// Raw CIDs (the `raw` codec) with a BLAKE3 hash, stored as the digest.
    Blake3,
    Format::BLAKE3,
    32
);

digest_only!(
    /// Raw CIDs (the `raw` codec) with a SHA-256 hash, stored as the digest.
    Sha256,
    Format::SHA2_256,
    32
);

impl Cid<Dasl> {
    /// The CID of `bytes`, with SHA-256.
    pub fn of(codec: Codec, bytes: &[u8]) -> Self {
        let header = short_header(
            Format {
                codec: codec.code(),
                hash: Multicodec::SHA2_256,
            },
            32,
        );
        Cid::stored(pad(&[&header, &HashFn::Sha256.digest(bytes)]))
    }

    /// The same digest with `codec`.
    pub fn with_codec(self, codec: Codec) -> Self {
        let mut bytes = self.bytes;
        bytes[1] = codec.code().0 as u8;
        Cid::stored(bytes)
    }
}

impl Cid<Bdasl> {
    /// The CID with this codec, hash function and digest.
    pub fn from_digest(codec: Codec, hash: HashFn, digest: [u8; 32]) -> Self {
        let header = short_header(
            Format {
                codec: codec.code(),
                hash: hash.code(),
            },
            32,
        );
        Cid::stored(pad(&[&header, &digest]))
    }

    /// The CID of `bytes`.
    pub fn of(codec: Codec, hash: HashFn, bytes: &[u8]) -> Self {
        Self::from_digest(codec, hash, hash.digest(bytes))
    }

    /// The same hash and digest with `codec`.
    pub fn with_codec(self, codec: Codec) -> Self {
        let mut bytes = self.bytes;
        bytes[1] = codec.code().0 as u8;
        Cid::stored(bytes)
    }
}

impl Hashed for crate::storage::Blake3 {
    const FORMAT: Format = Format {
        codec: Multicodec::RAW,
        hash: Multicodec::BLAKE3,
    };

    fn digest_of(content: &[u8]) -> [u8; 32] {
        HashFn::Blake3.digest(content)
    }
}

impl Hashed for crate::storage::Sha256 {
    const FORMAT: Format = Format {
        codec: Multicodec::RAW,
        hash: Multicodec::SHA2_256,
    };

    fn digest_of(content: &[u8]) -> [u8; 32] {
        HashFn::Sha256.digest(content)
    }
}
