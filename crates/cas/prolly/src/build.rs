//! Chunking: the boundary rule and the bottom-up builder. No I/O: closed
//! nodes wait in the builder until the caller stores them.

use std::mem;

use covalence_lib_cid::{Bdasl, Cid, HashFn};
use covalence_lib_dasl::Value;
use covalence_lib_data::Bytes;
use covalence_lib_error::snafu::OptionExt;

use crate::{
    Error, TooLargeSnafu,
    node::{Child, Node},
};

/// Expected node width.
const TARGET_WIDTH: u64 = 64;
/// Nodes never end with fewer items than this, except the last of a level.
const MIN_WIDTH: usize = 2;
/// Nodes never grow wider than this.
const MAX_WIDTH: usize = 1024;
/// The most levels of branches above the leaves. Each level at most halves,
/// so a tree whose counts fit a `u64` is no higher.
pub(crate) const MAX_HEIGHT: usize = 64;

/// The boundary rule, the only one in this crate; see the crate docs.
///
/// The level byte makes each level's choice independent: with `BLAKE3(key)`
/// alone, only single-child nodes could end at upper levels. With at least two
/// items per closed node, each level at most halves.
fn ends_node(level: usize, key: &[u8], width: usize) -> bool {
    if width >= MAX_WIDTH {
        return true;
    }
    if width < MIN_WIDTH {
        return false;
    }
    let level = u8::try_from(level).expect("trees are at most `MAX_HEIGHT` high");
    let digest = HashFn::Blake3.digest(&[&[level], key].concat());
    let head = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    u64::from(head) < (1 << 32) / TARGET_WIDTH
}

/// Whether the rule cuts a node of `level` with items keyed `keys`: no item
/// but the last ends a node, and the last does if `closed`, that is, if the
/// node is not the last of its level.
pub(crate) fn is_chunk(level: usize, keys: &[Bytes], closed: bool) -> bool {
    let Some((last, init)) = keys.split_last() else {
        return false;
    };
    let mut init = init.iter().enumerate();
    init.all(|(i, key)| !ends_node(level, key, i + 1))
        && (!closed || ends_node(level, last, keys.len()))
}

/// An encoded node and its CID, waiting to be stored.
pub(crate) type Block = (Cid<Bdasl>, Bytes);

/// Builds a tree from items fed in key order.
///
/// Feeding a whole node is the same as feeding its entries when the levels
/// up to the node's own are empty, and either the node ended at a boundary
/// (it is not the last node of its level) or nothing follows it.
#[derive(Default)]
pub(crate) struct Builder {
    entries: Vec<(Bytes, Value)>,
    /// `children[l - 1]` holds the pending items of level `l`.
    children: Vec<Vec<Child>>,
    /// Closed nodes not yet taken by [`Builder::drain`].
    blocks: Vec<Block>,
}

impl Builder {
    /// Feed an entry. Keys must be fed in strictly increasing order.
    pub(crate) fn push_entry(&mut self, key: Bytes, value: Value) -> Result<(), Error> {
        let end = ends_node(0, &key, self.entries.len() + 1);
        self.entries.push((key, value));
        if end {
            let child = self.close_leaf()?;
            self.push_child(1, child)?;
        }
        Ok(())
    }

    /// Feed a node of level `level - 1` as an item of `level`.
    pub(crate) fn push_child(&mut self, level: usize, child: Child) -> Result<(), Error> {
        if self.children.len() < level {
            self.children.resize_with(level, Vec::new);
        }
        let items = &mut self.children[level - 1];
        let end = ends_node(level, &child.key, items.len() + 1);
        items.push(child);
        if end {
            let child = self.close_branch(level)?;
            self.push_child(level + 1, child)?;
        }
        Ok(())
    }

    /// Whether levels `0..=level` hold no pending items.
    pub(crate) fn is_clean(&self, level: usize) -> bool {
        self.entries.is_empty() && self.children.iter().take(level).all(Vec::is_empty)
    }

    /// Take the closed nodes, to be stored.
    pub(crate) fn drain(&mut self) -> std::vec::Drain<'_, Block> {
        self.blocks.drain(..)
    }

    fn close_leaf(&mut self) -> Result<Child, Error> {
        let node = Node::Leaf(mem::take(&mut self.entries));
        self.close(node)
    }

    fn close_branch(&mut self, level: usize) -> Result<Child, Error> {
        let node = Node::Branch(mem::take(&mut self.children[level - 1]));
        self.close(node)
    }

    fn close(&mut self, node: Node) -> Result<Child, Error> {
        let key = match &node {
            Node::Leaf(entries) => entries[0].0.clone(),
            Node::Branch(children) => children[0].key.clone(),
        };
        let count = node.count().context(TooLargeSnafu)?;
        let block = node.encode()?;
        let cid = block.0;
        self.blocks.push(block);
        Ok(Child { key, count, cid })
    }

    /// Close the last node of every level and return the top one. It may be
    /// a branch with one child, if that was fed whole; see [`crate::apply`].
    pub(crate) fn finish(&mut self) -> Result<Cid<Bdasl>, Error> {
        if !self.entries.is_empty() {
            let child = self.close_leaf()?;
            self.children
                .resize_with(self.children.len().max(1), Vec::new);
            self.children[0].push(child);
        }
        for level in 1..=self.children.len() {
            let top = self.children[level..].iter().all(Vec::is_empty);
            match self.children[level - 1].len() {
                0 => continue,
                1 if top => return Ok(self.children[level - 1][0].cid),
                _ => {}
            }
            let child = self.close_branch(level)?;
            if top {
                return Ok(child.cid);
            }
            self.children[level].push(child);
        }
        let block = Node::Leaf(Vec::new()).encode()?;
        let cid = block.0;
        self.blocks.push(block);
        Ok(cid)
    }
}
