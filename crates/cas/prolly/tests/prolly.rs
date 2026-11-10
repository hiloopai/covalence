use std::{
    collections::{BTreeMap, BTreeSet},
    ops::{Bound, RangeBounds},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::Relaxed},
    },
};

use covalence_cas_memory::MemoryCas;
use covalence_cas_prolly::{Change, Edit, Error, Map};
use covalence_cas_trait::{
    AsyncGet, AsyncPut, Cas, CasFuture, Error as CasError, Get, GetExt, Put, ready,
};
use covalence_lib_cid::{Bdasl, Blake3, Cid, Cid32, Codec, HashFn, Sha256};
use covalence_lib_dasl::Value;
use covalence_lib_data::Bytes;
use proptest::prelude::*;

type Model = BTreeMap<Bytes, Value>;

/// Short keys from a small alphabet, so maps share keys and prefixes.
fn key() -> impl Strategy<Value = Bytes> {
    prop::collection::vec(0..64u8, 1..4).prop_map(Bytes::from)
}

fn value() -> impl Strategy<Value = Value> {
    prop_oneof![
        (0..10i64).prop_map(Value::Integer),
        (0..1000u32).prop_map(|i| Value::String(format!("v{i}"))),
        key().prop_map(Value::Bytes),
    ]
}

fn model(max: usize) -> impl Strategy<Value = Model> {
    prop::collection::btree_map(key(), value(), 0..max)
}

/// Sorted, distinct edits; `None` removes.
fn edits(max: usize) -> impl Strategy<Value = Vec<Edit>> {
    prop::collection::btree_map(key(), prop::option::of(value()), 0..max).prop_map(|edits| {
        edits
            .into_iter()
            .map(|(key, value)| match value {
                Some(value) => Edit::Insert(key, value),
                None => Edit::Remove(key),
            })
            .collect()
    })
}

fn edited(model: &Model, edits: &[Edit]) -> Model {
    let mut model = model.clone();
    for edit in edits {
        match edit {
            Edit::Insert(key, value) => model.insert(key.clone(), value.clone()),
            Edit::Remove(key) => model.remove(key),
        };
    }
    model
}

fn build(cas: &Arc<impl Cas + 'static>, model: &Model) -> Map {
    Map::build(cas.clone(), model.clone()).unwrap()
}

/// Check every read of `map` at `key` and `end` against `model`.
fn check(map: &Map, model: &Model, key: &Bytes, end: &Bytes) {
    map.verify().unwrap();
    assert_eq!(map.len().unwrap(), model.len() as u64);
    assert_eq!(map.is_empty(), model.is_empty());
    let all: Model = map.range(..).unwrap().map(Result::unwrap).collect();
    assert_eq!(&all, model);
    assert_eq!(map.get(key).unwrap().as_ref(), model.get(key));
    let rank = map.rank(key).unwrap();
    assert_eq!(rank, model.range(..key.clone()).count() as u64);
    let found = map.nth(rank).unwrap();
    assert_eq!(
        found.as_ref().map(|(k, v)| (k, v)),
        model.iter().nth(rank as usize)
    );
    assert_eq!(map.nth(model.len() as u64).unwrap(), None);
    for bounds in [
        (Bound::Included(key), Bound::Excluded(end)),
        (Bound::Excluded(key), Bound::Included(end)),
        (Bound::Unbounded, Bound::Included(end)),
    ] {
        let byte_bounds = (bounds.0.map(|k| &k[..]), bounds.1.map(|k| &k[..]));
        let got: Vec<_> = map
            .range(byte_bounds)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let want: Vec<_> = model
            .iter()
            .filter(|(k, _)| (bounds.0.cloned(), bounds.1.cloned()).contains(*k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        assert_eq!(got, want);
    }
}

/// Edits taking `old` to `new`.
fn edits_of(old: &Model, new: &Model) -> Vec<Edit> {
    changes(old, new)
        .into_iter()
        .map(|change| match change.new {
            Some(value) => Edit::Insert(change.key, value),
            None => Edit::Remove(change.key),
        })
        .collect()
}

fn changes(old: &Model, new: &Model) -> Vec<Change> {
    let keys: BTreeSet<&Bytes> = old.keys().chain(new.keys()).collect();
    keys.into_iter()
        .filter(|key| old.get(*key) != new.get(*key))
        .map(|key| Change {
            key: key.clone(),
            old: old.get(key).cloned(),
            new: new.get(key).cloned(),
        })
        .collect()
}

/// The resolver the union tests use: both values, ours first.
fn both(_: &Bytes, ours: Value, theirs: Value) -> Value {
    Value::List(vec![ours, theirs])
}

fn union_model(ours: &Model, theirs: &Model) -> Model {
    let mut union = ours.clone();
    for (key, value) in theirs {
        let merged = match union.remove(key) {
            Some(ours) if ours != *value => both(key, ours, value.clone()),
            _ => value.clone(),
        };
        union.insert(key.clone(), merged);
    }
    union
}

/// A small deterministic shuffle, seeded by the test case.
fn shuffle<T>(items: &mut [T], mut seed: u64) {
    for i in (1..items.len()).rev() {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        items.swap(i, (seed >> 33) as usize % (i + 1));
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn matches_btreemap(
        model in prop_oneof![model(20), model(3000)],
        edits in prop_oneof![edits(5), edits(300)],
        key in key(),
        end in key(),
    ) {
        let cas = Arc::new(MemoryCas::default());
        let map = build(&cas, &model);
        check(&map, &model, &key, &end);
        let new_model = edited(&model, &edits);
        let new = map.apply(edits).unwrap();
        check(&new, &new_model, &key, &end);
        prop_assert_eq!(new.root(), build(&cas, &new_model).root());
        prop_assert_eq!(map.diff(&new).unwrap(), changes(&model, &new_model));
        prop_assert_eq!(new.diff(&map).unwrap(), changes(&new_model, &model));
    }

    #[test]
    fn canonical_whatever_the_edit_order(
        model in model(3000),
        edits in edits(400),
        batch in prop::collection::vec(0..8usize, 400),
    ) {
        let cas = Arc::new(MemoryCas::default());
        let want = build(&cas, &edited(&model, &edits));
        let mut batches = vec![Vec::new(); 8];
        for (edit, i) in edits.iter().cloned().zip(batch) {
            batches[i].push(edit);
        }
        let mut map = build(&cas, &model);
        for batch in batches {
            map = map.apply(batch.clone()).unwrap();
        }
        prop_assert_eq!(map.root(), want.root());
        let undo = edits_of(&edited(&model, &edits), &model);
        prop_assert_eq!(map.apply(undo).unwrap().root(), build(&cas, &model).root());
    }

    /// Building, inserting one entry at a time in any order, and any history
    /// of single inserts and removes give the same root, which verifies.
    #[test]
    fn canonical_whatever_the_history(
        model in model(600),
        noise in edits(200),
        seed in any::<u64>(),
    ) {
        let cas = Arc::new(MemoryCas::default());
        let want = build(&cas, &model);
        want.verify().unwrap();

        let mut entries: Vec<_> = model.clone().into_iter().collect();
        shuffle(&mut entries, seed);
        let mut map = Map::empty(cas.clone()).unwrap();
        for (key, value) in entries {
            map = map.insert(key, value).unwrap();
        }
        prop_assert_eq!(map.root(), want.root());

        // Random single edits, then single edits back to `model`, all in
        // shuffled order.
        let mut history: Vec<_> = noise.into_iter().chain(edits_of(&Model::new(), &model)).collect();
        shuffle(&mut history, seed ^ 1);
        let (mut map, mut now) = (Map::empty(cas.clone()).unwrap(), Model::new());
        for edit in history {
            now = edited(&now, std::slice::from_ref(&edit));
            map = map.apply([edit]).unwrap();
        }
        map.verify().unwrap();
        let mut back = edits_of(&now, &model);
        shuffle(&mut back, seed ^ 2);
        for edit in back {
            map = map.apply([edit]).unwrap();
        }
        prop_assert_eq!(map.root(), want.root());
    }

    /// Union and diff of two edits of one map, which share most subtrees,
    /// match the model and give canonical maps.
    #[test]
    fn union_and_diff(
        base in prop_oneof![model(50), model(3000)],
        ours in edits(200),
        theirs in edits(200),
    ) {
        let cas = Arc::new(MemoryCas::default());
        let base = build(&cas, &base);
        let (ours_model, theirs_model) = {
            let all: Model = base.range(..).unwrap().map(Result::unwrap).collect();
            (edited(&all, &ours), edited(&all, &theirs))
        };
        let ours = base.apply(ours).unwrap();
        let theirs = base.apply(theirs).unwrap();
        prop_assert_eq!(ours.diff(&theirs).unwrap(), changes(&ours_model, &theirs_model));
        let union = ours.union(&theirs, both).unwrap();
        let want = union_model(&ours_model, &theirs_model);
        prop_assert_eq!(union.root(), build(&cas, &want).root());
        union.verify().unwrap();
        prop_assert_eq!(ours.union(&ours, both).unwrap().root(), ours.root());
    }
}

/// Union reads `other` from its own store and writes to this map's.
#[test]
fn union_across_stores() {
    let (ours_cas, theirs_cas) = (
        Arc::new(MemoryCas::default()),
        Arc::new(MemoryCas::default()),
    );
    let key = |i: u64| Bytes::from(format!("key{i:05}"));
    let ours: Model = (0..3000).map(|i| (key(i), Value::Integer(1))).collect();
    let theirs: Model = (2000..5000).map(|i| (key(i), Value::Integer(2))).collect();
    let union = build(&ours_cas, &ours)
        .union(&build(&theirs_cas, &theirs), both)
        .unwrap();
    union.verify().unwrap();
    let want = union_model(&ours, &theirs);
    assert_eq!(
        union.root(),
        build(&Arc::new(MemoryCas::default()), &want).root()
    );
}

#[test]
fn one_entry_at_a_time() {
    let cas = Arc::new(MemoryCas::default());
    let key = |i: u64| Bytes::from(format!("{:08}", i.wrapping_mul(0x9e37_79b9) % 100_000_000));
    let model: Model = (0..1500).map(|i| (key(i), Value::Null)).collect();
    let mut map = Map::empty(cas.clone()).unwrap();
    for i in 0..1500 {
        map = map.insert(key(i), Value::Null).unwrap();
    }
    assert_eq!(map.root(), build(&cas, &model).root());
    for i in (0..1500).rev() {
        map = map.remove(key(i)).unwrap();
    }
    assert!(map.is_empty());
}

/// Whether `key` ends a node of `level` that holds at least 2 items.
fn ends(level: u8, key: &[u8]) -> bool {
    let digest = HashFn::Blake3.digest(&[&[level], key].concat());
    u32::from_be_bytes(digest[..4].try_into().unwrap()) < 1 << 26
}

/// The first key `{prefix}{i}` that ends a node of `level`, or does not.
fn key_at(prefix: &str, level: u8, boundary: bool) -> String {
    (0..)
        .map(|i| format!("{prefix}{i}"))
        .find(|key| ends(level, key.as_bytes()) == boundary)
        .unwrap()
}

/// Keys that never end a leaf by hash.
fn quiet_keys(n: usize) -> Model {
    (0u32..)
        .map(|i| Bytes::from(i.to_be_bytes().to_vec()))
        .filter(|key| !ends(0, key))
        .take(n)
        .map(|key| (key, Value::Null))
        .collect()
}

/// Leaves that end at the maximum width.
#[test]
fn full_width_leaves() {
    let cas = Arc::new(MemoryCas::default());
    let model = quiet_keys(3000);
    let map = build(&cas, &model);
    let edits: Vec<_> = model
        .keys()
        .step_by(7)
        .map(|k| Edit::Remove(k.clone()))
        .collect();
    let new_model = edited(&model, &edits);
    let new = map.apply(edits).unwrap();
    assert_eq!(new.root(), build(&cas, &new_model).root());
    check(
        &new,
        &new_model,
        &Bytes::from_static(&[0, 0, 1]),
        &Bytes::from_static(&[0, 0, 9]),
    );
}

/// A boundary key does not end a node of one item.
#[test]
fn min_width_two() {
    let cas = Arc::new(MemoryCas::default());
    let boundary = key_at("", 0, true);
    let map = Map::build(
        cas.clone(),
        [(boundary.into(), Value::Null), ("z".into(), Value::Null)],
    )
    .unwrap();
    let fact = cas.get_fact(&Cid32::from(map.root())).unwrap().unwrap();
    let Value::Map(record) = Value::decode(fact.bytes()).unwrap() else {
        panic!("not a record");
    };
    assert_eq!(record["$type"], Value::String("vc.cov.prolly.leaf".into()));
}

/// The root of a fixed map, pinned so that a change to how trees are built
/// shows up.
#[test]
fn golden_root() {
    let cas = Arc::new(MemoryCas::default());
    let model: Model = (0..2000)
        .map(|i| (Bytes::from(format!("key{i:05}")), Value::Integer(i)))
        .collect();
    assert_eq!(
        build(&cas, &model).root().to_string(),
        "bafyr4idaucrnpulmucff2ca3xvtw6uj74fh24sp3fjzhlhyrgblcanwfku"
    );
}

/// Keys out of order are an error, also across a leaf closed at the maximum
/// width, and no map is returned.
#[test]
fn rejects_unsorted_keys() {
    let cas = Arc::new(MemoryCas::default());
    let mut entries: Vec<_> = quiet_keys(1025).into_iter().collect();
    let repeated = entries[1023].clone();
    entries[1024] = repeated.clone();
    assert!(matches!(
        Map::build(cas.clone(), entries),
        Err(Error::Unsorted { key }) if key == repeated.0
    ));
    let map = Map::empty(cas.clone()).unwrap();
    let edits = [Edit::Remove("b".into()), Edit::Remove("a".into())];
    assert!(matches!(map.apply(edits), Err(Error::Unsorted { key }) if key == "a"));
    let edits = [Edit::Remove("a".into()), Edit::Remove("a".into())];
    assert!(matches!(map.apply(edits), Err(Error::Unsorted { .. })));
}

/// Counts reads and puts, and can lose blocks.
#[derive(Default)]
struct Counting {
    cas: MemoryCas,
    reads: AtomicUsize,
    puts: AtomicUsize,
    lost: Vec<Cid32>,
}

impl Counting {
    fn take(&self) -> (usize, usize) {
        (self.reads.swap(0, Relaxed), self.puts.swap(0, Relaxed))
    }
}

impl Get for Counting {
    fn get_unchecked(&self, key: &Cid32) -> Result<Option<Bytes>, CasError> {
        self.reads.fetch_add(1, Relaxed);
        if self.lost.contains(key) {
            return Ok(None);
        }
        self.cas.get_unchecked(key)
    }
}

impl AsyncGet for Counting {
    fn aget_unchecked<'a>(&'a self, key: &'a Cid32) -> CasFuture<'a, Option<Bytes>> {
        ready(self.get_unchecked(key))
    }
}

impl Put for Counting {
    fn put(&self, bytes: Bytes) -> Result<Cid32, CasError> {
        self.puts.fetch_add(1, Relaxed);
        self.cas.put(bytes)
    }
}

impl AsyncPut for Counting {
    fn aput(&self, bytes: Bytes) -> CasFuture<'_, Cid32> {
        ready(self.put(bytes))
    }
}

impl Cas for Counting {}

/// Keys blocks by SHA-256, so it cannot find nodes by their CIDs.
#[derive(Default)]
struct Rekeying(MemoryCas);

impl Get for Rekeying {
    fn get_unchecked(&self, key: &Cid32) -> Result<Option<Bytes>, CasError> {
        self.0.get_unchecked(key)
    }
}

impl AsyncGet for Rekeying {
    fn aget_unchecked<'a>(&'a self, key: &'a Cid32) -> CasFuture<'a, Option<Bytes>> {
        ready(self.get_unchecked(key))
    }
}

impl Put for Rekeying {
    fn put(&self, bytes: Bytes) -> Result<Cid32, CasError> {
        Ok(Sha256::of(&bytes).into())
    }
}

impl AsyncPut for Rekeying {
    fn aput(&self, bytes: Bytes) -> CasFuture<'_, Cid32> {
        ready(self.put(bytes))
    }
}

impl Cas for Rekeying {}

#[test]
fn stores_must_keep_nodes_by_cid() {
    let cas = Arc::new(Rekeying::default());
    let empty = Cid::<Bdasl>::of(Codec::Drisl, HashFn::Blake3, &leaf_bytes(&[]));
    assert!(matches!(Map::empty(cas.clone()), Err(Error::Rekeyed { cid }) if cid == empty));
}

/// 100k entries: edits, reads, diffs and unions touch few nodes.
#[test]
fn large_map() {
    let cas = Arc::new(Counting::default());
    let key = |i: u64| Bytes::from(format!("key{i:08}"));
    let n = 100_000;
    let map = Map::build(
        cas.clone(),
        (0..n).map(|i| (key(i), Value::Integer(i as i64))),
    )
    .unwrap();
    let edits = [
        Edit::Remove(key(10)),
        Edit::Insert(key(50_000), Value::Null),
        Edit::Insert("key00070000x".into(), Value::Null),
    ];
    cas.take();
    let new = map.apply(edits).unwrap();
    let (reads, puts) = cas.take();
    assert!(reads <= 20 && puts <= 15, "{reads} reads, {puts} puts");
    assert_eq!(new.len().unwrap(), n);
    cas.take();
    assert_eq!(
        new.nth(69_999).unwrap(),
        Some((key(70_000), Value::Integer(70_000)))
    );
    assert_eq!(new.rank(&key(70_001)).unwrap(), 70_001);
    assert_eq!(new.get(&key(10)).unwrap(), None);
    let (reads, _) = cas.take();
    assert!(reads <= 3 * 4, "{reads} reads");
    assert_eq!(map.diff(&new).unwrap().len(), 3);
    let (reads, _) = cas.take();
    assert!(reads < 30, "{reads} reads");
    let union = map.union(&new, |_, _, theirs| theirs).unwrap();
    let (reads, puts) = cas.take();
    assert!(reads < 50 && puts <= 15, "{reads} reads, {puts} puts");
    assert_eq!(union.len().unwrap(), n + 1);
}

#[test]
fn apply_at_the_end_is_canonical() {
    let cas = Arc::new(MemoryCas::default());
    let model: Model = (0..4092)
        .map(|i| (Bytes::from(format!("2044/{i:05}")), Value::Null))
        .collect();
    let map = build(&cas, &model);
    let edits = [Edit::Remove("2044/00000".into())];
    let map = map.apply(edits.clone()).unwrap();
    assert_eq!(map.root(), build(&cas, &edited(&model, &edits)).root());
}

/// Store `value` under its DRISL CID.
fn put_value(cas: &MemoryCas, value: &Value) -> Cid<Bdasl> {
    let _: Cid32 = cas.put(value.encode().unwrap().into()).unwrap();
    value.cid().unwrap()
}

fn record_value(kind: &str, field: &str, items: Vec<Value>) -> Value {
    let record = [
        ("$type", Value::String(format!("vc.cov.prolly.{kind}"))),
        (field, Value::List(items)),
    ];
    Value::Map(record.map(|(k, v)| (k.to_owned(), v)).into())
}

fn record(cas: &MemoryCas, kind: &str, field: &str, items: Vec<Value>) -> Cid<Bdasl> {
    put_value(cas, &record_value(kind, field, items))
}

fn leaf_value(keys: &[&str]) -> Value {
    let entries = keys
        .iter()
        .map(|key| {
            Value::List(vec![
                Value::Bytes(key.as_bytes().to_vec().into()),
                Value::Null,
            ])
        })
        .collect();
    record_value("leaf", "entries", entries)
}

fn leaf_bytes(keys: &[&str]) -> Vec<u8> {
    leaf_value(keys).encode().unwrap()
}

fn leaf(cas: &MemoryCas, keys: &[&str]) -> Cid<Bdasl> {
    put_value(cas, &leaf_value(keys))
}

fn branch_value(children: &[(&str, i64, Cid<Bdasl>)]) -> Value {
    let children = children
        .iter()
        .map(|(key, count, cid)| {
            Value::List(vec![
                Value::Bytes(key.as_bytes().to_vec().into()),
                Value::Integer(*count),
                Value::Link(*cid),
            ])
        })
        .collect();
    record_value("branch", "children", children)
}

fn branch(cas: &MemoryCas, children: &[(&str, i64, Cid<Bdasl>)]) -> Cid<Bdasl> {
    put_value(cas, &branch_value(children))
}

/// The CID of a node that is not stored.
fn unstored(value: &Value) -> Cid<Bdasl> {
    value.cid().unwrap()
}

/// The first node a read of `root` rejects.
fn malformed(cas: &Arc<MemoryCas>, root: Cid<Bdasl>) -> Option<Cid<Bdasl>> {
    let map = Map::open(cas.clone(), root);
    let reads = [
        map.get(b"b").map(drop),
        map.get(b"d").map(drop),
        map.range(..)
            .and_then(|range| range.collect::<Result<Vec<_>, _>>())
            .map(drop),
        map.verify(),
    ];
    let mut cids = reads.into_iter().filter_map(|read| match read {
        Err(Error::Malformed { cid }) => Some(cid),
        _ => None,
    });
    cids.next()
}

#[test]
fn rejects_other_records() {
    let cas = Arc::new(MemoryCas::default());
    let bad = [
        record(
            &cas,
            "leaf",
            "entries",
            vec![Value::List(vec![Value::Integer(1), Value::Null])],
        ),
        record(&cas, "leaf", "children", vec![]),
        record(&cas, "branch", "children", vec![]),
        // Another field, or none.
        put_value(&cas, &{
            let Value::Map(mut map) = leaf_value(&["a"]) else {
                unreachable!()
            };
            map.insert("extra".into(), Value::Null);
            Value::Map(map)
        }),
        put_value(&cas, &Value::Null),
        Cid::from(Put::<Blake3>::put(&*cas, "raw".into()).unwrap()),
        // A leaf stored under a SHA-256 CID.
        Cid::<Bdasl>::of(Codec::Drisl, HashFn::Sha256, &leaf_bytes(&["a"])),
    ];
    for root in bad {
        let got = Map::open(cas.clone(), root).get(b"a");
        assert!(matches!(got, Err(Error::Malformed { cid }) if cid == root));
    }
    assert_eq!(leaf(&cas, &[]), Map::empty(cas.clone()).unwrap().root());
    let missing = Cid::<Bdasl>::of(Codec::Drisl, HashFn::Blake3, b"missing");
    let got = Map::open(cas.clone(), missing).get(b"a");
    assert!(matches!(got, Err(Error::Missing { cid }) if cid == missing));
}

/// Keys `a < b < c < d` with which `[[a, b], [c, d]]` is canonical: `b` ends
/// a leaf and `c` does not.
fn abcd() -> [String; 4] {
    let (b, c) = (key_at("b", 0, true), key_at("c", 0, false));
    ["a".into(), b, c, "d".into()]
}

/// Trees whose answers would be wrong are rejected where they are read.
#[test]
fn checks_keys_links_and_counts() {
    let cas = Arc::new(MemoryCas::default());
    let [a, b, c, d] = abcd();
    let (a, b, c, d) = (&a[..], &b[..], &c[..], &d[..]);
    let ab = leaf(&cas, &[a, b]);
    let cd = leaf(&cas, &[c, d]);
    let good = branch(&cas, &[(a, 2, ab), (c, 2, cd)]);
    assert_eq!(malformed(&cas, good), None);
    let model: Model = [a, b, c, d]
        .map(|key| (Bytes::from(key.to_owned()), Value::Null))
        .into();
    assert_eq!(build(&cas, &model).root(), good);

    let unsorted = leaf(&cas, &[b, a]);
    let repeated = leaf(&cas, &[a, a]);
    let bd = leaf(&cas, &[b, d]);
    let raw = ab.with_codec(Codec::Raw);
    let sha = Cid::<Bdasl>::of(Codec::Drisl, HashFn::Sha256, &leaf_bytes(&[a, b]));
    // Each tree, and the child that is rejected (`None` for the root).
    for (root, child) in [
        (unsorted, None),
        (repeated, None),
        // Children out of order, a zero count, or a raw child link.
        (branch(&cas, &[(c, 2, cd), (a, 2, ab)]), None),
        (branch(&cas, &[(a, 0, ab), (c, 2, cd)]), None),
        (branch(&cas, &[(a, 2, raw), (c, 2, cd)]), None),
        (branch(&cas, &[(a, 2, sha), (c, 2, cd)]), None),
        // Counts that overflow a `u64`.
        (
            branch(&cas, &[(a, i64::MAX, ab), (c, i64::MAX, cd), ("e", 2, cd)]),
            None,
        ),
        // A child that does not start at its key, or holds another count.
        (branch(&cas, &[(a, 2, bd), (c, 2, cd)]), Some(bd)),
        (branch(&cas, &[(a, 3, ab), (c, 2, cd)]), Some(ab)),
        // A child with keys from the next child's key on.
        (branch(&cas, &[(a, 2, ab), (b, 2, bd)]), Some(ab)),
    ] {
        assert_eq!(malformed(&cas, root), Some(child.unwrap_or(root)));
    }
}

/// Trees with the right keys but another shape are rejected: reads reject
/// the nodes they load, and `verify` any node.
#[test]
fn rejects_non_canonical_trees() {
    let cas = Arc::new(MemoryCas::default());
    let [a, b, c, d] = abcd();
    let (a, b, c, d) = (&a[..], &b[..], &c[..], &d[..]);
    let ab = leaf(&cas, &[a, b]);
    let cd = leaf(&cas, &[c, d]);
    let good = branch(&cas, &[(a, 2, ab), (c, 2, cd)]);
    let bc = key_at("bc", 0, false);
    let abc = leaf(&cas, &[a, b, &bc]);
    let abcd_leaf = leaf(&cas, &[a, b, c, d]);
    let a_ = leaf(&cas, &[a]);
    let bcd = leaf(&cas, &[b, c, d]);
    let a_bc = leaf(&cas, &[a, &bc]);
    let over_ab = branch(&cas, &[(a, 2, ab)]);
    let over_cd = branch(&cas, &[(c, 2, cd)]);
    let over_good = branch(&cas, &[(a, 4, good)]);
    // Each tree, and the node that is rejected.
    for (root, bad) in [
        // A leaf that holds a boundary before its end.
        (branch(&cas, &[(a, 3, abc), (c, 2, cd)]), abc),
        (abcd_leaf, abcd_leaf),
        // A leaf split early, under-full, or not at a boundary.
        (branch(&cas, &[(a, 1, a_), (b, 3, bcd)]), a_),
        (branch(&cas, &[(a, 2, a_bc), (c, 2, cd)]), a_bc),
        // A one-child root.
        (over_ab, over_ab),
        (over_good, over_good),
        // An extra level of one-child branches, and uneven heights.
        (branch(&cas, &[(a, 2, over_ab), (c, 2, over_cd)]), over_ab),
        (branch(&cas, &[(a, 2, ab), (c, 2, over_cd)]), over_cd),
    ] {
        assert_eq!(malformed(&cas, root), Some(bad));
    }
}

/// A node wider than the maximum is rejected, even the root.
#[test]
fn rejects_wide_nodes() {
    let cas = Arc::new(MemoryCas::default());
    let model = quiet_keys(1025);
    let entries = model
        .keys()
        .map(|key| Value::List(vec![Value::Bytes(key.clone()), Value::Null]))
        .collect();
    let wide = record(&cas, "leaf", "entries", entries);
    assert_eq!(malformed(&cas, wide), Some(wide));
    assert_ne!(build(&cas, &model).root(), wide);
}

/// Reads trust the counts of subtrees they skip; `verify` does not.
#[test]
fn verify_checks_skipped_counts() {
    let cas = Arc::new(MemoryCas::default());
    let [a, b, c, d] = abcd();
    let ab = leaf(&cas, &[&a, &b]);
    let cd = leaf(&cas, &[&c, &d]);
    let root = branch(&cas, &[(&a, 2, ab), (&c, 5, cd)]);
    let map = Map::open(cas.clone(), root);
    assert_eq!(map.len().unwrap(), 7);
    assert_eq!(map.get(a.as_bytes()).unwrap(), Some(Value::Null));
    assert!(matches!(map.verify(), Err(Error::Malformed { cid }) if cid == cd));
}

/// `apply` rewrites leaves only at the leftmost one's depth, at most 64
/// levels down.
#[test]
fn apply_checks_height() {
    let cas = Arc::new(MemoryCas::default());
    let [a, b, c, d] = abcd();
    let ab = leaf(&cas, &[&a, &b]);
    let cd = leaf(&cas, &[&c, &d]);
    let over_cd = branch(&cas, &[(&c, 2, cd)]);
    let uneven = Map::open(cas.clone(), branch(&cas, &[(&a, 2, ab), (&c, 2, over_cd)]));
    let edit = || [Edit::Insert(d.clone().into(), Value::Integer(1))];
    assert!(matches!(uneven.apply(edit()), Err(Error::Malformed { cid }) if cid == over_cd));

    let mut tall = ab;
    for _ in 0..65 {
        tall = branch(&cas, &[(&a, 2, tall)]);
    }
    let got = Map::open(cas.clone(), tall).apply(edit());
    assert!(matches!(got, Err(Error::Malformed { cid }) if cid == tall));
}

/// Counts of children fed whole are trusted, but a total too large to
/// record is an error.
#[test]
fn apply_rejects_huge_counts() {
    let cas = Arc::new(MemoryCas::default());
    let unread = |name: &[u8]| Cid::<Bdasl>::of(Codec::Drisl, HashFn::Blake3, name);
    let [a, b, ..] = abcd();
    // Keys that do not end the root.
    let (x, y, z) = (key_at("x", 1, false), key_at("y", 1, false), "z");
    let ab = leaf(&cas, &[&a, &b]);
    let z_ = leaf(&cas, &[z]);
    // `2^64 - 1` entries, as the root records.
    let root = branch(
        &cas,
        &[
            (&a, 2, ab),
            (&x, i64::MAX, unread(b"x")),
            (&y, i64::MAX - 2, unread(b"y")),
            (z, 1, z_),
        ],
    );
    let map = Map::open(cas.clone(), root);
    assert_eq!(map.len().unwrap(), u64::MAX);
    assert!(matches!(
        map.insert("zz".into(), Value::Null),
        Err(Error::TooLarge)
    ));
}

/// A value too deep to encode in a node is an error.
#[test]
fn rejects_deep_values() {
    let cas = Arc::new(MemoryCas::default());
    let mut value = Value::Null;
    for _ in 0..covalence_lib_dasl::MAX_DEPTH {
        value = Value::List(vec![value]);
    }
    assert!(matches!(
        Map::build(cas.clone(), [("a".into(), value)]),
        Err(Error::Encode { .. })
    ));
}

/// A range stops before children past its end, so it does not read them.
#[test]
fn range_reads_only_its_leaves() {
    let [a, b, c, d] = abcd();
    let mut cas = Counting::default();
    let ab = put_value(&cas.cas, &leaf_value(&[&a, &b]));
    let cd = unstored(&leaf_value(&[&c, &d]));
    let root = put_value(&cas.cas, &branch_value(&[(&a, 2, ab), (&c, 2, cd)]));
    cas.lost.push(cd.into());
    let cas = Arc::new(cas);
    let map = Map::open(cas.clone(), root);
    let got: Vec<_> = map
        .range((Bound::Unbounded, Bound::Excluded(c.as_bytes())))
        .unwrap()
        .map(|entry| entry.unwrap().0)
        .collect();
    assert_eq!(got, [Bytes::from(a), Bytes::from(b)]);
    assert_eq!(cas.take().0, 2);
    let got = map
        .range((Bound::Unbounded, Bound::Included(c.as_bytes())))
        .unwrap()
        .last()
        .unwrap();
    assert!(matches!(got, Err(Error::Missing { cid }) if cid == cd));
}

/// `diff` reads entries before a shared subtree first, so it skips the
/// subtree together on both sides, unread.
#[test]
fn diff_skips_shared_subtrees_unread() {
    let cas = Arc::new(MemoryCas::default());
    let model = |range: std::ops::Range<u32>| -> Model {
        range
            .map(|i| (Bytes::from(format!("key{i:05}")), Value::Null))
            .collect()
    };
    let old = build(&cas, &model(0..5000));
    let new = build(&cas, &model(1000..5000));
    // Lose every node of `new` that `old` also has, but the roots.
    let reachable = |map: &Map| -> BTreeSet<Cid<Bdasl>> {
        let mut cids = BTreeSet::new();
        let mut todo = vec![map.root()];
        while let Some(cid) = todo.pop() {
            cids.insert(cid);
            let fact = cas.get_fact(&Cid32::from(cid)).unwrap().unwrap();
            if let Value::Map(record) = Value::decode(fact.bytes()).unwrap()
                && let Some(Value::List(children)) = record.get("children")
            {
                for child in children {
                    let Value::List(fields) = child else { panic!() };
                    let Value::Link(cid) = fields[2] else {
                        panic!()
                    };
                    todo.push(cid);
                }
            }
        }
        cids
    };
    let lossy = Counting::default();
    let (old_nodes, new_nodes) = (reachable(&old), reachable(&new));
    for cid in old_nodes.union(&new_nodes) {
        let fact = cas.get_fact(&Cid32::from(*cid)).unwrap().unwrap();
        let _: Cid32 = lossy.cas.put(fact.bytes().clone()).unwrap();
    }
    let shared: Vec<Cid32> = old_nodes
        .intersection(&new_nodes)
        .map(|cid| (*cid).into())
        .collect();
    assert!(!shared.is_empty());
    let lossy = Arc::new(Counting {
        lost: shared,
        ..lossy
    });
    let (old, new) = (
        Map::open(lossy.clone(), old.root()),
        Map::open(lossy.clone(), new.root()),
    );
    let want = changes(&model(0..5000), &model(1000..5000));
    assert_eq!(old.diff(&new).unwrap(), want);
}

/// When every leaf of one side is shared, `diff` learns its height from the
/// other side, and still reads no shared node.
#[test]
fn diff_learns_heights_from_shared_subtrees() {
    let [a, b, ..] = abcd();
    let (c, e, f) = (key_at("c", 1, false), "e", "f");
    let c2 = key_at(&format!("{c}z"), 0, true);
    let mut cas = Counting::default();
    let la = put_value(&cas.cas, &leaf_value(&[&a, &b]));
    let lc = unstored(&leaf_value(&[&c, &c2]));
    let le = unstored(&leaf_value(&[e, f]));
    cas.lost.extend([Cid32::from(lc), Cid32::from(le)]);
    let old = branch_value(&[(&a, 2, la), (&c, 2, lc), (e, 2, le)]);
    let old = put_value(&cas.cas, &old);
    let new = put_value(&cas.cas, &branch_value(&[(&c, 2, lc), (e, 2, le)]));
    let removed = |key: &str| Change {
        key: key.to_owned().into(),
        old: Some(Value::Null),
        new: None,
    };
    let cas = Arc::new(cas);
    let (old, new) = (Map::open(cas.clone(), old), Map::open(cas.clone(), new));
    assert_eq!(old.diff(&new).unwrap(), [removed(&a), removed(&b)]);
    assert_eq!(cas.take().0, 3);
}

/// Maps derived from one another, or made with `at`, read a checked node
/// from the store once; an opened map starts afresh.
#[test]
fn derived_maps_share_checked_nodes() {
    let cas = Arc::new(Counting::default());
    let key = |i: u32| Bytes::from(format!("key{i:05}"));
    let entries = (0..10_000).map(|i| (key(i), Value::Null));
    let root = Map::build(cas.clone(), entries).unwrap().root();
    let map = Map::open(cas.clone(), root);
    cas.take();
    assert_eq!(map.get(&key(42)).unwrap(), Some(Value::Null));
    assert!(cas.take().0 >= 2);
    assert_eq!(map.get(&key(42)).unwrap(), Some(Value::Null));
    assert_eq!(map.at(root).rank(&key(42)).unwrap(), 42);
    let new = map.insert(key(10_000), Value::Null).unwrap();
    cas.take();
    assert_eq!(new.get(&key(42)).unwrap(), Some(Value::Null));
    assert_eq!(cas.take().0, 0);
    Map::open(cas.clone(), root).get(&key(42)).unwrap();
    assert!(cas.take().0 >= 2);
}

/// A key that ends no node at levels 0 and 1.
fn quiet_key(prefix: &str) -> String {
    (0..)
        .map(|i| format!("{prefix}{i}"))
        .find(|key| !ends(0, key.as_bytes()) && !ends(1, key.as_bytes()))
        .unwrap()
}

/// A cached node is checked again where its place differs: a leaf that may
/// end its level cannot be reused before another leaf.
#[test]
fn cached_nodes_are_checked_in_place() {
    let cas = Arc::new(MemoryCas::default());
    let (a, b) = ("a".to_owned(), key_at("b", 0, true));
    let (c, d, e, f) = (quiet_key("c"), quiet_key("d"), quiet_key("e"), "f");
    let ab = leaf(&cas, &[&a, &b]);
    let cd = leaf(&cas, &[&c, &d]);
    let ef = leaf(&cas, &[&e, f]);
    let good = Map::open(cas.clone(), branch(&cas, &[(&a, 2, ab), (&c, 2, cd)]));
    assert_eq!(good.get(d.as_bytes()).unwrap(), Some(Value::Null));
    let bad = branch(&cas, &[(&a, 2, ab), (&c, 2, cd), (&e, 2, ef)]);
    let got = good.at(bad).get(d.as_bytes());
    assert!(matches!(got, Err(Error::Malformed { cid }) if cid == cd));
    assert_eq!(malformed(&cas, bad), Some(cd));
}

/// A node too large to cache is returned but does not push out the others.
#[test]
fn large_nodes_skip_the_cache() {
    let cas = Arc::new(Counting::default());
    let key = |i: u32| Bytes::from(format!("key{i:05}"));
    let mut entries: Vec<_> = (0..10_000).map(|i| (key(i), Value::Null)).collect();
    let big = Value::Bytes(Bytes::from(vec![0; 17 << 20]));
    entries.push((Bytes::from("zz"), big.clone()));
    let map = Map::open(
        cas.clone(),
        Map::build(cas.clone(), entries).unwrap().root(),
    );
    map.get(&key(42)).unwrap();
    assert_eq!(map.get(b"zz").unwrap(), Some(big));
    cas.take();
    assert_eq!(map.get(&key(42)).unwrap(), Some(Value::Null));
    assert_eq!(cas.take().0, 0);
}
