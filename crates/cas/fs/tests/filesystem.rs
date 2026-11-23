use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering::Relaxed},
};

use covalence_cas_fs::{Directory, Entry, EntryKind, Error, LeafMode, Trees};
use covalence_cas_memory::MemoryCas;
use covalence_cas_trait::{AsyncGet, AsyncPut, Cas, CasFuture, Get, Put, ready};
use covalence_lib_cid::{Bdasl, Blake3, Cid, Cid32, Codec, Fact, HashFn};
use covalence_lib_dasl::Value;
use covalence_lib_data::Bytes;

type CasError = covalence_cas_trait::Error;

#[derive(Clone, Copy, Default)]
enum Fault {
    #[default]
    None,
    WriteError,
    WrongKey,
    MissingRead,
    ReadError,
    WrongBytes,
}

#[derive(Default)]
struct Probe {
    mem: MemoryCas,
    reads: AtomicUsize,
    writes: AtomicUsize,
    fault: Mutex<Fault>,
}

impl Probe {
    fn counts(&self) -> (usize, usize) {
        (self.reads.load(Relaxed), self.writes.load(Relaxed))
    }
}

impl Get for Probe {
    fn get_unchecked(&self, key: &Cid) -> Result<Option<Bytes>, CasError> {
        self.reads.fetch_add(1, Relaxed);
        match *self.fault.lock().unwrap() {
            Fault::MissingRead => Ok(None),
            Fault::ReadError => Err(CasError::Io {
                source: std::io::Error::other("read failed"),
            }),
            Fault::WrongBytes => Ok(Some(Bytes::from_static(b"unrelated bytes"))),
            _ => self.mem.get_unchecked(key),
        }
    }
}

impl Put for Probe {
    fn put(&self, bytes: Bytes) -> Result<Cid, CasError> {
        self.writes.fetch_add(1, Relaxed);
        self.mem.put(bytes)
    }

    fn put_fact(&self, fact: Fact) -> Result<Cid, CasError> {
        self.writes.fetch_add(1, Relaxed);
        assert_eq!(fact.claim().codec(), Codec::Drisl.code());
        match *self.fault.lock().unwrap() {
            Fault::WriteError => Err(CasError::Io {
                source: std::io::Error::other("write failed"),
            }),
            Fault::WrongKey => Ok(Blake3::of(b"wrong key").into()),
            _ => self.mem.put_fact(fact),
        }
    }
}

impl AsyncGet for Probe {
    fn aget_unchecked<'a>(&'a self, key: &'a Cid) -> CasFuture<'a, Option<Bytes>> {
        ready(self.get_unchecked(key))
    }
}

impl AsyncPut for Probe {
    fn aput(&self, bytes: Bytes) -> CasFuture<'_, Cid> {
        ready(self.put(bytes))
    }
}

impl Cas for Probe {}

fn entry(name: &'static [u8], kind: EntryKind, cid: impl Into<Cid<Bdasl>>) -> Entry {
    Entry {
        name: Bytes::from_static(name),
        kind,
        cid: cid.into(),
    }
}

#[test]
fn bottom_up_hash_and_retained_cids_match_without_materializing_children() {
    let cas = Arc::new(Probe::default());
    let mut trees = Trees::new(cas.clone());
    let payload = Value::Bytes("hello".into()).cid().unwrap();
    let subdir = Directory::new(
        LeafMode::Dasl,
        vec![entry(b"\xff", EntryKind::Leaf, payload)],
    )
    .unwrap();
    let subdir_cid = subdir.cid().unwrap();
    let root = Directory::new(
        LeafMode::Bytes,
        vec![entry(b"sub", EntryKind::Directory, subdir_cid)],
    )
    .unwrap();
    let root_cid = root.cid().unwrap();
    assert_eq!(cas.counts(), (0, 0));

    let id = trees.insert(&root).unwrap();
    assert_eq!(trees.root(id), Some(root_cid));
    assert_eq!(cas.counts(), (1, 1));
    assert_eq!(trees.load(subdir_cid).unwrap(), None);
    assert_eq!(trees.store(&subdir).unwrap(), subdir_cid);
    assert_eq!(cas.counts(), (3, 2));
    assert_eq!(cas.mem.get_unchecked(&Cid32::from(payload)).unwrap(), None);

    let next = trees.insert(&subdir).unwrap();
    let repeated = trees.insert(&root).unwrap();
    assert_ne!(id, repeated);
    assert_eq!(trees.root(id), Some(root_cid));
    assert_eq!(trees.root(next), Some(subdir_cid));
    assert_eq!(trees.root(repeated), Some(root_cid));

    let reopened = Trees::new(cas);
    assert_eq!(reopened.root(id), None);
    assert_eq!(reopened.load(root_cid).unwrap(), Some(root));
    assert_eq!(reopened.load(subdir_cid).unwrap(), Some(subdir));
}

#[test]
fn canonical_bytes_and_cid_are_fixed() {
    // Independent DRISL bytes, with SHA-256 raw and BLAKE3 directory links.
    const WIRE: &[u8] = b"\x83\x73covalence.fs.dir.v1\x00\x82\
        \x83\x41a\x00\xd8\x2a\x58\x25\x00\x01\x55\x12\x20aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\
        \x83\x41\xff\x01\xd8\x2a\x58\x25\x00\x01\x71\x1e\x20bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let expected: Cid<Bdasl> = "bafyr4ifecknqvtfbk4k2dl2tcw6ksdrpgspgd7f7dvwfnx4idxl3mnoz6y"
        .parse()
        .unwrap();
    let a = entry(
        b"a",
        EntryKind::Leaf,
        Cid::<Bdasl>::from_digest(Codec::Raw, HashFn::Sha256, [b'a'; 32]),
    );
    let b = entry(
        b"\xff",
        EntryKind::Directory,
        Cid::<Bdasl>::from_digest(Codec::Drisl, HashFn::Blake3, [b'b'; 32]),
    );
    let forward = Directory::new(LeafMode::Bytes, vec![a.clone(), b.clone()]).unwrap();
    let reverse = Directory::new(LeafMode::Bytes, vec![b.clone(), a.clone()]).unwrap();
    assert_eq!(forward, reverse);
    assert_eq!(forward.children(), &[a, b.clone()]);
    assert_eq!(forward.child(b"\xff"), Some(&b));
    assert_eq!(forward.child(b"missing"), None);

    let cas = Arc::new(MemoryCas::default());
    let trees = Trees::new(cas.clone());
    assert_eq!(forward.cid().unwrap(), expected);
    assert_eq!(trees.store(&reverse).unwrap(), expected);
    assert_eq!(
        cas.get_unchecked(&Cid32::from(expected)).unwrap().unwrap(),
        WIRE
    );
    assert_eq!(trees.load(expected).unwrap(), Some(forward));
}

#[test]
fn names_are_lossless_components_and_unique_across_kinds() {
    let cid = Blake3::of(b"data");
    let dir = |entries| Directory::new(LeafMode::Bytes, entries);
    for name in [b"".as_slice(), b".", b"..", b"a/b", b"a\0b"] {
        assert!(matches!(
            dir(vec![entry(name, EntryKind::Leaf, cid)]),
            Err(Error::Name { .. })
        ));
    }
    let nfc = entry("é".as_bytes(), EntryKind::Leaf, cid);
    let nfd = entry("e\u{301}".as_bytes(), EntryKind::Leaf, cid);
    assert_eq!(
        dir(vec![nfc.clone(), nfd.clone()]).unwrap().children(),
        &[nfd, nfc]
    );

    let dir_cid = Cid::<Bdasl>::from(cid).with_codec(Codec::Drisl);
    assert!(matches!(
        dir(vec![
            entry(b"x", EntryKind::Leaf, cid),
            entry(b"x", EntryKind::Directory, dir_cid)
        ]),
        Err(Error::DuplicateName { .. })
    ));
}

#[test]
fn link_boundaries_check_codec_and_directory_profile_without_inspecting_targets() {
    let cas = Arc::new(Probe::default());
    let trees = Trees::new(cas.clone());
    let value = Value::Bytes("hello".into());
    let value_bytes = value.encode().unwrap();
    let empty = |mode| Directory::new(mode, vec![]).unwrap().cid().unwrap();
    assert_ne!(empty(LeafMode::Bytes), empty(LeafMode::Dasl));

    for hash in [HashFn::Blake3, HashFn::Sha256] {
        let raw = Cid::<Bdasl>::of(Codec::Raw, hash, b"hello");
        let drisl = Cid::<Bdasl>::of(Codec::Drisl, hash, &value_bytes);
        assert_ne!(raw.digest(), drisl.digest());
        for (mode, accepted, rejected) in
            [(LeafMode::Bytes, raw, drisl), (LeafMode::Dasl, drisl, raw)]
        {
            assert!(Directory::new(mode, vec![entry(b"f", EntryKind::Leaf, accepted)]).is_ok());
            assert!(matches!(
                Directory::new(mode, vec![entry(b"f", EntryKind::Leaf, rejected)]),
                Err(Error::ChildLink { .. })
            ));
        }
    }

    let raw = Cid::<Bdasl>::from(Blake3::of(b"raw"));
    let sha_dir = Cid::<Bdasl>::of(Codec::Drisl, HashFn::Sha256, b"placeholder");
    // A directory edge can be named even when its DRISL CID is for a scalar.
    let scalar = Value::Null.cid().unwrap();
    for mode in [LeafMode::Bytes, LeafMode::Dasl] {
        assert!(Directory::new(mode, vec![entry(b"d", EntryKind::Directory, scalar)]).is_ok());
        for wrong in [raw, sha_dir] {
            assert!(matches!(
                Directory::new(mode, vec![entry(b"d", EntryKind::Directory, wrong)]),
                Err(Error::ChildLink { .. })
            ));
            assert!(matches!(trees.load(wrong), Err(Error::DirectoryCid { .. })));
        }
    }
    assert_eq!(cas.counts(), (0, 0));
    Put::<Cid>::put(&cas.mem, Bytes::from_static(b"\xf6")).unwrap();
    assert!(matches!(trees.load(scalar), Err(Error::Malformed { .. })));
}

#[test]
fn load_rejects_wrong_tags_shapes_order_and_links_even_at_the_correct_digest() {
    let cas = Arc::new(MemoryCas::default());
    let trees = Trees::new(cas.clone());
    let raw = Cid::<Bdasl>::from(Blake3::of(b"f"));
    let child = |name: &'static str, kind, cid| {
        Value::List(vec![
            Value::Bytes(name.into()),
            Value::Integer(kind),
            Value::Link(cid),
        ])
    };
    let block = |tag: &str, mode, children| {
        Value::List(vec![
            Value::String(tag.into()),
            Value::Integer(mode),
            Value::List(children),
        ])
    };
    let tag = "covalence.fs.dir.v1";
    for value in [
        block("covalence.fs.dir.v2", 0, vec![]),
        block(tag, 2, vec![]),
        block(tag, 0, vec![Value::Null]),
        block(tag, 0, vec![child("a", 2, raw)]),
        block(tag, 0, vec![child("b", 0, raw), child("a", 0, raw)]),
        block(tag, 0, vec![child("a", 0, raw), child("a", 0, raw)]),
        block(tag, 0, vec![child(".", 0, raw)]),
        block(tag, 0, vec![child("a", 1, raw)]),
        block(tag, 1, vec![child("a", 0, raw)]),
    ] {
        Put::<Cid>::put(&*cas, value.encode().unwrap().into()).unwrap();
        assert!(matches!(
            trees.load(value.cid().unwrap()),
            Err(Error::Malformed { .. }
                | Error::DuplicateName { .. }
                | Error::Name { .. }
                | Error::ChildLink { .. })
        ));
    }
    let bytes = Bytes::from_static(b"\x98\x03\x73covalence.fs.dir.v1\x00\x80");
    let cid = Cid::<Bdasl>::of(Codec::Drisl, HashFn::Blake3, &bytes);
    Put::<Cid>::put(&*cas, bytes).unwrap();
    assert!(matches!(trees.load(cid), Err(Error::Dasl { .. })));
}

#[test]
fn failed_writes_and_readback_do_not_allocate_slots() {
    let cas = Arc::new(Probe::default());
    let mut trees = Trees::new(cas.clone());
    let mut control = Trees::new(Arc::new(MemoryCas::default()));
    let a = Directory::new(LeafMode::Bytes, vec![]).unwrap();
    let b = Directory::new(LeafMode::Dasl, vec![]).unwrap();
    let first = trees.insert(&a).unwrap();
    assert_eq!(first, control.insert(&a).unwrap());
    let next = control.insert(&b).unwrap();

    *cas.fault.lock().unwrap() = Fault::WriteError;
    assert!(matches!(trees.insert(&b), Err(Error::Cas { .. })));
    *cas.fault.lock().unwrap() = Fault::WrongKey;
    assert!(matches!(trees.insert(&b), Err(Error::Rekeyed { .. })));
    assert_eq!(cas.counts(), (1, 3));
    *cas.fault.lock().unwrap() = Fault::MissingRead;
    assert!(matches!(
        trees.insert(&b),
        Err(Error::MissingAfterWrite { .. })
    ));
    *cas.fault.lock().unwrap() = Fault::ReadError;
    assert!(matches!(trees.insert(&b), Err(Error::Cas { .. })));
    *cas.fault.lock().unwrap() = Fault::WrongBytes;
    assert!(matches!(
        trees.insert(&b),
        Err(Error::Cas {
            source: CasError::Check { .. }
        })
    ));
    assert!(matches!(
        trees.load(a.cid().unwrap()),
        Err(Error::Cas {
            source: CasError::Check { .. }
        })
    ));

    assert_eq!(trees.root(first), Some(a.cid().unwrap()));
    assert_eq!(trees.root(next), None);
    *cas.fault.lock().unwrap() = Fault::None;
    assert_eq!(trees.load(b.cid().unwrap()).unwrap(), Some(b.clone()));
    assert_eq!(trees.insert(&b).unwrap(), next);
    assert_eq!(trees.root(next), Some(b.cid().unwrap()));
}
