use std::{
    io::Read,
    task::{Context, Poll, Waker},
};

use covalence_kv_trait::{Error, Kv, KvFuture, Op};
use covalence_lib_data::Bytes;

/// Ordinary operations must never be used to imitate an atomic write.
struct Ordinary;

impl Kv for Ordinary {
    fn get(&self, _: &[u8]) -> Result<Option<Bytes>, Error> {
        panic!("atomic default must not read")
    }
    fn put(&self, _: &[u8], _: Bytes) -> Result<(), Error> {
        panic!("atomic default must not write")
    }
    fn aget<'a>(&'a self, _: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
        panic!("atomic default must not read asynchronously")
    }
    fn aput<'a>(&'a self, _: &'a [u8], _: Bytes) -> KvFuture<'a, ()> {
        panic!("atomic default must not write asynchronously")
    }
}

struct Unread;

impl Read for Unread {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        panic!("unsupported write must not consume its source")
    }
}

#[test]
fn atomic_defaults_are_unsupported_without_side_effects() {
    let store = Ordinary.into_dyn();
    assert!(matches!(
        store.put_new(b"key", "value".into()),
        Err(Error::Unsupported { op: Op::PutNew })
    ));
    assert!(matches!(
        store.put_new_from(b"key", &mut Unread, 5),
        Err(Error::Unsupported { op: Op::PutNew })
    ));
    let mut future = store.aput_new(b"key", "value".into());
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(
        future.as_mut().poll(&mut context),
        Poll::Ready(Err(Error::Unsupported { op: Op::PutNew }))
    ));
}
