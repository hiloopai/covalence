//! Loading and storing nodes on a [`Cas`]: the crate's only I/O.

use std::sync::Arc;

use covalence_cas_trait::Cas;
use covalence_lib_cid::{Bdasl, Cid};
use covalence_lib_data::Bytes;
use covalence_lib_error::snafu::ensure;

use crate::{
    Error, MalformedSnafu, RekeyedSnafu,
    build::MAX_HEIGHT,
    cache::Cache,
    node::{Child, Node, upper_of},
};

/// Store an encoded node, checking that the store finds it by its CID.
pub(crate) fn store(cas: &dyn Cas, (cid, bytes): (Cid<Bdasl>, Bytes)) -> Result<(), Error> {
    let key = cas.put(bytes)?;
    ensure!(key.same_multihash(&cid.into()), RekeyedSnafu { cid });
    Ok(())
}

/// Loads nodes of one tree, checking each against its path: as [`Node::decode`] and [`Node::fits`] check it, that it is
/// a leaf exactly at the tree's height, and that the boundary rule cuts it at
/// its level, the height minus its depth. The height is known once a leaf is
/// loaded or [`Loader::assume_height`] sets it; nodes loaded before then are
/// checked then.
pub(crate) struct Loader {
    cas: Arc<dyn Cas>,
    cache: Arc<Cache>,
    height: Option<usize>,
    waiting: Vec<Waiting>,
}

/// A loaded node whose level is not yet known.
struct Waiting {
    cid: Cid<Bdasl>,
    node: Arc<Node>,
    depth: usize,
    leaf: bool,
    /// Whether the node is not the last of its level.
    closed: bool,
}

impl Loader {
    pub(crate) fn new(cas: Arc<dyn Cas>, cache: Arc<Cache>) -> Self {
        Self {
            cas,
            cache,
            height: None,
            waiting: Vec::new(),
        }
    }

    pub(crate) fn height(&self) -> Option<usize> {
        self.height
    }

    /// Load the root at `cid`: a leaf, or a branch of at least two children.
    pub(crate) fn root(&mut self, cid: Cid<Bdasl>) -> Result<Arc<Node>, Error> {
        let node = self.cache.load(&*self.cas, cid)?;
        let one_child = matches!(&*node, Node::Branch(children) if children.len() < 2);
        ensure!(!one_child, MalformedSnafu { cid });
        self.check(cid, &node, 0, false)?;
        Ok(node)
    }

    /// Load `child`, at `depth`, whose keys must be below `upper`. Only the
    /// last node of a level has no `upper`.
    pub(crate) fn child(
        &mut self,
        child: &Child,
        upper: &Option<Bytes>,
        depth: usize,
    ) -> Result<Arc<Node>, Error> {
        let node = self.cache.load(&*self.cas, child.cid)?;
        ensure!(node.fits(child, upper), MalformedSnafu { cid: child.cid });
        self.check(child.cid, &node, depth, upper.is_some())?;
        Ok(node)
    }

    /// Load the leftmost path below `root`, which this loaded, and return
    /// the height.
    pub(crate) fn spine(&mut self, root: &Arc<Node>) -> Result<usize, Error> {
        let (mut node, mut upper, mut depth) = (root.clone(), None, 0);
        while let Node::Branch(children) = &*node {
            upper = upper_of(children, 0, &upper);
            depth += 1;
            node = self.child(&children[0], &upper, depth)?;
        }
        Ok(depth)
    }

    /// Set the height, unknown so far, learned from another tree.
    pub(crate) fn assume_height(&mut self, height: usize) -> Result<(), Error> {
        self.height = Some(height);
        self.flush()
    }

    fn check(
        &mut self,
        cid: Cid<Bdasl>,
        node: &Arc<Node>,
        depth: usize,
        closed: bool,
    ) -> Result<(), Error> {
        let leaf = matches!(&**node, Node::Leaf(_));
        ensure!(leaf || depth < MAX_HEIGHT, MalformedSnafu { cid });
        if leaf {
            match self.height {
                Some(height) => ensure!(height == depth, MalformedSnafu { cid }),
                None => self.height = Some(depth),
            }
            // Only the root, the empty map, can be empty: others must fit a
            // child.
            if node.count() == Some(0) {
                return Ok(());
            }
        }
        self.waiting.push(Waiting {
            cid,
            node: node.clone(),
            depth,
            leaf,
            closed,
        });
        self.flush()
    }

    fn flush(&mut self) -> Result<(), Error> {
        let Some(height) = self.height else {
            return Ok(());
        };
        for node in self.waiting.drain(..) {
            let level = height
                .checked_sub(node.depth)
                .filter(|&level| level <= MAX_HEIGHT);
            let ok = level.is_some_and(|level| {
                (level == 0) == node.leaf
                    && self
                        .cache
                        .is_chunk(node.cid, &node.node, level, node.closed)
            });
            ensure!(ok, MalformedSnafu { cid: node.cid });
        }
        Ok(())
    }
}
