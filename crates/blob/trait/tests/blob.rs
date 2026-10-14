mod common;

use std::sync::{Arc, Mutex};

use common::{check_ranges, pattern};
use covalence_blob_trait::{Blob, Error, Slice};
use covalence_lib_cid::{Blake3, Cid, Fact};
use covalence_lib_data::Bytes;

#[test]
fn bytes_follow_the_range_contract() {
    check_ranges(&pattern(10), &pattern(10));
    check_ranges(&Bytes::new(), &[]);
}

#[test]
fn bytes_ranges_share_storage() {
    let bytes = pattern(10);
    let range = bytes.read_range(2, 5).unwrap();
    assert_eq!(range.as_ptr(), bytes[2..].as_ptr());
    assert_eq!(bytes.read_all().unwrap().as_ptr(), bytes.as_ptr());
}

#[test]
fn slices_follow_the_range_contract() {
    let data = pattern(20);
    let blob: Arc<dyn Blob> = Arc::new(data.clone());
    check_ranges(&Slice::new(blob.clone(), 5, 10), &data[5..15]);
    check_ranges(&Slice::new(blob.clone(), 0, 0), &[]);
    check_ranges(&Slice::new(blob.clone(), 0, u64::MAX), &data);
    check_ranges(&Slice::new(Arc::new(Bytes::new()), 0, 5), &[]);
}

#[test]
fn slices_clip_to_the_blob() {
    let data = pattern(10);
    let blob: Arc<dyn Blob> = Arc::new(data.clone());
    check_ranges(&Slice::new(blob.clone(), 4, 100), &data[4..]);
    check_ranges(&Slice::new(blob.clone(), 10, 5), &[]);
    check_ranges(&Slice::new(blob.clone(), 20, 5), &[]);
    check_ranges(&Slice::new(blob.clone(), u64::MAX, u64::MAX), &[]);
    check_ranges(&Slice::new(blob, u64::MAX - 1, u64::MAX), &[]);
}

#[test]
fn slices_of_bytes_share_storage() {
    let bytes = pattern(10);
    let slice = Slice::new(Arc::new(bytes.clone()), 2, 5);
    assert_eq!(
        slice.read_range(1, 2).unwrap().as_ptr(),
        bytes[3..].as_ptr()
    );
    assert_eq!(slice.read_all().unwrap().as_ptr(), bytes[2..].as_ptr());
}

#[test]
fn slices_of_slices_compose() {
    let data = pattern(20);
    let blob: Arc<dyn Blob> = Arc::new(data.clone());
    let outer = Slice::new(blob, 5, 10);
    let inner = outer.slice(2, 5);
    check_ranges(&inner, &data[7..12]);
    check_ranges(&inner.slice(3, 100), &data[10..12]);
    check_ranges(&inner.slice(5, 1), &[]);
    check_ranges(&outer.slice(20, 1), &[]);
    check_ranges(&outer.slice(4, u64::MAX), &data[9..15]);
    check_ranges(&outer.slice(u64::MAX, u64::MAX), &[]);
}

#[test]
fn slices_of_slices_do_not_nest() {
    let data = pattern(20);
    let mut slice = Slice::new(Arc::new(data.clone()), 0, u64::MAX);
    // Nested slices would overflow the stack reading or dropping this.
    for _ in 0..1_000_000 {
        slice = slice.slice(0, u64::MAX);
    }
    check_ranges(&slice, &data);
}

/// A blob whose every method answers differently from the defaults.
struct Overrides;

impl Blob for Overrides {
    fn size(&self) -> Result<u64, Error> {
        Ok(7)
    }
    fn read_range(&self, _start: u64, _len: u64) -> Result<Bytes, Error> {
        Ok(Bytes::from_static(b"range"))
    }
    fn read_all(&self) -> Result<Bytes, Error> {
        Ok(Bytes::from_static(b"all"))
    }
    fn fact_unchecked(&self, _cid: &Cid) -> Result<Option<Fact>, Error> {
        Ok(Some(
            Fact::<Blake3>::of(Bytes::from_static(b"fact")).widen(),
        ))
    }
}

#[test]
fn shared_blobs_forward_overrides() {
    let blob: Arc<dyn Blob> = Arc::new(Overrides);
    let blob: &dyn Blob = &blob;
    assert_eq!(blob.size().unwrap(), 7);
    assert_eq!(blob.read_range(0, 1).unwrap(), "range");
    assert_eq!(blob.read_all().unwrap(), "all");
    let cid = Blake3::of(b"fact").into();
    let fact = blob.fact_unchecked(&cid).unwrap().unwrap();
    assert_eq!((fact.claim(), fact.bytes()), (&cid, &Bytes::from("fact")));
}

/// A blob whose bytes can be replaced, or removed so reads fail.
#[derive(Default)]
struct Mutable(Mutex<Option<Bytes>>);

impl Mutable {
    fn set(&self, bytes: Option<Bytes>) {
        *self.0.lock().unwrap() = bytes;
    }
    fn get(&self) -> Result<Bytes, Error> {
        let bytes = self.0.lock().unwrap().clone();
        bytes.ok_or_else(|| Error::Backend {
            source: "unavailable".into(),
        })
    }
}

impl Blob for Mutable {
    fn size(&self) -> Result<u64, Error> {
        self.get()?.size()
    }
    fn read_range(&self, start: u64, len: u64) -> Result<Bytes, Error> {
        self.get()?.read_range(start, len)
    }
}

#[test]
fn slices_follow_the_blob_size_when_read() {
    let data = pattern(20);
    let mutable = Arc::new(Mutable::default());
    mutable.set(Some(data.clone()));
    let slice = Slice::new(mutable.clone(), 5, 10);
    check_ranges(&slice, &data[5..15]);
    mutable.set(Some(data.slice(..8)));
    check_ranges(&slice, &data[5..8]);
    mutable.set(Some(data.slice(..3)));
    check_ranges(&slice, &[]);
    mutable.set(Some(data.clone()));
    check_ranges(&slice, &data[5..15]);
}

#[test]
fn slices_and_shared_blobs_pass_errors_through() {
    let shared: Arc<dyn Blob> = Arc::new(Mutable::default());
    let slice = Slice::new(shared.clone(), 0, 5);
    let blobs: [&dyn Blob; 3] = [&shared, &slice, &slice.slice(1, 2)];
    for blob in blobs {
        assert!(matches!(blob.size(), Err(Error::Backend { .. })));
        assert!(matches!(blob.read_range(0, 1), Err(Error::Backend { .. })));
        assert!(matches!(blob.read_all(), Err(Error::Backend { .. })));
    }
}
