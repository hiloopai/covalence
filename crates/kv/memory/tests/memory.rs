use std::{collections::BTreeMap, sync::Arc};

use covalence_kv_memory::MemoryKv;
use covalence_kv_trait::{Error, Kv};
use covalence_lib_dasl::Value;
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
