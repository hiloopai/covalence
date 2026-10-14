use std::fmt;

use sha2::Digest;

/// A multicodec table code, naming a codec or a hash function.
///
/// Any code is representable, though a [`Cid`](crate::Cid) holds only codes
/// below 2^63. [`Multicodec::name`] names the codes this crate has constants
/// for, and `Debug` shows the name or the code in hex.
///
/// ```
/// use covalence_lib_cid::Multicodec;
///
/// assert_eq!(format!("{:?}", Multicodec::DAG_CBOR), "dag-cbor");
/// assert_eq!(format!("{:?}", Multicodec(0xb220)), "0xb220");
/// assert_eq!(Multicodec::SHA2_256.digest(b"").map(|d| d.len()), Some(32));
/// assert_eq!(Multicodec::RAW.digest(b""), None);
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Multicodec(pub u64);

impl Multicodec {
    /// Raw bytes.
    pub const RAW: Multicodec = Multicodec(0x55);
    /// `dag-cbor`. DASL CIDs use it for DRISL.
    pub const DAG_CBOR: Multicodec = Multicodec(0x71);
    /// A Git object, with its header.
    pub const GIT_RAW: Multicodec = Multicodec(0x78);
    /// SHA-1, which is not collision resistant.
    pub const SHA1: Multicodec = Multicodec(0x11);
    /// SHA-256.
    pub const SHA2_256: Multicodec = Multicodec(0x12);
    /// SHA-512.
    pub const SHA2_512: Multicodec = Multicodec(0x13);
    /// BLAKE3.
    pub const BLAKE3: Multicodec = Multicodec(0x1e);

    /// The table name of a code this crate has a constant for, or `None`.
    pub const fn name(self) -> Option<&'static str> {
        Some(match self.0 {
            0x11 => "sha1",
            0x12 => "sha2-256",
            0x13 => "sha2-512",
            0x1e => "blake3",
            0x55 => "raw",
            0x71 => "dag-cbor",
            0x78 => "git-raw",
            _ => return None,
        })
    }

    /// The digest of `bytes`, if this is a hash function this crate computes:
    /// SHA-1, SHA-256, SHA-512 or BLAKE3 (32 bytes).
    pub fn digest(self, bytes: &[u8]) -> Option<Vec<u8>> {
        Some(match self {
            Multicodec::SHA1 => sha1::Sha1::digest(bytes).to_vec(),
            Multicodec::SHA2_256 => sha2::Sha256::digest(bytes).to_vec(),
            Multicodec::SHA2_512 => sha2::Sha512::digest(bytes).to_vec(),
            Multicodec::BLAKE3 => blake3::hash(bytes).as_bytes().to_vec(),
            _ => return None,
        })
    }
}

/// The codec and hash function of a CID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Format {
    /// The codec of the content.
    pub codec: Multicodec,
    /// The hash function.
    pub hash: Multicodec,
}

impl Format {
    /// Raw bytes with SHA-256.
    pub const SHA2_256: Format = Format::raw(Multicodec::SHA2_256);
    /// Raw bytes with SHA-512.
    pub const SHA2_512: Format = Format::raw(Multicodec::SHA2_512);
    /// Raw bytes with BLAKE3.
    pub const BLAKE3: Format = Format::raw(Multicodec::BLAKE3);
    /// Raw bytes with SHA-1.
    pub const SHA1: Format = Format::raw(Multicodec::SHA1);
    /// DRISL (`dag-cbor`) with SHA-256: a DASL CID.
    pub const DRISL_SHA256: Format = Format {
        codec: Multicodec::DAG_CBOR,
        hash: Multicodec::SHA2_256,
    };
    /// DRISL (`dag-cbor`) with BLAKE3: a BDASL CID.
    pub const DRISL_BLAKE3: Format = Format {
        codec: Multicodec::DAG_CBOR,
        hash: Multicodec::BLAKE3,
    };
    /// A Git object with SHA-1.
    pub const GIT_SHA1: Format = Format {
        codec: Multicodec::GIT_RAW,
        hash: Multicodec::SHA1,
    };
    /// A Git object with SHA-256.
    pub const GIT_SHA256: Format = Format {
        codec: Multicodec::GIT_RAW,
        hash: Multicodec::SHA2_256,
    };

    const fn raw(hash: Multicodec) -> Format {
        Format {
            codec: Multicodec::RAW,
            hash,
        }
    }
}

/// The table name, or the code in hex.
impl fmt::Debug for Multicodec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(name) => f.write_str(name),
            None => write!(f, "{:#x}", self.0),
        }
    }
}
