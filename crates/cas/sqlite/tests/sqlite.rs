use std::sync::Arc;

use covalence_cas_sqlite::{Sha256Policy, SqliteCas};
use covalence_cas_trait::{AsyncGet, AsyncGetExt, AsyncPut, Cas, Error, Get, GetExt, Op, Put, map};
use covalence_lib_cid::{Blake3, Cid, Fact, Format, O256, Packed32, Sha256};
use covalence_lib_dasl::Value;
use covalence_lib_sqlite::Connection;
use pollster::block_on;

fn raw(bytes: &[u8]) -> Cid {
    Blake3::of(bytes).into()
}

#[test]
fn facts_claim_the_key() -> Result<(), Error> {
    let store = SqliteCas::open_in_memory()?.into_dyn();
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
    let store = SqliteCas::open_in_memory()?;
    let cid: Cid = store.put("hello".into())?;
    assert_eq!(cid, raw(b"hello"));
    let again: Blake3 = store.put("hello".into())?;
    assert_eq!(Cid::from(again), cid);

    assert_eq!(store.get(&cid)?.unwrap(), "hello");
    assert_eq!(store.get(&again)?.unwrap(), "hello");
    assert_eq!(store.get_unchecked(&cid)?.unwrap(), "hello");
    let fact = Fact::new(again, "hello".into())?;
    assert_eq!(store.get_fact(&again)?, Some(fact));
    assert_eq!(store.get(&raw(b"missing"))?, None);
    assert_eq!(store.get_fact(&raw(b"missing"))?, None);
    Ok(())
}

#[test]
fn keyed_by_blake3_digest() -> Result<(), Error> {
    let store = SqliteCas::open_in_memory()?;
    let cid: Cid = store.put("hello".into())?;
    let drisl: Cid = Cid::new(Format::DRISL_BLAKE3, cid.digest()).unwrap();
    assert_eq!(store.get(&drisl)?.unwrap(), "hello");
    assert_eq!(store.get_fact(&drisl)?.unwrap().claim(), &drisl);
    let sha256: Cid = Cid::new(Format::SHA2_256, &[0; 32]).unwrap();
    assert_eq!(store.get(&sha256)?, None);
    Ok(())
}

#[test]
fn keeps_data_in_a_file() -> Result<(), Error> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cas.db");
    let cid: Cid = SqliteCas::open(&path)?.put("kept".into())?;
    assert_eq!(SqliteCas::open(&path)?.get(&cid)?.unwrap(), "kept");
    Ok(())
}

#[test]
fn checks_corrupt_rows() -> Result<(), Error> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cas.db");
    let store = SqliteCas::open(&path)?;
    let cid: Cid = store.put("hello".into())?;
    Connection::open(&path)
        .unwrap()
        .execute("UPDATE blobs SET data = CAST('corrupt' AS BLOB)", [])
        .unwrap();
    assert_eq!(store.get_unchecked(&cid)?.unwrap(), "corrupt");
    assert_eq!(store.size(&cid)?, Some(7));
    assert_eq!(store.get_range(&cid, 1, 3)?.unwrap(), "orr");
    assert_eq!(block_on(store.asize(&cid))?, Some(7));
    assert_eq!(block_on(store.aget_range(&cid, 1, 3))?.unwrap(), "orr");
    assert!(store.has(&cid)?);
    assert!(store.missing(&[cid])?.is_empty());
    assert!(matches!(store.get(&cid), Err(Error::Check { .. })));
    assert!(matches!(store.get_fact(&cid), Err(Error::Check { .. })));
    assert!(matches!(
        block_on(store.aget(&cid)),
        Err(Error::Check { .. })
    ));
    Ok(())
}

#[test]
fn unknown_storage_is_an_error() -> Result<(), Error> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cas.db");
    let store = SqliteCas::open(&path)?;
    let cid: Cid = store.put("hello".into())?;
    Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE blobs SET storage = 's3://bucket/key', data = NULL",
            [],
        )
        .unwrap();
    assert!(matches!(
        store.get_unchecked(&cid),
        Err(Error::Backend { .. })
    ));
    assert!(matches!(store.get(&cid), Err(Error::Backend { .. })));
    assert!(matches!(store.size(&cid), Err(Error::Backend { .. })));
    assert!(matches!(
        store.get_range(&cid, 0, 1),
        Err(Error::Backend { .. })
    ));
    assert!(matches!(
        block_on(store.asize(&cid)),
        Err(Error::Backend { .. })
    ));
    assert!(matches!(
        block_on(store.aget_range(&cid, 0, 1)),
        Err(Error::Backend { .. })
    ));
    assert!(matches!(store.has(&cid), Err(Error::Backend { .. })));
    assert!(matches!(store.missing(&[cid]), Err(Error::Backend { .. })));
    Ok(())
}

#[test]
fn sizes_and_ranges() -> Result<(), Error> {
    let store = SqliteCas::open_in_memory()?;
    let sha256: Sha256 = store.put("hello".into())?;
    let cid = raw(b"hello");
    assert_eq!(store.size(&sha256)?, Some(5));
    assert_eq!(
        block_on(AsyncGet::<Sha256>::asize(&store, &sha256))?,
        Some(5)
    );
    assert_eq!(store.get_range(&sha256, 1, 3)?.unwrap(), "ell");
    assert_eq!(
        block_on(AsyncGet::<Sha256>::aget_range(&store, &sha256, 1, 3))?.unwrap(),
        "ell"
    );
    let store = store.into_dyn();
    for cid in [cid, sha256.into()] {
        assert_eq!(store.size(&cid)?, Some(5));
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
    let missing = raw(b"missing");
    assert_eq!(store.size(&missing)?, None);
    assert_eq!(store.get_range(&missing, u64::MAX, 1)?, None);
    assert_eq!(store.get_range(&missing, 0, 0)?, None);
    let empty = store.put("".into())?;
    assert_eq!(store.size(&empty)?, Some(0));
    assert!(store.get_range(&empty, 0, 1)?.unwrap().is_empty());
    Ok(())
}

/// Ranges of a blob spanning many overflow pages match slices of it.
#[test]
fn ranges_of_large_blobs() -> Result<(), Error> {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteCas::open(dir.path().join("cas.db"))?;
    let data: Vec<u8> = (0..1 << 20).map(|i: u32| (i % 251) as u8).collect();
    let cid: Cid = store.put(data.clone().into())?;
    assert_eq!(store.size(&cid)?, Some(1 << 20));
    for (start, len) in [
        (0, 1),
        (4095, 2),
        (65_536, 100_000),
        ((1 << 20) - 3, 10),
        (0, 1 << 20),
    ] {
        let first = start as usize;
        let last = (first + len as usize).min(data.len());
        assert_eq!(
            store.get_range(&cid, start, len)?.unwrap(),
            data[first..last]
        );
    }
    Ok(())
}

fn shared_across_threads(store: SqliteCas) {
    std::thread::scope(|scope| {
        for i in 0..100u8 {
            let store = &store;
            scope.spawn(move || {
                Put::<Blake3>::put(store, vec![i % 50].into()).unwrap();
                assert_eq!(store.get(&raw(&[i % 50])).unwrap().unwrap(), vec![i % 50]);
            });
        }
    });
    for i in 0..50u8 {
        assert_eq!(store.get(&raw(&[i])).unwrap().unwrap(), vec![i]);
    }
    let stats = map([
        ("blocks", Value::Integer(50)),
        ("bytes", Value::Integer(50)),
    ]);
    assert_eq!(store.stats().unwrap(), stats);
}

#[test]
fn memory_shared_across_threads() {
    shared_across_threads(SqliteCas::open_in_memory().unwrap());
}

#[test]
fn file_shared_across_threads() {
    let dir = tempfile::tempdir().unwrap();
    shared_across_threads(SqliteCas::open(dir.path().join("cas.db")).unwrap());
}

#[test]
fn memory_stores_are_separate() -> Result<(), Error> {
    let a = SqliteCas::open_in_memory()?;
    let cid: Cid = a.put("a".into())?;
    assert_eq!(SqliteCas::open_in_memory()?.get(&cid)?, None);
    assert_eq!(a.get(&cid)?.unwrap(), "a");
    Ok(())
}

#[test]
fn works_as_dyn_cas() -> Result<(), Error> {
    let store: Arc<dyn Cas> = Arc::new(SqliteCas::open_in_memory()?);
    let a = store.put("a".into())?;
    let b = block_on(store.aput("b".into()))?;
    assert_eq!(block_on(store.aget(&a))?.unwrap(), "a");
    assert_eq!(store.get(&b)?.unwrap(), "b");
    let stats = map([("blocks", Value::Integer(2)), ("bytes", Value::Integer(2))]);
    assert_eq!(store.stats()?, stats);
    Ok(())
}

#[test]
fn id_is_unset_by_default() -> Result<(), Error> {
    let store = SqliteCas::open_in_memory()?;
    assert_eq!(store.id()?, None);
    let info = map([
        ("name", Value::String("sqlite".into())),
        ("id", Value::Null),
    ]);
    assert_eq!(store.info()?, info);
    Ok(())
}

#[test]
fn id_can_be_set_and_replaced() -> Result<(), Error> {
    let store = SqliteCas::open_in_memory()?;
    let mut rng = covalence_lib_rand::rng();
    let (a, b) = (O256::random(&mut rng), O256::random(&mut rng));
    store.set_id(a)?;
    assert_eq!(store.id()?, Some(a));
    store.set_id(b)?;
    assert_eq!(store.id()?, Some(b));
    let info = map([
        ("name", Value::String("sqlite".into())),
        ("id", Value::String(b.to_string())),
    ]);
    assert_eq!(store.info()?, info);
    Ok(())
}

#[test]
fn id_follows_the_file() -> Result<(), Error> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cas.db");
    let id = O256::random(&mut covalence_lib_rand::rng());
    SqliteCas::open(&path)?.set_id(id)?;
    assert_eq!(SqliteCas::open(&path)?.id()?, Some(id));
    assert_eq!(SqliteCas::open(dir.path().join("other.db"))?.id()?, None);
    Ok(())
}

const POLICIES: [Sha256Policy; 3] = [
    Sha256Policy::Always,
    Sha256Policy::OnDemand,
    Sha256Policy::Never,
];

fn blocks(store: &SqliteCas) -> Value {
    let Value::Map(stats) = store.stats().unwrap() else {
        panic!("stats are a map")
    };
    stats["blocks"].clone()
}

#[test]
fn blake3_puts_index_sha256_only_always() -> Result<(), Error> {
    for policy in POLICIES {
        let store = SqliteCas::open_in_memory_with(policy)?;
        let _: Blake3 = store.put("hello".into())?;
        let sha256 = Sha256::of(b"hello");
        let found = store.get(&sha256)?;
        assert_eq!(
            found.is_some(),
            policy == Sha256Policy::Always,
            "{policy:?}"
        );
        assert_eq!(store.get(&Cid::<Packed32>::from(sha256))?, found);
    }
    Ok(())
}

#[test]
fn sha256_puts_index_both() -> Result<(), Error> {
    for policy in [Sha256Policy::Always, Sha256Policy::OnDemand] {
        let store = SqliteCas::open_in_memory_with(policy)?;
        let sha256: Sha256 = store.put("hello".into())?;
        assert_eq!(sha256, Sha256::of(b"hello"));
        assert_eq!(store.get(&sha256)?.unwrap(), "hello");
        assert_eq!(store.get(&raw(b"hello"))?.unwrap(), "hello");
        let drisl: Cid = Cid::new(Format::DRISL_SHA256, sha256.digest()).unwrap();
        assert_eq!(store.get(&drisl)?.unwrap(), "hello");
        assert_eq!(store.get_fact(&drisl)?.unwrap().claim(), &drisl);
        assert_eq!(store.get(&Sha256::of(b"missing"))?, None);
    }
    Ok(())
}

#[test]
fn hashes_share_one_blob() -> Result<(), Error> {
    for policy in [Sha256Policy::Always, Sha256Policy::OnDemand] {
        let store = SqliteCas::open_in_memory_with(policy)?;
        let _: Blake3 = store.put("hello".into())?;
        let sha256: Sha256 = store.put("hello".into())?;
        let _: Blake3 = store.put("hello".into())?;
        let _: Sha256 = store.put("hello".into())?;
        assert_eq!(blocks(&store), Value::Integer(1));
        assert_eq!(store.get(&sha256)?.unwrap(), "hello");
    }
    Ok(())
}

#[test]
fn never_rejects_sha256_puts_until_indexed() -> Result<(), Error> {
    let store = SqliteCas::open_in_memory_with(Sha256Policy::Never)?;
    assert!(matches!(
        Put::<Sha256>::put(&store, "hello".into()),
        Err(Error::Unsupported { op: Op::Put })
    ));
    let _: Blake3 = store.put("hello".into())?;
    let _: Blake3 = store.put("world".into())?;
    assert_eq!(store.get(&Sha256::of(b"hello"))?, None);
    assert_eq!(store.index_sha256()?, 2);
    assert_eq!(store.index_sha256()?, 0);
    assert_eq!(store.get(&Sha256::of(b"hello"))?.unwrap(), "hello");
    Ok(())
}

#[test]
fn policy_is_recorded() -> Result<(), Error> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cas.db");
    assert_eq!(SqliteCas::open(&path)?.policy(), Sha256Policy::OnDemand);
    SqliteCas::open_with(&path, Sha256Policy::Always)?;
    let store = SqliteCas::open(&path)?;
    assert_eq!(store.policy(), Sha256Policy::Always);
    let _: Blake3 = store.put("hello".into())?;
    assert_eq!(store.get(&Sha256::of(b"hello"))?.unwrap(), "hello");
    Ok(())
}

#[test]
fn checks_corrupt_rows_under_both_hashes() -> Result<(), Error> {
    for policy in [Sha256Policy::Always, Sha256Policy::OnDemand] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cas.db");
        let store = SqliteCas::open_with(&path, policy)?;
        let sha256: Sha256 = store.put("hello".into())?;
        let blake3 = Blake3::of(b"hello");
        assert_eq!(store.verify()?, Vec::<i64>::new());
        Connection::open(&path)
            .unwrap()
            .execute("UPDATE blobs SET data = CAST('corrupt' AS BLOB)", [])
            .unwrap();
        assert_eq!(store.get_unchecked(&sha256)?.unwrap(), "corrupt");
        assert!(matches!(store.get(&sha256), Err(Error::Check { .. })));
        assert!(matches!(store.get(&blake3), Err(Error::Check { .. })));
        assert!(matches!(
            store.get(&Cid::<Packed32>::from(sha256)),
            Err(Error::Check { .. })
        ));
        assert!(matches!(
            block_on(AsyncGet::<Sha256>::aget(&store, &sha256)),
            Err(Error::Check { .. })
        ));
        assert_eq!(store.verify()?, vec![1]);
    }
    Ok(())
}

#[test]
fn checks_wrong_sha256_index_rows() -> Result<(), Error> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cas.db");
    let store = SqliteCas::open_with(&path, Sha256Policy::Always)?;
    let _: Blake3 = store.put("hello".into())?;
    let other = Sha256::of(b"other");
    Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE hashes SET digest = ?1 WHERE hashfn = 18",
            [other.digest()],
        )
        .unwrap();
    assert_eq!(store.get_unchecked(&other)?.unwrap(), "hello");
    assert!(matches!(store.get(&other), Err(Error::Check { .. })));
    assert!(matches!(store.get_fact(&other), Err(Error::Check { .. })));
    assert_eq!(store.get(&Blake3::of(b"hello"))?.unwrap(), "hello");
    assert_eq!(store.verify()?, vec![1]);
    Ok(())
}

#[test]
fn put_fact_matches_put() -> Result<(), Error> {
    let cas = SqliteCas::open_in_memory()?;
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
    let cas = SqliteCas::open_in_memory()?;
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
