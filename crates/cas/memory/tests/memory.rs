use std::sync::Arc;

use covalence_cas_memory::MemoryCas;
use covalence_cas_trait::{AsyncGet, AsyncGetExt, AsyncPut, Cas, Error, Get, GetExt, Put, map};
use covalence_lib_cid::{Blake3, Cid, Fact, Format};
use covalence_lib_dasl::Value;
use pollster::block_on;

fn raw(bytes: &[u8]) -> Cid {
    Blake3::of(bytes).into()
}

#[test]
fn facts_claim_the_key() -> Result<(), Error> {
    let store = MemoryCas::default().into_dyn();
    let value = Value::List(vec![Value::Integer(1), Value::Null]);
    let raw: Cid = store.put(value.encode().unwrap().into())?;
    let drisl = Cid::new(Format::DRISL_BLAKE3, raw.digest()).unwrap();
    let fact = store.get_fact(&drisl)?.unwrap();
    assert_eq!(fact.claim(), &drisl);
    assert_eq!(Value::from_fact(&fact).unwrap(), value);
    let fact = block_on(store.aget_fact(&drisl))?.unwrap();
    assert_eq!(fact.claim(), &drisl);
    assert_eq!(Value::from_fact(&fact).unwrap(), value);
    let fact = store.get_fact(&raw)?.unwrap();
    assert_eq!(fact.claim(), &raw);
    assert!(matches!(Value::from_fact(&fact).unwrap(), Value::Bytes(_)));
    Ok(())
}

#[test]
fn put_and_get() -> Result<(), Error> {
    let store = MemoryCas::default();
    let cid: Cid = store.put("hello".into())?;
    assert_eq!(cid, raw(b"hello"));
    let again: Blake3 = store.put("hello".into())?;
    assert_eq!(Cid::from(again), cid);
    assert_eq!(store.len(), 1);

    assert_eq!(store.get(&cid)?.unwrap(), "hello");
    assert_eq!(store.get(&again)?.unwrap(), "hello");
    let fact = Fact::new(again, "hello".into())?;
    assert_eq!(store.get_fact(&again)?, Some(fact));
    assert_eq!(store.get(&raw(b"missing"))?, None);
    Ok(())
}

#[test]
fn keyed_by_blake3_digest() -> Result<(), Error> {
    let store = MemoryCas::default();
    let cid: Cid = store.put("hello".into())?;
    let drisl: Cid = Cid::new(Format::DRISL_BLAKE3, cid.digest()).unwrap();
    assert_eq!(store.get_fact(&drisl)?.unwrap().claim(), &drisl);
    let sha256: Cid = Cid::new(Format::SHA2_256, &[0; 32]).unwrap();
    assert_eq!(store.get(&sha256)?, None);
    Ok(())
}

#[test]
fn shared_across_threads() {
    let store = MemoryCas::default();
    std::thread::scope(|scope| {
        for i in 0..100u8 {
            let store = &store;
            scope.spawn(move || Put::<Blake3>::put(store, vec![i % 50].into()).unwrap());
        }
    });
    assert_eq!(store.len(), 50);
    for i in 0..50u8 {
        assert_eq!(store.get(&raw(&[i])).unwrap().unwrap(), vec![i]);
    }
}

#[test]
fn works_as_dyn_cas() -> Result<(), Error> {
    let store: Arc<dyn Cas> = Arc::new(MemoryCas::default());
    let a = store.put("a".into())?;
    let b = block_on(store.aput("b".into()))?;
    assert_eq!(block_on(store.aget(&a))?.unwrap(), "a");
    assert_eq!(store.get(&b)?.unwrap(), "b");
    let stats = map([("blocks", Value::Integer(2)), ("bytes", Value::Integer(2))]);
    assert_eq!(store.stats()?, stats);
    assert_eq!(
        store.info()?,
        map([("name", Value::String("memory".into()))])
    );
    Ok(())
}

#[test]
fn sizes_and_ranges() -> Result<(), Error> {
    let store = MemoryCas::default().into_dyn();
    let missing = raw(b"missing");
    assert_eq!(store.size(&missing)?, None);
    assert_eq!(block_on(store.asize(&missing))?, None);
    assert_eq!(store.get_range(&missing, 0, 1)?, None);
    assert_eq!(block_on(store.aget_range(&missing, 0, 1))?, None);
    assert_eq!(store.get_range(&missing, 0, 0)?, None);
    let cid = store.put("hello".into())?;
    let drisl = Cid::new(Format::DRISL_BLAKE3, cid.digest()).unwrap();
    for cid in [cid, drisl] {
        assert_eq!(store.size(&cid)?, Some(5));
        assert_eq!(block_on(store.asize(&cid))?, Some(5));
        for (start, len, expected) in [
            (1, 3, "ell"),
            (4, 10, "o"),
            (5, 1, ""),
            (u64::MAX, 2, ""),
            (2, u64::MAX, "llo"),
            (0, 0, ""),
        ] {
            assert_eq!(store.get_range(&cid, start, len)?.unwrap(), expected);
            assert_eq!(
                block_on(store.aget_range(&cid, start, len))?.unwrap(),
                expected
            );
        }
    }
    let empty = store.put("".into())?;
    assert_eq!(store.size(&empty)?, Some(0));
    assert!(store.get_range(&empty, 0, 1)?.unwrap().is_empty());
    Ok(())
}

#[test]
fn put_fact_matches_put() -> Result<(), Error> {
    let cas = MemoryCas::default();
    let world = Fact::new(Blake3::of(b"world"), "world".into())?;
    assert_eq!(
        Put::<Blake3>::put_fact(&cas, world.clone())?,
        *world.claim()
    );
    let store = cas.into_dyn();
    let hello = raw(b"hello");
    let drisl = Cid::new(Format::DRISL_BLAKE3, hello.digest()).unwrap();
    assert_eq!(store.put_fact(Fact::new(drisl, "hello".into())?)?, hello);
    assert_eq!(store.put("hello".into())?, hello);
    assert_eq!(block_on(store.aput_fact(world.widen()))?, raw(b"world"));
    let digest = Format::SHA2_256.hash.digest(b"sha").unwrap();
    let sha256 = Cid::new(Format::SHA2_256, &digest).unwrap();
    assert_eq!(
        store.put_fact(Fact::new(sha256, "sha".into())?)?,
        raw(b"sha")
    );
    assert_eq!(store.get_fact(&drisl)?.unwrap().bytes(), "hello");
    assert_eq!(store.get(&raw(b"world"))?.unwrap(), "world");
    assert_eq!(store.get(&raw(b"sha"))?.unwrap(), "sha");
    Ok(())
}

/// Each `put_fact` path stores fresh bytes: reading them straight back finds them.
#[test]
fn put_fact_stores_bytes() -> Result<(), Error> {
    let cas = MemoryCas::default();
    let one = Fact::new(Blake3::of(b"one"), "one".into())?;
    Put::<Blake3>::put_fact(&cas, one)?;
    assert_eq!(cas.get(&raw(b"one"))?.unwrap(), "one");

    let store = cas.into_dyn();
    let two = Cid::new(Format::DRISL_BLAKE3, raw(b"two").digest()).unwrap();
    store.put_fact(Fact::new(two, "two".into())?)?;
    assert_eq!(store.get(&raw(b"two"))?.unwrap(), "two");

    let three = Fact::new(Blake3::of(b"three"), "three".into())?;
    block_on(store.aput_fact(three.widen()))?;
    assert_eq!(store.get(&raw(b"three"))?.unwrap(), "three");

    let digest = Format::SHA2_256.hash.digest(b"four").unwrap();
    let four = Cid::new(Format::SHA2_256, &digest).unwrap();
    store.put_fact(Fact::new(four, "four".into())?)?;
    assert_eq!(store.get(&raw(b"four"))?.unwrap(), "four");
    Ok(())
}

/// By default, streamed blocks are read into memory and checked, through
/// `Arc<dyn Cas>` too.
#[test]
fn streams_by_default() {
    let store = MemoryCas::default().into_dyn();
    let cid = Blake3::of(b"hello");
    // Only `len` bytes are read.
    assert_eq!(
        store.put_from(cid, &mut &b"hello world"[..], 5).unwrap(),
        cid.into()
    );
    let mut out = Vec::new();
    assert_eq!(store.get_to(&cid.into(), &mut out).unwrap(), Some(5));
    assert_eq!(out, b"hello");
    assert_eq!(store.get_to(&raw(b"missing"), &mut out).unwrap(), None);
    let other = Blake3::of(b"other");
    let error = store.put_from(other, &mut &b"hello"[..], 5).unwrap_err();
    assert!(matches!(error, Error::Check { .. }), "{error}");
    let error = store.put_from(other, &mut &b"oth"[..], 5).unwrap_err();
    assert!(matches!(error, Error::Io { .. }), "{error}");
    assert!(!store.has(&other.into()).unwrap());
}
