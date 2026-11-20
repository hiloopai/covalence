//! Native targets keep the `Send` and `Sync` bounds that `wasm32` drops: each
//! function below compiles only if its bound still implies them.
#![cfg(not(target_arch = "wasm32"))]

use covalence_cas_kv::KvCas;
use covalence_cas_trait::{AsyncGet, AsyncPut, Cas, CasFuture, Key};
use covalence_kv_memory::MemoryKv;
use covalence_kv_trait::{Kv, KvFuture};
use covalence_lib_cid::{Blake3, Cid};
use covalence_lib_data::{ByteStream, MaybeSend, MaybeSync};
use pollster::block_on;

fn send<T: Send + ?Sized>() {}
fn sync<T: Sync + ?Sized>() {}
fn send_sync<T: Send + Sync + ?Sized>() {}
fn maybe_send<T: MaybeSend + ?Sized>() {
    send::<T>()
}
fn maybe_sync<T: MaybeSync + ?Sized>() {
    sync::<T>()
}
fn kv<T: Kv + ?Sized>() {
    send_sync::<T>()
}
fn cas<T: Cas + ?Sized>() {
    send_sync::<T>()
}
fn async_get<T: AsyncGet + ?Sized>() {
    send_sync::<T>()
}
fn async_put<T: AsyncPut + ?Sized>() {
    send_sync::<T>()
}
fn key<K: Key>() {
    send_sync::<K>()
}
fn sent<T: Send>(value: T) -> T {
    value
}

#[test]
fn native_stores_futures_and_streams_are_send_and_sync() {
    maybe_send::<dyn Kv>();
    maybe_sync::<dyn Kv>();
    send::<ByteStream<'static>>();
    send::<KvFuture<'static, bool>>();
    send::<KvFuture<'static, Option<(u64, ByteStream<'static>)>>>();
    send::<CasFuture<'static, Cid>>();
    send::<CasFuture<'static, Option<(u64, ByteStream<'static>)>>>();
    kv::<dyn Kv>();
    kv::<MemoryKv>();
    cas::<dyn Cas>();
    cas::<KvCas>();
    async_get::<dyn Cas>();
    async_put::<dyn Cas>();
    key::<Cid>();

    let kv = MemoryKv::default().into_dyn();
    let store = KvCas::new(kv.clone());
    let cid = Blake3::of(b"hello");
    kv.put(b"key", "hello".into()).unwrap();
    let (len, stream) = block_on(sent(kv.aget_stream(b"key"))).unwrap().unwrap();
    let stored = block_on(sent(store.aput_from(cid, sent(stream), len)));
    assert_eq!(stored.unwrap(), cid.into());
    let (_, stream) = block_on(sent(store.aget_stream(&cid.into())))
        .unwrap()
        .unwrap();
    drop(sent(stream));
}
