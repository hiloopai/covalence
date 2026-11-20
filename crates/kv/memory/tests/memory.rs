use std::{
    collections::{BTreeMap, VecDeque},
    future::poll_fn,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use covalence_kv_memory::MemoryKv;
use covalence_kv_trait::{Error, Kv};
use covalence_lib_dasl::Value;
use covalence_lib_data::{ByteStream, Bytes, Stream};
use pollster::block_on;

fn map<const N: usize>(entries: [(&str, Value); N]) -> Value {
    Value::Map(BTreeMap::from(entries.map(|(k, v)| (k.into(), v))))
}

fn store() -> Arc<dyn Kv> {
    MemoryKv::default().into_dyn()
}

#[test]
fn put_and_get() -> Result<(), Error> {
    let store = store();
    assert_eq!(store.get(b"a/b")?, None);
    store.put(b"a/b", "one".into())?;
    store.put(b"a/b", "two".into())?;
    assert_eq!(store.get(b"a/b")?.unwrap(), "two");
    for key in [&b""[..], b"/", b"a//b", b"..", b"\xff"] {
        store.put(key, key.to_vec().into())?;
        assert_eq!(store.get(key)?.unwrap(), key, "{key:?}");
    }
    Ok(())
}

/// A new value goes only under a free key, also through `Arc<dyn Kv>`, and
/// racing writers of one key store once.
#[test]
fn new_values_never_replace() -> Result<(), Error> {
    let store = store();
    assert!(store.put_new(b"a", "one".into())?);
    assert!(!store.put_new(b"a", "two".into())?);
    assert!(!block_on(store.aput_new(b"a", "two".into()))?);
    assert!(!store.put_new_from(b"a", &mut &b"two"[..], 3)?);
    assert_eq!(store.get(b"a")?.unwrap(), "one");
    assert!(store.put_new_from(b"b", &mut &b"two!"[..], 3)?);
    assert_eq!(store.get(b"b")?.unwrap(), "two");
    let error = store.put_new_from(b"c", &mut &b"ab"[..], 3).unwrap_err();
    assert!(matches!(error, Error::Io { .. }), "{error}");
    assert!(!store.exists(b"c")?);
    let stored = std::thread::scope(|scope| {
        let store = &store;
        let racers: Vec<_> = (0..8u8)
            .map(|i| scope.spawn(move || store.put_new(b"race", vec![i].into()).unwrap()))
            .collect();
        racers
            .into_iter()
            .map(|racer| racer.join().unwrap())
            .filter(|&stored| stored)
            .count()
    });
    assert_eq!(stored, 1);
    Ok(())
}

#[test]
fn async_matches_sync() -> Result<(), Error> {
    let store = store();
    block_on(store.aput(b"a", "async".into()))?;
    assert_eq!(store.get(b"a")?.unwrap(), "async");
    store.put(b"b", "sync".into())?;
    assert_eq!(block_on(store.aget(b"b"))?.unwrap(), "sync");
    assert_eq!(block_on(store.aget(b"c"))?, None);
    Ok(())
}

/// Sizes and ranges slice the stored value.
#[test]
fn ranges_slice_values() -> Result<(), Error> {
    let store = store();
    store.put(b"a", "value".into())?;
    assert_eq!(store.size(b"a")?, Some(5));
    assert_eq!(block_on(store.asize(b"missing"))?, None);
    assert_eq!(store.get_range(b"a", 1, 3)?.unwrap(), "alu");
    assert_eq!(block_on(store.aget_range(b"a", 3, 9))?.unwrap(), "ue");
    assert_eq!(store.get_range(b"a", 5, 1)?.unwrap(), "");
    assert_eq!(store.get_range(b"missing", 0, 1)?, None);
    assert_eq!(store.get_range(b"missing", 0, 0)?, None);
    Ok(())
}

#[test]
fn shared_across_threads() {
    let store = store();
    std::thread::scope(|scope| {
        for i in 0..100u8 {
            let store = &store;
            scope.spawn(move || {
                let key = [i % 50];
                match i % 2 {
                    0 => store.put(&key, vec![i % 50].into()).unwrap(),
                    _ => block_on(store.aput(&key, vec![i % 50].into())).unwrap(),
                }
            });
        }
    });
    for i in 0..50u8 {
        assert_eq!(store.get(&[i]).unwrap().unwrap(), vec![i]);
    }
    assert_eq!(store.get(&[50]).unwrap(), None);
}

#[test]
fn info_and_stats() -> Result<(), Error> {
    let store = store();
    assert!(Arc::ptr_eq(&store.clone().into_dyn(), &store));
    store.put(b"a", "one".into())?;
    store.put(b"b", "two".into())?;
    let stats = map([("keys", Value::Integer(2)), ("bytes", Value::Integer(6))]);
    assert_eq!(store.stats()?, stats);
    let info = map([("name", Value::String("memory".into()))]);
    assert_eq!(store.info()?, info);
    Ok(())
}

/// A stream of the given chunks and errors.
struct Parts(VecDeque<io::Result<Bytes>>);

impl Stream for Parts {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.get_mut().0.pop_front())
    }
}

fn parts<const N: usize>(parts: [io::Result<&'static str>; N]) -> ByteStream<'static> {
    Box::pin(Parts(
        parts
            .into_iter()
            .map(|part| part.map(Bytes::from))
            .collect(),
    ))
}

/// A streamed value goes only under a free key, and only if the stream
/// succeeds with exactly the length given.
#[test]
fn streamed_values_are_new_and_exact() -> Result<(), Error> {
    let store = store();
    assert!(block_on(store.aput_new_from(
        b"a",
        parts([Ok("he"), Ok(""), Ok("llo")]),
        5
    ))?);
    assert_eq!(store.get(b"a")?.unwrap(), "hello");
    assert!(!block_on(store.aput_new_from(
        b"a",
        parts([Ok("world")]),
        5
    ))?);
    assert_eq!(store.get(b"a")?.unwrap(), "hello");
    let failing = [
        (parts([Ok("hel")]), io::ErrorKind::UnexpectedEof),
        (parts([Ok("hello!")]), io::ErrorKind::InvalidData),
        (parts([Ok("hello"), Ok("!")]), io::ErrorKind::InvalidData),
        (
            parts([Ok("he"), Err(io::Error::other("source aborted"))]),
            io::ErrorKind::Other,
        ),
    ];
    for (stream, kind) in failing {
        let error = block_on(store.aput_new_from(b"b", stream, 5)).unwrap_err();
        let Error::Io { source } = &error else {
            panic!("{error}")
        };
        assert_eq!(source.kind(), kind, "{error}");
        assert!(!store.exists(b"b")?);
    }
    let error =
        block_on(store.aput_new_from(b"b", parts([Err(io::Error::other("source aborted"))]), 5))
            .unwrap_err();
    assert_eq!(error.to_string(), "source aborted");
    Ok(())
}

/// An empty value streams in and out like any other, and a stream for an
/// empty value must be empty.
#[test]
fn empty_streamed_values_round_trip() -> Result<(), Error> {
    let store = store();
    let error = block_on(store.aput_new_from(b"e", parts([Ok("x")]), 0)).unwrap_err();
    assert!(
        matches!(&error, Error::Io { source } if source.kind() == io::ErrorKind::InvalidData),
        "{error}"
    );
    assert!(!store.exists(b"e")?);
    assert!(block_on(store.aput_new_from(b"e", parts([]), 0))?);
    assert_eq!(store.get(b"e")?.unwrap(), "");
    let (len, mut stream) = block_on(store.aget_stream(b"e"))?.unwrap();
    assert_eq!(len, 0);
    assert!(block_on(poll_fn(|cx| stream.as_mut().poll_next(cx))).is_none());
    Ok(())
}

/// Streamed reads yield the stored value in slices of at most 64 KiB.
#[test]
fn streamed_reads_yield_chunks() -> Result<(), Error> {
    let store = store();
    let value: Bytes = (0..200_000u32).map(|i| i as u8).collect::<Vec<_>>().into();
    store.put(b"big", value.clone())?;
    let (len, mut stream) = block_on(store.aget_stream(b"big"))?.unwrap();
    assert_eq!(len, 200_000);
    let mut chunks = Vec::new();
    while let Some(chunk) = block_on(poll_fn(|cx| stream.as_mut().poll_next(cx))) {
        chunks.push(chunk.unwrap());
    }
    let sizes: Vec<_> = chunks.iter().map(Bytes::len).collect();
    assert_eq!(sizes, [65_536, 65_536, 65_536, 3_392]);
    assert_eq!(chunks.concat(), value);
    assert!(block_on(store.aget_stream(b"missing"))?.is_none());
    Ok(())
}
