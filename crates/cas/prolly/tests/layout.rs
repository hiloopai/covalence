use std::{collections::BTreeMap, ops::Bound};

use covalence_cas_memory::MemoryCas;
use covalence_cas_prolly::{
    Arena, Change, Child, Edit, Error, Layout, LexiconLayout, LexiconTree, Node, Tree,
};
use covalence_cas_trait::{Cas, GetExt};
use covalence_lib_cid::{Bdasl, Cid, Codec, HashFn};
use covalence_lib_dasl::Value;
use covalence_lib_data::Bytes;

#[derive(Clone, Debug, PartialEq)]
struct Name(Bytes);

impl AsRef<[u8]> for Name {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

// This test layout wraps the Lexicon record. Its typed entries carry no value.
struct Names;

impl Layout for Names {
    type Key = Name;
    type Value = ();

    fn encode(node: &Node<Name, ()>) -> Result<Value, Error> {
        let lexicon = match node {
            Node::Leaf(entries) => Node::Leaf(
                entries
                    .iter()
                    .map(|(name, ())| (name.0.clone(), Value::Null))
                    .collect(),
            ),
            Node::Branch(children) => Node::Branch(
                children
                    .iter()
                    .map(|child| Child {
                        key: child.key.0.clone(),
                        count: child.count,
                        cid: child.cid,
                    })
                    .collect(),
            ),
        };
        Ok(Value::Map(BTreeMap::from([(
            "names".into(),
            LexiconLayout::encode(&lexicon)?,
        )])))
    }

    fn decode(record: Value) -> Option<Node<Name, ()>> {
        let Value::Map(mut record) = record else {
            return None;
        };
        let lexicon = record.remove("names")?;
        if !record.is_empty() {
            return None;
        }
        Some(match LexiconLayout::decode(lexicon)? {
            Node::Leaf(entries) => Node::Leaf(
                entries
                    .into_iter()
                    .map(|(key, value)| match value {
                        Value::Null => Some((Name(key), ())),
                        _ => None,
                    })
                    .collect::<Option<_>>()?,
            ),
            Node::Branch(children) => Node::Branch(
                children
                    .into_iter()
                    .map(|child| Child {
                        key: Name(child.key),
                        count: child.count,
                        cid: child.cid,
                    })
                    .collect(),
            ),
        })
    }
}

type NameSet = Tree<Names>;

#[test]
fn typed_set_builds_opens_and_edits_its_own_layout() {
    let cas = MemoryCas::default().into_dyn();
    let name = |i| Name(Bytes::from(format!("name{i:04}")));
    let entries = (0..1200).map(|i| (name(i), ()));
    let mut arena = Arena::<Names>::new(cas.clone());
    let id = arena.insert_map(entries.clone()).unwrap();
    let built = arena.get(id).unwrap();
    assert_eq!(arena.map_cid(entries.clone()).unwrap(), built.root());
    let lexicon =
        LexiconTree::build(cas.clone(), entries.map(|(name, ())| (name.0, Value::Null))).unwrap();
    let root = cas.get_fact(&built.root().into()).unwrap().unwrap();
    assert!(matches!(
        Names::decode(Value::decode(root.bytes()).unwrap()),
        Some(Node::Branch(_))
    ));

    let read = NameSet::open(cas.clone(), built.root());
    read.verify().unwrap();
    assert!(matches!(
        read.at(lexicon.root()).verify(),
        Err(Error::Malformed { .. })
    ));
    assert!(matches!(
        lexicon.at(read.root()).verify(),
        Err(Error::Malformed { .. })
    ));
    assert_eq!(read.get(name(3).as_ref()).unwrap(), Some(()));
    assert_eq!(read.nth(3).unwrap(), Some((name(3), ())));
    assert_eq!(read.rank(name(500).as_ref()).unwrap(), 500);

    let start = name(3);
    let end = name(6);
    let range = (
        Bound::Excluded(start.as_ref()),
        Bound::Excluded(end.as_ref()),
    );
    let changed = read
        .apply([Edit::Remove(name(4)), Edit::Insert(name(1200), ())])
        .unwrap();
    assert_eq!(
        changed
            .range(range)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        [(name(5), ())]
    );
    assert_eq!(
        read.diff(&changed).unwrap(),
        [
            Change {
                key: name(4),
                old: Some(()),
                new: None
            },
            Change {
                key: name(1200),
                old: None,
                new: Some(())
            },
        ]
    );
    assert_eq!(
        changed
            .insert(name(4), ())
            .unwrap()
            .remove(name(1200))
            .unwrap()
            .root(),
        read.root()
    );
    let empty = NameSet::empty(cas.clone()).unwrap();
    assert!(read.at(empty.root()).is_empty());
    assert!(!read.at(LexiconTree::empty(cas).unwrap().root()).is_empty());
}

#[test]
fn structural_validation_applies_to_nodes_returned_by_a_layout() {
    let cas = MemoryCas::default().into_dyn();
    let key = Name("a".into());
    let unordered = Node::Leaf(vec![(key.clone(), ()), (key.clone(), ())]);
    let raw_link = cas.put(Bytes::from_static(b"raw")).unwrap().cast().unwrap();
    let raw_child = Node::Branch(vec![
        Child {
            key,
            count: 1,
            cid: raw_link,
        },
        Child {
            key: Name("b".into()),
            count: 1,
            cid: raw_link,
        },
    ]);
    let empty_branch = Node::Branch(vec![]);
    for node in [unordered, raw_child, empty_branch] {
        let bytes = Names::encode(&node).unwrap().encode().unwrap();
        let root = Cid::<Bdasl>::of(Codec::Drisl, HashFn::Blake3, &bytes);
        cas.put(bytes.into()).unwrap();
        assert!(
            matches!(NameSet::open(cas.clone(), root).verify(), Err(Error::Malformed { cid }) if cid == root)
        );
    }
}
