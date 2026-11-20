use std::{
    collections::VecDeque,
    future::poll_fn,
    io,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
};

use covalence_blob_verify::Error as VerifyError;
use covalence_cas_kv::{Compression, KvCas};
use covalence_cas_trait::{Cas, Error, Op};
use covalence_kv_memory::MemoryKv;
use covalence_kv_trait::{Kv, KvFuture};
use covalence_lib_cid::{Blake3, Cid, Format};
use covalence_lib_dasl::Value;
use covalence_lib_data::{ByteStream, Bytes, Stream};
use pollster::block_on;

/// Assert that `kv` holds `value` under `key` and nothing else.
fn holds_only(kv: &Arc<dyn Kv>, key: &str, value: &[u8]) {
    assert_eq!(kv.get(key.as_bytes()).unwrap().unwrap(), value);
    let expected = MemoryKv::default();
    expected
        .put(key.as_bytes(), Bytes::copy_from_slice(value))
        .unwrap();
    assert_eq!(kv.stats().unwrap(), expected.stats().unwrap());
}

/// A stream of the given chunks and errors.
struct Parts(VecDeque<io::Result<Bytes>>);

impl Stream for Parts {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.get_mut().0.pop_front())
    }
}

fn parts<const N: usize>(parts: [io::Result<&[u8]>; N]) -> ByteStream<'static> {
    Box::pin(Parts(
        parts
            .into_iter()
            .map(|part| part.map(Bytes::copy_from_slice))
            .collect(),
    ))
}

fn store() -> (Arc<dyn Kv>, KvCas) {
    let kv = MemoryKv::default().into_dyn();
    let store = KvCas::with_prefix(kv.clone(), "cas").unwrap();
    (kv, store)
}

/// The key of the block with `cid` under the `cas` prefix.
fn key(cid: &Blake3) -> String {
    let digest = data_encoding::BASE32_NOPAD.encode(cid.digest());
    format!("cas/blake3/{}/bytes", digest.to_ascii_lowercase())
}

/// Every item of `stream`, in order.
fn collect(mut stream: ByteStream<'_>) -> Vec<io::Result<Bytes>> {
    let mut items = Vec::new();
    while let Some(item) = block_on(poll_fn(|cx| stream.as_mut().poll_next(cx))) {
        items.push(item);
    }
    items
}

/// The verification error a stream failed with.
fn reason(item: &io::Result<Bytes>) -> VerifyError {
    *VerifyError::of(item.as_ref().unwrap_err()).unwrap()
}

/// Streamed puts land under the golden key and stream back, also through
/// `Arc<dyn Cas>` and for CIDs with another codec.
#[test]
fn streams_round_trip_under_golden_keys() {
    let (kv, store) = store();
    let store = store.into_dyn();
    let cid =
        block_on(store.aput_from(Blake3::of(b"hello"), parts([Ok(b"he"), Ok(b"llo")]), 5)).unwrap();
    assert_eq!(
        cid.to_string(),
        "bafkr4ihkr4ld3m4gqkjf4reryxsy2s5tkbxprqkow6fin2iiyvreuzzab4"
    );
    let golden = "cas/blake3/5khrmpntq2bjexseshc6ldklwnig56gbj23yvbxjbdcwestheahq/bytes";
    assert_eq!(kv.get(golden.as_bytes()).unwrap().unwrap(), "hello");

    let drisl = Cid::new(Format::DRISL_BLAKE3, cid.digest()).unwrap();
    for cid in [cid, drisl] {
        let (len, stream) = block_on(store.aget_stream(&cid)).unwrap().unwrap();
        assert_eq!(len, 5);
        let bytes: Vec<_> = collect(stream).into_iter().map(Result::unwrap).collect();
        assert_eq!(bytes.concat(), b"hello");
    }

    let missing: Cid = Blake3::of(b"missing").into();
    assert!(block_on(store.aget_stream(&missing)).unwrap().is_none());
    let sha256 = Cid::new(Format::SHA2_256, &[0; 32]).unwrap();
    assert!(block_on(store.aget_stream(&sha256)).unwrap().is_none());

    let empty = Blake3::of(b"");
    assert_eq!(
        block_on(store.aput_from(empty, parts([]), 0)).unwrap(),
        empty.into()
    );
    let (len, stream) = block_on(store.aget_stream(&empty.into())).unwrap().unwrap();
    assert_eq!(len, 0);
    assert!(collect(stream).is_empty());
}

/// A streamed put never replaces a block it finds, but still reads and
/// checks the whole stream: an existing block does not make a put of other
/// bytes, or of too few or too many, succeed. This holds for a store that
/// reads the stream before finding the block and for one that reads none.
#[test]
fn streamed_puts_never_replace_and_always_check() {
    let cid = Blake3::of(b"hello");
    let memory = MemoryKv::default().into_dyn();
    let skipping: Arc<dyn Kv> = Arc::new(Slow::new(Box::new(|_| Ok(()))));
    for kv in [memory, skipping] {
        let store = KvCas::with_prefix(kv.clone(), "cas").unwrap();
        kv.put(key(&cid).as_bytes(), "wrong".into()).unwrap();
        let stored = block_on(store.aput_from(cid, parts([Ok(b"hel"), Ok(b"lo")]), 5));
        assert_eq!(stored.unwrap(), cid.into());
        for stream in [parts([Ok(b"hellO")]), parts([Ok(b"jello")])] {
            let error = block_on(store.aput_from(cid, stream, 5)).unwrap_err();
            assert!(matches!(error, Error::Check { .. }), "{error}");
        }
        for (stream, kind) in [
            (parts([Ok(b"hell")]), io::ErrorKind::UnexpectedEof),
            (parts([Ok(b"hello"), Ok(b"!")]), io::ErrorKind::InvalidData),
            (
                parts([Ok(b"he"), Err(io::Error::other("source aborted"))]),
                io::ErrorKind::Other,
            ),
        ] {
            let error = block_on(store.aput_from(cid, stream, 5)).unwrap_err();
            let Error::Io { source } = &error else {
                panic!("{error}")
            };
            assert_eq!(source.kind(), kind, "{error}");
        }
        holds_only(&kv, &key(&cid), b"wrong");
    }
}

/// Bytes with another CID, a short or long stream, or a failing source store
/// nothing.
#[test]
fn bad_streams_store_nothing() {
    let (kv, store) = store();
    let cid = Blake3::of(b"hello");
    for stream in [
        parts([Ok(b"hellO")]),
        parts([Ok(b"hel"), Ok(b"lO")]),
        parts([Ok(b"jello")]),
    ] {
        let error = block_on(store.aput_from(cid, stream, 5)).unwrap_err();
        assert!(matches!(error, Error::Check { .. }), "{error}");
    }
    for (stream, kind) in [
        (parts([Ok(b"hell")]), io::ErrorKind::UnexpectedEof),
        (parts([Ok(b"hello!")]), io::ErrorKind::InvalidData),
        (parts([Ok(b"hello"), Ok(b"!")]), io::ErrorKind::InvalidData),
        (
            parts([Ok(b"he"), Err(io::Error::other("source aborted"))]),
            io::ErrorKind::Other,
        ),
    ] {
        let error = block_on(store.aput_from(cid, stream, 5)).unwrap_err();
        let Error::Io { source } = &error else {
            panic!("{error}")
        };
        assert_eq!(source.kind(), kind, "{error}");
    }
    // An empty stream is checked even if the store would not read it.
    let error = block_on(store.aput_from(cid, parts([]), 0)).unwrap_err();
    assert!(matches!(error, Error::Check { .. }), "{error}");
    assert_eq!(kv.stats().unwrap(), MemoryKv::default().stats().unwrap());
}

/// A corrupt block streams every chunk but the last, then an error in its
/// place; an intact one streams every chunk.
#[test]
fn corrupt_blocks_are_refused_before_their_last_chunk() {
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let cid = Blake3::of(&data);
    let (kv, store) = store();
    let stored = block_on(store.aput_from(cid, Box::pin(Parts(chunks(&data))), 200_000));
    assert_eq!(stored.unwrap(), cid.into());

    let (len, stream) = block_on(store.aget_stream(&cid.into())).unwrap().unwrap();
    assert_eq!(len, 200_000);
    let items = collect(stream);
    assert_eq!(items.len(), 4);
    let bytes: Vec<_> = items.into_iter().map(Result::unwrap).collect();
    assert_eq!(bytes.concat(), data);

    // The first byte differs, so the check fails only once all have arrived.
    let mut corrupt = data.clone();
    corrupt[0] ^= 1;
    kv.put(key(&cid).as_bytes(), corrupt.into()).unwrap();
    let (_, stream) = block_on(store.aget_stream(&cid.into())).unwrap().unwrap();
    let items = collect(stream);
    assert_eq!(items.len(), 4);
    for item in &items[..3] {
        assert_eq!(item.as_ref().unwrap().len(), 65_536);
    }
    assert_eq!(
        items[3].as_ref().unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert!(matches!(reason(&items[3]), VerifyError::Mismatch { .. }));

    // A truncated block fails the same way: its length is the store's, but
    // its digest is not the CID's.
    kv.put(key(&cid).as_bytes(), data[..199_999].to_vec().into())
        .unwrap();
    let (len, stream) = block_on(store.aget_stream(&cid.into())).unwrap().unwrap();
    assert_eq!(len, 199_999);
    let items = collect(stream);
    assert!(matches!(
        reason(items.last().unwrap()),
        VerifyError::Mismatch { .. }
    ));
    assert!(items[..items.len() - 1].iter().all(Result::is_ok));
}

/// `data` in chunks of 64 KiB.
fn chunks(data: &[u8]) -> VecDeque<io::Result<Bytes>> {
    data.chunks(65_536)
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
        .collect()
}

/// A key-value store that panics if anything touches it.
struct Untouched;

impl Kv for Untouched {
    fn get(&self, _: &[u8]) -> Result<Option<Bytes>, covalence_kv_trait::Error> {
        panic!("the store was read")
    }
    fn put(&self, _: &[u8], _: Bytes) -> Result<(), covalence_kv_trait::Error> {
        panic!("the store was written")
    }
    fn aget<'a>(&'a self, _: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
        panic!("the store was read")
    }
    fn aput<'a>(&'a self, _: &'a [u8], _: Bytes) -> KvFuture<'a, ()> {
        panic!("the store was written")
    }
    fn aput_new_from<'a>(
        &'a self,
        _: &'a [u8],
        _: ByteStream<'static>,
        _: u64,
    ) -> KvFuture<'a, bool> {
        panic!("the store was written")
    }
    fn aget_stream<'a>(&'a self, _: &'a [u8]) -> KvFuture<'a, Option<(u64, ByteStream<'static>)>> {
        panic!("the store was read")
    }
}

/// A stream that panics if it is polled.
struct Unpolled;

impl Stream for Unpolled {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        panic!("the stream was read")
    }
}

/// Compressed stores, of every codec, do not stream asynchronously: both
/// methods fail before touching the store or the stream.
#[test]
fn compressed_stores_refuse_streams_without_touching_store_or_stream() {
    let cid = Blake3::of(b"hello");
    for compression in [
        Compression::zstd(0),
        Compression::deflate(6),
        Compression::gzip(6),
        Compression::br(4),
    ] {
        let store = KvCas::new(Untouched.into_dyn()).with_compression(compression);
        let error = block_on(store.aput_from(cid, Box::pin(Unpolled), 5)).unwrap_err();
        assert!(
            matches!(error, Error::Unsupported { op: Op::Put }),
            "{error}"
        );
        let error = block_on(store.aget_stream(&cid.into())).err().unwrap();
        assert!(
            matches!(error, Error::Unsupported { op: Op::Get }),
            "{error}"
        );
    }
}

/// A key-value store with only the required methods, like one without
/// atomic or streamed writes.
struct Plain(MemoryKv);

impl Kv for Plain {
    fn get(&self, key: &[u8]) -> Result<Option<Bytes>, covalence_kv_trait::Error> {
        self.0.get(key)
    }
    fn put(&self, key: &[u8], value: Bytes) -> Result<(), covalence_kv_trait::Error> {
        self.0.put(key, value)
    }
    fn aget<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
        self.0.aget(key)
    }
    fn aput<'a>(&'a self, key: &'a [u8], value: Bytes) -> KvFuture<'a, ()> {
        self.0.aput(key, value)
    }
}

/// Operations the key-value store does not support are unsupported for the
/// CAS too, not backend failures, so callers can fall back; streams are not
/// read.
#[test]
fn unsupported_key_value_operations_stay_unsupported() {
    let store = KvCas::new(Plain(MemoryKv::default()).into_dyn()).into_dyn();
    let cid = Blake3::of(b"hello");
    let unsupported_put = |result: Result<Cid, Error>| {
        let error = result.unwrap_err();
        assert!(
            matches!(error, Error::Unsupported { op: Op::Put }),
            "{error}"
        );
    };
    unsupported_put(store.put("hello".into()));
    unsupported_put(block_on(store.aput("hello".into())));
    unsupported_put(store.put_from(cid, &mut &b"hello"[..], 5));
    unsupported_put(block_on(store.aput_from(cid, Box::pin(Unpolled), 5)));
    let error = block_on(store.aget_stream(&cid.into())).err().unwrap();
    assert!(
        matches!(error, Error::Unsupported { op: Op::Get }),
        "{error}"
    );
}

/// A store that serves one value as the given length and stream.
struct Serving(Mutex<Option<(u64, ByteStream<'static>)>>);

/// A store serving `stream` as a value of `len` bytes.
fn serving(len: u64, stream: ByteStream<'static>) -> Arc<dyn Kv> {
    Arc::new(Serving(Mutex::new(Some((len, stream)))))
}

impl Kv for Serving {
    fn get(&self, _: &[u8]) -> Result<Option<Bytes>, covalence_kv_trait::Error> {
        unimplemented!()
    }
    fn put(&self, _: &[u8], _: Bytes) -> Result<(), covalence_kv_trait::Error> {
        unimplemented!()
    }
    fn aget<'a>(&'a self, _: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
        unimplemented!()
    }
    fn aput<'a>(&'a self, _: &'a [u8], _: Bytes) -> KvFuture<'a, ()> {
        unimplemented!()
    }
    fn aget_stream<'a>(&'a self, _: &'a [u8]) -> KvFuture<'a, Option<(u64, ByteStream<'static>)>> {
        covalence_kv_trait::ready(Ok(self.0.lock().unwrap().take()))
    }
}

/// A read stream that ends early fails with `UnexpectedEof`, one longer
/// than the store's length with `InvalidData`, each in place of the last
/// chunk; a failing store stream passes its own error on and ends.
#[test]
fn bad_store_streams_fail_in_place_of_the_last_chunk() {
    let cid: Cid = Blake3::of(b"hello").into();
    for (len, stream, kind) in [
        (
            5,
            parts([Ok(b"he"), Ok(b"ll")]),
            io::ErrorKind::UnexpectedEof,
        ),
        (
            5,
            parts([Ok(b"hello"), Ok(b"!")]),
            io::ErrorKind::InvalidData,
        ),
        (5, parts([Ok(b"hello!")]), io::ErrorKind::InvalidData),
    ] {
        let store = KvCas::new(serving(len, stream));
        let (_, stream) = block_on(store.aget_stream(&cid)).unwrap().unwrap();
        let items = collect(stream);
        let (last, before) = items.split_last().unwrap();
        assert!(before.iter().all(Result::is_ok));
        assert_eq!(last.as_ref().unwrap_err().kind(), kind);
    }

    let failing = parts([Ok(b"he"), Err(io::Error::other("store aborted"))]);
    let store = KvCas::new(serving(5, failing));
    let (_, stream) = block_on(store.aget_stream(&cid)).unwrap().unwrap();
    let items = collect(stream);
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].as_ref().unwrap(), "he");
    let error = items[1].as_ref().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert_eq!(error.to_string(), "store aborted");
}

/// A store that reads the whole stream, then fails as a store whose cleanup
/// failed would.
struct Spoiled;

impl Kv for Spoiled {
    fn get(&self, _: &[u8]) -> Result<Option<Bytes>, covalence_kv_trait::Error> {
        Ok(None)
    }
    fn put(&self, _: &[u8], _: Bytes) -> Result<(), covalence_kv_trait::Error> {
        unimplemented!()
    }
    fn aget<'a>(&'a self, _: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
        unimplemented!()
    }
    fn aput<'a>(&'a self, _: &'a [u8], _: Bytes) -> KvFuture<'a, ()> {
        unimplemented!()
    }
    fn aput_new_from<'a>(
        &'a self,
        _: &'a [u8],
        mut stream: ByteStream<'static>,
        _: u64,
    ) -> KvFuture<'a, bool> {
        Box::pin(async move {
            while poll_fn(|cx| stream.as_mut().poll_next(cx)).await.is_some() {}
            Err(covalence_kv_trait::Error::Backend {
                source: "cleanup failed".into(),
            })
        })
    }
}

/// A store failure beyond the failed check, such as a failed cleanup, is
/// what a mismatched write reports.
#[test]
fn store_failures_outrank_mismatches() {
    let store = KvCas::new(Arc::new(Spoiled));
    let error = block_on(store.aput_from(Blake3::of(b"hello"), parts([Ok(b"hellO")]), 5));
    let error = error.unwrap_err();
    assert!(
        matches!(&error, Error::Backend { source } if source.to_string() == "backend: cleanup failed"),
        "{error}"
    );
}

/// A source error reaches the caller with its kind and message, and as its
/// source, whether the store reported it or found the block and stopped
/// reading.
#[test]
fn source_errors_keep_their_kind_message_and_source() {
    let cid = Blake3::of(b"hello");
    let memory = MemoryKv::default().into_dyn();
    let skipping: Arc<dyn Kv> = Arc::new(Slow::new(Box::new(|_| Ok(()))));
    skipping.put(key(&cid).as_bytes(), "hello".into()).unwrap();
    for kv in [memory, skipping] {
        let store = KvCas::with_prefix(kv, "cas").unwrap();
        let cause = io::Error::new(io::ErrorKind::ConnectionReset, "peer reset");
        let failing = io::Error::new(io::ErrorKind::TimedOut, Caused(cause));
        let error = block_on(store.aput_from(cid, parts([Ok(b"he"), Err(failing)]), 5));
        let error = error.unwrap_err();
        let Error::Io { source } = &error else {
            panic!("{error}")
        };
        assert_eq!(source.kind(), io::ErrorKind::TimedOut);
        assert_eq!(source.to_string(), "timed out");
        let original = std::error::Error::source(source).unwrap();
        let original = original.downcast_ref::<io::Error>().unwrap();
        assert_eq!(original.kind(), io::ErrorKind::TimedOut);
        assert!(original.get_ref().unwrap().is::<Caused>());
        let cause = std::error::Error::source(original).unwrap();
        assert_eq!(cause.to_string(), "peer reset");
    }
}

/// An error with a message and a cause.
#[derive(Debug)]
struct Caused(io::Error);

impl std::fmt::Display for Caused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("timed out")
    }
}

impl std::error::Error for Caused {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// A store that commits as soon as it has `len` bytes, without waiting for
/// the stream to end, as an upload of a known length may.
struct Eager(MemoryKv);

impl Kv for Eager {
    fn get(&self, key: &[u8]) -> Result<Option<Bytes>, covalence_kv_trait::Error> {
        self.0.get(key)
    }
    fn put(&self, key: &[u8], value: Bytes) -> Result<(), covalence_kv_trait::Error> {
        self.0.put(key, value)
    }
    fn aget<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
        self.0.aget(key)
    }
    fn aput<'a>(&'a self, key: &'a [u8], value: Bytes) -> KvFuture<'a, ()> {
        self.0.aput(key, value)
    }
    fn aput_new_from<'a>(
        &'a self,
        key: &'a [u8],
        mut stream: ByteStream<'static>,
        len: u64,
    ) -> KvFuture<'a, bool> {
        Box::pin(async move {
            let mut value = Vec::new();
            while (value.len() as u64) < len {
                match poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
                    Some(Ok(chunk)) => value.extend_from_slice(&chunk),
                    Some(Err(source)) => return Err(covalence_kv_trait::Error::Io { source }),
                    None => break,
                }
            }
            if value.len() as u64 != len {
                return Err(covalence_kv_trait::Error::Io {
                    source: io::ErrorKind::UnexpectedEof.into(),
                });
            }
            self.0.put_new(key, value.into())
        })
    }
}

/// The last chunk is withheld until the bytes match and the stream ends, so
/// a store that commits on the last byte stores nothing bad.
#[test]
fn stores_that_commit_at_the_length_store_nothing_bad() {
    let kv = Arc::new(Eager(MemoryKv::default()));
    let store = KvCas::new(kv.clone());
    let cid = Blake3::of(b"hello");
    let error = block_on(store.aput_from(cid, parts([Ok(b"hel"), Ok(b"lO")]), 5)).unwrap_err();
    assert!(matches!(error, Error::Check { .. }), "{error}");
    let error = block_on(store.aput_from(cid, parts([Ok(b"hello"), Ok(b"!")]), 5)).unwrap_err();
    assert!(matches!(error, Error::Io { .. }), "{error}");
    assert!(!store.has(&cid.into()).unwrap());
    block_on(store.aput_from(cid, parts([Ok(b"hel"), Ok(b"lo")]), 5)).unwrap();
    assert!(store.has(&cid.into()).unwrap());
}

/// An empty block's stream is read to its end and checked before the store
/// sees it, so a store that commits at the length without reading stores
/// nothing bad.
#[test]
fn empty_blocks_are_checked_before_the_store_sees_them() {
    let kv = Arc::new(Eager(MemoryKv::default()));
    let store = KvCas::new(kv.clone());
    let empty = Blake3::of(b"");
    for (stream, kind) in [
        (parts([Ok(b"x")]), io::ErrorKind::InvalidData),
        (
            parts([Err(io::Error::other("source aborted"))]),
            io::ErrorKind::Other,
        ),
    ] {
        let error = block_on(store.aput_from(empty, stream, 0)).unwrap_err();
        let Error::Io { source } = &error else {
            panic!("{error}")
        };
        assert_eq!(source.kind(), kind, "{error}");
    }
    let error = block_on(store.aput_from(Blake3::of(b"hello"), parts([]), 0)).unwrap_err();
    assert!(matches!(error, Error::Check { .. }), "{error}");
    assert_eq!(kv.0.stats().unwrap(), MemoryKv::default().stats().unwrap());
    let stored = block_on(store.aput_from(empty, parts([Ok(b"")]), 0));
    assert_eq!(stored.unwrap(), empty.into());
    assert!(store.has(&empty.into()).unwrap());
}

/// The CIDs and keys of blocks, as the Python and JavaScript bindings give
/// them: bytes, CID and the key's digest part.
const GOLDEN: [(&str, &str, &str); 3] = [
    (
        "hello",
        "bafkr4ihkr4ld3m4gqkjf4reryxsy2s5tkbxprqkow6fin2iiyvreuzzab4",
        "5khrmpntq2bjexseshc6ldklwnig56gbj23yvbxjbdcwestheahq",
    ),
    (
        "",
        "bafkr4ifpcne3t5pzugtkaqcn5i3nzskjtpfslsnnyejlpte2spfoihzsmi",
        "v4jutopv7gq2nicajxvdnxgjjgn4wjojvxarfn6mtkj4vza7gjra",
    ),
    (
        "0..=255, five times",
        "bafkr4idvq35ccjdoryl3hbfy7z4jjdmrncaoltskqdidb5cvasksu2paka",
        "owdpuijen2hbpm4exd7hrfensfuibzoojkanamhukucjkktj4bia",
    ),
];

/// Streamed puts store under the golden CIDs and keys, whatever the chunking.
#[test]
fn streamed_puts_match_golden_vectors() {
    let repeated: Vec<u8> = (0..=255u8).cycle().take(1280).collect();
    for ((name, golden, digest), bytes) in GOLDEN.into_iter().zip([&b"hello"[..], b"", &repeated]) {
        for size in [1, 7, 4096] {
            let (kv, store) = store();
            let chunks = bytes
                .chunks(size)
                .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
                .collect();
            let len = bytes.len() as u64;
            let cid = block_on(store.aput_from(Blake3::of(bytes), Box::pin(Parts(chunks)), len));
            assert_eq!(cid.unwrap().to_string(), golden, "{name}");
            let key = format!("cas/blake3/{digest}/bytes");
            holds_only(&kv, &key, bytes);
        }
    }
}

/// A source that counts the chunks it yields and notes when it is dropped.
struct Counted {
    chunks: VecDeque<Bytes>,
    yielded: Arc<AtomicUsize>,
    dropped: Arc<AtomicBool>,
}

impl Counted {
    fn new(data: &[u8], yielded: &Arc<AtomicUsize>, dropped: &Arc<AtomicBool>) -> Self {
        Self {
            chunks: data.chunks(65_536).map(Bytes::copy_from_slice).collect(),
            yielded: yielded.clone(),
            dropped: dropped.clone(),
        }
    }
}

impl Stream for Counted {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let chunk = this.chunks.pop_front();
        if chunk.is_some() {
            this.yielded.fetch_add(1, Ordering::SeqCst);
        }
        Poll::Ready(chunk.map(Ok))
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

/// Pending once, waking itself: a store that is slow between chunks.
async fn yield_now() {
    let mut yielded = false;
    poll_fn(|cx| {
        if yielded {
            return Poll::Ready(());
        }
        yielded = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    })
    .await;
}

/// What a [`Slow`] store calls after reading the given number of chunks;
/// an error ends the write there.
type Hook = Box<dyn Fn(usize) -> Result<(), covalence_kv_trait::Error> + Send + Sync>;

/// A memory store that finds an existing value without reading the stream,
/// reads a new one one chunk at a time, yielding between chunks and calling
/// a hook after each, and streams values from a [`Counted`] source.
struct Slow {
    kv: MemoryKv,
    after_chunk: Hook,
    yielded: Arc<AtomicUsize>,
    dropped: Arc<AtomicBool>,
}

impl Slow {
    fn new(after_chunk: Hook) -> Self {
        Self {
            kv: MemoryKv::default(),
            after_chunk,
            yielded: Arc::default(),
            dropped: Arc::default(),
        }
    }
}

impl Kv for Slow {
    fn get(&self, key: &[u8]) -> Result<Option<Bytes>, covalence_kv_trait::Error> {
        self.kv.get(key)
    }
    fn stats(&self) -> Result<Value, covalence_kv_trait::Error> {
        self.kv.stats()
    }
    fn put(&self, key: &[u8], value: Bytes) -> Result<(), covalence_kv_trait::Error> {
        self.kv.put(key, value)
    }
    fn aget<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
        self.kv.aget(key)
    }
    fn aput<'a>(&'a self, key: &'a [u8], value: Bytes) -> KvFuture<'a, ()> {
        self.kv.aput(key, value)
    }
    fn aput_new_from<'a>(
        &'a self,
        key: &'a [u8],
        mut stream: ByteStream<'static>,
        len: u64,
    ) -> KvFuture<'a, bool> {
        Box::pin(async move {
            // Like a conditional upload, find an existing value without
            // reading the stream.
            if self.kv.exists(key)? {
                return Ok(false);
            }
            let mut value = Vec::new();
            let mut chunks = 0;
            while let Some(chunk) = poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
                let chunk = chunk.map_err(|source| covalence_kv_trait::Error::Io { source })?;
                value.extend_from_slice(&chunk);
                chunks += 1;
                (self.after_chunk)(chunks)?;
                yield_now().await;
            }
            if value.len() as u64 != len {
                return Err(covalence_kv_trait::Error::Io {
                    source: io::ErrorKind::UnexpectedEof.into(),
                });
            }
            self.kv.put_new(key, value.into())
        })
    }
    fn aget_stream<'a>(
        &'a self,
        key: &'a [u8],
    ) -> KvFuture<'a, Option<(u64, ByteStream<'static>)>> {
        Box::pin(async move {
            Ok(self.kv.get(key)?.map(|value| {
                let stream: ByteStream<'static> =
                    Box::pin(Counted::new(&value, &self.yielded, &self.dropped));
                (value.len() as u64, stream)
            }))
        })
    }
}

/// The most chunks a slow store's source got ahead of the store, which the
/// hook records.
fn ahead_of(yielded: &Arc<AtomicUsize>) -> (Hook, Arc<AtomicUsize>) {
    let most = Arc::new(AtomicUsize::new(0));
    let (yielded, most_ahead) = (yielded.clone(), most.clone());
    let hook: Hook = Box::new(move |read| {
        let ahead = yielded.load(Ordering::SeqCst) - read;
        most_ahead.fetch_max(ahead, Ordering::SeqCst);
        Ok(())
    });
    (hook, most)
}

/// A slow store pulls the source: the core reads at most one chunk ahead of
/// the store, writing and reading, and dropping a read stream drops the
/// store's.
#[test]
fn slow_stores_pull_the_stream() {
    let data: Vec<u8> = (0..40 * 65_536u32).map(|i| (i % 249) as u8).collect();
    let cid = Blake3::of(&data);
    let source_yielded = Arc::new(AtomicUsize::new(0));
    let (hook, most_ahead) = ahead_of(&source_yielded);
    let kv = Arc::new(Slow::new(hook));
    let store = KvCas::new(kv.clone());

    let source = Counted::new(&data, &source_yielded, &Arc::default());
    let stored = block_on(store.aput_from(cid, Box::pin(source), data.len() as u64));
    assert_eq!(stored.unwrap(), cid.into());
    assert_eq!(source_yielded.load(Ordering::SeqCst), 40);
    assert!(most_ahead.load(Ordering::SeqCst) <= 1, "{most_ahead:?}");

    let (_, mut stream) = block_on(store.aget_stream(&cid.into())).unwrap().unwrap();
    for read in 1..=10 {
        block_on(poll_fn(|cx| stream.as_mut().poll_next(cx)))
            .unwrap()
            .unwrap();
        let yielded = kv.yielded.load(Ordering::SeqCst);
        assert!(
            yielded <= read + 1,
            "{yielded} chunks yielded for {read} read"
        );
    }
    assert!(!kv.dropped.load(Ordering::SeqCst));
    drop(stream);
    assert!(kv.dropped.load(Ordering::SeqCst));
}

/// A store that gives up partway stores nothing, reads no further and drops
/// the source; so does a write whose future is dropped partway.
#[test]
fn abandoned_writes_store_nothing() {
    let data: Vec<u8> = (0..10 * 65_536u32).map(|i| (i % 247) as u8).collect();
    let cid = Blake3::of(&data);
    let slow = KvCas::new(Arc::new(Slow::new(Box::new(|read| match read {
        3 => Err(covalence_kv_trait::Error::Backend {
            source: "store gave up".into(),
        }),
        _ => Ok(()),
    }))));
    let (yielded, dropped) = (Arc::default(), Arc::default());
    let source = Counted::new(&data, &yielded, &dropped);
    let error = block_on(slow.aput_from(cid, Box::pin(source), data.len() as u64)).unwrap_err();
    assert!(matches!(error, Error::Backend { .. }), "{error}");
    assert!(yielded.load(Ordering::SeqCst) <= 4);
    assert!(dropped.load(Ordering::SeqCst));
    assert!(!slow.has(&cid.into()).unwrap());

    let (kv, store) = store();
    let feed = Feed::default();
    feed.send(&data[..65_536]);
    let mut put = store.aput_from(cid, feed.stream(), data.len() as u64);
    assert!(poll(put.as_mut()).is_pending());
    drop(put);
    assert!(feed.dropped());
    assert_eq!(kv.stats().unwrap(), MemoryKv::default().stats().unwrap());
}

/// A source the test feeds chunk by chunk; it is pending while it has none.
#[derive(Clone, Default)]
struct Feed(Arc<Mutex<(VecDeque<Bytes>, bool)>>);

/// A [`Feed`]'s stream.
struct Fed(Feed);

impl Feed {
    fn send(&self, chunk: &[u8]) {
        self.0
            .lock()
            .unwrap()
            .0
            .push_back(Bytes::copy_from_slice(chunk));
    }
    fn close(&self) {
        self.0.lock().unwrap().1 = true;
    }
    fn stream(&self) -> ByteStream<'static> {
        Box::pin(Fed(self.clone()))
    }
    fn dropped(&self) -> bool {
        Arc::strong_count(&self.0) == 1
    }
}

impl Stream for Fed {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut state = self.0.0.lock().unwrap();
        match state.0.pop_front() {
            Some(chunk) => Poll::Ready(Some(Ok(chunk))),
            None if state.1 => Poll::Ready(None),
            None => Poll::Pending,
        }
    }
}

/// One poll of `future`, which must not need waking.
fn poll<T>(future: Pin<&mut (impl Future<Output = T> + ?Sized)>) -> Poll<T> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

/// After a failed write, a retry stores the block once; writers racing on
/// one block each report the outcome of their own bytes, and the block is
/// stored once.
#[test]
fn retries_and_racing_writers_store_once() {
    let (kv, retried) = store();
    let cid = Blake3::of(b"hello");
    for stream in [parts([Ok(b"hel")]), parts([Ok(b"hellO")])] {
        block_on(retried.aput_from(cid, stream, 5)).unwrap_err();
    }
    let stored = block_on(retried.aput_from(cid, parts([Ok(b"hel"), Ok(b"lo")]), 5));
    assert_eq!(stored.unwrap(), cid.into());
    holds_only(&kv, &key(&cid), b"hello");

    for bad in [false, true] {
        let (kv, store) = store();
        let feeds = [Feed::default(), Feed::default(), Feed::default()];
        let mut puts: Vec<_> = feeds
            .iter()
            .map(|feed| store.aput_from(cid, feed.stream(), 5))
            .collect();
        // Every writer has read part of its body before any finishes.
        for (feed, put) in feeds.iter().zip(&mut puts) {
            feed.send(b"hel");
            assert!(poll(put.as_mut()).is_pending());
        }
        for (index, feed) in feeds.iter().enumerate() {
            feed.send(if bad && index == 1 { b"lO" } else { b"lo" });
            feed.close();
        }
        let results: Vec<_> = puts.iter_mut().map(|put| poll(put.as_mut())).collect();
        for (index, result) in results.into_iter().enumerate() {
            let Poll::Ready(result) = result else {
                panic!("writer {index} is still pending")
            };
            if bad && index == 1 {
                assert!(matches!(result, Err(Error::Check { .. })), "{result:?}");
            } else {
                assert_eq!(result.unwrap(), cid.into());
            }
        }
        holds_only(&kv, &key(&cid), b"hello");
    }
}
