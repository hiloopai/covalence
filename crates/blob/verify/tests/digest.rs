use std::io::{self, Cursor, Read};

use covalence_blob_verify::{
    DigestAccumulator, DigestAlgorithms, DigestError, DigestResult, HashFn,
};
use covalence_lib_cid::Blake3;

#[test]
fn digest_types_are_send_sync_and_accumulator_clones_and_formats() {
    fn assert_send_sync<T: Send + Sync + 'static>() {}
    assert_send_sync::<DigestAlgorithms>();
    assert_send_sync::<DigestResult>();
    assert_send_sync::<DigestError>();
    assert_send_sync::<DigestAccumulator>();

    let mut base = DigestAccumulator::new(DigestAlgorithms::Both, Some(5));
    base.update(b"he").unwrap();
    let _ = format!("{base:?}");

    let mut left = base.clone();
    let mut right = base;
    left.update(b"llo").unwrap();
    right.update(b"lp!").unwrap();
    assert_eq!(
        left.finish().unwrap().raw_blake3(),
        Some(Blake3::of(b"hello"))
    );
    assert_eq!(
        right.finish().unwrap().raw_blake3(),
        Some(Blake3::of(b"help!"))
    );

    let mut poisoned = DigestAccumulator::new(DigestAlgorithms::Blake3, Some(1));
    let error = DigestError::TooLong { max_len: 1 };
    assert_eq!(poisoned.update(b"ab"), Err(error));
    assert_eq!(poisoned.clone().finish(), Err(error));
}

#[test]
fn requested_digests_match_one_shot_at_chunk_boundaries() {
    let bytes: Vec<_> = (0..140_001).map(|index| (index % 251) as u8).collect();
    let sha256 = HashFn::Sha256.digest(&bytes);
    let blake3 = HashFn::Blake3.digest(&bytes);
    for (algorithms, want_sha256, want_blake3) in [
        (DigestAlgorithms::Both, Some(&sha256), Some(&blake3)),
        (DigestAlgorithms::Sha256, Some(&sha256), None),
        (DigestAlgorithms::Blake3, None, Some(&blake3)),
    ] {
        let mut accumulator = DigestAccumulator::new(algorithms, Some(bytes.len() as u64));
        accumulator.update(&[]).unwrap();
        for chunk in bytes.chunks(1023) {
            accumulator.update(chunk).unwrap();
        }
        let result = accumulator.finish().unwrap();
        assert_eq!(result.len(), bytes.len() as u64);
        assert_eq!(result.sha256(), want_sha256);
        assert_eq!(result.blake3(), want_blake3);
        assert_eq!(result.raw_blake3(), want_blake3.map(|_| Blake3::of(&bytes)));
    }
}

#[test]
fn reader_includes_prefix_and_starts_at_current_position() {
    let mut accumulator = DigestAccumulator::new(DigestAlgorithms::Both, Some(5));
    accumulator.update(b"he").unwrap();
    let mut reader = Cursor::new(b"unusedllo");
    reader.set_position(6);
    let result = accumulator.read(&mut reader).unwrap();
    assert_eq!(result.len(), 5);
    assert_eq!(result.sha256(), Some(&HashFn::Sha256.digest(b"hello")));
    assert_eq!(result.raw_blake3(), Some(Blake3::of(b"hello")));
    assert_eq!(reader.position(), 9);
}

#[test]
fn reader_checks_eof_with_at_most_one_byte_past_the_limit() {
    let empty = DigestAccumulator::new(DigestAlgorithms::Both, Some(0))
        .read(io::empty())
        .unwrap();
    assert!(empty.is_empty());
    assert_eq!(empty.raw_blake3(), Some(Blake3::of(b"")));
    for max_len in [0, 2] {
        let mut source: &[u8] = b"abcdef";
        let error = DigestAccumulator::new(DigestAlgorithms::Both, Some(max_len))
            .read(&mut source)
            .unwrap_err();
        assert_eq!(source, &b"abcdef"[max_len as usize + 1..]);
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            DigestError::of(&error),
            Some(&DigestError::TooLong { max_len })
        );
    }
}

#[test]
fn reader_retries_interrupted_and_preserves_other_errors() {
    let mut calls = 0;
    let reader = io::repeat(b'a').take(1).chain(on_read(|_| {
        calls += 1;
        Err(match calls {
            1 => io::ErrorKind::Interrupted.into(),
            _ => io::Error::new(io::ErrorKind::BrokenPipe, "reader failed"),
        })
    }));
    let error = DigestAccumulator::new(DigestAlgorithms::Both, Some(1))
        .read(reader)
        .unwrap_err();
    assert_eq!(calls, 2);
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(error.to_string(), "reader failed");
    assert_eq!(DigestError::of(&error), None);
}

fn on_read(callback: impl FnMut(&mut [u8]) -> io::Result<usize>) -> impl Read {
    struct Reader<F>(F);
    impl<F: FnMut(&mut [u8]) -> io::Result<usize>> Read for Reader<F> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            (self.0)(buffer)
        }
    }
    Reader(callback)
}

#[test]
fn reader_rejects_over_reporting_and_skips_reading_when_already_poisoned() {
    let over_reporting = on_read(|buffer| Ok(buffer.len() + 1));
    let error = DigestAccumulator::new(DigestAlgorithms::Both, Some(1))
        .read(over_reporting)
        .unwrap_err();
    assert_eq!(DigestError::of(&error), None);
    assert!(error.to_string().contains("more than it was asked"));

    let mut poisoned = DigestAccumulator::new(DigestAlgorithms::Both, Some(1));
    assert_eq!(
        poisoned.update(b"ab"),
        Err(DigestError::TooLong { max_len: 1 })
    );
    let mut calls = 0;
    let error = poisoned
        .read(on_read(|_| {
            calls += 1;
            Ok(0)
        }))
        .unwrap_err();
    assert_eq!(calls, 0);
    assert_eq!(
        DigestError::of(&error),
        Some(&DigestError::TooLong { max_len: 1 })
    );
}

#[cfg(feature = "tokio")]
#[tokio::test]
async fn async_reader_waits_for_eof_and_probes_past_the_limit() {
    use tokio::io::AsyncWriteExt as _;

    let (reader, mut writer) = tokio::io::duplex(2);
    let writer = tokio::spawn(async move {
        writer.write_all(b"hello").await.unwrap();
        writer.shutdown().await.unwrap();
    });
    let exact = DigestAccumulator::new(DigestAlgorithms::Both, Some(5))
        .read_async(reader)
        .await
        .unwrap();
    writer.await.unwrap();
    assert_eq!(exact.len(), 5);
    assert_eq!(exact.raw_blake3(), Some(Blake3::of(b"hello")));

    let mut source: &[u8] = b"abcdef";
    let error = DigestAccumulator::new(DigestAlgorithms::Both, Some(2))
        .read_async(&mut source)
        .await
        .unwrap_err();
    assert_eq!(source, b"def");
    assert_eq!(
        DigestError::of(&error),
        Some(&DigestError::TooLong { max_len: 2 })
    );

    let mut poisoned = DigestAccumulator::new(DigestAlgorithms::Both, Some(1));
    assert_eq!(
        poisoned.update(b"ab"),
        Err(DigestError::TooLong { max_len: 1 })
    );
    let mut untouched: &[u8] = b"xy";
    let error = poisoned.read_async(&mut untouched).await.unwrap_err();
    assert_eq!(untouched, b"xy");
    assert_eq!(
        DigestError::of(&error),
        Some(&DigestError::TooLong { max_len: 1 })
    );
}
