use std::{
    io::{self, Read, Write},
    sync::Arc,
};

use covalence_cas_kv::{Compression, KvCas};
use covalence_cas_trait::{AsyncGet, AsyncPut, Cas, Error, Get, Put, map};
use covalence_kv_memory::MemoryKv;
use covalence_kv_trait::{Kv, KvFuture, Op, ready};
use covalence_lib_cid::{Blake3, Cid};
use covalence_lib_dasl::Value;
use covalence_lib_data::Bytes;
use pollster::block_on;

const BASE: &str = "blake3/5khrmpntq2bjexseshc6ldklwnig56gbj23yvbxjbdcwestheahq/bytes";
// Python stdlib zlib.compress(b"hello") and gzip.compress(b"hello", mtime=0).
const ZLIB: &[u8] = b"\x78\x9c\xcb\x48\xcd\xc9\xc9\x07\x00\x06\x2c\x02\x15";
const GZIP: &[u8] = b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\xff\xcb\x48\xcd\xc9\xc9\x07\x00\x86\xa6\x10\x36\x05\x00\x00\x00";
// The same member with FHCRC set and a valid CRC16 in its header.
const FHCRC: &[u8] = b"\x1f\x8b\x08\x02\x00\x00\x00\x00\x00\xff\x90\xc9\xcb\x48\xcd\xc9\xc9\x07\x00\x86\xa6\x10\x36\x05\x00\x00\x00";

type Constructor = fn(i32) -> Compression;
type Coding = (Constructor, &'static str, &'static str, &'static [u8]);
fn codings() -> [Coding; 2] {
    [
        (Compression::deflate, "deflate", ".zlib", ZLIB),
        (Compression::gzip, "gzip", ".gz", GZIP),
    ]
}

#[test]
fn stdlib_fixtures_use_logical_cids_and_preserve_existing_streams() {
    let cid: Cid = Blake3::of(b"hello").into();
    for (constructor, _, suffix, fixture) in codings() {
        let kv = Arc::new(MemoryKv::default());
        let key = format!("{BASE}{suffix}");
        kv.put(key.as_bytes(), fixture.into()).unwrap();
        let store = KvCas::new(kv.clone())
            .with_compression(constructor(3))
            .into_dyn();
        assert_eq!(store.get(&cid).unwrap().unwrap(), "hello");
        assert_eq!(block_on(store.aget(&cid)).unwrap().unwrap(), "hello");
        assert_eq!(store.size(&cid).unwrap(), Some(5));
        assert_eq!(block_on(store.asize(&cid)).unwrap(), Some(5));
        for (start, len, expected) in [
            (1, 3, "ell"),
            (4, u64::MAX, "o"),
            (5, 2, ""),
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
        assert_eq!(store.put("hello".into()).unwrap(), cid);
        assert_eq!(block_on(store.aput("hello".into())).unwrap(), cid);
        assert_eq!(
            store
                .put_from(Blake3::of(b"hello"), &mut &b"hello tail"[..], 5)
                .unwrap(),
            cid
        );
        assert_eq!(kv.get(key.as_bytes()).unwrap().unwrap(), fixture);
        for other in [
            Compression::identity(),
            Compression::zstd(3),
            if suffix == ".gz" {
                Compression::deflate(3)
            } else {
                Compression::gzip(3)
            },
        ] {
            let other = KvCas::new(kv.clone()).with_compression(other).into_dyn();
            assert!(!other.has(&cid).unwrap());
            assert_eq!(other.get(&cid).unwrap(), None);
        }
        let missing: Cid = Blake3::of(b"missing").into();
        assert_eq!(store.get_to(&missing, &mut io::sink()).unwrap(), None);
        assert_eq!(block_on(store.aget(&missing)).unwrap(), None);
    }
}

#[test]
fn effective_levels_and_empty_blocks_round_trip() {
    for (constructor, name, suffix, _) in codings() {
        for requested in [i32::MIN, 0, 1, 3, 9, i32::MAX] {
            let kv = Arc::new(MemoryKv::default());
            let store = KvCas::with_prefix(kv.clone(), "test")
                .unwrap()
                .with_compression(constructor(requested))
                .into_dyn();
            assert_eq!(
                store.info().unwrap(),
                map([
                    ("name", Value::String("kv".into())),
                    ("prefix", Value::String("test/".into())),
                    ("kv", kv.info().unwrap()),
                    ("compression", Value::String(name.into())),
                    ("level", Value::Integer(requested.clamp(0, 9).into())),
                ])
            );
            for data in [Bytes::new(), Bytes::from(vec![b'x'; 100_000])] {
                let cid: Cid = Blake3::of(&data).into();
                assert_eq!(store.put(data.clone()).unwrap(), cid);
                assert_eq!(store.get(&cid).unwrap().unwrap(), data);
                assert_eq!(block_on(store.aget(&cid)).unwrap().unwrap(), data);
                let mut output = Vec::new();
                assert_eq!(
                    store.get_to(&cid, &mut output).unwrap(),
                    Some(data.len() as u64)
                );
                assert_eq!(output, data);
                assert_eq!(store.size(&cid).unwrap(), Some(data.len() as u64));
            }
            let cid: Cid = store.put("hello".into()).unwrap();
            assert_eq!(cid, Blake3::of(b"hello").into());
            assert!(
                kv.exists(format!("test/{BASE}{suffix}").as_bytes())
                    .unwrap()
            );
        }
    }
}

/// Split input at arbitrary boundaries while disallowing whole sync reads.
struct Chunked {
    encoded: Bytes,
    chunk: usize,
}
impl Kv for Chunked {
    fn get(&self, _: &[u8]) -> Result<Option<Bytes>, covalence_kv_trait::Error> {
        Err(Op::Get.unsupported())
    }
    fn put(&self, _: &[u8], _: Bytes) -> Result<(), covalence_kv_trait::Error> {
        Err(Op::Put.unsupported())
    }
    fn aget<'a>(&'a self, _: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
        ready(Ok(Some(self.encoded.clone())))
    }
    fn aput<'a>(&'a self, key: &'a [u8], value: Bytes) -> KvFuture<'a, ()> {
        ready(self.put(key, value))
    }
    fn get_to(
        &self,
        _: &[u8],
        writer: &mut dyn Write,
    ) -> Result<Option<u64>, covalence_kv_trait::Error> {
        for chunk in self.encoded.chunks(self.chunk) {
            writer
                .write_all(chunk)
                .map_err(|source| covalence_kv_trait::Error::Io { source })?;
        }
        Ok(Some(self.encoded.len() as u64))
    }
}

#[test]
fn every_truncated_prefix_and_trailing_bytes_are_rejected_across_chunks() {
    let cid: Cid = Blake3::of(b"hello").into();
    for (constructor, name, _, fixture) in codings() {
        let mut bad_checksum = fixture.to_vec();
        bad_checksum[fixture.len() - 5] ^= 1;
        let mut bad_size = fixture.to_vec();
        bad_size[fixture.len() - 1] ^= 1;
        let mut invalid_flags = fixture.to_vec();
        if name == "gzip" {
            invalid_flags[3] |= 0x20;
        } else {
            invalid_flags[1] ^= 0x20;
        }
        let broken = (0..fixture.len())
            .map(|len| fixture[..len].to_vec())
            .chain([
                [fixture, b"junk"].concat(),
                [fixture, fixture].concat(),
                bad_checksum,
                bad_size,
                invalid_flags,
                // Raw DEFLATE and a zlib stream requesting a preset dictionary.
                b"\xcb\x48\xcd\xc9\xc9\x07\x00".to_vec(),
                b"\x78\xbb\x06\x2c\x02\x15\xcb\x00\x11\x00\x06\x2c\x02\x15".to_vec(),
            ]);
        for encoded in broken {
            for chunk in [1, 3, fixture.len()] {
                let store = KvCas::new(
                    Chunked {
                        encoded: encoded.clone().into(),
                        chunk,
                    }
                    .into_dyn(),
                )
                .with_compression(constructor(3))
                .into_dyn();
                assert!(
                    store.get(&cid).is_err(),
                    "accepted {encoded:?}, chunk {chunk}"
                );
                assert!(block_on(store.aget(&cid)).is_err());
                assert!(store.get_to(&cid, &mut io::sink()).is_err());
                assert!(store.size(&cid).is_err());
                assert!(store.get_range(&cid, 0, 1).is_err());
            }
        }
        for chunk in [1, 3, fixture.len()] {
            let store = KvCas::new(
                Chunked {
                    encoded: fixture.into(),
                    chunk,
                }
                .into_dyn(),
            )
            .with_compression(constructor(3))
            .into_dyn();
            assert_eq!(store.get(&cid).unwrap().unwrap(), "hello");
        }
    }
    let mut bad_fhcrc = FHCRC.to_vec();
    bad_fhcrc[10] ^= 1;
    for chunk in [1, 3, FHCRC.len()] {
        let store = KvCas::new(
            Chunked {
                encoded: FHCRC.into(),
                chunk,
            }
            .into_dyn(),
        )
        .with_compression(Compression::gzip(3))
        .into_dyn();
        assert_eq!(store.get(&cid).unwrap().unwrap(), "hello");
        let store = KvCas::new(
            Chunked {
                encoded: bad_fhcrc.clone().into(),
                chunk,
            }
            .into_dyn(),
        )
        .with_compression(Compression::gzip(3))
        .into_dyn();
        assert!(store.get(&cid).is_err());
    }
}

struct Broken;
impl Read for Broken {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("source failed"))
    }
}

#[test]
fn staged_writes_check_input_before_publishing() {
    for (constructor, _, _, _) in codings() {
        let kv = Arc::new(MemoryKv::default());
        let store = KvCas::new(kv).with_compression(constructor(3)).into_dyn();
        let cid = Blake3::of(b"hello");
        for (bytes, len) in [
            (b"wrong".as_slice(), 5),
            (b"hell".as_slice(), 5),
            (b"".as_slice(), 0),
        ] {
            assert!(store.put_from(cid, &mut &bytes[..], len).is_err());
            assert!(!store.has(&cid.into()).unwrap());
        }
        assert!(matches!(
            store.put_from(cid, &mut Broken, 5),
            Err(Error::Io { .. })
        ));
        assert!(!store.has(&cid.into()).unwrap());
        let mut source = &b"hello tail"[..];
        assert_eq!(store.put_from(cid, &mut source, 5).unwrap(), cid.into());
        assert_eq!(source, b" tail");
        assert_eq!(store.get(&cid.into()).unwrap().unwrap(), "hello");
    }
}
