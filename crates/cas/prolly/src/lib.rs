//! Prolly maps: canonical ordered maps from bytes to DASL values, stored in a
//! content-addressed store.
//!
//! A [Prolly tree](https://www.dolthub.com/docs/architecture/storage-engine/prolly-tree)
//! ("probabilistic B-tree") is a search tree whose node boundaries are chosen
//! by hashing keys, so the same entries make the same tree whatever edits
//! produced them. A [`Map`] is one stored on a [`Cas`]:
//!
//! - Keys are byte strings, ordered bytewise; values are DASL values.
//! - Each node is a DRISL record named by its DRISL BLAKE3 CID, so the root
//!   CID depends only on the entries.
//! - Maps are values: [`Map::insert`], [`Map::remove`], [`Map::apply`] and
//!   [`Map::union`] return new maps, sharing unchanged nodes with the old one.
//! - [`Map::diff`] skips subtrees the two maps share without reading them.
//!
//! Terms:
//!
//! - [DASL](https://dasl.ing/) (Data-Addressed Structures & Links) is a small
//!   set of specs for content-addressed data. A DASL value is null, a
//!   boolean, an integer, bytes, a string, a list, a map with string keys, or
//!   a [CID](https://dasl.ing/cid.html) link.
//! - [DRISL](https://dasl.ing/drisl.html) is DASL's deterministic CBOR
//!   encoding, so equal values encode to equal bytes and get equal CIDs.
//!
//! ```
//! use covalence_cas_memory::MemoryCas;
//! use covalence_cas_trait::Cas;
//! use covalence_lib_dasl::Value;
//! use covalence_cas_prolly::{Error, Map};
//! let cas = MemoryCas::default().into_dyn();
//! let map = Map::build(cas.clone(), [("a".into(), Value::Integer(1))])?;
//! let map = map.insert("b".into(), Value::Null)?;
//! assert_eq!(map.get(b"a")?, Some(Value::Integer(1)));
//! assert_eq!(map.rank(b"b")?, 1);
//! // The root depends only on the entries, not on how they were made.
//! let built = Map::build(cas, [("a".into(), Value::Integer(1)), ("b".into(), Value::Null)])?;
//! assert_eq!(map.root(), built.root());
//! # Ok::<(), Error>(())
//! ```
//!
//! # Nodes
//!
//! - A leaf is `{"$type": "vc.cov.prolly.leaf", "entries": [[key, value],
//!   ...]}`.
//! - A branch is `{"$type": "vc.cov.prolly.branch", "children": [[first_key,
//!   count, link], ...]}`, where `count` is the number of entries under the
//!   child and `link` its CID.
//! - A node's level is 0 for a leaf and one more than its children's for a
//!   branch. All leaves are at level 0, and a tree has at most 64 levels of
//!   branches.
//! - The root is the top node, or the empty leaf for the empty map. A branch
//!   with one child is never the root.
//!
//! # Chunking
//!
//! Items are cut into nodes left to right at each level: entries at level 0,
//! and children, by their first key, at level `l > 0`. A node of level `l`
//! ends after an item when it holds 1024 items, or when it holds at least 2
//! and the first 4 bytes of `BLAKE3([l] ‖ key)` (the byte `l`, then the key),
//! read big-endian, are below `2^32 / 64`. The last node of a level ends
//! wherever its items do.
//!
//! # Checks
//!
//! Every node a read or edit loads is checked against its place in the tree:
//!
//! - Its bytes hash to its CID, else [`Error::Cas`]. The other checks fail
//!   with [`Error::Malformed`].
//! - Its CID is a DRISL BLAKE3 CID, as are its child links.
//! - It is a leaf or branch record and nothing else. Its keys strictly
//!   increase, a branch has children, and child counts are positive with a
//!   total that fits a `u64`.
//! - Its first key and count are those its parent records, and its keys stay
//!   below the next key in its parent, or its parent's bound.
//! - It is a leaf exactly at level 0, and the chunking rule cuts it at its
//!   level.
//! - A root branch has at least two children.
//!
//! A read learns the tree's height at the first leaf it reaches, or in
//! [`Map::diff`] from a subtree both trees share, and checks the levels of the
//! nodes it loaded before then.
//!
//! What is not loaded is trusted: the counts, chunking and heights of skipped
//! subtrees, which [`Map::len`], [`Map::nth`] and [`Map::rank`] sum and
//! [`Map::apply`] reuses. [`Map::verify`] loads every node, deciding whether
//! the whole tree is canonical.
//!
//! Maps derived from one another, by an edit, [`Map::at`] or `clone`, share a
//! cache of checked nodes. A cached node's hash and decoding are not checked
//! again, nor the chunking rule at a level where it already passed; checks
//! that depend on the node's place in the tree run at every load.
#![deny(missing_docs)]

mod build;
mod cache;
mod io;
mod node;

use std::{
    cmp::Ordering,
    fmt,
    iter::Peekable,
    ops::{Bound, RangeBounds},
    sync::Arc,
};

use covalence_cas_trait::Cas;
use covalence_lib_cid::{Bdasl, Cid};
use covalence_lib_dasl::Value;
use covalence_lib_data::Bytes;
use covalence_lib_error::snafu::{OptionExt, Snafu, ensure};

use build::Builder;
use cache::Cache;
use io::{Loader, store};
use node::{Child, Node, upper_of};

/// Errors from reading and storing maps.
#[derive(Debug, Snafu)]
#[snafu(crate_root(covalence_lib_error::snafu))]
#[non_exhaustive]
pub enum Error {
    /// The store failed, or a node's bytes do not hash to its CID.
    #[snafu(context(false), display("{source}"))]
    Cas {
        /// The store's error.
        source: covalence_cas_trait::Error,
    },
    /// The store has no node `cid`.
    #[snafu(display("{cid} not found"))]
    Missing {
        /// The node's CID.
        cid: Cid<Bdasl>,
    },
    /// Node `cid` fails a check; see [Checks](crate#checks).
    #[snafu(display("{cid} is not a valid Prolly node here"))]
    Malformed {
        /// The node's CID.
        cid: Cid<Bdasl>,
    },
    /// A node cannot be encoded as DRISL, such as one holding a value nested
    /// too deeply.
    #[snafu(context(false), display("{source}"))]
    Encode {
        /// The encoding error.
        source: covalence_lib_dasl::Error,
    },
    /// The store put node `cid` under a key with another hash or digest.
    #[snafu(display("the store did not keep {cid} under its hash"))]
    Rekeyed {
        /// The node's CID.
        cid: Cid<Bdasl>,
    },
    /// A count of entries is too large to record as a DASL integer.
    #[snafu(display("a count does not fit a DASL integer"))]
    TooLarge,
    /// Entries or edits were not in strictly increasing key order.
    #[snafu(display("key {key:?} is not after the one before it"))]
    Unsorted {
        /// The first key out of order.
        key: Bytes,
    },
}

/// One change to a map.
#[derive(Clone, Debug, PartialEq)]
pub enum Edit {
    /// Insert or replace.
    Insert(Bytes, Value),
    /// Remove if present.
    Remove(Bytes),
}

impl Edit {
    /// The key this edit changes.
    pub fn key(&self) -> &Bytes {
        match self {
            Edit::Insert(key, _) | Edit::Remove(key) => key,
        }
    }
}

/// A key whose value differs between two maps: added if `old` is `None`,
/// removed if `new` is.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    /// The key.
    pub key: Bytes,
    /// The value in the first map, if any.
    pub old: Option<Value>,
    /// The value in the second map, if any.
    pub new: Option<Value>,
}

/// A map stored on a [`Cas`], named by its root.
#[derive(Clone)]
pub struct Map {
    cas: Arc<dyn Cas>,
    cache: Arc<Cache>,
    root: Cid<Bdasl>,
}

impl fmt::Debug for Map {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Map").field(&self.root).finish()
    }
}

impl Map {
    /// The map with root `root`, with a new cache. Its nodes are checked as
    /// they are read, so this does no I/O and cannot fail.
    pub fn open(cas: Arc<dyn Cas>, root: Cid<Bdasl>) -> Self {
        let cache = Arc::default();
        Self { cas, cache, root }
    }

    /// The map with root `root` on this map's store, sharing its cache.
    pub fn at(&self, root: Cid<Bdasl>) -> Self {
        let (cas, cache) = (self.cas.clone(), self.cache.clone());
        Self { cas, cache, root }
    }

    fn loader(&self) -> Loader {
        Loader::new(self.cas.clone(), self.cache.clone())
    }

    /// The empty map, stored.
    pub fn empty(cas: Arc<dyn Cas>) -> Result<Self, Error> {
        Self::build(cas, [])
    }

    /// Build a map from entries in strictly increasing key order, or fail
    /// with [`Error::Unsorted`] at the first key out of order.
    pub fn build(
        cas: Arc<dyn Cas>,
        entries: impl IntoIterator<Item = (Bytes, Value)>,
    ) -> Result<Self, Error> {
        let mut builder = Builder::default();
        let mut last: Option<Bytes> = None;
        for (key, value) in entries {
            ensure!(last.is_none_or(|last| last < key), UnsortedSnafu { key });
            last = Some(key.clone());
            builder.push_entry(key, value)?;
            flush(&*cas, &mut builder)?;
        }
        finish(cas, Arc::default(), builder)
    }

    /// The root's CID, which names the map's entries.
    pub fn root(&self) -> Cid<Bdasl> {
        self.root
    }

    /// The value under `key`, or `None` if missing. Reads one path from the
    /// root to a leaf.
    pub fn get(&self, key: &[u8]) -> Result<Option<Value>, Error> {
        let leaf = self.walk(|children| child_for(children, key).unwrap_or(0))?;
        let entries = leaf.entries();
        let found = entries.binary_search_by(|(k, _)| k[..].cmp(key));
        Ok(found.ok().map(|i| entries[i].1.clone()))
    }

    /// Insert or replace an entry.
    pub fn insert(&self, key: Bytes, value: Value) -> Result<Self, Error> {
        self.apply([Edit::Insert(key, value)])
    }

    /// Remove `key`'s entry, if any.
    pub fn remove(&self, key: Bytes) -> Result<Self, Error> {
        self.apply([Edit::Remove(key)])
    }

    /// Apply edits in strictly increasing key order, or fail with
    /// [`Error::Unsorted`]. Only the paths to the edited keys are rewritten,
    /// with the nodes after them that chunking must cut again.
    pub fn apply(&self, edits: impl IntoIterator<Item = Edit>) -> Result<Self, Error> {
        let edits: Vec<Edit> = edits.into_iter().collect();
        if let Some(pair) = edits.windows(2).find(|pair| pair[0].key() >= pair[1].key()) {
            return UnsortedSnafu {
                key: pair[1].key().clone(),
            }
            .fail();
        }
        if edits.is_empty() {
            return Ok(self.clone());
        }
        let mut tree = self.loader();
        let top = tree.root(self.root)?;
        let height = tree.spine(&top)?;
        let mut rewrite = Rewrite {
            cas: &*self.cas,
            tree,
            height,
            builder: Builder::default(),
            edits: edits.into_iter().peekable(),
        };
        rewrite.node(top, 0, &None)?;
        finish(self.cas.clone(), self.cache.clone(), rewrite.builder)
    }

    /// The number of entries, as the root records. Reads the leftmost path,
    /// to check the root.
    pub fn len(&self) -> Result<u64, Error> {
        let mut tree = self.loader();
        let node = tree.root(self.root)?;
        tree.spine(&node)?;
        Ok(node.count().expect("checked on load"))
    }

    /// Whether the root is the empty leaf. Reads nothing.
    pub fn is_empty(&self) -> bool {
        self.root == empty_root()
    }

    /// The entry at position `index` in key order, or `None` if there are
    /// not that many.
    pub fn nth(&self, mut index: u64) -> Result<Option<(Bytes, Value)>, Error> {
        let leaf = self.walk(|children| {
            // Skip whole children until `index` falls inside one, or the
            // last.
            let mut i = 0;
            while i + 1 < children.len() && index >= children[i].count {
                index -= children[i].count;
                i += 1;
            }
            i
        })?;
        let i = usize::try_from(index).ok();
        Ok(i.and_then(|i| leaf.entries().get(i).cloned()))
    }

    /// The number of keys less than `key`: its position if present, or where
    /// it would be inserted.
    pub fn rank(&self, key: &[u8]) -> Result<u64, Error> {
        let mut before = 0;
        let leaf = self.walk(|children| {
            let i = child_for(children, key).unwrap_or(0);
            before += children[..i].iter().map(|child| child.count).sum::<u64>();
            i
        })?;
        Ok(before + leaf.entries().partition_point(|(k, _)| k[..] < *key) as u64)
    }

    /// Walk from the root to a leaf, taking the child `pick` chooses, and
    /// return the leaf.
    fn walk(&self, mut pick: impl FnMut(&[Child]) -> usize) -> Result<Arc<Node>, Error> {
        let mut tree = self.loader();
        let (mut node, mut upper, mut depth) = (tree.root(self.root)?, None, 0);
        while let Node::Branch(children) = &*node {
            let i = pick(children);
            upper = upper_of(children, i, &upper);
            depth += 1;
            node = tree.child(&children[i], &upper, depth)?;
        }
        Ok(node)
    }

    /// The entries with keys in `bounds`, in key order. The path to the first
    /// leaf is read now, and later leaves as the iterator reaches them.
    ///
    /// ```
    /// use std::ops::Bound;
    ///
    /// use covalence_cas_memory::MemoryCas;
    /// use covalence_cas_trait::Cas;
    /// use covalence_lib_dasl::Value;
    /// use covalence_cas_prolly::{Error, Map};
    /// let cas = MemoryCas::default().into_dyn();
    /// let entries = ["a", "b", "c"].map(|key| (key.into(), Value::Null));
    /// let map = Map::build(cas, entries)?;
    /// let keys: Vec<_> = map
    ///     .range((Bound::Included(&b"b"[..]), Bound::Unbounded))?
    ///     .map(|entry| Ok(entry?.0))
    ///     .collect::<Result<_, Error>>()?;
    /// assert_eq!(keys, ["b", "c"]);
    /// # Ok::<(), Error>(())
    /// ```
    pub fn range(&self, bounds: impl RangeBounds<[u8]>) -> Result<Range, Error> {
        let mut tree = self.loader();
        let root = tree.root(self.root)?;
        let mut range = Range {
            tree,
            path: Vec::new(),
            leaf: Arc::new(Node::Leaf(Vec::new())),
            next: 0,
            end: bounds.end_bound().map(Bytes::copy_from_slice),
            done: false,
        };
        range.descend(root, None, bounds.start_bound())?;
        Ok(range)
    }

    /// The keys whose values differ from this map to `other`, in key order.
    /// Subtrees with the same CID on both sides are skipped unread.
    ///
    /// ```
    /// use covalence_cas_memory::MemoryCas;
    /// use covalence_cas_trait::Cas;
    /// use covalence_lib_dasl::Value;
    /// use covalence_cas_prolly::{Change, Error, Map};
    /// let cas = MemoryCas::default().into_dyn();
    /// let old = Map::build(cas, [("a".into(), Value::Integer(1))])?;
    /// let new = old.insert("a".into(), Value::Integer(2))?;
    /// let change = Change {
    ///     key: "a".into(),
    ///     old: Some(Value::Integer(1)),
    ///     new: Some(Value::Integer(2)),
    /// };
    /// assert_eq!(old.diff(&new)?, [change]);
    /// # Ok::<(), Error>(())
    /// ```
    pub fn diff(&self, other: &Map) -> Result<Vec<Change>, Error> {
        let mut changes = Vec::new();
        if self.root == other.root {
            return Ok(changes);
        }
        let mut trees = [self.loader(), other.loader()];
        // Pending items of each side, in reverse key order. The item under a
        // child is the next one after it, so its key bounds the child's keys.
        let mut sides = [Vec::new(), Vec::new()];
        for ((tree, items), root) in trees
            .iter_mut()
            .zip(&mut sides)
            .zip([self.root, other.root])
        {
            expand(items, &*tree.root(root)?, 0);
        }
        // The first subtree skipped on both sides, and its depth in each.
        let mut shared = None;
        loop {
            let [a, b] = &mut sides;
            let side = match (a.last(), b.last()) {
                (None, None) => break,
                (Some(Item::Child(x, dx)), Some(Item::Child(y, dy))) if x.cid == y.cid => {
                    // A subtree is as high in both trees.
                    let (cid, ex, ey) = *shared.get_or_insert((x.cid, *dx, *dy));
                    ensure!(ex + dy == ey + dx, MalformedSnafu { cid });
                    a.pop();
                    b.pop();
                    continue;
                }
                (Some(x), Some(y)) => match (x.key().cmp(y.key()), x, y) {
                    (Ordering::Less, ..) => 0,
                    (Ordering::Greater, ..) => 1,
                    // At the same key, open children first, the bigger one
                    // first.
                    (_, Item::Child(x, _), Item::Child(y, _)) => usize::from(x.count < y.count),
                    (_, Item::Child(..), _) => 0,
                    (_, _, Item::Child(..)) => 1,
                    (_, Item::Entry(..), Item::Entry(..)) => {
                        let (Some(Item::Entry(key, old)), Some(Item::Entry(_, new))) =
                            (a.pop(), b.pop())
                        else {
                            unreachable!()
                        };
                        if old != new {
                            let (old, new) = (Some(old), Some(new));
                            changes.push(Change { key, old, new });
                        }
                        continue;
                    }
                },
                (Some(_), None) => 0,
                (None, Some(_)) => 1,
            };
            let items = &mut sides[side];
            match items.pop() {
                Some(Item::Child(child, depth)) => {
                    let upper = items.last().map(|item| item.key().clone());
                    expand(items, &*trees[side].child(&child, &upper, depth)?, depth);
                }
                Some(Item::Entry(key, value)) => {
                    let (old, new) = if side == 0 {
                        (Some(value), None)
                    } else {
                        (None, Some(value))
                    };
                    changes.push(Change { key, old, new });
                }
                None => unreachable!(),
            }
        }
        if let Some((cid, da, db)) = shared {
            let [a, b] = &mut trees;
            if a.height().is_none() && b.height().is_none() {
                // Every leaf was shared: learn one height from its leftmost
                // path.
                let top = a.root(self.root)?;
                a.spine(&top)?;
            }
            // `ha - da == hb - db`.
            match (a.height(), b.height()) {
                (Some(ha), Some(hb)) => ensure!(ha + db == hb + da, MalformedSnafu { cid }),
                (Some(ha), None) => {
                    b.assume_height((ha + db).checked_sub(da).context(MalformedSnafu { cid })?)?
                }
                (None, Some(hb)) => {
                    a.assume_height((hb + da).checked_sub(db).context(MalformedSnafu { cid })?)?
                }
                (None, None) => unreachable!("the spine reaches a leaf"),
            }
        }
        Ok(changes)
    }

    /// This map with the entries of `other` added, stored on this map's
    /// store. For a key in both with different values, `resolve(key, ours,
    /// theirs)` gives the value. Subtrees the maps share are not read.
    pub fn union(
        &self,
        other: &Map,
        mut resolve: impl FnMut(&Bytes, Value, Value) -> Value,
    ) -> Result<Self, Error> {
        let edits = self.diff(other)?.into_iter().filter_map(|change| {
            let value = match (change.old, change.new?) {
                (Some(ours), theirs) => resolve(&change.key, ours, theirs),
                (None, theirs) => theirs,
            };
            Some(Edit::Insert(change.key, value))
        });
        self.apply(edits.collect::<Vec<_>>())
    }

    /// Check that the map is canonical, loading every node and checking every
    /// count. Fails as reads do, with [`Error::Malformed`] for a node that
    /// fails a check.
    pub fn verify(&self) -> Result<(), Error> {
        let mut tree = self.loader();
        let root = tree.root(self.root)?;
        verify(&mut tree, &root, &None, 0)
    }
}

fn verify(
    tree: &mut Loader,
    node: &Node,
    upper: &Option<Bytes>,
    depth: usize,
) -> Result<(), Error> {
    if let Node::Branch(children) = node {
        for (i, child) in children.iter().enumerate() {
            let upper = upper_of(children, i, upper);
            let node = tree.child(child, &upper, depth + 1)?;
            verify(tree, &node, &upper, depth + 1)?;
        }
    }
    Ok(())
}

/// The root of the empty map.
fn empty_root() -> Cid<Bdasl> {
    Node::Leaf(Vec::new())
        .encode()
        .expect("the empty leaf encodes")
        .0
}

fn flush(cas: &dyn Cas, builder: &mut Builder) -> Result<(), Error> {
    builder.drain().try_for_each(|block| store(cas, block))
}

/// Store the rest of the tree and return its root, skipping one-child
/// branches at the root.
fn finish(cas: Arc<dyn Cas>, cache: Arc<Cache>, mut builder: Builder) -> Result<Map, Error> {
    let mut root = builder.finish()?;
    flush(&*cas, &mut builder)?;
    while let Node::Branch(children) = &*cache.load(&*cas, root)?
        && let [child] = &children[..]
    {
        root = child.cid;
    }
    Ok(Map { cas, cache, root })
}

/// A rewrite of a tree of `height` by [`Map::apply`].
struct Rewrite<'a, I: Iterator<Item = Edit>> {
    cas: &'a dyn Cas,
    tree: Loader,
    height: usize,
    builder: Builder,
    edits: Peekable<I>,
}

impl<I: Iterator<Item = Edit>> Rewrite<'_, I> {
    /// Rebuild `node`, at `depth`, with the edits below `upper` applied.
    /// Untouched children are fed whole. Only the last node of a level has
    /// no `upper`, and it is untouched only when no edits follow.
    fn node(&mut self, node: Arc<Node>, depth: usize, upper: &Option<Bytes>) -> Result<(), Error> {
        let below = |edit: &Edit, upper: &Option<Bytes>| {
            upper.as_ref().is_none_or(|upper| edit.key() < upper)
        };
        match &*node {
            Node::Leaf(entries) => {
                let mut entries = entries.iter().cloned().peekable();
                loop {
                    let edit = self.edits.next_if(|edit| {
                        below(edit, upper)
                            && entries.peek().is_none_or(|(key, _)| edit.key() <= key)
                    });
                    match edit {
                        Some(edit) => {
                            entries.next_if(|(key, _)| key == edit.key());
                            if let Edit::Insert(key, value) = edit {
                                self.builder.push_entry(key, value)?;
                            }
                        }
                        None => match entries.next() {
                            Some((key, value)) => self.builder.push_entry(key, value)?,
                            None => return flush(self.cas, &mut self.builder),
                        },
                    }
                }
            }
            Node::Branch(children) => {
                // The loader checked that branches are above the leaves.
                let level = self.height - depth;
                for (i, child) in children.iter().enumerate() {
                    let child_upper = upper_of(children, i, upper);
                    let untouched = self
                        .edits
                        .peek()
                        .is_none_or(|edit| !below(edit, &child_upper));
                    if untouched && self.builder.is_clean(level - 1) {
                        self.builder.push_child(level, child.clone())?;
                    } else {
                        let node = self.tree.child(child, &child_upper, depth + 1)?;
                        self.node(node, depth + 1, &child_upper)?;
                    }
                }
                flush(self.cas, &mut self.builder)
            }
        }
    }
}

/// The child whose range holds `key`, or `None` if `key` is before them all.
fn child_for(children: &[Child], key: &[u8]) -> Option<usize> {
    children
        .partition_point(|child| child.key[..] <= *key)
        .checked_sub(1)
}

/// Whether `key` is within `end`.
fn before_end(end: &Bound<Bytes>, key: &Bytes) -> bool {
    match end {
        Bound::Included(end) => key <= end,
        Bound::Excluded(end) => key < end,
        Bound::Unbounded => true,
    }
}

/// An iterator over a key range of a map; see [`Map::range`].
///
/// It yields an error, then stops, if a node it loads fails.
pub struct Range {
    tree: Loader,
    /// Branches from the root, each with the child being read and its bound.
    path: Vec<(Arc<Node>, usize, Option<Bytes>)>,
    /// The leaf being read, and the index of its next entry.
    leaf: Arc<Node>,
    next: usize,
    end: Bound<Bytes>,
    done: bool,
}

impl Range {
    /// Walk down from `node`, at the end of the path and with keys below
    /// `upper`, to the leaf holding the first key after `start`.
    fn descend(
        &mut self,
        mut node: Arc<Node>,
        mut upper: Option<Bytes>,
        start: Bound<&[u8]>,
    ) -> Result<(), Error> {
        while let Node::Branch(children) = &*node {
            let i = match start {
                Bound::Included(start) | Bound::Excluded(start) => {
                    child_for(children, start).unwrap_or(0)
                }
                Bound::Unbounded => 0,
            };
            let child_upper = upper_of(children, i, &upper);
            let depth = self.path.len() + 1;
            let child = self.tree.child(&children[i], &child_upper, depth)?;
            self.path.push((node, i, upper));
            (node, upper) = (child, child_upper);
        }
        self.next = node.entries().partition_point(|(key, _)| match start {
            Bound::Included(start) => key[..] < *start,
            Bound::Excluded(start) => key[..] <= *start,
            Bound::Unbounded => false,
        });
        self.leaf = node;
        Ok(())
    }

    /// Move to the next leaf; `false` at the end of the map or the range.
    fn next_leaf(&mut self) -> Result<bool, Error> {
        loop {
            let depth = self.path.len();
            let Some((node, i, upper)) = self.path.last_mut() else {
                return Ok(false);
            };
            let children = node.children();
            if *i + 1 == children.len() {
                self.path.pop();
                continue;
            }
            *i += 1;
            if !before_end(&self.end, &children[*i].key) {
                return Ok(false);
            }
            let child_upper = upper_of(children, *i, upper);
            let child = self.tree.child(&children[*i], &child_upper, depth)?;
            self.descend(child, child_upper, Bound::Unbounded)?;
            return Ok(true);
        }
    }
}

impl Iterator for Range {
    type Item = Result<(Bytes, Value), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.done {
            if let Some((key, value)) = self.leaf.entries().get(self.next).cloned() {
                self.next += 1;
                if before_end(&self.end, &key) {
                    return Some(Ok((key, value)));
                }
                self.done = true;
                return None;
            }
            match self.next_leaf() {
                Ok(true) => {}
                Ok(false) => self.done = true,
                Err(error) => {
                    self.done = true;
                    return Some(Err(error));
                }
            }
        }
        None
    }
}

/// An item pending in [`Map::diff`]: an entry, or a child at its depth.
enum Item {
    Entry(Bytes, Value),
    Child(Child, usize),
}

impl Item {
    fn key(&self) -> &Bytes {
        match self {
            Item::Entry(key, _) => key,
            Item::Child(child, _) => &child.key,
        }
    }
}

/// Push the items of `node`, at `depth`, in reverse key order.
fn expand(items: &mut Vec<Item>, node: &Node, depth: usize) {
    match node {
        Node::Leaf(entries) => items.extend(
            entries
                .iter()
                .rev()
                .map(|(k, v)| Item::Entry(k.clone(), v.clone())),
        ),
        Node::Branch(children) => items.extend(
            children
                .iter()
                .rev()
                .map(|child| Item::Child(child.clone(), depth + 1)),
        ),
    }
}
