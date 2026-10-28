use std::io::{self, Read};

use covalence_cas_kv::{Compression, KvCas};
use covalence_cas_trait::{Cas, Error, Op};
use covalence_kv_memory::MemoryKv;
use covalence_kv_trait::{Error as KvError, Kv, KvFuture};
use covalence_lib_cid::{Blake3, Cid};
use covalence_lib_dasl::Value;
use covalence_lib_data::Bytes;
use pollster::block_on;

/// Disabled codecs must fail before touching the backend or consuming input.
struct Untouched;
impl Kv for Untouched {
    fn get(&self, _: &[u8]) -> Result<Option<Bytes>, KvError> {
        panic!("disabled codec accessed the backend")
    }
    fn put(&self, _: &[u8], _: Bytes) -> Result<(), KvError> {
        panic!("disabled codec accessed the backend")
    }
    fn aget<'a>(&'a self, _: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
        panic!("disabled codec accessed the backend")
    }
    fn aput<'a>(&'a self, _: &'a [u8], _: Bytes) -> KvFuture<'a, ()> {
        panic!("disabled codec accessed the backend")
    }
    fn put_new(&self, _: &[u8], _: Bytes) -> Result<bool, KvError> {
        panic!("disabled codec accessed the backend")
    }
    fn aput_new<'a>(&'a self, _: &'a [u8], _: Bytes) -> KvFuture<'a, bool> {
        panic!("disabled codec accessed the backend")
    }
    fn info(&self) -> Result<Value, KvError> {
        panic!("disabled codec accessed the backend")
    }
    fn stats(&self) -> Result<Value, KvError> {
        panic!("disabled codec accessed the backend")
    }
}
impl Read for Untouched {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        panic!("disabled codec consumed input")
    }
}

fn unsupported_get<T>(result: Result<T, Error>) {
    assert!(matches!(result, Err(Error::Unsupported { op: Op::Get })));
}
fn unsupported_put<T>(result: Result<T, Error>) {
    assert!(matches!(result, Err(Error::Unsupported { op: Op::Put })));
}

#[test]
fn selected_codecs_follow_their_features_and_disabled_codecs_are_explicit() {
    for (compression, enabled) in [
        (Compression::zstd(3), cfg!(feature = "zstd")),
        (Compression::deflate(3), cfg!(feature = "deflate")),
        (Compression::gzip(3), cfg!(feature = "deflate")),
        (Compression::br(3), cfg!(feature = "brotli")),
    ] {
        if enabled {
            let store = KvCas::new(MemoryKv::default().into_dyn())
                .with_compression(compression)
                .into_dyn();
            let cid = store.put("hello".into()).unwrap();
            assert_eq!(store.get(&cid).unwrap().unwrap(), "hello");
            assert_eq!(block_on(store.aget(&cid)).unwrap().unwrap(), "hello");
            continue;
        }
        let store = KvCas::new(Untouched.into_dyn())
            .with_compression(compression)
            .into_dyn();
        let cid: Cid = Blake3::of(b"hello").into();
        unsupported_get(store.get(&cid));
        unsupported_get(store.get_unchecked(&cid));
        unsupported_get(store.size(&cid));
        unsupported_get(store.size_unchecked(&cid));
        unsupported_get(store.get_range(&cid, 0, 1));
        unsupported_get(store.get_range_unchecked(&cid, 0, 1));
        unsupported_get(block_on(store.aget(&cid)));
        unsupported_get(block_on(store.aget_unchecked(&cid)));
        unsupported_get(block_on(store.asize(&cid)));
        unsupported_get(block_on(store.asize_unchecked(&cid)));
        unsupported_get(block_on(store.aget_range(&cid, 0, 1)));
        unsupported_get(block_on(store.aget_range_unchecked(&cid, 0, 1)));
        unsupported_get(store.get_to(&cid, &mut io::sink()));
        unsupported_get(store.has(&cid));
        unsupported_get(store.missing(&[cid]));
        unsupported_get(store.info());
        unsupported_get(store.stats());
        unsupported_put(store.put("hello".into()));
        unsupported_put(block_on(store.aput("hello".into())));
        unsupported_put(store.put_from(Blake3::of(b"hello"), &mut Untouched, 5));
    }
}

#[test]
fn identity_works_with_every_feature_selection() {
    let kv = MemoryKv::default().into_dyn();
    let store = KvCas::new(kv.clone()).into_dyn();
    let cid = store.put("hello".into()).unwrap();
    assert_eq!(store.get(&cid).unwrap().unwrap(), "hello");
    assert_eq!(store.size(&cid).unwrap(), Some(5));
    assert_eq!(store.get_range(&cid, 1, 3).unwrap().unwrap(), "ell");
    assert_eq!(block_on(store.aget(&cid)).unwrap().unwrap(), "hello");
    let key = b"blake3/5khrmpntq2bjexseshc6ldklwnig56gbj23yvbxjbdcwestheahq/bytes";
    assert_eq!(kv.get(key).unwrap().unwrap(), "hello");
}
