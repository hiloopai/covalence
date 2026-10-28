use std::{
    io::{self, Read, Seek, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use covalence_cas_kv::{Compression, KvCas};
use covalence_cas_trait::{AsyncGet, AsyncPut, Cas, Error, Get, Put};
use covalence_kv_memory::MemoryKv;
use covalence_kv_trait::{Kv, KvFuture, Op, ready};
use covalence_lib_cid::{Blake3, Cid, Format};
use covalence_lib_data::Bytes;
use pollster::block_on;

const HELLO: &str = "blake3/5khrmpntq2bjexseshc6ldklwnig56gbj23yvbxjbdcwestheahq/bytes.zst";
// Python compression.zstd.ZstdCompressor's streaming frame, without a content size.
const LEGACY_HELLO: &[u8] = b"\x28\xb5\x2f\xfd\x00\x58\x29\x00\x00hello";
const LEGACY_WORLD: &[u8] = b"\x28\xb5\x2f\xfd\x00\x58\x31\x00\x00 world";

fn compressed(kv: Arc<dyn Kv>) -> KvCas {
    KvCas::new(kv).with_compression(Compression::zstd(3))
}

#[test]
fn empty_blocks_round_trip_with_logical_sizes_and_ranges() {
    let store = compressed(MemoryKv::default().into_dyn()).into_dyn();
    let cid: Cid = store.put("".into()).unwrap();
    assert!(store.has(&cid).unwrap());
    assert!(store.get(&cid).unwrap().unwrap().is_empty());
    assert!(block_on(store.aget(&cid)).unwrap().unwrap().is_empty());
    assert_eq!(store.size(&cid).unwrap(), Some(0));
    assert_eq!(block_on(store.asize(&cid)).unwrap(), Some(0));
    assert!(
        store
            .get_range(&cid, 0, u64::MAX)
            .unwrap()
            .unwrap()
            .is_empty()
    );
    let mut output = Vec::new();
    assert_eq!(store.get_to(&cid, &mut output).unwrap(), Some(0));
    assert!(output.is_empty());
}

#[test]
fn raw_defaults_share_the_raw_namespace_without_copying_values() {
    let kv = Arc::new(MemoryKv::default());
    let bytes = Bytes::from(b"hello".to_vec());
    let cid: Cid = Blake3::of(&bytes).into();
    let raw_key = HELLO.strip_suffix(".zst").unwrap().as_bytes();
    for store in [
        KvCas::new(kv.clone()),
        KvCas::new(kv.clone()).with_compression(Compression::default()),
        compressed(kv.clone()).with_compression(Compression::identity()),
    ] {
        let store = store.into_dyn();
        assert_eq!(store.put(bytes.clone()).unwrap(), cid);
        assert_eq!(block_on(store.aput(bytes.clone())).unwrap(), cid);
        let stored = kv.get(raw_key).unwrap().unwrap();
        assert_eq!(stored, bytes);
        assert_eq!(stored.as_ptr(), bytes.as_ptr());
        let read = store.get_unchecked(&cid).unwrap().unwrap();
        assert_eq!(read.as_ptr(), bytes.as_ptr());
        let read = block_on(store.aget_unchecked(&cid)).unwrap().unwrap();
        assert_eq!(read.as_ptr(), bytes.as_ptr());
        assert_eq!(
            store.info().unwrap(),
            KvCas::new(kv.clone()).info().unwrap()
        );
    }
    assert!(!kv.exists(HELLO.as_bytes()).unwrap());
}

/// Only native metadata and range reads are available; whole reads fail.
struct NativeRawKv(MemoryKv);
impl Kv for NativeRawKv {
    fn get(&self, _: &[u8]) -> Result<Option<Bytes>, covalence_kv_trait::Error> {
        Err(Op::Get.unsupported())
    }
    fn put(&self, _: &[u8], _: Bytes) -> Result<(), covalence_kv_trait::Error> {
        Err(Op::Put.unsupported())
    }
    fn aget<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
        ready(self.get(key))
    }
    fn aput<'a>(&'a self, key: &'a [u8], value: Bytes) -> KvFuture<'a, ()> {
        ready(self.put(key, value))
    }
    fn size(&self, key: &[u8]) -> Result<Option<u64>, covalence_kv_trait::Error> {
        self.0.size(key)
    }
    fn get_range(
        &self,
        key: &[u8],
        start: u64,
        len: u64,
    ) -> Result<Option<Bytes>, covalence_kv_trait::Error> {
        self.0.get_range(key, start, len)
    }
    fn asize<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<u64>> {
        self.0.asize(key)
    }
    fn aget_range<'a>(
        &'a self,
        key: &'a [u8],
        start: u64,
        len: u64,
    ) -> KvFuture<'a, Option<Bytes>> {
        self.0.aget_range(key, start, len)
    }
}

#[test]
fn raw_sizes_and_ranges_keep_backend_native_operations() {
    let memory = MemoryKv::default();
    memory
        .put(
            HELLO.strip_suffix(".zst").unwrap().as_bytes(),
            "hello".into(),
        )
        .unwrap();
    let kv = NativeRawKv(memory).into_dyn();
    let cid: Cid = Blake3::of(b"hello").into();
    for store in [
        KvCas::new(kv.clone()),
        KvCas::new(kv.clone()).with_compression(Compression::default()),
        KvCas::new(kv.clone()).with_compression(Compression::identity()),
    ] {
        let store = store.into_dyn();
        assert_eq!(store.size(&cid).unwrap(), Some(5));
        assert_eq!(block_on(store.asize(&cid)).unwrap(), Some(5));
        assert_eq!(store.get_range(&cid, 1, 3).unwrap().unwrap(), "ell");
        assert_eq!(
            block_on(store.aget_range(&cid, 1, 3)).unwrap().unwrap(),
            "ell"
        );
    }
}

#[test]
fn legacy_frames_logical_addresses_and_raw_isolation() {
    let kv = Arc::new(MemoryKv::default());
    kv.put(HELLO.as_bytes(), LEGACY_HELLO.into()).unwrap();
    let store = compressed(kv.clone()).into_dyn();
    let cid: Cid = Blake3::of(b"hello").into();
    let drisl = Cid::new(Format::DRISL_BLAKE3, cid.digest()).unwrap();
    assert_eq!(store.get(&drisl).unwrap().unwrap(), "hello");
    assert_eq!(block_on(store.aget(&drisl)).unwrap().unwrap(), "hello");
    assert_eq!(store.size(&cid).unwrap(), Some(5));
    assert_eq!(block_on(store.asize(&cid)).unwrap(), Some(5));
    for (start, len, expected) in [
        (1, 3, "ell"),
        (4, u64::MAX, "o"),
        (5, 2, ""),
        (0, 0, ""),
        (u64::MAX, 1, ""),
    ] {
        assert_eq!(
            store.get_range(&cid, start, len).unwrap().unwrap(),
            expected
        );
        assert_eq!(
            block_on(store.aget_range(&cid, start, len))
                .unwrap()
                .unwrap(),
            expected
        );
    }
    assert_eq!(block_on(store.aput("hello".into())).unwrap(), cid);
    assert_eq!(
        zstd::stream::decode_all(kv.get(HELLO.as_bytes()).unwrap().unwrap().as_ref()).unwrap(),
        b"hello"
    );
    let raw = KvCas::new(kv.clone()).into_dyn();
    assert!(!raw.has(&cid).unwrap());
    assert_eq!(raw.put("hello".into()).unwrap(), cid);
    assert_eq!(
        kv.get(HELLO.strip_suffix(".zst").unwrap().as_bytes())
            .unwrap()
            .unwrap(),
        "hello"
    );
    let prefixed = KvCas::with_prefix(kv.clone(), "a/b/")
        .unwrap()
        .with_compression(Compression::zstd(0));
    let _: Cid = prefixed.put("hello".into()).unwrap();
    assert!(kv.exists(format!("a/b/{HELLO}").as_bytes()).unwrap());
    let missing: Cid = Blake3::of(b"missing").into();
    assert_eq!(store.get(&missing).unwrap(), None);
    assert_eq!(store.size(&missing).unwrap(), None);
    assert_eq!(store.get_range(&missing, 0, 0).unwrap(), None);
    assert_eq!(store.get_to(&missing, &mut Vec::new()).unwrap(), None);
}

#[test]
fn repeated_writes_preserve_the_first_compressed_frame() {
    let kv = Arc::new(MemoryKv::default());
    let store = compressed(kv.clone()).into_dyn();
    let cid: Cid = Blake3::of(b"hello").into();
    // This existing frame has a different representation from a newly encoded
    // frame. None of the CAS write paths may silently replace it.
    kv.put(HELLO.as_bytes(), LEGACY_HELLO.into()).unwrap();
    assert_eq!(store.put("hello".into()).unwrap(), cid);
    assert_eq!(kv.get(HELLO.as_bytes()).unwrap().unwrap(), LEGACY_HELLO);
    assert_eq!(block_on(store.aput("hello".into())).unwrap(), cid);
    assert_eq!(kv.get(HELLO.as_bytes()).unwrap().unwrap(), LEGACY_HELLO);
    assert_eq!(
        store
            .put_from(Blake3::of(b"hello"), &mut &b"hello"[..], 5)
            .unwrap(),
        cid
    );
    assert_eq!(kv.get(HELLO.as_bytes()).unwrap().unwrap(), LEGACY_HELLO);
}

#[test]
fn concatenated_frames_are_one_logical_block_and_trailing_damage_is_rejected() {
    let kv = Arc::new(MemoryKv::default());
    let store = compressed(kv.clone()).into_dyn();
    let cid: Cid = Blake3::of(b"hello world").into();
    let digest = data_encoding::BASE32_NOPAD
        .encode(cid.digest())
        .to_ascii_lowercase();
    let key = format!("blake3/{digest}/bytes.zst");
    let frames = [LEGACY_HELLO, LEGACY_WORLD].concat();
    kv.put(key.as_bytes(), frames.clone().into()).unwrap();
    assert_eq!(store.get(&cid).unwrap().unwrap(), "hello world");
    let mut out = Vec::new();
    assert_eq!(store.get_to(&cid, &mut out).unwrap(), Some(11));
    assert_eq!(out, b"hello world");
    assert_eq!(store.get_range(&cid, 4, 3).unwrap().unwrap(), "o w");
    for broken in [
        frames[..frames.len() - 1].to_vec(),
        [frames.as_slice(), b"junk"].concat(),
    ] {
        kv.put(key.as_bytes(), broken.into()).unwrap();
        assert!(matches!(
            store.get_to(&cid, &mut io::sink()),
            Err(Error::Io { .. } | Error::Check { .. })
        ));
        // A range still scans and validates the suffix, even once its output is complete.
        assert!(matches!(
            store.get_range(&cid, 0, 1),
            Err(Error::Io { .. } | Error::Check { .. })
        ));
    }
}

struct CountKv {
    memory: MemoryKv,
    writes: AtomicUsize,
}
impl Kv for CountKv {
    fn get(&self, key: &[u8]) -> Result<Option<Bytes>, covalence_kv_trait::Error> {
        self.memory.get(key)
    }
    fn put(&self, key: &[u8], bytes: Bytes) -> Result<(), covalence_kv_trait::Error> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.memory.put(key, bytes)
    }
    fn aget<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
        ready(self.get(key))
    }
    fn aput<'a>(&'a self, key: &'a [u8], bytes: Bytes) -> KvFuture<'a, ()> {
        ready(self.put(key, bytes))
    }
    fn put_new(&self, key: &[u8], bytes: Bytes) -> Result<bool, covalence_kv_trait::Error> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.memory.put_new(key, bytes)
    }
    fn aput_new<'a>(&'a self, key: &'a [u8], bytes: Bytes) -> KvFuture<'a, bool> {
        ready(self.put_new(key, bytes))
    }
    fn put_new_from(
        &self,
        key: &[u8],
        reader: &mut dyn Read,
        len: u64,
    ) -> Result<bool, covalence_kv_trait::Error> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.memory.put_new_from(key, reader, len)
    }
}

struct BrokenReader;
impl Read for BrokenReader {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("source failed"))
    }
}

#[test]
fn mismatched_lengths_hashes_and_source_errors_never_publish() {
    let kv = Arc::new(CountKv {
        memory: MemoryKv::default(),
        writes: AtomicUsize::new(0),
    });
    kv.memory
        .put(HELLO.as_bytes(), LEGACY_HELLO.into())
        .unwrap();
    let store = compressed(kv.clone()).into_dyn();
    let hello = Blake3::of(b"hello");
    for (bytes, len) in [
        (b"wrong".as_slice(), 5),
        (b"hell".as_slice(), 5),
        (b"hello".as_slice(), 6),
        (b"".as_slice(), 0),
    ] {
        assert!(store.put_from(hello, &mut &bytes[..], len).is_err());
        assert_eq!(kv.writes.load(Ordering::Relaxed), 0);
        assert_eq!(
            kv.memory.get(HELLO.as_bytes()).unwrap().unwrap(),
            LEGACY_HELLO
        );
    }
    assert!(matches!(
        store.put_from(hello, &mut BrokenReader, 5),
        Err(Error::Io { .. })
    ));
    assert_eq!(kv.writes.load(Ordering::Relaxed), 0);
    let mut source = &b"hello tail"[..];
    assert_eq!(store.put_from(hello, &mut source, 5).unwrap(), hello.into());
    assert_eq!(source, b" tail");
    let empty = Blake3::of(b"");
    assert_eq!(
        store.put_from(empty, &mut &b""[..], 0).unwrap(),
        empty.into()
    );
    assert_eq!(kv.writes.load(Ordering::Relaxed), 2);
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
struct BrokenWriter;
impl Write for BrokenWriter {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("destination failed"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct InterruptOnce {
    interrupted: bool,
    bytes: Vec<u8>,
}
impl Write for InterruptOnce {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if !self.interrupted {
            self.interrupted = true;
            return Err(io::ErrorKind::Interrupted.into());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn retried_interruptions_allow_valid_reads_and_do_not_hide_damage() {
    let kv = Arc::new(MemoryKv::default());
    let store = compressed(kv.clone()).into_dyn();
    let cid: Cid = Blake3::of(b"hello").into();
    kv.put(HELLO.as_bytes(), LEGACY_HELLO.into()).unwrap();
    let mut output = InterruptOnce {
        interrupted: false,
        bytes: Vec::new(),
    };
    assert_eq!(store.get_to(&cid, &mut output).unwrap(), Some(5));
    assert!(output.interrupted);
    assert_eq!(output.bytes, b"hello");

    for broken in [
        [LEGACY_HELLO, b"junk"].concat(),
        LEGACY_HELLO[..LEGACY_HELLO.len() - 1].to_vec(),
    ] {
        kv.put(HELLO.as_bytes(), broken.into()).unwrap();
        let mut output = InterruptOnce {
            interrupted: false,
            bytes: Vec::new(),
        };
        assert!(matches!(
            store.get_to(&cid, &mut output),
            Err(Error::Io { .. } | Error::Check { .. })
        ));
        assert!(output.interrupted);
        // Actual destination failures still win when decoded output cannot be written.
        assert!(
            matches!(store.get_to(&cid, &mut BrokenWriter), Err(Error::Io { source }) if source.kind() == io::ErrorKind::Other)
        );
        assert!(
            matches!(store.get_to(&cid, &mut LimitedWriter { limit: 0, bytes: Vec::new() }), Err(Error::Io { source }) if source.kind() == io::ErrorKind::WriteZero)
        );
    }
}

struct ReadKv(fn(&mut dyn Write) -> Result<Option<u64>, covalence_kv_trait::Error>);
impl Kv for ReadKv {
    fn get(&self, _: &[u8]) -> Result<Option<Bytes>, covalence_kv_trait::Error> {
        Err(Op::Get.unsupported())
    }
    fn put(&self, _: &[u8], _: Bytes) -> Result<(), covalence_kv_trait::Error> {
        Err(Op::Put.unsupported())
    }
    fn aget<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
        ready(self.get(key))
    }
    fn aput<'a>(&'a self, key: &'a [u8], bytes: Bytes) -> KvFuture<'a, ()> {
        ready(self.put(key, bytes))
    }
    fn get_to(
        &self,
        _: &[u8],
        writer: &mut dyn Write,
    ) -> Result<Option<u64>, covalence_kv_trait::Error> {
        self.0(writer)
    }
}

#[test]
fn writes_require_atomic_create_support_with_or_without_compression() {
    for compression in [
        Compression::identity(),
        Compression::zstd(3),
        Compression::deflate(3),
        Compression::gzip(3),
    ] {
        let store = KvCas::new(ReadKv(|_| unreachable!()).into_dyn())
            .with_compression(compression)
            .into_dyn();
        for error in [
            store.put("hello".into()).unwrap_err(),
            block_on(store.aput("hello".into())).unwrap_err(),
            store
                .put_from(Blake3::of(b"hello"), &mut &b"hello"[..], 5)
                .unwrap_err(),
        ] {
            assert!(
                matches!(&error, Error::Backend { source }
                    if matches!(source.downcast_ref::<covalence_kv_trait::Error>(),
                        Some(covalence_kv_trait::Error::Unsupported { op: Op::PutNew }))),
                "{error}"
            );
        }
    }
}

#[test]
fn a_backend_failure_after_malformed_output_is_not_hidden() {
    let kv = ReadKv(|writer| {
        assert!(writer.write_all(b"not zstd").is_err());
        Err(covalence_kv_trait::Error::Backend {
            source: Box::new(io::Error::other("backend cleanup failed")),
        })
    });
    let store = compressed(kv.into_dyn()).into_dyn();
    let cid: Cid = Blake3::of(b"hello").into();
    assert!(
        matches!(store.get_to(&cid, &mut io::sink()), Err(Error::Backend { source }) if source.to_string().contains("backend cleanup failed"))
    );
}

struct FlushInterrupted(usize);
impl Write for FlushInterrupted {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Err(io::ErrorKind::Interrupted.into())
    }
}

#[test]
fn destination_flush_interruptions_remain_io_errors() {
    let kv = ReadKv(|writer| {
        writer
            .write_all(LEGACY_HELLO)
            .and_then(|()| writer.flush())
            .map_err(|source| covalence_kv_trait::Error::Io { source })?;
        Ok(Some(LEGACY_HELLO.len() as u64))
    });
    let store = compressed(kv.into_dyn()).into_dyn();
    let cid: Cid = Blake3::of(b"hello").into();
    let mut writer = FlushInterrupted(0);
    assert!(
        matches!(store.get_to(&cid, &mut writer), Err(Error::Io { source }) if source.kind() == io::ErrorKind::Interrupted)
    );
    assert_eq!(writer.0, 5);
}

#[test]
fn malformed_frames_fail_and_destination_errors_remain_io() {
    let kv = Arc::new(MemoryKv::default());
    let store = compressed(kv.clone()).into_dyn();
    let cid = store.put("hello".into()).unwrap();
    let mut short = LimitedWriter {
        limit: 1,
        bytes: Vec::new(),
    };
    assert_eq!(store.get_to(&cid, &mut short).unwrap(), Some(5));
    assert_eq!(short.bytes, b"hello");
    assert!(matches!(
        store.get_to(&cid, &mut BrokenWriter),
        Err(Error::Io { .. })
    ));
    assert!(
        matches!(store.get_to(&cid, &mut LimitedWriter { limit: 0, bytes: Vec::new() }), Err(Error::Io { source }) if source.kind() == io::ErrorKind::WriteZero)
    );
    // A tiny frame advertising a 256 MiB window exceeds the decoder's limit.
    let mut oversized_window = LEGACY_HELLO.to_vec();
    oversized_window[5] = 0x90;
    for broken in [
        b"not zstd".to_vec(),
        Vec::new(),
        LEGACY_HELLO[..LEGACY_HELLO.len() - 1].to_vec(),
        oversized_window,
    ] {
        kv.put(HELLO.as_bytes(), broken.into()).unwrap();
        assert!(matches!(
            store.get(&cid),
            Err(Error::Io { .. } | Error::Check { .. })
        ));
        assert!(matches!(
            block_on(store.aget(&cid)),
            Err(Error::Io { .. } | Error::Check { .. })
        ));
        assert!(matches!(
            store.get_to(&cid, &mut io::sink()),
            Err(Error::Io { .. } | Error::Check { .. })
        ));
    }

    // A valid frame holding different logical bytes is still a CID mismatch.
    kv.put(
        HELLO.as_bytes(),
        zstd::stream::encode_all(&b"wrong"[..], 3).unwrap().into(),
    )
    .unwrap();
    assert!(matches!(store.get(&cid), Err(Error::Check { .. })));
    assert!(matches!(
        block_on(store.aget(&cid)),
        Err(Error::Check { .. })
    ));
    assert!(matches!(
        store.get_to(&cid, &mut io::sink()),
        Err(Error::Check { .. })
    ));
}

/// A backend whose only supported transfers stream through a file.
struct StreamingKv(Mutex<Option<(Vec<u8>, tempfile::NamedTempFile)>>);
impl Kv for StreamingKv {
    fn get(&self, _: &[u8]) -> Result<Option<Bytes>, covalence_kv_trait::Error> {
        Err(Op::Get.unsupported())
    }
    fn put(&self, _: &[u8], _: Bytes) -> Result<(), covalence_kv_trait::Error> {
        Err(Op::Put.unsupported())
    }
    fn aget<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
        ready(self.get(key))
    }
    fn aput<'a>(&'a self, key: &'a [u8], bytes: Bytes) -> KvFuture<'a, ()> {
        ready(self.put(key, bytes))
    }
    fn put_new_from(
        &self,
        key: &[u8],
        reader: &mut dyn Read,
        len: u64,
    ) -> Result<bool, covalence_kv_trait::Error> {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        assert_eq!(io::copy(&mut reader.take(len), &mut file).unwrap(), len);
        let mut value = self.0.lock().unwrap();
        if value.as_ref().is_some_and(|(stored, _)| stored == key) {
            return Ok(false);
        }
        *value = Some((key.to_vec(), file));
        Ok(true)
    }
    fn get_to(
        &self,
        key: &[u8],
        writer: &mut dyn Write,
    ) -> Result<Option<u64>, covalence_kv_trait::Error> {
        let mut value = self.0.lock().unwrap();
        let Some((stored, file)) = value.as_mut().filter(|(stored, _)| stored == key) else {
            return Ok(None);
        };
        assert_eq!(stored, key);
        file.rewind().unwrap();
        let mut bytes = [0; 31];
        let mut copied = 0;
        loop {
            let read = file.read(&mut bytes).unwrap();
            if read == 0 {
                break;
            }
            writer
                .write_all(&bytes[..read])
                .map_err(|source| covalence_kv_trait::Error::Io { source })?;
            copied += read as u64;
        }
        Ok(Some(copied))
    }
}

/// A large logical source without a whole-value allocation.
struct Repeated(u64);
impl Read for Repeated {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        assert!(bytes.len() <= 128 * 1024);
        let read = self.0.min(bytes.len() as u64) as usize;
        bytes[..read].fill(b'x');
        self.0 -= read as u64;
        Ok(read)
    }
}
struct Counting(u64);
impl Write for Counting {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        assert!(bytes.len() <= 128 * 1024);
        assert!(bytes.iter().all(|byte| *byte == b'x'));
        self.0 += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn streamed_reads_sizes_and_ranges_never_use_whole_backend_reads() {
    for compression in [
        Compression::zstd(3),
        Compression::deflate(3),
        Compression::gzip(3),
    ] {
        let len = 32 << 20;
        let mut hasher = blake3::Hasher::new();
        io::copy(&mut Repeated(len), &mut hasher).unwrap();
        let cid = Blake3::new(Format::BLAKE3, hasher.finalize().as_bytes()).unwrap();
        let store = KvCas::new(StreamingKv(Mutex::new(None)).into_dyn())
            .with_compression(compression)
            .into_dyn();
        store.put_from(cid, &mut Repeated(len), len).unwrap();
        let mut sink = Counting(0);
        assert_eq!(store.get_to(&cid.into(), &mut sink).unwrap(), Some(len));
        assert_eq!(sink.0, len);
        assert_eq!(store.size(&cid.into()).unwrap(), Some(len));
        assert_eq!(
            store
                .get_range(&cid.into(), len - 7, 32)
                .unwrap()
                .unwrap()
                .as_ref(),
            b"xxxxxxx"
        );
    }
}

/// Async reads must use the backend's async view, which can differ from its sync view.
struct AsyncOnly(MemoryKv);
impl Kv for AsyncOnly {
    fn get(&self, _: &[u8]) -> Result<Option<Bytes>, covalence_kv_trait::Error> {
        Err(Op::Get.unsupported())
    }
    fn put(&self, _: &[u8], _: Bytes) -> Result<(), covalence_kv_trait::Error> {
        Err(Op::Put.unsupported())
    }
    fn aget<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
        ready(self.0.get(key))
    }
    fn aput<'a>(&'a self, key: &'a [u8], bytes: Bytes) -> KvFuture<'a, ()> {
        ready(self.0.put(key, bytes))
    }
    fn aput_new<'a>(&'a self, key: &'a [u8], bytes: Bytes) -> KvFuture<'a, bool> {
        ready(self.0.put_new(key, bytes))
    }
}

#[test]
fn async_reads_keep_the_backends_async_view() {
    for compression in [
        Compression::zstd(3),
        Compression::deflate(3),
        Compression::gzip(3),
    ] {
        let store = KvCas::new(AsyncOnly(MemoryKv::default()).into_dyn())
            .with_compression(compression)
            .into_dyn();
        let cid = block_on(store.aput("hello".into())).unwrap();
        assert!(store.get(&cid).is_err());
        assert_eq!(block_on(store.aget(&cid)).unwrap().unwrap(), "hello");
        assert_eq!(block_on(store.asize(&cid)).unwrap(), Some(5));
        assert_eq!(
            block_on(store.aget_range(&cid, 1, 3)).unwrap().unwrap(),
            "ell"
        );
        let missing: Cid = Blake3::of(b"missing").into();
        assert_eq!(block_on(store.asize(&missing)).unwrap(), None);
        assert_eq!(block_on(store.aget_range(&missing, 0, 0)).unwrap(), None);
    }
}
