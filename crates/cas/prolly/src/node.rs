//! Node records: their DRISL encoding and the checks made on decoding. No I/O.

use std::collections::BTreeMap;

use covalence_lib_cid::{Bdasl, Cid, Codec, Format, HashFn};
use covalence_lib_dasl::Value;
use covalence_lib_data::Bytes;
use covalence_lib_error::snafu::OptionExt;

use crate::{Error, TooLargeSnafu};

const LEAF: &str = "vc.cov.prolly.leaf";
const BRANCH: &str = "vc.cov.prolly.branch";

/// The bound on the keys of child `i` of a branch whose keys are below
/// `upper`: the next child's key, or `upper` for the last child.
pub(crate) fn upper_of(children: &[Child], i: usize, upper: &Option<Bytes>) -> Option<Bytes> {
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
pub(crate) struct Child {
    pub(crate) key: Bytes,
    pub(crate) count: u64,
    pub(crate) cid: Cid<Bdasl>,
}

#[derive(Clone)]
pub(crate) enum Node {
    Leaf(Vec<(Bytes, Value)>),
    Branch(Vec<Child>),
}

impl Node {
    /// Decode a node from DRISL `bytes`. Keys must be strictly increasing,
    /// branches nonempty, child counts positive with a total that fits a
    /// `u64`, and child links DRISL BLAKE3 CIDs.
    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        Self::from_value(Value::decode(bytes).ok()?)
    }

    /// Whether this node can be `child`, whose keys must be below `upper`: it
    /// starts at the child's key and holds its count.
    pub(crate) fn fits(&self, child: &Child, upper: &Option<Bytes>) -> bool {
        let (first, last) = self.bounds();
        first == Some(&child.key)
            && self.count() == Some(child.count)
            && upper.as_ref().is_none_or(|upper| last < Some(upper))
    }

    /// The first and last keys of this node's items.
    fn bounds(&self) -> (Option<&Bytes>, Option<&Bytes>) {
        match self {
            Node::Leaf(entries) => (
                entries.first().map(|(key, _)| key),
                entries.last().map(|(key, _)| key),
            ),
            Node::Branch(children) => (
                children.first().map(|child| &child.key),
                children.last().map(|child| &child.key),
            ),
        }
    }

    /// A leaf's entries; none for a branch.
    pub(crate) fn entries(&self) -> &[(Bytes, Value)] {
        match self {
            Node::Leaf(entries) => entries,
            Node::Branch(_) => &[],
        }
    }

    /// A branch's children; none for a leaf.
    pub(crate) fn children(&self) -> &[Child] {
        match self {
            Node::Leaf(_) => &[],
            Node::Branch(children) => children,
        }
    }

    /// The keys of this node's items.
    pub(crate) fn keys(&self) -> Vec<Bytes> {
        match self {
            Node::Leaf(entries) => entries.iter().map(|(key, _)| key.clone()).collect(),
            Node::Branch(children) => children.iter().map(|child| child.key.clone()).collect(),
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

    /// The DRISL encoding and its CID. Counts must be DASL integers.
    pub(crate) fn encode(&self) -> Result<(Cid<Bdasl>, Bytes), Error> {
        let (kind, field, items) = match self {
            Node::Leaf(entries) => (
                LEAF,
                "entries",
                entries
                    .iter()
                    .map(|(key, value)| Value::List(vec![Value::Bytes(key.clone()), value.clone()]))
                    .collect(),
            ),
            Node::Branch(children) => (
                BRANCH,
                "children",
                children
                    .iter()
                    .map(|child| {
                        let count = i64::try_from(child.count).ok().context(TooLargeSnafu)?;
                        Ok(Value::List(vec![
                            Value::Bytes(child.key.clone()),
                            Value::Integer(count),
                            Value::Link(child.cid),
                        ]))
                    })
                    .collect::<Result<_, Error>>()?,
            ),
        };
        let record = BTreeMap::from([
            ("$type".to_owned(), Value::String(kind.to_owned())),
            (field.to_owned(), Value::List(items)),
        ]);
        let bytes = Value::Map(record).encode()?;
        let cid = Cid::<Bdasl>::of(Codec::Drisl, HashFn::Blake3, &bytes);
        Ok((cid, bytes.into()))
    }

    fn from_value(value: Value) -> Option<Self> {
        let Value::Map(mut record) = value else {
            return None;
        };
        let Some(Value::String(kind)) = record.remove("$type") else {
            return None;
        };
        let field = match kind.as_str() {
            LEAF => "entries",
            BRANCH => "children",
            _ => return None,
        };
        let Some(Value::List(items)) = record.remove(field) else {
            return None;
        };
        if !record.is_empty() {
            return None;
        }
        let items = items.into_iter().map(|item| match item {
            Value::List(fields) => Some(fields),
            _ => None,
        });
        let node = if field == "entries" {
            let entries = items.map(|fields| match <[Value; 2]>::try_from(fields?) {
                Ok([Value::Bytes(key), value]) => Some((key, value)),
                _ => None,
            });
            let entries: Vec<_> = entries.collect::<Option<_>>()?;
            if !entries.is_sorted_by(|a, b| a.0 < b.0) {
                return None;
            }
            Node::Leaf(entries)
        } else {
            let children = items.map(|fields| match <[Value; 3]>::try_from(fields?) {
                Ok([Value::Bytes(key), Value::Integer(count), Value::Link(cid)])
                    if count > 0 && names_node(cid) =>
                {
                    Some(Child {
                        key,
                        count: count.unsigned_abs(),
                        cid,
                    })
                }
                _ => None,
            });
            let children: Vec<_> = children.collect::<Option<_>>()?;
            if children.is_empty() || !children.is_sorted_by(|a, b| a.key < b.key) {
                return None;
            }
            Node::Branch(children)
        };
        node.count().is_some().then_some(node)
    }
}
