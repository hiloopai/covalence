use std::{
    net::TcpListener,
    path::Path,
    sync::Arc,
    thread::{self, JoinHandle},
};

use covalence_cas_kv::KvCas;
use covalence_cas_trait::{AsyncGet, AsyncGetExt, AsyncPut, Cas, Error, Get, GetExt, Put};
use covalence_kv_memory::MemoryKv;
use covalence_kv_s3::{Credentials, S3Kv};
use covalence_kv_trait::{Kv, KvFuture, Op, ready};
use covalence_lib_cid::{Blake3, Cid, Format};
use covalence_lib_data::Bytes;
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder,
};
use pollster::block_on;
use s3s::{auth::SimpleAuth, service::S3ServiceBuilder};
use s3s_fs::FileSystem;
use tokio::sync::oneshot;

/// The key of "hello": its BLAKE3 digest in lowercase unpadded base32.
const HELLO: &str = "blake3/5khrmpntq2bjexseshc6ldklwnig56gbj23yvbxjbdcwestheahq/bytes";

/// Store "hello" and check reads, then `corrupt` it and check that checked
/// reads notice, and that `put` leaves the block it finds: it never replaces.
fn check(store: &KvCas, corrupt: impl FnOnce()) {
    let cid: Cid = store.put("hello".into()).unwrap();
    assert_eq!(cid, Blake3::of(b"hello").into());
    assert_eq!(store.get(&cid).unwrap().unwrap(), "hello");
    let drisl: Cid = Cid::new(Format::DRISL_BLAKE3, cid.digest()).unwrap();
    assert_eq!(store.get_fact(&drisl).unwrap().unwrap().bytes(), "hello");

    let world = block_on(store.aput("world".into())).unwrap();
    assert_eq!(block_on(store.aget(&world)).unwrap().unwrap(), "world");
    assert_eq!(
        block_on(store.aget_fact(&world)).unwrap().unwrap().bytes(),
        "world"
    );

    let missing: Cid = Blake3::of(b"missing").into();
    assert_eq!(store.get(&missing).unwrap(), None);
    let sha256: Cid = Cid::new(Format::SHA2_256, &[0; 32]).unwrap();
    assert_eq!(store.get(&sha256).unwrap(), None);

    assert!(store.has(&cid).unwrap() && store.has(&drisl).unwrap());
    assert!(!store.has(&missing).unwrap() && !store.has(&sha256).unwrap());
    let keys = [missing, cid, sha256, world];
    assert_eq!(store.missing(&keys).unwrap(), [missing, sha256]);

    corrupt();
    assert_eq!(store.get_unchecked(&cid).unwrap().unwrap(), "wrong");
    assert!(matches!(store.get(&cid), Err(Error::Check { .. })));
    assert!(matches!(
        block_on(store.aget(&cid)),
        Err(Error::Check { .. })
    ));
    assert!(matches!(store.get_fact(&cid), Err(Error::Check { .. })));
    let _: Cid = store.put("hello".into()).unwrap();
    assert_eq!(store.get_unchecked(&cid).unwrap().unwrap(), "wrong");
    let _ = block_on(store.aput("hello".into())).unwrap();
    assert_eq!(store.get_unchecked(&cid).unwrap().unwrap(), "wrong");
    let put_from = store.put_from(Blake3::of(b"hello"), &mut &b"hello"[..], 5);
    assert_eq!(put_from.unwrap(), cid);
    assert_eq!(store.get_unchecked(&cid).unwrap().unwrap(), "wrong");
}

#[test]
fn memory() {
    let kv = Arc::new(MemoryKv::default());
    check(&KvCas::new(kv.clone()), || {
        assert_eq!(kv.get(HELLO.as_bytes()).unwrap().unwrap(), "hello");
        kv.put(HELLO.as_bytes(), "wrong".into()).unwrap();
    });
}

/// A key-value store that cannot read values, only whether they exist.
struct Blind(MemoryKv, usize);

impl Kv for Blind {
    fn get(&self, _: &[u8]) -> Result<Option<Bytes>, covalence_kv_trait::Error> {
        Err(Op::Get.unsupported())
    }
    fn put(&self, key: &[u8], value: Bytes) -> Result<(), covalence_kv_trait::Error> {
        self.0.put(key, value)
    }
    fn aget<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
        ready(self.get(key))
    }
    fn aput<'a>(&'a self, key: &'a [u8], value: Bytes) -> KvFuture<'a, ()> {
        ready(self.put(key, value))
    }
    fn exists(&self, key: &[u8]) -> Result<bool, covalence_kv_trait::Error> {
        self.0.exists(key)
    }
    fn put_new(&self, key: &[u8], value: Bytes) -> Result<bool, covalence_kv_trait::Error> {
        self.0.put_new(key, value)
    }
    fn aput_new<'a>(&'a self, key: &'a [u8], value: Bytes) -> KvFuture<'a, bool> {
        self.0.aput_new(key, value)
    }
    fn put_new_from(
        &self,
        key: &[u8],
        reader: &mut dyn std::io::Read,
        len: u64,
    ) -> Result<bool, covalence_kv_trait::Error> {
        if self.0.exists(key)? {
            reader
                .read_exact(&mut vec![0; self.1])
                .map_err(|source| covalence_kv_trait::Error::Io { source })?;
            return Ok(false);
        }
        self.0.put_new_from(key, reader, len)
    }
}

/// An existing block may succeed with invalid or failing input left unread.
#[test]
fn existing_blocks_leave_unused_stream_bytes_unchecked() {
    for consumed in [0, 2] {
        // A backend may skip immediately or after reading part of the source.
        let store = KvCas::new(Blind(MemoryKv::default(), consumed).into_dyn());
        let cid: Cid = store.put("hello".into()).unwrap();
        let claim = Blake3::of(b"hello");
        for data in [&b"wrong"[..], &b"bad"[..]] {
            let mut source = std::io::Cursor::new(data);
            assert_eq!(store.put_from(claim, &mut source, 5).unwrap(), cid);
            assert_eq!(source.position(), consumed as u64);
        }
        // An error beyond the consumed prefix must not be observed either.
        let mut source = std::io::Read::chain(std::io::Cursor::new(b"he"), FailingReader);
        assert_eq!(store.put_from(claim, &mut source, 5).unwrap(), cid);
        assert_eq!(source.get_ref().0.position(), consumed as u64);
    }
}

struct FailingReader;

impl std::io::Read for FailingReader {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("source failed"))
    }
}

/// `has` and `missing` ask whether keys exist rather than reading values,
/// through `Arc<dyn Cas>` too.
#[test]
fn existence_without_reads() {
    let store = KvCas::new(Blind(MemoryKv::default(), 0).into_dyn()).into_dyn();
    let cid = store.put("hello".into()).unwrap();
    assert_eq!(store.put("hello".into()).unwrap(), cid);
    let missing: Cid = Blake3::of(b"missing").into();
    assert!(store.has(&cid).unwrap() && !store.has(&missing).unwrap());
    assert_eq!(store.missing(&[cid, missing]).unwrap(), [missing]);
    assert!(store.get(&cid).is_err());
}

/// Keys are `blake3/<base32 digest>/bytes`, here for the empty string, whose
/// BLAKE3 digest is af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262.
#[test]
fn keys_are_the_base32_blake3_digest() {
    let kv = Arc::new(MemoryKv::default());
    let _: Cid = KvCas::new(kv.clone()).put("".into()).unwrap();
    let key = b"blake3/v4jutopv7gq2nicajxvdnxgjjgn4wjojvxarfn6mtkj4vza7gjra/bytes";
    assert_eq!(kv.get(key).unwrap().unwrap(), "");
}

#[test]
fn prefix() {
    let kv = Arc::new(MemoryKv::default());
    for prefix in ["p/q", "p/q/"] {
        let store = KvCas::with_prefix(kv.clone(), prefix).unwrap();
        let _: Cid = store.put("hello".into()).unwrap();
    }
    let key = format!("p/q/{HELLO}");
    assert_eq!(kv.get(key.as_bytes()).unwrap().unwrap(), "hello");
    assert_eq!(kv.get(HELLO.as_bytes()).unwrap(), None);
    for prefix in ["", "/", "/p", "p//q", ".", "p/..", "p//"] {
        assert!(KvCas::with_prefix(kv.clone(), prefix).is_err(), "{prefix}");
    }
}

const KEY: &str = "test-key";
const SECRET: &str = "test-secret";

/// An S3 server on its own thread, stopped and joined when dropped.
struct Server {
    endpoint: String,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.thread.take().unwrap().join().unwrap();
    }
}

/// Serve an S3 bucket named `test` from `root`.
fn serve(root: &Path) -> Server {
    std::fs::create_dir(root.join("test")).unwrap();
    let mut builder = S3ServiceBuilder::new(FileSystem::new(root).unwrap());
    builder.set_auth(SimpleAuth::from_single(KEY, SECRET));
    let service = builder.build();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let (stop, stopped) = oneshot::channel();
    let thread = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // Dropping the runtime when `stopped` resolves cancels these tasks.
        runtime.spawn(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let service = service.clone();
                tokio::spawn(async move {
                    let _ = Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(socket), service)
                        .await;
                });
            }
        });
        let _ = runtime.block_on(stopped);
    });
    Server {
        endpoint,
        stop: Some(stop),
        thread: Some(thread),
    }
}

/// A store under `s3://test/p` on the server at `endpoint`, keyed by `prefix`.
fn s3(endpoint: &str, secret: &str, prefix: &str) -> KvCas {
    let credentials = Credentials::new(KEY, secret);
    let location = "s3://test/p".parse().unwrap();
    let kv = S3Kv::new(endpoint, "us-east-1", location, credentials).unwrap();
    KvCas::with_prefix(kv.into_dyn(), prefix).unwrap()
}

#[test]
fn s3_files() {
    let root = tempfile::tempdir().unwrap();
    let server = serve(root.path());
    let file = root.path().join("test/p/blocks").join(HELLO);
    check(&s3(&server.endpoint, SECRET, "blocks"), || {
        assert_eq!(std::fs::read(&file).unwrap(), b"hello");
        std::fs::write(&file, "wrong").unwrap();
    });
}

#[test]
fn s3_failure_is_a_backend_error() {
    let root = tempfile::tempdir().unwrap();
    let server = serve(root.path());
    let store = s3(&server.endpoint, "wrong", "blocks");
    let error = Put::<Cid>::put(&store, "hello".into()).unwrap_err();
    assert!(matches!(error, Error::Backend { .. }), "{error}");
}

#[test]
fn s3_sizes_and_ranges_are_untrusted() {
    let root = tempfile::tempdir().unwrap();
    let server = serve(root.path());
    let store = s3(&server.endpoint, SECRET, "blocks").into_dyn();
    let cid = store.put("hello".into()).unwrap();
    assert_eq!(store.size(&cid).unwrap(), Some(5));
    assert_eq!(block_on(store.asize(&cid)).unwrap(), Some(5));
    assert_eq!(store.get_range(&cid, 1, 3).unwrap().unwrap(), "ell");
    assert_eq!(
        block_on(store.aget_range(&cid, 4, 10)).unwrap().unwrap(),
        "o"
    );
    let missing: Cid = Blake3::of(b"missing").into();
    assert_eq!(store.size(&missing).unwrap(), None);
    assert_eq!(store.get_range(&missing, 0, 1).unwrap(), None);
    // The metadata and partial bytes are reported without proving the CID.
    std::fs::write(root.path().join("test/p/blocks").join(HELLO), "corrupt").unwrap();
    assert_eq!(store.size(&cid).unwrap(), Some(7));
    assert_eq!(block_on(store.asize(&cid)).unwrap(), Some(7));
    assert_eq!(store.get_range(&cid, 0, 3).unwrap().unwrap(), "cor");
    assert_eq!(
        store.get_range(&cid, 1, u64::MAX).unwrap().unwrap(),
        "orrupt"
    );
    assert_eq!(store.get_range(&missing, 0, 0).unwrap(), None);
    assert_eq!(
        block_on(store.aget_range(&cid, 0, 3)).unwrap().unwrap(),
        "cor"
    );
    assert!(matches!(store.get_fact(&cid), Err(Error::Check { .. })));
    let denied = s3(&server.endpoint, "wrong", "blocks");
    assert!(matches!(denied.size(&cid), Err(Error::Backend { .. })));
    assert!(matches!(
        block_on(denied.aget_range(&cid, 0, 1)),
        Err(Error::Backend { .. })
    ));
}

/// The key of the block with `cid`.
fn key(cid: &Blake3) -> String {
    let digest = data_encoding::BASE32_NOPAD.encode(cid.digest());
    format!("blake3/{}/bytes", digest.to_ascii_lowercase())
}

/// Stream `data` into `store` and back, then `corrupt` it and check that
/// streaming it back fails. Bytes that do not hash to the CID given, or
/// that end early, fail and store nothing.
fn streams(store: &KvCas, data: &[u8], corrupt: impl FnOnce(&Blake3)) {
    let len = data.len() as u64;
    let cid = Blake3::of(data);
    assert_eq!(
        store.put_from(cid, &mut &data[..], len).unwrap(),
        cid.into()
    );
    let mut out = Vec::new();
    let drisl: Cid = Cid::new(Format::DRISL_BLAKE3, cid.digest()).unwrap();
    assert_eq!(store.get_to(&drisl, &mut out).unwrap(), Some(len));
    assert_eq!(out, data);
    let missing: Cid = Blake3::of(b"missing").into();
    let mut out = Vec::new();
    assert_eq!(store.get_to(&missing, &mut out).unwrap(), None);
    assert!(out.is_empty());

    let other = Blake3::of(b"other");
    let error = store.put_from(other, &mut &data[..], len).unwrap_err();
    assert!(matches!(error, Error::Check { .. }), "{error}");
    assert!(!store.has(&other.into()).unwrap());
    let error = store.put_from(other, &mut &b""[..], 0).unwrap_err();
    assert!(matches!(error, Error::Check { .. }), "{error}");
    let short = &data[..data.len() - 1];
    let error = store
        .put_from(Blake3::of(short), &mut &short[..], len)
        .unwrap_err();
    assert!(matches!(error, Error::Io { .. }), "{error}");
    assert!(!store.has(&Blake3::of(short).into()).unwrap());
    let empty = Blake3::of(b"");
    assert_eq!(
        store.put_from(empty, &mut &b""[..], 0).unwrap(),
        empty.into()
    );

    corrupt(&cid);
    let error = store.get_to(&cid.into(), &mut Vec::new()).unwrap_err();
    assert!(matches!(error, Error::Check { .. }), "{error}");
}

#[test]
fn memory_streams() {
    let kv = Arc::new(MemoryKv::default());
    streams(&KvCas::new(kv.clone()), b"hello", |cid| {
        kv.put(key(cid).as_bytes(), "wrong".into()).unwrap();
    });
}

/// Blocks over the part size stream through multipart uploads and ranged
/// reads, and a mismatch aborts the upload.
#[test]
fn s3_streams_in_parts() {
    let root = tempfile::tempdir().unwrap();
    let server = serve(root.path());
    let credentials = Credentials::new(KEY, SECRET);
    let location = "s3://test/p".parse().unwrap();
    let kv = S3Kv::new(&server.endpoint, "us-east-1", location, credentials).unwrap();
    let store = KvCas::new(kv.part_size(5 << 20).into_dyn());
    let data: Vec<u8> = (0..11 << 20).map(|i: u32| (i % 251) as u8).collect();
    streams(&store, &data, |cid| {
        std::fs::write(root.path().join("test/p").join(key(cid)), "wrong").unwrap();
    });
    // A changed byte in a range read beside others fails the check too.
    let cid = Blake3::of(&data);
    store
        .put_from(cid, &mut &data[..], data.len() as u64)
        .unwrap();
    let mut tampered = data.clone();
    tampered[6 << 20] ^= 1;
    std::fs::write(root.path().join("test/p").join(key(&cid)), tampered).unwrap();
    let error = store.get_to(&cid.into(), &mut Vec::new()).unwrap_err();
    assert!(matches!(error, Error::Check { .. }), "{error}");
    let uploads = std::fs::read_dir(root.path())
        .unwrap()
        .filter(|entry| {
            let name = entry.as_ref().unwrap().file_name();
            name.to_string_lossy().starts_with(".upload")
        })
        .count();
    assert_eq!(uploads, 0);
}

/// A key-value store whose `put_from` reads everything, then fails to clean
/// up, as an aborted S3 upload whose abort fails does.
struct Leaky;

impl Kv for Leaky {
    fn get(&self, _: &[u8]) -> Result<Option<Bytes>, covalence_kv_trait::Error> {
        Ok(None)
    }
    fn put(&self, _: &[u8], _: Bytes) -> Result<(), covalence_kv_trait::Error> {
        Ok(())
    }
    fn put_new_from(
        &self,
        _: &[u8],
        reader: &mut dyn std::io::Read,
        _: u64,
    ) -> Result<bool, covalence_kv_trait::Error> {
        let _ = reader.read_to_end(&mut Vec::new());
        Err(covalence_kv_trait::Error::Backend {
            source: "abort failed".into(),
        })
    }
    fn aget<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
        ready(self.get(key))
    }
    fn aput<'a>(&'a self, key: &'a [u8], value: Bytes) -> KvFuture<'a, ()> {
        ready(self.put(key, value))
    }
}

/// A mismatch whose cleanup fails reports the cleanup failure.
#[test]
fn mismatch_reports_failed_cleanup() {
    let store = KvCas::new(Leaky.into_dyn());
    let error = store
        .put_from(Blake3::of(b"other"), &mut &b"hello"[..], 5)
        .unwrap_err();
    assert!(
        matches!(&error, Error::Backend { source } if source.to_string() == "backend: abort failed"),
        "{error}"
    );
}
