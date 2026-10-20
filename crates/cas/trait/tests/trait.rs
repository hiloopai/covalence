use std::{
    collections::HashMap,
    sync::atomic::{AtomicUsize, Ordering},
    sync::{Arc, RwLock},
};

use covalence_cas_trait::{
    AsyncGet, AsyncGetExt, AsyncPut, Cas, CasFuture, Error, EvictionCandidate, EvictionPolicy,
    Flush, Get, GetExt, Put, ready,
};
use covalence_lib_cid::{Blake3, Cid, Fact, Format};
use covalence_lib_dasl::Value;
use covalence_lib_data::Bytes;
use pollster::block_on;

fn key(bytes: &[u8]) -> Cid {
    Blake3::of(bytes).into()
}

/// A store over a map, which may hold wrong bytes.
#[derive(Default)]
struct Map(RwLock<HashMap<Cid, Bytes>>);

impl Get for Map {
    fn get_unchecked(&self, key: &Cid) -> Result<Option<Bytes>, Error> {
        Ok(self.0.read().unwrap().get(key).cloned())
    }
}

impl AsyncGet for Map {
    fn aget_unchecked<'a>(&'a self, key: &'a Cid) -> CasFuture<'a, Option<Bytes>> {
        ready(self.get_unchecked(key))
    }
}

impl Put for Map {
    fn put(&self, bytes: Bytes) -> Result<Cid, Error> {
        let key = key(&bytes);
        self.0.write().unwrap().insert(key, bytes);
        Ok(key)
    }
}

impl AsyncPut for Map {
    fn aput(&self, bytes: Bytes) -> CasFuture<'_, Cid> {
        ready(self.put(bytes))
    }
}

impl Cas for Map {}

/// A store that returns the same fact for every key.
struct Liar(Fact);

impl Get for Liar {
    fn get_unchecked(&self, _: &Cid) -> Result<Option<Bytes>, Error> {
        Ok(Some(self.0.bytes().clone()))
    }
    fn get_fact_unchecked(&self, _: &Cid) -> Result<Option<Fact>, Error> {
        Ok(Some(self.0.clone()))
    }
}

impl AsyncGet for Liar {
    fn aget_unchecked<'a>(&'a self, key: &'a Cid) -> CasFuture<'a, Option<Bytes>> {
        ready(self.get_unchecked(key))
    }
    fn aget_fact_unchecked<'a>(&'a self, key: &'a Cid) -> CasFuture<'a, Option<Fact>> {
        ready(self.get_fact_unchecked(key))
    }
}

#[test]
fn dyn_cas_round_trips() -> Result<(), Error> {
    let store = Map::default().into_dyn();
    let cid = store.put("hello".into())?;
    assert_eq!(cid, key(b"hello"));
    assert_eq!(store.get(&cid)?.unwrap(), "hello");
    assert_eq!(store.get_fact(&cid)?.unwrap().bytes(), "hello");
    assert_eq!(store.get(&key(b"missing"))?, None);

    let cid = block_on(store.aput("world".into()))?;
    assert_eq!(block_on(store.aget(&cid))?.unwrap(), "world");
    assert_eq!(block_on(store.aget_fact(&cid))?.unwrap().bytes(), "world");
    assert_eq!(block_on(store.aget(&key(b"missing")))?, None);
    Ok(())
}

#[test]
fn wrong_bytes_fail_facts() {
    let store = Map::default();
    let cid = key(b"hello");
    store.0.write().unwrap().insert(cid, "jello".into());
    assert_eq!(store.get_unchecked(&cid).unwrap().unwrap(), "jello");
    assert_eq!(store.get(&cid).unwrap().unwrap(), "jello");
    assert!(matches!(store.get_fact(&cid), Err(Error::Check { .. })));
    assert!(matches!(
        block_on(store.aget_fact(&cid)),
        Err(Error::Check { .. })
    ));
}

/// A store whose `get` checks.
struct Checked(Map);

impl Get for Checked {
    fn get_unchecked(&self, key: &Cid) -> Result<Option<Bytes>, Error> {
        self.0.get_unchecked(key)
    }
    fn get(&self, key: &Cid) -> Result<Option<Bytes>, Error> {
        Ok(self.get_fact(key)?.map(|fact| fact.bytes().clone()))
    }
}

#[test]
fn stores_choose_how_get_checks() {
    let store = Checked(Map::default());
    let cid = key(b"hello");
    store.0.0.write().unwrap().insert(cid, "jello".into());
    assert_eq!(store.get_unchecked(&cid).unwrap().unwrap(), "jello");
    assert!(matches!(store.get(&cid), Err(Error::Check { .. })));
}

#[test]
fn facts_for_other_keys_are_caught() {
    let fact = Fact::new(key(b"other"), "other".into()).unwrap();
    let store = Liar(fact);
    let cid = key(b"hello");
    assert!(matches!(store.get_fact(&cid), Err(Error::Mismatch)));
    assert!(matches!(
        block_on(store.aget_fact(&cid)),
        Err(Error::Mismatch)
    ));
}

#[test]
fn facts_prove_any_codec() -> Result<(), Error> {
    let raw = key(b"hello");
    let drisl = Cid::new(Format::DRISL_BLAKE3, raw.digest()).unwrap();
    let store = Liar(Fact::new(raw, "hello".into())?);
    assert_eq!(store.get_fact(&drisl)?.unwrap().claim(), &drisl);
    assert_eq!(block_on(store.aget_fact(&drisl))?.unwrap().claim(), &drisl);
    assert_eq!(store.get_fact(&raw)?.unwrap().claim(), &raw);
    assert_eq!(block_on(store.aget_fact(&raw))?.unwrap().claim(), &raw);
    assert!(Value::from_fact(&store.get_fact(&drisl)?.unwrap()).is_err());
    Ok(())
}

#[test]
fn equal_digest_bytes_with_another_hash_do_not_prove() {
    let raw = key(b"hello");
    let other_hash = Cid::new(Format::SHA2_256, raw.digest()).unwrap();
    let store = Liar(Fact::new(raw, "hello".into()).unwrap());
    assert!(matches!(store.get_fact(&other_hash), Err(Error::Mismatch)));
    assert!(matches!(
        block_on(store.aget_fact(&other_hash)),
        Err(Error::Mismatch)
    ));
}

/// A store answering both BLAKE3 and general CIDs.
struct Both(Map);

impl Get<Blake3> for Both {
    fn get_unchecked(&self, key: &Blake3) -> Result<Option<Bytes>, Error> {
        self.0.get_unchecked(&(*key).into())
    }
}

impl Get for Both {
    fn get_unchecked(&self, key: &Cid) -> Result<Option<Bytes>, Error> {
        self.0.get_unchecked(key)
    }
}

#[test]
fn reads_dispatch_on_key_type() -> Result<(), Error> {
    let store = Both(Map::default());
    let cid = store.0.put("hello".into())?;
    let blake3 = Blake3::of(b"hello");
    assert_eq!(store.get(&blake3)?.unwrap(), "hello");
    assert_eq!(store.get(&cid)?.unwrap(), "hello");
    assert_eq!(store.get_fact(&blake3)?.unwrap().claim(), &blake3);
    Ok(())
}

#[test]
fn info_and_stats_default_to_null() {
    let store = Map::default();
    assert_eq!(store.info().unwrap(), Value::Null);
    assert_eq!(store.stats().unwrap(), Value::Null);
}

#[test]
fn errors_are_small() {
    assert!(size_of::<Error>() <= 24);
}

/// Sizes and ranges fetch and slice by default, and the vouched forms use
/// the unchecked ones.
#[test]
fn ranges_slice_by_default() -> Result<(), Error> {
    let store = Map::default().into_dyn();
    let cid = store.put("hello".into())?;
    let missing = key(b"missing");
    assert_eq!(store.size(&cid)?, Some(5));
    assert_eq!(block_on(store.asize(&cid))?, Some(5));
    assert_eq!(store.size(&missing)?, None);
    for (start, len, want) in [
        (1, 3, "ell"),
        (4, 10, "o"),
        (5, 1, ""),
        (9, 1, ""),
        (1, u64::MAX, "ello"),
    ] {
        assert_eq!(store.get_range(&cid, start, len)?.unwrap(), want);
        assert_eq!(block_on(store.aget_range(&cid, start, len))?.unwrap(), want);
    }
    assert_eq!(store.get_range(&missing, 0, 1)?, None);
    assert_eq!(store.get_range(&missing, 0, 0)?, None);
    Ok(())
}

/// A store with native sizes and ranges, which never fetches whole values.
struct Native;

impl Get for Native {
    fn get_unchecked(&self, _: &Cid) -> Result<Option<Bytes>, Error> {
        panic!("must not fetch the whole value");
    }
    fn size_unchecked(&self, _: &Cid) -> Result<Option<u64>, Error> {
        Ok(Some(7))
    }
    fn get_range_unchecked(&self, _: &Cid, start: u64, _: u64) -> Result<Option<Bytes>, Error> {
        Ok(Some(format!("sync {start}").into()))
    }
}

impl AsyncGet for Native {
    fn aget_unchecked<'a>(&'a self, _: &'a Cid) -> CasFuture<'a, Option<Bytes>> {
        panic!("must not fetch the whole value");
    }
    fn asize_unchecked<'a>(&'a self, _: &'a Cid) -> CasFuture<'a, Option<u64>> {
        ready(Ok(Some(8)))
    }
    fn aget_range_unchecked<'a>(
        &'a self,
        _: &'a Cid,
        start: u64,
        _: u64,
    ) -> CasFuture<'a, Option<Bytes>> {
        ready(Ok(Some(format!("async {start}").into())))
    }
}

impl Put for Native {
    fn put(&self, _: Bytes) -> Result<Cid, Error> {
        unimplemented!()
    }
}

impl AsyncPut for Native {
    fn aput(&self, _: Bytes) -> CasFuture<'_, Cid> {
        unimplemented!()
    }
}

impl Cas for Native {}

/// The vouched forms use native unchecked reads, also through `Arc<dyn Cas>`.
#[test]
fn native_ranges_are_used_and_forwarded() -> Result<(), Error> {
    let store = Native.into_dyn();
    let key = key(b"anything");
    for size in [store.size(&key)?, store.size_unchecked(&key)?] {
        assert_eq!(size, Some(7));
    }
    for size in [
        block_on(store.asize(&key))?,
        block_on(store.asize_unchecked(&key))?,
    ] {
        assert_eq!(size, Some(8));
    }
    assert_eq!(store.get_range(&key, 1, 1)?.unwrap(), "sync 1");
    assert_eq!(store.get_range_unchecked(&key, 2, 1)?.unwrap(), "sync 2");
    assert_eq!(block_on(store.aget_range(&key, 3, 1))?.unwrap(), "async 3");
    assert_eq!(
        block_on(store.aget_range_unchecked(&key, 4, 1))?.unwrap(),
        "async 4"
    );
    Ok(())
}

#[test]
fn shared_stores_are_not_rewrapped() {
    fn shared(store: impl Cas + 'static) -> Arc<dyn Cas> {
        store.into_dyn()
    }
    let store = Map::default().into_dyn();
    assert!(Arc::ptr_eq(&shared(store.clone()), &store));
    let cid = shared(store.clone()).put("hello".into()).unwrap();
    assert_eq!(store.get(&cid).unwrap().unwrap(), "hello");
}

#[test]
fn put_fact_puts_by_default() -> Result<(), Error> {
    let store = Map::default().into_dyn();
    let raw = key(b"hello");
    let drisl = Cid::new(Format::DRISL_BLAKE3, raw.digest()).unwrap();
    assert_eq!(store.put_fact(Fact::new(drisl, "hello".into())?)?, raw);
    let fact = Fact::new(key(b"world"), "world".into())?;
    assert_eq!(block_on(store.aput_fact(fact))?, key(b"world"));
    assert_eq!(store.get(&raw)?.unwrap(), "hello");
    Ok(())
}

/// A store that keeps facts under their claims, counting them.
#[derive(Default)]
struct Facts(Map, AtomicUsize);

impl Get for Facts {
    fn get_unchecked(&self, key: &Cid) -> Result<Option<Bytes>, Error> {
        self.0.get_unchecked(key)
    }
}

impl AsyncGet for Facts {
    fn aget_unchecked<'a>(&'a self, key: &'a Cid) -> CasFuture<'a, Option<Bytes>> {
        self.0.aget_unchecked(key)
    }
}

impl Put for Facts {
    fn put(&self, bytes: Bytes) -> Result<Cid, Error> {
        self.0.put(bytes)
    }
    fn put_fact(&self, fact: Fact) -> Result<Cid, Error> {
        self.1.fetch_add(1, Ordering::Relaxed);
        let key = *fact.claim();
        self.0.0.write().unwrap().insert(key, fact.bytes().clone());
        Ok(key)
    }
}

impl AsyncPut for Facts {
    fn aput(&self, bytes: Bytes) -> CasFuture<'_, Cid> {
        self.0.aput(bytes)
    }
    fn aput_fact(&self, fact: Fact) -> CasFuture<'_, Cid> {
        ready(self.put_fact(fact))
    }
}

impl Cas for Facts {}

#[test]
fn shared_stores_forward_put_fact() -> Result<(), Error> {
    let facts = Arc::new(Facts::default());
    let store: Arc<dyn Cas> = facts.clone();
    let fact = Fact::new(key(b"hello"), "hello".into())?;
    assert_eq!(store.put_fact(fact.clone())?, store.put("hello".into())?);
    assert_eq!(block_on(store.aput_fact(fact))?, key(b"hello"));
    assert_eq!(facts.1.load(Ordering::Relaxed), 2);
    Ok(())
}

/// By default `has` reads unchecked and `missing` keeps the keys it lacks,
/// in order.
#[test]
fn has_and_missing_default_to_unchecked_reads() {
    let store = Map::default();
    let one = store.put(Bytes::from_static(b"one")).unwrap();
    let wrong = key(b"two");
    store
        .0
        .write()
        .unwrap()
        .insert(wrong, Bytes::from_static(b"not two"));
    let [three, four] = [key(b"three"), key(b"four")];
    assert!(store.has(&one).unwrap() && store.has(&wrong).unwrap());
    assert!(!store.has(&three).unwrap());
    let store = store.into_dyn();
    let keys = [four, one, wrong, three];
    assert_eq!(store.missing(&keys).unwrap(), [four, three]);
    assert_eq!(store.missing(&[]).unwrap(), []);
}

/// A cache that distinguishes its sync and async policy dispatch.
#[derive(Default)]
struct Selecting {
    sync: AtomicUsize,
    asynchronous: AtomicUsize,
}

impl Get for Selecting {
    fn get_unchecked(&self, _: &Cid) -> Result<Option<Bytes>, Error> {
        Ok(None)
    }
}
impl Put for Selecting {
    fn put(&self, bytes: Bytes) -> Result<Cid, Error> {
        Ok(key(&bytes))
    }
}
impl AsyncGet for Selecting {
    fn aget_unchecked<'a>(&'a self, cid: &'a Cid) -> CasFuture<'a, Option<Bytes>> {
        ready(self.get_unchecked(cid))
    }
}
impl AsyncPut for Selecting {
    fn aput(&self, bytes: Bytes) -> CasFuture<'_, Cid> {
        ready(self.put(bytes))
    }
}
impl Selecting {
    fn select(flags: Flush, policy: Option<&dyn EvictionPolicy>) {
        if flags.contains(Flush::EVICT) {
            let candidate = EvictionCandidate::new(
                key(b"borrowed"),
                3,
                Some(std::time::SystemTime::UNIX_EPOCH),
            );
            assert!(policy.unwrap().should_evict(&candidate));
        }
    }
}
impl Cas for Selecting {
    fn flush(&self, flags: Flush, policy: Option<&dyn EvictionPolicy>) -> Result<(), Error> {
        self.sync.fetch_add(1, Ordering::Relaxed);
        Self::select(flags, policy);
        Ok(())
    }
    fn aflush<'a>(
        &'a self,
        flags: Flush,
        policy: Option<&'a dyn EvictionPolicy>,
    ) -> CasFuture<'a, ()> {
        Box::pin(async move {
            self.asynchronous.fetch_add(1, Ordering::Relaxed);
            Self::select(flags, policy);
            Ok(())
        })
    }
}

#[test]
fn flush_policies_borrow_closures_and_forward_async_dispatch_through_shared_stores() {
    let concrete = Arc::new(Selecting::default());
    let store: Arc<dyn Cas> = concrete.clone();
    let borrowed = String::from("borrowed");
    let calls = AtomicUsize::new(0);
    let policy = |candidate: &EvictionCandidate| {
        calls.fetch_add(1, Ordering::Relaxed);
        assert_eq!(candidate.stored_size, 3);
        assert_eq!(
            candidate.modified_at,
            Some(std::time::SystemTime::UNIX_EPOCH)
        );
        candidate.cid == key(borrowed.as_bytes())
    };
    store.flush(Flush::EVICT, Some(&policy)).unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    let future = store.aflush(Flush::EVICT, Some(&policy));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    block_on(future).unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(concrete.sync.load(Ordering::Relaxed), 1);
    assert_eq!(concrete.asynchronous.load(Ordering::Relaxed), 1);
    let unused = |_: &EvictionCandidate| panic!("a policy needs EVICT");
    let defaults = Map::default().into_dyn();
    defaults.flush(Flush::empty(), Some(&unused)).unwrap();
    block_on(defaults.aflush(Flush::empty(), Some(&unused))).unwrap();
}
