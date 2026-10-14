use covalence_blob_trait::Blob;
use covalence_lib_data::Bytes;

/// The first `len` bytes of the test pattern.
pub fn pattern(len: usize) -> Bytes {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// Check the range contract on `blob`, which holds `data`.
pub fn check_ranges(blob: &dyn Blob, data: &[u8]) {
    let size = data.len() as u64;
    assert_eq!(blob.size().unwrap(), size);
    assert_eq!(blob.read_all().unwrap(), data);
    for (start, len) in [
        (0, 0),
        (0, 3),
        (2, 5),
        (size.saturating_sub(2), 10),
        (size, 1),
        (size + 1, 1),
        (0, u64::MAX),
        (1, u64::MAX),
        (u64::MAX, 1),
        (u64::MAX, u64::MAX),
    ] {
        let first = start.min(size) as usize;
        let last = start.saturating_add(len).min(size) as usize;
        assert_eq!(
            blob.read_range(start, len).unwrap(),
            &data[first..last],
            "range {start}+{len}"
        );
    }
}
