//! Logical node records and checks that apply to every layout.

use covalence_lib_cid::{Bdasl, Cid, Codec, Format, HashFn};
use covalence_lib_dasl::Value;
use covalence_lib_data::Bytes;

use crate::{Error, Layout, NodeOf};

/// The bound on the keys of child `i` of a branch whose keys are below
/// `upper`: the next child's key, or `upper` for the last child.
pub(crate) fn upper_of<K: Clone>(children: &[Child<K>], i: usize, upper: &Option<K>) -> Option<K> {
    children
        .get(i + 1)
        .map(|next| next.key.clone())
        .or_else(|| upper.clone())
}

/// Whether `cid` can name a node: nodes are named by their DRISL BLAKE3 CIDs.
pub(crate) fn names_node(cid: Cid<Bdasl>) -> bool {
    cid.format() == Format::DRISL_BLAKE3
}

/// A link to a subtree: its first key, its number of keys and its CID.
#[derive(Clone, Debug)]
pub struct Child<K = Bytes> {
    /// The child's first key.
    pub key: K,
    /// The number of entries below the child.
    pub count: u64,
    /// The CID of the child node.
    pub cid: Cid<Bdasl>,
}

/// The logical contents of one tree node.
pub enum Node<K = Bytes, V = Value> {
    /// Entries in key order.
    Leaf(Vec<(K, V)>),
    /// Links to child nodes in key order.
    Branch(Vec<Child<K>>),
}

impl<K: AsRef<[u8]>, V> Node<K, V> {
    /// Whether the decoded node obeys the rules independent of its location.
    pub(crate) fn valid(&self) -> bool {
        match self {
            Node::Leaf(entries) => entries.is_sorted_by(|a, b| a.0.as_ref() < b.0.as_ref()),
            Node::Branch(children) => {
                !children.is_empty()
                    && children.is_sorted_by(|a, b| a.key.as_ref() < b.key.as_ref())
                    && children
                        .iter()
                        .all(|child| child.count > 0 && names_node(child.cid))
                    && self.count().is_some()
            }
        }
    }

    /// Whether this node can be `child`, whose keys must be below `upper`: it
    /// starts at the child's key and holds its count.
    pub(crate) fn fits(&self, child: &Child<K>, upper: &Option<K>) -> bool {
        let (first, last) = self.bounds();
        first == Some(child.key.as_ref())
            && self.count() == Some(child.count)
            && upper
                .as_ref()
                .is_none_or(|upper| last < Some(upper.as_ref()))
    }

    /// The first and last keys of this node's items.
    fn bounds(&self) -> (Option<&[u8]>, Option<&[u8]>) {
        match self {
            Node::Leaf(entries) => (
                entries.first().map(|(key, _)| key.as_ref()),
                entries.last().map(|(key, _)| key.as_ref()),
            ),
            Node::Branch(children) => (
                children.first().map(|child| child.key.as_ref()),
                children.last().map(|child| child.key.as_ref()),
            ),
        }
    }

    /// A leaf's entries; none for a branch.
    pub(crate) fn entries(&self) -> &[(K, V)] {
        match self {
            Node::Leaf(entries) => entries,
            Node::Branch(_) => &[],
        }
    }

    /// A branch's children; none for a leaf.
    pub(crate) fn children(&self) -> &[Child<K>] {
        match self {
            Node::Leaf(_) => &[],
            Node::Branch(children) => children,
        }
    }

    /// The keys of this node's items.
    pub(crate) fn keys(&self) -> Vec<&[u8]> {
        match self {
            Node::Leaf(entries) => entries.iter().map(|(key, _)| key.as_ref()).collect(),
            Node::Branch(children) => children.iter().map(|child| child.key.as_ref()).collect(),
        }
    }

    /// The number of keys below, or `None` if it overflows.
    pub(crate) fn count(&self) -> Option<u64> {
        match self {
            Node::Leaf(entries) => Some(entries.len() as u64),
            Node::Branch(children) => children
                .iter()
                .try_fold(0u64, |sum, child| sum.checked_add(child.count)),
        }
    }
}

/// An encoded node and its CID, waiting to be stored.
pub(crate) type Block = (Cid<Bdasl>, Bytes);

/// Encode a logical node as DRISL and compute its CID.
pub(crate) fn encode<L: Layout>(node: &NodeOf<L>) -> Result<Block, Error> {
    let bytes = L::encode(node)?.encode()?;
    let cid = Cid::<Bdasl>::of(Codec::Drisl, HashFn::Blake3, &bytes);
    Ok((cid, bytes.into()))
}

/// Decode a DRISL record and check the resulting node's structure.
pub(crate) fn decode<L: Layout>(bytes: &[u8]) -> Option<NodeOf<L>> {
    let node = L::decode(Value::decode(bytes).ok()?)?;
    node.valid().then_some(node)
}
