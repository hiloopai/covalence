use std::{
    io::{self, Read, Write},
    sync::Arc,
};

use covalence_cas_kv::KvCas;
use covalence_cas_trait::{AsyncGet, AsyncPut, Cas, Error, Get, Put};
use covalence_kv_memory::MemoryKv;
use covalence_kv_trait::Kv;
use covalence_lib_cid::{Blake3, Cid, Format};
use pollster::block_on;

const HELLO: &str = "blake3/5khrmpntq2bjexseshc6ldklwnig56gbj23yvbxjbdcwestheahq/bytes.zst";

#[test]
fn legacy_compressed_layout_and_decompressed_reads() {
    let kv = Arc::new(MemoryKv::default());
    let legacy = zstd::stream::encode_all(&b"hello"[..], 3).unwrap();
    kv.put(HELLO.as_bytes(), legacy.into()).unwrap();
    let store = KvCas::compressed(kv.clone(), "", 3).unwrap().into_dyn();
    let cid: Cid = Blake3::of(b"hello").into();
    let drisl = Cid::new(Format::DRISL_BLAKE3, cid.digest()).unwrap();
    assert_eq!(store.get(&drisl).unwrap().unwrap(), "hello");
    assert!(store.has(&cid).unwrap());
    assert_eq!(store.size(&cid).unwrap(), Some(5));
    assert_eq!(block_on(store.asize(&cid)).unwrap(), Some(5));
    assert_eq!(store.get_range(&cid, 1, 3).unwrap().unwrap(), "ell");
    assert_eq!(
        block_on(store.aget_range(&cid, 4, 100)).unwrap().unwrap(),
        "o"
    );
    let written = block_on(store.aput("hello".into())).unwrap();
    assert_eq!(written, cid);
    assert_eq!(
        zstd::stream::decode_all(kv.get(HELLO.as_bytes()).unwrap().unwrap().as_ref()).unwrap(),
        b"hello"
    );
    let uncompressed = HELLO.strip_suffix(".zst").unwrap();
    assert!(kv.get(uncompressed.as_bytes()).unwrap().is_none());
    let prefixed = KvCas::compressed(kv.clone(), "a/b/", 3).unwrap();
    let _: Cid = prefixed.put("hello".into()).unwrap();
    assert!(kv.exists(format!("a/b/{HELLO}").as_bytes()).unwrap());
}

#[test]
fn compressed_stream_checks_before_publication() {
    let kv = Arc::new(MemoryKv::default());
    let store = KvCas::compressed(kv.clone(), "", 3).unwrap().into_dyn();
    let data: Vec<u8> = (0..3 << 20).map(|index| (index % 251) as u8).collect();
    let cid = Blake3::of(&data);
    let mut source = data.as_slice();
    let len = data.len() as u64;
    let stored = store.put_from(cid, &mut source, len).unwrap();
    assert_eq!((stored, len), (cid.into(), data.len() as u64));
    assert!(source.is_empty());
    let mut out = Vec::new();
    assert_eq!(store.get_to(&stored, &mut out).unwrap(), Some(len));
    assert_eq!(out, data);
    let hello = Blake3::of(b"hello");
    let wrong = store.put_from(hello, &mut &b"wrong"[..], 5).unwrap_err();
    assert!(matches!(wrong, Error::Check { .. }));
    assert!(!kv.exists(HELLO.as_bytes()).unwrap());
    let short = store.put_from(hello, &mut &b"hell"[..], 5).unwrap_err();
    assert!(matches!(short, Error::Io { .. }));
    assert!(!kv.exists(HELLO.as_bytes()).unwrap());
    let mut source = &b"hello tail"[..];
    assert_eq!(store.put_from(hello, &mut source, 5).unwrap(), hello.into());
    assert_eq!(source, b" tail");
    assert_eq!(
        store.put_from(Blake3::of(b""), &mut &b""[..], 0).unwrap(),
        Blake3::of(b"").into()
    );
}

struct BrokenReader;
impl Read for BrokenReader {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("source failed"))
    }
}
struct BrokenWriter;
impl Write for BrokenWriter {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("destination failed"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct LimitedWriter {
    limit: usize,
    bytes: Vec<u8>,
}

impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.limit.min(bytes.len());
        self.bytes.extend_from_slice(&bytes[..written]);
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn compressed_stream_handles_short_and_zero_destination_writes() {
    let store = KvCas::compressed(MemoryKv::default().into_dyn(), "", 3)
        .unwrap()
        .into_dyn();
    let cid = store.put("hello".into()).unwrap();
    let mut short = LimitedWriter {
        limit: 1,
        bytes: Vec::new(),
    };
    assert_eq!(store.get_to(&cid, &mut short).unwrap(), Some(5));
    assert_eq!(short.bytes, b"hello");
    let mut zero = LimitedWriter {
        limit: 0,
        bytes: Vec::new(),
    };
    assert!(
        matches!(store.get_to(&cid, &mut zero), Err(Error::Io { source }) if source.kind() == io::ErrorKind::WriteZero)
    );
    assert!(zero.bytes.is_empty());
}

#[test]
fn corruption_is_integrity_failure_and_destination_errors_remain_io() {
    let kv = Arc::new(MemoryKv::default());
    let store = KvCas::compressed(kv.clone(), "", 3).unwrap().into_dyn();
    let cid = store.put("hello".into()).unwrap();
    assert!(matches!(
        store.get_to(&cid, &mut BrokenWriter),
        Err(Error::Io { .. })
    ));
    assert!(matches!(
        store.put_from(Blake3::of(b"x"), &mut BrokenReader, 1),
        Err(Error::Io { .. })
    ));
    let mut truncated = zstd::stream::encode_all(&b"hello"[..], 3).unwrap();
    truncated.pop();
    for corrupt in [
        b"not zstd".to_vec(),
        truncated,
        zstd::stream::encode_all(&b"wrong"[..], 3).unwrap(),
    ] {
        kv.put(HELLO.as_bytes(), corrupt.into()).unwrap();
        assert!(matches!(store.get(&cid), Err(Error::Check { .. })));
        assert!(matches!(
            block_on(store.aget(&cid)),
            Err(Error::Check { .. })
        ));
        assert!(matches!(
            store.get_to(&cid, &mut Vec::new()),
            Err(Error::Check { .. })
        ));
    }
}

#[test]
fn uncompressed_stream_uses_existing_layout() {
    let kv = Arc::new(MemoryKv::default());
    let store = KvCas::new(kv.clone()).into_dyn();
    assert_eq!(
        store
            .put_from(Blake3::of(b"hello"), &mut &b"hello"[..], 5)
            .unwrap(),
        Blake3::of(b"hello").into()
    );
    assert_eq!(
        kv.get(HELLO.strip_suffix(".zst").unwrap().as_bytes())
            .unwrap()
            .unwrap(),
        "hello"
    );
}

#[test]
fn flush_rejects_unsupported_modes_before_touching_kv() {
    use covalence_cas_trait::{Flush, Op};
    use covalence_kv_trait::{KvFuture, ready};
    use covalence_lib_data::Bytes;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Count(Arc<AtomicUsize>);
    impl Kv for Count {
        fn get(&self, _: &[u8]) -> Result<Option<Bytes>, covalence_kv_trait::Error> {
            Ok(None)
        }
        fn put(&self, _: &[u8], _: Bytes) -> Result<(), covalence_kv_trait::Error> {
            Ok(())
        }
        fn aget<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
            ready(self.get(key))
        }
        fn aput<'a>(&'a self, key: &'a [u8], bytes: Bytes) -> KvFuture<'a, ()> {
            ready(self.put(key, bytes))
        }
        fn flush(&self) -> Result<(), covalence_kv_trait::Error> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn aflush(&self) -> KvFuture<'_, ()> {
            Box::pin(async move {
                self.0.fetch_add(10, Ordering::Relaxed);
                Ok(())
            })
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let store = KvCas::new(Count(calls.clone()).into_dyn()).into_dyn();
    let policy = |_: &covalence_cas_trait::EvictionCandidate| {
        panic!("unsupported or absent EVICT must not call a policy")
    };
    for mode in [
        Flush::REMOTE,
        Flush::EVICT,
        Flush::PERSIST | Flush::REMOTE,
        Flush::PERSIST | Flush::EVICT,
        Flush::from_bits_retain(128),
    ] {
        assert!(matches!(
            store.flush(mode, Some(&policy)),
            Err(Error::Unsupported { op: Op::Flush })
        ));
        assert!(matches!(
            block_on(store.aflush(mode, Some(&policy))),
            Err(Error::Unsupported { op: Op::Flush })
        ));
    }
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    store.flush(Flush::empty(), Some(&policy)).unwrap();
    block_on(store.aflush(Flush::empty(), Some(&policy))).unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    store.flush(Flush::PERSIST, Some(&policy)).unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    let flush = store.aflush(Flush::PERSIST, Some(&policy));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    block_on(flush).unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 11);
    let unsupported = KvCas::new(MemoryKv::default().into_dyn()).into_dyn();
    assert!(matches!(
        unsupported.flush(Flush::PERSIST, Some(&policy)),
        Err(Error::Unsupported { op: Op::Flush })
    ));
    assert!(matches!(
        block_on(unsupported.aflush(Flush::PERSIST, Some(&policy))),
        Err(Error::Unsupported { op: Op::Flush })
    ));
}
