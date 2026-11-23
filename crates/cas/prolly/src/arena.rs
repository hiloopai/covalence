use std::sync::Arc;

use covalence_cas_trait::Cas;
use covalence_lib_cid::{Bdasl, Cid};
use covalence_lib_error::snafu::OptionExt;

use crate::{
    ArenaFullSnafu, Edit, Entry, Error, InvalidMapIdSnafu, Layout, LexiconLayout, Tree,
    build::build_root, cache::Cache,
};

/// An opaque slot index for a map in an [`Arena`].
///
/// Keep the ID with its issuing arena. An ID carries only a `u32` index,
/// so another arena may resolve it to a different map at the same index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MapId(u32);

/// An in-memory table of typed Prolly map roots on a shared [`Cas`].
///
/// Each slot holds one root. IDs are assigned in order and never recycled.
/// Trees from this arena share a cache of decoded nodes by CID.
/// Use root CIDs to reopen maps after the arena is dropped.
pub struct Arena<L: Layout = LexiconLayout> {
    cas: Arc<dyn Cas>,
    cache: Arc<Cache<L>>,
    roots: Vec<Cid<Bdasl>>,
}

impl<L: Layout> Arena<L> {
    /// Create an empty arena on `cas`.
    pub fn new(cas: Arc<dyn Cas>) -> Self {
        Self {
            cas,
            cache: Arc::default(),
            roots: Vec::new(),
        }
    }

    /// Store a map and assign it a new slot on success.
    ///
    /// Entries must be in strictly increasing bytewise key order.
    /// Returns [`Error::ArenaFull`] before building if no ID fits.
    /// A failed build can leave blocks in the store.
    pub fn insert_map(
        &mut self,
        entries: impl IntoIterator<Item = Entry<L>>,
    ) -> Result<MapId, Error> {
        let index = u32::try_from(self.roots.len())
            .ok()
            .context(ArenaFullSnafu)?;
        let root = self.store_map(entries)?;
        self.roots.push(root);
        Ok(MapId(index))
    }

    /// Store a map and return its root CID without assigning a slot.
    ///
    /// Entries must be in strictly increasing bytewise key order.
    /// A failed build can leave blocks in the store.
    pub fn store_map(
        &self,
        entries: impl IntoIterator<Item = Entry<L>>,
    ) -> Result<Cid<Bdasl>, Error> {
        Ok(Tree::build_with_cache(self.cas.clone(), self.cache.clone(), entries)?.root())
    }

    /// Compute a map's root CID without accessing the store or assigning a slot.
    ///
    /// Entries must be in strictly increasing bytewise key order.
    pub fn map_cid(
        &self,
        entries: impl IntoIterator<Item = Entry<L>>,
    ) -> Result<Cid<Bdasl>, Error> {
        build_root::<L>(entries, |_| Ok(()))
    }

    /// Get a snapshot of a map without accessing the store.
    ///
    /// Returns [`Error::InvalidMapId`] if the index is out of bounds.
    pub fn get(&self, id: MapId) -> Result<Tree<L>, Error> {
        let root = *self.roots.get(id.0 as usize).context(InvalidMapIdSnafu)?;
        Ok(Tree {
            cas: self.cas.clone(),
            cache: self.cache.clone(),
            root,
        })
    }

    /// Apply edits to a slot and return its new root CID.
    ///
    /// Edits must be in strictly increasing bytewise key order.
    /// Returns [`Error::InvalidMapId`] if the index is out of bounds.
    /// The slot changes only on success. Earlier snapshots keep their roots,
    /// and a failed edit can leave unreferenced blocks in the store.
    pub fn apply(
        &mut self,
        id: MapId,
        edits: impl IntoIterator<Item = Edit<L::Key, L::Value>>,
    ) -> Result<Cid<Bdasl>, Error> {
        let root = self.get(id)?.apply(edits)?.root();
        self.roots[id.0 as usize] = root;
        Ok(root)
    }
}
