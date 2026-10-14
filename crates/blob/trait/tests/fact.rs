mod common;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use common::{check_ranges, pattern};
use covalence_blob_trait::{Blob, BlobExt, Error, Slice};
use covalence_lib_cid::{Blake3, Cid, Fact, Format, Multicodec};
use covalence_lib_data::Bytes;

/// Every format whose digest lib-cid computes and `Cid` holds.
const FORMATS: [Format; 7] = [
    Format::BLAKE3,
    Format::SHA2_256,
    Format::SHA1,
    Format::DRISL_BLAKE3,
    Format::DRISL_SHA256,
    Format::GIT_SHA1,
    Format::GIT_SHA256,
];

/// The format of `codec` and `hash`.
fn format(codec: u64, hash: Multicodec) -> Format {
    let codec = Multicodec(codec);
    Format { codec, hash }
}

/// The CID of `data` in `format`.
fn cid_of(format: Format, data: &[u8]) -> Cid {
    Cid::new(format, &format.hash.digest(data).unwrap()).unwrap()
}

/// `data` as a fact for its raw BLAKE3 CID.
fn fact_of(data: &Bytes) -> Fact {
    Fact::<Blake3>::of(data.clone()).widen()
}

/// Whether `result` is a check error from lib-cid, such as for an unsupported hash.
fn is_check(result: &Result<Option<Fact>, Error>) -> bool {
    matches!(result, Err(Error::Check { .. }))
}

#[test]
fn facts_follow_the_range_contract() {
    for data in [pattern(10), pattern(1), Bytes::new()] {
        check_ranges(&fact_of(&data), &data);
    }
}

#[test]
fn fact_ranges_share_storage() {
    let data = pattern(10);
    let fact = fact_of(&data);
    assert_eq!(fact.read_range(2, 5).unwrap().as_ptr(), data[2..].as_ptr());
    assert_eq!(fact.read_all().unwrap().as_ptr(), data.as_ptr());
}

#[test]
fn every_supported_hash_checks_matching_bytes() {
    for data in [pattern(300), Bytes::new()] {
        for format in FORMATS {
            let cid = cid_of(format, &data);
            for fact in [
                data.fact_unchecked(&cid).unwrap().unwrap(),
                data.fact(&cid).unwrap().unwrap(),
            ] {
                assert_eq!((fact.claim(), fact.bytes()), (&cid, &data), "{format:?}");
            }
        }
    }
}

#[test]
fn every_supported_hash_rejects_other_bytes() {
    let data = pattern(300);
    let other = [pattern(299), pattern(301), Bytes::new(), Bytes::from("x")];
    for format in FORMATS {
        for other in &other {
            let cid = cid_of(format, other);
            assert!(data.fact_unchecked(&cid).unwrap().is_none(), "{format:?}");
            assert!(data.fact(&cid).unwrap().is_none(), "{format:?}");
        }
    }
}

#[test]
fn unsupported_hashes_are_errors() {
    let blake3 = cid_of(Format::BLAKE3, b"");
    let digest = blake3.digest();
    let unsupported = [
        Cid::new(format(0x55, Multicodec(0x1b)), digest).unwrap(),
        Cid::new(Format::BLAKE3, &digest[..16]).unwrap(),
        Cid::new(Format::SHA2_256, &digest[..20]).unwrap(),
    ];
    let blake3_fact = fact_of(&Bytes::new());
    for cid in unsupported {
        for data in [Bytes::new(), pattern(10)] {
            assert!(is_check(&data.fact_unchecked(&cid)), "{cid}");
            assert!(is_check(&data.fact(&cid)), "{cid}");
        }
        // A fact for another hash checks its bytes, and cannot here.
        assert!(is_check(&blake3_fact.fact(&cid)), "{cid}");
    }
}

/// A blob whose size, or else whose reads, fail.
struct Failing {
    size: bool,
}

impl Failing {
    fn error() -> Error {
        Error::Backend {
            source: "unreachable".into(),
        }
    }
}

impl Blob for Failing {
    fn size(&self) -> Result<u64, Error> {
        if self.size { Err(Self::error()) } else { Ok(0) }
    }
    fn read_range(&self, _start: u64, _len: u64) -> Result<Bytes, Error> {
        Err(Self::error())
    }
}

#[test]
fn read_failures_are_errors_not_none() {
    // A failing blob may claim to be empty, but a failed read is never `None`.
    for cid in [cid_of(Format::BLAKE3, b""), cid_of(Format::SHA2_256, b"x")] {
        for size in [false, true] {
            let failing = Failing { size };
            let is_backend = |result| matches!(result, Err(Error::Backend { .. }));
            assert!(is_backend(failing.fact_unchecked(&cid)));
            assert!(is_backend(failing.fact(&cid)));
            let shared: Arc<dyn Blob> = Arc::new(Failing { size });
            assert!(is_backend(shared.fact(&cid)));
        }
    }
}

/// Bytes that can be replaced, counting whole reads.
struct Mutable {
    bytes: Mutex<Bytes>,
    reads: AtomicUsize,
}

impl Mutable {
    fn new(bytes: Bytes) -> Self {
        let bytes = Mutex::new(bytes);
        let reads = AtomicUsize::new(0);
        Self { bytes, reads }
    }

    fn set(&self, bytes: Bytes) {
        *self.bytes.lock().unwrap() = bytes;
    }
}

impl Blob for Mutable {
    fn size(&self) -> Result<u64, Error> {
        self.bytes.lock().unwrap().size()
    }
    fn read_range(&self, start: u64, len: u64) -> Result<Bytes, Error> {
        self.bytes.lock().unwrap().read_range(start, len)
    }
    fn read_all(&self) -> Result<Bytes, Error> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.bytes.lock().unwrap().read_all()
    }
}

// This replaces the bytes a blob holds, not bytes it already returned:
// `facts_copy_what_they_read` covers storage that changes under them.
#[test]
fn facts_read_once_and_keep_their_bytes() {
    let original = pattern(100);
    let blob = Mutable::new(original.clone());
    let cid = cid_of(Format::BLAKE3, &original);
    let fact = blob.fact(&cid).unwrap().unwrap();
    assert_eq!(blob.reads.load(Ordering::Relaxed), 1);
    blob.set(pattern(99));
    assert_eq!(fact.bytes(), &original);
    assert_eq!(fact.read_all().unwrap(), original);
    assert!(blob.fact(&cid).unwrap().is_none());
    blob.set(original.clone());
    assert_eq!(blob.fact(&cid).unwrap().unwrap(), fact);
}

#[test]
fn facts_copy_what_they_read() {
    // Bytes may borrow storage that changes, such as a memory map.
    let data = pattern(100);
    let cid = cid_of(Format::BLAKE3, &data);
    let fact = data.fact(&cid).unwrap().unwrap();
    assert_eq!(fact.bytes(), &data);
    assert_ne!(fact.bytes().as_ptr(), data.as_ptr());
}

// That a fact also skips hashing its bytes again holds by construction: no
// test can tell hashing bytes that match from not hashing them.
#[test]
fn facts_prove_their_own_cid_from_their_own_bytes() {
    for data in [pattern(100), Bytes::new()] {
        let fact = fact_of(&data);
        let shared: Arc<dyn Blob> = Arc::new(fact.clone());
        for blob in [&fact as &dyn Blob, &shared] {
            for found in [
                blob.fact_unchecked(fact.claim()).unwrap().unwrap(),
                blob.fact(fact.claim()).unwrap().unwrap(),
            ] {
                assert_eq!(found, fact);
                // The default copies, so shared storage shows it was skipped.
                assert_eq!(found.bytes().as_ptr(), data.as_ptr());
            }
        }
    }
}

#[test]
fn facts_relabel_to_the_codec_asked_for() {
    for data in [pattern(100), Bytes::new()] {
        let raw = fact_of(&data);
        let drisl = cid_of(Format::DRISL_BLAKE3, &data);
        let unknown = cid_of(format(0x0129, Multicodec::BLAKE3), &data);
        for cid in [drisl, unknown] {
            for found in [raw.fact_unchecked(&cid), raw.fact(&cid)] {
                let found = found.unwrap().unwrap();
                assert_eq!((found.claim(), found.bytes()), (&cid, &data));
                assert_eq!(found.bytes().as_ptr(), data.as_ptr());
            }
            // Relabelling back gives the original fact.
            let relabelled = raw.fact(&cid).unwrap().unwrap();
            assert_eq!(relabelled.fact(raw.claim()).unwrap().unwrap(), raw);
        }
        // Plain bytes relabel too, from SHA-256.
        let dasl = cid_of(Format::DRISL_SHA256, &data);
        assert_eq!(data.fact(&dasl).unwrap().unwrap().claim(), &dasl);
    }
}

#[test]
fn facts_check_their_bytes_for_other_hashes() {
    for data in [pattern(100), Bytes::new()] {
        let fact = fact_of(&data);
        for format in FORMATS {
            let cid = cid_of(format, &data);
            let found = fact.fact(&cid).unwrap().unwrap();
            assert_eq!((found.claim(), found.bytes()), (&cid, &data));
            assert!(fact.fact(&cid_of(format, b"other")).unwrap().is_none());
        }
    }
}

/// A blob whose fact is fixed, and whose reads panic.
struct Fixed(Option<Fact>);

impl Blob for Fixed {
    fn size(&self) -> Result<u64, Error> {
        panic!("size is not read")
    }
    fn read_range(&self, _start: u64, _len: u64) -> Result<Bytes, Error> {
        panic!("ranges are not read")
    }
    fn read_all(&self) -> Result<Bytes, Error> {
        panic!("the blob is not read")
    }
    fn fact_unchecked(&self, _cid: &Cid) -> Result<Option<Fact>, Error> {
        Ok(self.0.clone())
    }
}

#[test]
fn facts_for_other_cids_are_rejected() {
    let fact = fact_of(&pattern(10));
    let liar = Fixed(Some(fact.clone()));
    let shared: Arc<dyn Blob> = Arc::new(Fixed(Some(fact.clone())));
    for blob in [&liar as &dyn Blob, &shared] {
        for format in FORMATS {
            let other = cid_of(format, &pattern(11));
            // The override is unchecked, so it may lie.
            assert_eq!(blob.fact_unchecked(&other).unwrap().unwrap(), fact);
            assert!(matches!(blob.fact(&other), Err(Error::Mismatch)));
        }
        // Another hash of the same bytes is still another CID to the check.
        let sha256 = cid_of(Format::SHA2_256, &pattern(10));
        assert!(matches!(blob.fact(&sha256), Err(Error::Mismatch)));
        assert_eq!(blob.fact(fact.claim()).unwrap().unwrap(), fact);
    }
}

#[test]
fn overrides_are_relabelled_to_the_cid_asked_for() {
    let data = pattern(10);
    let blob: Arc<dyn Blob> = Arc::new(Fixed(Some(fact_of(&data))));
    let drisl = cid_of(Format::DRISL_BLAKE3, &data);
    let found = blob.fact(&drisl).unwrap().unwrap();
    assert_eq!((found.claim(), found.bytes()), (&drisl, &data));
}

#[test]
fn overrides_returning_none_stay_none() {
    let blob = Fixed(None);
    let shared: Arc<dyn Blob> = Arc::new(Fixed(None));
    for format in FORMATS {
        let cid = cid_of(format, b"");
        assert!(blob.fact(&cid).unwrap().is_none());
        assert!(shared.fact(&cid).unwrap().is_none());
    }
}

#[test]
fn slices_of_facts_are_plain_blobs() {
    let data = pattern(100);
    let fact = fact_of(&data);
    let shared: Arc<dyn Blob> = Arc::new(fact.clone());
    let part = Slice::new(shared.clone(), 2, 5);
    assert!(part.fact(fact.claim()).unwrap().is_none());
    let own = cid_of(Format::BLAKE3, &data[2..7]);
    assert_eq!(part.fact(&own).unwrap().unwrap().bytes(), &data[2..7]);
    // A slice of the whole fact matches, but reads and hashes again.
    let whole = Slice::new(shared, 0, u64::MAX);
    let found = whole.fact(fact.claim()).unwrap().unwrap();
    assert_eq!(found, fact);
    assert_ne!(found.bytes().as_ptr(), data.as_ptr());
    assert!(
        Slice::new(Arc::new(fact.clone()), 0, 99)
            .fact(fact.claim())
            .unwrap()
            .is_none()
    );
}
