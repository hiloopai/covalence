#![doc = include_str!("../README.md")]
#![deny(missing_docs)]

mod directory;

use std::sync::Arc;

use covalence_cas_trait::{Cas, GetExt};
use covalence_lib_cid::{Bdasl, Cid, Format};
use covalence_lib_data::Bytes;
use covalence_lib_error::snafu::{OptionExt, Snafu, ensure};

pub use directory::{Directory, Entry, EntryKind, LeafMode};

/// An opaque slot index issued by a [`Trees`] instance.
///
/// Use the ID with its issuing table. Another table can have a different root
/// at the same index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TreeId(u32);

/// An append-only, in-memory table of immutable directory roots on a CAS.
///
/// IDs are dense and stable for the life of this table. Save the root CIDs to
/// reopen directories later with [`Trees::load`].
pub struct Trees {
    cas: Arc<dyn Cas>,
    roots: Vec<Cid<Bdasl>>,
}

impl Trees {
    /// Create an empty root table on `cas`.
    pub fn new(cas: Arc<dyn Cas>) -> Self {
        Self {
            cas,
            roots: Vec::new(),
        }
    }

    /// Store one directory and append a root slot after successful readback.
    ///
    /// Each success assigns a new ID, even for a repeated CID. If all `u32`
    /// slots are used, returns [`Error::Full`] before accessing the CAS.
    /// A failed call can leave the block in the CAS.
    pub fn insert(&mut self, directory: &Directory) -> Result<TreeId, Error> {
        let slot = u32::try_from(self.roots.len()).ok().context(FullSnafu)?;
        let root = self.store(directory)?;
        self.roots.push(root);
        Ok(TreeId(slot))
    }

    /// Store one directory and return its portable CID without assigning a slot.
    ///
    /// The CAS must return the BLAKE3 multihash of the block and serve a checked
    /// read at the directory CID. Children are neither fetched nor stored.
    pub fn store(&self, directory: &Directory) -> Result<Cid<Bdasl>, Error> {
        let fact = directory.block()?;
        let cid = *fact.claim();
        let key = self.cas.put_fact(fact.widen())?;
        let request = cid.into();
        ensure!(key.same_multihash(&request), RekeyedSnafu { cid, key });
        ensure!(
            self.cas.get_fact(&request)?.is_some(),
            MissingAfterWriteSnafu { cid }
        );
        Ok(cid)
    }

    /// Look up a root slot without accessing the CAS.
    ///
    /// Returns `None` if the index is out of bounds.
    pub fn root(&self, id: TreeId) -> Option<Cid<Bdasl>> {
        self.roots.get(id.0 as usize).copied()
    }

    /// Fetch, check and decode one directory at `cid`.
    ///
    /// Returns `None` if the block is absent. The requested CID must use
    /// DRISL and BLAKE3. Invalid bytes and read failures return errors.
    pub fn load(&self, cid: Cid<Bdasl>) -> Result<Option<Directory>, Error> {
        ensure!(
            cid.format() == Format::DRISL_BLAKE3,
            DirectoryCidSnafu { cid }
        );
        self.cas
            .get_fact(&cid.into())?
            .map(|fact| Directory::decode(fact.bytes()))
            .transpose()
    }
}

/// An error constructing, storing or loading a filesystem directory.
#[derive(Debug, Snafu)]
#[snafu(crate_root(covalence_lib_error::snafu))]
#[non_exhaustive]
pub enum Error {
    /// A name is empty, is `.` or `..`, or contains a slash or NUL.
    #[snafu(display("invalid directory child name {name:?}"))]
    Name {
        /// The invalid name.
        name: Bytes,
    },
    /// Two children have the same name.
    #[snafu(display("duplicate directory child name {name:?}"))]
    DuplicateName {
        /// The repeated name.
        name: Bytes,
    },
    /// A child CID has the wrong format for its kind and the leaf mode.
    #[snafu(display("invalid {kind:?} link {cid} for {name:?} in {mode:?} leaf mode"))]
    ChildLink {
        /// The child's name.
        name: Bytes,
        /// The declared kind.
        kind: EntryKind,
        /// The enclosing directory's leaf mode.
        mode: LeafMode,
        /// The supplied CID.
        cid: Cid<Bdasl>,
    },
    /// The requested directory CID does not use DRISL and BLAKE3.
    #[snafu(display("directory CID {cid} must use DRISL and BLAKE3"))]
    DirectoryCid {
        /// The requested CID.
        cid: Cid<Bdasl>,
    },
    /// A DRISL value is outside the versioned directory layout.
    #[snafu(display("invalid directory block: {message}"))]
    Malformed {
        /// What is wrong.
        message: &'static str,
    },
    /// The root table has used every slot index.
    #[snafu(display("filesystem root table is full"))]
    Full,
    /// The CAS stored a block under a different multihash.
    #[snafu(display("CAS returned {key} when storing directory {cid}"))]
    Rekeyed {
        /// The computed directory CID.
        cid: Cid<Bdasl>,
        /// The key returned by the store.
        key: Cid,
    },
    /// The CAS reported a successful write but did not serve the directory CID.
    #[snafu(display("CAS did not serve directory {cid} after writing it"))]
    MissingAfterWrite {
        /// The computed directory CID.
        cid: Cid<Bdasl>,
    },
    /// A CID operation failed.
    #[snafu(display("invalid CID: {source}"), context(false))]
    Cid {
        /// The CID error.
        source: covalence_lib_cid::Error,
    },
    /// The DRISL codec failed.
    #[snafu(display("directory codec: {source}"), context(false))]
    Dasl {
        /// The DASL error.
        source: covalence_lib_dasl::Error,
    },
    /// A CAS operation failed.
    #[snafu(display("directory store: {source}"), context(false))]
    Cas {
        /// The storage error.
        source: covalence_cas_trait::Error,
    },
}
