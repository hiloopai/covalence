//! Node layouts and the fixed Lexicon record encoding.

use std::collections::BTreeMap;

use covalence_lib_dasl::Value;
use covalence_lib_data::Bytes;
use covalence_lib_error::snafu::OptionExt;

use crate::{Child, Error, Node, TooLargeSnafu};

/// Converts typed tree nodes to and from DASL records.
///
/// A layout is fixed by its type. It must encode equal nodes identically and
/// decode its records back to the same keys and values. It must encode an
/// empty leaf and reject records outside its format.
///
/// The tree handles DRISL bytes, CIDs, bytewise order, chunking and structural
/// checks. A layout with `Value = ()` represents a set of its keys.
pub trait Layout {
    /// The key type, whose bytes determine its identity, order and hash.
    type Key: AsRef<[u8]> + Clone + Send + Sync;

    /// The value kept alongside each key.
    type Value: Clone + PartialEq + Send + Sync;

    /// Convert a logical leaf or branch into its DASL record.
    fn encode(node: &Node<Self::Key, Self::Value>) -> Result<Value, Error>;

    /// Recover a logical node, or return `None` for an unrecognized record.
    fn decode(record: Value) -> Option<Node<Self::Key, Self::Value>>;
}

/// The original Lexicon records with byte keys and DASL values.
pub struct LexiconLayout;

const LEAF: &str = "vc.cov.prolly.leaf";
const BRANCH: &str = "vc.cov.prolly.branch";

impl Layout for LexiconLayout {
    type Key = Bytes;
    type Value = Value;

    fn encode(node: &Node) -> Result<Value, Error> {
        let (kind, field, items) = match node {
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
        Ok(Value::Map(BTreeMap::from([
            ("$type".to_owned(), Value::String(kind.to_owned())),
            (field.to_owned(), Value::List(items)),
        ])))
    }

    fn decode(value: Value) -> Option<Node> {
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
        if field == "entries" {
            let entries = items.map(|fields| match <[Value; 2]>::try_from(fields?) {
                Ok([Value::Bytes(key), value]) => Some((key, value)),
                _ => None,
            });
            Some(Node::Leaf(entries.collect::<Option<_>>()?))
        } else {
            let children = items.map(|fields| match <[Value; 3]>::try_from(fields?) {
                Ok([Value::Bytes(key), Value::Integer(count), Value::Link(cid)]) => Some(Child {
                    key,
                    count: u64::try_from(count).ok()?,
                    cid,
                }),
                _ => None,
            });
            Some(Node::Branch(children.collect::<Option<_>>()?))
        }
    }
}
