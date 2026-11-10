//! Checked nodes kept in memory, so that maps derived from one another load
//! and check each node once.
//!
//! How much is kept, and which nodes go first, is private to this module:
//! callers only load nodes through [`Cache::load`] and test the boundary rule
//! through [`Cache::is_chunk`].

use std::sync::{Arc, Mutex, MutexGuard};

use covalence_cas_trait::{Cas, GetExt};
use covalence_lib_cid::{Bdasl, Cid};
use covalence_lib_error::snafu::{OptionExt, ensure};
use hashlink::LruCache;

use crate::{
    Error, MalformedSnafu, MissingSnafu,
    build::is_chunk,
    node::{Node, names_node},
};

/// The most encoded bytes a [`Cache`] keeps.
const BUDGET: usize = 16 << 20;

/// Decoded nodes by CID.
///
/// A node is kept once its bytes hash to its CID and decode. Up to
/// [`BUDGET`] bytes of encoded nodes are kept, dropping the least recently
/// used; a larger node is returned but not kept, so it does not push out the
/// others.
///
/// For each kept node, the cache also remembers where the boundary rule was
/// found to end it: the level, and whether the node was closed (not the last
/// of its level). Checks that depend on the node's place in a tree are not
/// cached; the caller makes them at every load.
#[derive(Default)]
pub(crate) struct Cache(Mutex<Nodes>);

struct Nodes {
    lru: LruCache<Cid<Bdasl>, Kept>,
    bytes: usize,
}

impl Default for Nodes {
    fn default() -> Self {
        let lru = LruCache::new_unbounded();
        Self { lru, bytes: 0 }
    }
}

struct Kept {
    node: Arc<Node>,
    size: usize,
    /// Each level where the rule ends the node, and whether closed there.
    ends: Vec<(usize, bool)>,
}

impl Cache {
    fn nodes(&self) -> MutexGuard<'_, Nodes> {
        self.0.lock().unwrap()
    }

    /// The node at `cid`, checked as [`Node::decode`] checks it.
    pub(crate) fn load(&self, cas: &dyn Cas, cid: Cid<Bdasl>) -> Result<Arc<Node>, Error> {
        if let Some(kept) = self.nodes().lru.get(&cid) {
            return Ok(kept.node.clone());
        }
        ensure!(names_node(cid), MalformedSnafu { cid });
        let fact = cas.get_fact(&cid.into())?.context(MissingSnafu { cid })?;
        let node = Arc::new(Node::decode(fact.bytes()).context(MalformedSnafu { cid })?);
        let size = fact.bytes().len();
        if size <= BUDGET {
            self.nodes().keep(cid, node.clone(), size);
        }
        Ok(node)
    }

    /// Whether the rule ends `node`, loaded from `cid`, at `level`, as
    /// [`is_chunk`] decides. A node that ends closed also ends open.
    pub(crate) fn is_chunk(
        &self,
        cid: Cid<Bdasl>,
        node: &Node,
        level: usize,
        closed: bool,
    ) -> bool {
        let known = self
            .nodes()
            .lru
            .peek(&cid)
            .is_some_and(|kept| kept.ends.iter().any(|&(l, c)| l == level && (c || !closed)));
        if known {
            return true;
        }
        let ends = is_chunk(level, &node.keys(), closed);
        if ends && let Some(kept) = self.nodes().lru.peek_mut(&cid) {
            kept.ends.push((level, closed));
        }
        ends
    }
}

impl Nodes {
    fn keep(&mut self, cid: Cid<Bdasl>, node: Arc<Node>, size: usize) {
        let ends = Vec::new();
        if let Some(old) = self.lru.insert(cid, Kept { node, size, ends }) {
            self.bytes -= old.size;
        }
        self.bytes += size;
        while self.bytes > BUDGET
            && let Some((_, old)) = self.lru.remove_lru()
        {
            self.bytes -= old.size;
        }
    }
}
