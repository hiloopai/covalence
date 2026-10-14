//! Git object IDs as CIDs. The block is a `git-raw` block: the inflated Git
//! object `kind SP length NUL content` (kind blob, tree, commit or tag; length
//! in minimal ASCII decimal). The digest is its SHA-1 or SHA-256 object ID.

use sha2::Digest;

use crate::{
    Cid, Format, Hashed, Multicodec,
    storage::{cid_header, fixed_header},
};

fixed_header!(
    /// A Git object ID with SHA-1 (20 bytes), stored as the binary CID.
    GitSha1,
    4 + 20,
    |codec, hash| codec == Multicodec::GIT_RAW && hash == Multicodec::SHA1
);

fixed_header!(
    /// A Git object ID with SHA-256 (32 bytes), stored as the binary CID.
    GitSha256,
    4 + 32,
    |codec, hash| codec == Multicodec::GIT_RAW && hash == Multicodec::SHA2_256
);

cid_header!(
    /// A Git object ID with SHA-1 (20 bytes) or SHA-256 (32 bytes), stored as
    /// the binary CID, zero-padded.
    Git,
    36,
    |codec, hash, len| codec == Multicodec::GIT_RAW
        && matches!((hash, len), (Multicodec::SHA1, 20) | (Multicodec::SHA2_256, 32))
);

impl Hashed for GitSha1 {
    const FORMAT: Format = Format::GIT_SHA1;

    fn digest_of(object: &[u8]) -> [u8; 20] {
        sha1::Sha1::digest(object).into()
    }
}

impl Hashed for GitSha256 {
    const FORMAT: Format = Format::GIT_SHA256;

    fn digest_of(object: &[u8]) -> [u8; 32] {
        sha2::Sha256::digest(object).into()
    }
}

/// Git object IDs, computed from the object with its header.
///
/// ```
/// use covalence_lib_cid::{Cid, Git};
///
/// // The ID Git gives a file holding "hello\n".
/// let id = Cid::<Git>::sha1(b"blob 6\0hello\n");
/// assert_eq!(id.to_string(), "baf4bcfgoae3ckaylvdn2sbxxk2lh7hu4uokemsq");
/// let hex: String = id.digest().iter().map(|b| format!("{b:02x}")).collect();
/// assert_eq!(hex, "ce013625030ba8dba906f756967f9e9ca394464a");
/// ```
impl Cid<Git> {
    /// The SHA-1 ID of `object`, a Git object with its header.
    pub fn sha1(object: &[u8]) -> Self {
        Cid::<GitSha1>::of(object).into()
    }

    /// The SHA-256 ID of `object`, a Git object with its header.
    pub fn sha256(object: &[u8]) -> Self {
        Cid::<GitSha256>::of(object).into()
    }
}
