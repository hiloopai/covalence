//! The blocking reader and writer: the check, the end of the stream,
//! stopping, and memory.

mod common;

use std::{
    fs,
    io::{self, Read, Write},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use common::{Generated, Trickle, pattern};
use covalence_blob_verify::{
    Cancel, Error, Expected, HashFn, Verifier, VerifyingReader, VerifyingWriter,
};

const HASHES: [HashFn; 2] = [HashFn::Sha256, HashFn::Blake3];

/// Read `reader` to its end or first error, keeping what it returned.
fn drain(reader: &mut impl Read) -> (Vec<u8>, io::Result<()>) {
    let mut out = Vec::new();
    let mut buf = [0; 7];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return (out, Ok(())),
            Ok(read) => out.extend_from_slice(&buf[..read]),
            Err(error) => return (out, Err(error)),
        }
    }
}

fn verification(result: io::Result<()>) -> Error {
    *Error::of(&result.unwrap_err()).expect("a verification error")
}

#[test]
fn valid_streams_pass_whole() {
    for hash in HASHES {
        for len in [0, 1, 2, 7, 8, 100] {
            let data = pattern(len);
            for step in [1, 3, 1000] {
                let source = Trickle {
                    inner: &data[..],
                    step,
                };
                let mut reader =
                    VerifyingReader::new(source, Expected::of(hash, &data)).require_eof();
                let (out, result) = drain(&mut reader);
                result.unwrap();
                assert_eq!(out, data);
                let verified = reader.verified().unwrap();
                assert_eq!(verified.expected(), &Expected::of(hash, &data));
                // The end is sticky.
                assert_eq!(reader.read(&mut [0; 4]).unwrap(), 0);
            }
        }
    }
}

#[test]
fn a_bad_digest_never_releases_the_last_byte() {
    for hash in HASHES {
        let data = pattern(50);
        let mut wrong = data.clone();
        *wrong.last_mut().unwrap() ^= 1;
        for step in [1, 4, 1000] {
            let source = Trickle {
                inner: &wrong[..],
                step,
            };
            let mut reader = VerifyingReader::new(source, Expected::of(hash, &data));
            let (out, result) = drain(&mut reader);
            assert_eq!(verification(result), Error::Mismatch { hash });
            assert_eq!(out, data[..49]);
            assert!(reader.verified().is_none());
            // Later reads fail the same way and return nothing.
            let error = reader.read(&mut [0; 4]).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(Error::of(&error), Some(&Error::Mismatch { hash }));
        }
    }
}

#[test]
fn read_exact_never_completes_unverified() {
    let data = pattern(64);
    let mut wrong = data.clone();
    wrong[0] ^= 1;
    let mut reader = VerifyingReader::new(&wrong[..], Expected::of(HashFn::Sha256, &data));
    let mut buf = vec![0; 64];
    let error = reader.read_exact(&mut buf).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn an_empty_stream_is_checked() {
    let empty = Expected::of(HashFn::Blake3, b"");
    let mut reader = VerifyingReader::new(&b""[..], empty).require_eof();
    assert_eq!(reader.read(&mut [0; 4]).unwrap(), 0);
    assert!(reader.verified().is_some());

    let wrong = Expected::new(0, HashFn::Blake3, [0; 32]);
    let mut reader = VerifyingReader::new(&b""[..], wrong);
    assert_eq!(
        verification(reader.read(&mut [0; 4]).map(drop)),
        Error::Mismatch {
            hash: HashFn::Blake3
        }
    );

    let mut reader = VerifyingReader::new(&b"x"[..], empty).require_eof();
    assert_eq!(
        verification(reader.read(&mut [0; 4]).map(drop)),
        Error::TooLong { expected: 0 }
    );
}

#[test]
fn a_truncated_stream_is_too_short() {
    let data = pattern(30);
    for step in [1, 1000] {
        let source = Trickle {
            inner: &data[..20],
            step,
        };
        let mut reader = VerifyingReader::new(source, Expected::of(HashFn::Sha256, &data));
        let (out, result) = drain(&mut reader);
        let error = result.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(
            Error::of(&error),
            Some(&Error::TooShort {
                expected: 30,
                received: 20
            })
        );
        assert_eq!(out, data[..20]);
    }
}

#[test]
fn trailing_bytes_are_left_unread_or_rejected() {
    let data = pattern(10);
    let mut long = data.clone();
    long.extend_from_slice(b"trailing");
    let expected = Expected::of(HashFn::Blake3, &data);

    // By default: exactly the expected bytes, and the rest stays in the source.
    let mut source = &long[..];
    let mut reader = VerifyingReader::new(&mut source, expected);
    let (out, result) = drain(&mut reader);
    result.unwrap();
    assert_eq!(out, data);
    assert_eq!(source, b"trailing");

    // With require_eof: rejected before the last byte is released.
    for step in [1, 1000] {
        let source = Trickle {
            inner: &long[..],
            step,
        };
        let mut reader = VerifyingReader::new(source, expected).require_eof();
        let (out, result) = drain(&mut reader);
        assert_eq!(verification(result), Error::TooLong { expected: 10 });
        assert_eq!(out, data[..9]);
    }
}

#[test]
fn cancel_stops_before_the_next_read() {
    let data = pattern(100);
    let cancel = Cancel::new();
    let mut reader = VerifyingReader::new(&data[..], Expected::of(HashFn::Sha256, &data))
        .with_cancel(cancel.clone());
    let mut buf = [0; 10];
    assert_eq!(reader.read(&mut buf).unwrap(), 10);
    cancel.clone().cancel();
    assert!(cancel.is_cancelled());
    let error = reader.read(&mut buf).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert_eq!(Error::of(&error), Some(&Error::Cancelled));
    assert_eq!(reader.verifier().received(), 10);
    assert_eq!(
        Error::of(&reader.read(&mut buf).unwrap_err()),
        Some(&Error::Cancelled)
    );
}

#[test]
fn a_passed_deadline_stops_reads_and_writes() {
    let data = pattern(10);
    let expected = Expected::of(HashFn::Sha256, &data);
    let past = Instant::now() - Duration::from_millis(1);

    let mut reader = VerifyingReader::new(&data[..], expected).with_deadline(past);
    let error = reader.read(&mut [0; 4]).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(Error::of(&error), Some(&Error::DeadlineExceeded));

    let mut writer = VerifyingWriter::new(Vec::new(), expected).with_deadline(past);
    let error = writer.write(&data).unwrap_err();
    assert_eq!(Error::of(&error), Some(&Error::DeadlineExceeded));
    assert!(writer.get_ref().is_empty());
    assert_eq!(writer.verified(), Err(Error::DeadlineExceeded));

    let future = Instant::now() + Duration::from_secs(3600);
    let mut reader = VerifyingReader::new(&data[..], expected).with_deadline(future);
    assert_eq!(drain(&mut reader).0, data);
}

#[test]
fn source_errors_pass_through_and_can_be_retried() {
    struct Flaky<'a> {
        data: &'a [u8],
        fail_next: bool,
    }
    impl Read for Flaky<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.fail_next = !self.fail_next;
            if !self.fail_next {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let read = self.data.len().min(buf.len()).min(3);
            buf[..read].copy_from_slice(&self.data[..read]);
            self.data = &self.data[read..];
            Ok(read)
        }
    }
    let data = pattern(20);
    let source = Flaky {
        data: &data,
        fail_next: true,
    };
    let mut reader =
        VerifyingReader::new(source, Expected::of(HashFn::Blake3, &data)).require_eof();
    let mut out = vec![0; 20];
    // read_exact retries interrupted reads.
    reader.read_exact(&mut out).unwrap();
    assert_eq!(out, data);
    assert_eq!(reader.read(&mut [0; 1]).unwrap(), 0);
    assert!(reader.verified().is_some());
}

#[test]
fn a_large_stream_is_read_in_step_with_its_consumer() {
    const LEN: u64 = 64 << 20;
    for hash in HASHES {
        let expected = Generated::expected(hash, LEN);
        let source = Generated::new(LEN);
        let produced = source.produced.clone();
        let mut reader = VerifyingReader::new(source, expected).require_eof();
        let mut buf = vec![0; 64 << 10];
        let mut consumed = 0_u64;
        loop {
            let read = reader.read(&mut buf).unwrap();
            if read == 0 {
                break;
            }
            for (i, value) in buf[..read].iter().enumerate().step_by(4093) {
                assert_eq!(*value, common::byte(consumed + i as u64));
            }
            consumed += read as u64;
            // Never ahead of the consumer by more than the withheld byte.
            let ahead = produced.load(Ordering::SeqCst) - consumed;
            assert!(ahead <= 1, "read {ahead} bytes ahead");
        }
        assert_eq!(consumed, LEN);
        assert!(reader.verified().is_some());
    }
    // The reader holds no buffer of its own: its size does not depend on the stream.
    assert!(size_of::<VerifyingReader<Generated>>() < 512);
}

#[test]
fn the_writer_refuses_bytes_past_the_size() {
    let data = pattern(10);
    let expected = Expected::of(HashFn::Sha256, &data);
    let mut writer = VerifyingWriter::new(Vec::new(), expected);
    writer.write_all(&data[..6]).unwrap();
    let error = writer.write(&data[..5]).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(Error::of(&error), Some(&Error::TooLong { expected: 10 }));
    // None of the refused write was passed on, and the writer stays failed.
    assert_eq!(writer.get_ref(), &data[..6]);
    assert!(writer.write(&data[6..]).is_err());
    assert_eq!(writer.verified(), Err(Error::TooLong { expected: 10 }));
}

#[test]
fn the_writer_checks_size_and_digest() {
    for hash in HASHES {
        let data = pattern(100);
        let expected = Expected::of(hash, &data);

        let mut writer = VerifyingWriter::new(Vec::new(), expected);
        for chunk in data.chunks(7) {
            writer.write_all(chunk).unwrap();
        }
        assert_eq!(writer.verified().unwrap().expected(), &expected);
        assert_eq!(writer.into_inner(), data);

        let mut writer = VerifyingWriter::new(Vec::new(), expected);
        writer.write_all(&data[..99]).unwrap();
        assert_eq!(
            writer.verified(),
            Err(Error::TooShort {
                expected: 100,
                received: 99
            })
        );

        let mut wrong = data.clone();
        wrong[50] ^= 1;
        let mut writer = VerifyingWriter::new(Vec::new(), expected);
        writer.write_all(&wrong).unwrap();
        assert_eq!(writer.verified(), Err(Error::Mismatch { hash }));
    }
}

#[test]
fn the_writer_hashes_only_what_its_inner_writer_took() {
    struct Short(Vec<u8>);
    impl Write for Short {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let n = buf.len().min(3);
            self.0.extend_from_slice(&buf[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let data = pattern(20);
    let mut writer = VerifyingWriter::new(Short(Vec::new()), Expected::of(HashFn::Blake3, &data));
    assert_eq!(writer.write(&data).unwrap(), 3);
    assert_eq!(writer.verifier().received(), 3);
    writer.write_all(&data[3..]).unwrap();
    writer.verified().unwrap();
}

#[test]
fn the_writer_stops_when_cancelled() {
    let data = pattern(10);
    let cancel = Cancel::new();
    let mut writer = VerifyingWriter::new(Vec::new(), Expected::of(HashFn::Sha256, &data))
        .with_cancel(cancel.clone());
    writer.write_all(&data[..4]).unwrap();
    cancel.cancel();
    let error = writer.write_all(&data[4..]).unwrap_err();
    assert_eq!(Error::of(&error), Some(&Error::Cancelled));
    assert_eq!(writer.get_ref(), &data[..4]);
    assert_eq!(writer.verified(), Err(Error::Cancelled));
}

/// Write `source` into a temporary file beside `destination`, and rename it
/// there only once it passes: the pattern the writer's docs describe.
fn stage(source: &[u8], expected: Expected, destination: &std::path::Path) -> io::Result<()> {
    let dir = destination.parent().unwrap();
    let staged = tempfile::NamedTempFile::new_in(dir)?;
    let mut writer = VerifyingWriter::new(staged, expected);
    io::copy(&mut &source[..], &mut writer)?;
    writer.flush()?;
    writer.verified()?;
    writer.into_inner().persist_noclobber(destination)?;
    Ok(())
}

#[test]
fn staged_files_appear_only_when_verified() {
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("file");
    let data = pattern(1000);
    let expected = Expected::of(HashFn::Sha256, &data);
    let entries = || fs::read_dir(dir.path()).unwrap().count();

    let mut wrong = data.clone();
    wrong[999] ^= 1;
    for source in [&wrong[..], &data[..999], &[&data[..], b"!"].concat()[..]] {
        stage(source, expected, &destination).unwrap_err();
        // The failed temporary file was removed, and nothing was published.
        assert_eq!(entries(), 0);
    }

    stage(&data, expected, &destination).unwrap();
    assert_eq!(fs::read(&destination).unwrap(), data);
    assert_eq!(entries(), 1);
}

#[test]
fn the_verifier_reports_progress() {
    let expected = Expected::of(HashFn::Sha256, b"hello");
    let mut verifier = Verifier::new(expected);
    verifier.update(b"he").unwrap();
    assert_eq!((verifier.received(), verifier.remaining()), (2, 3));
    assert_eq!(
        verifier.update(b"llo!"),
        Err(Error::TooLong { expected: 5 })
    );
    // A refused update changes nothing.
    assert_eq!(verifier.received(), 2);
    verifier.update(b"llo").unwrap();
    assert_eq!(verifier.finish().unwrap().expected(), &expected);
}

#[test]
fn an_empty_stream_is_unverified_until_read() {
    let mut reader =
        VerifyingReader::new(&b"x"[..], Expected::of(HashFn::Sha256, b"")).require_eof();
    // Reading nothing checks nothing.
    reader.read_exact(&mut []).unwrap();
    assert!(reader.verified().is_none());
    assert_eq!(
        verification(reader.read(&mut [0; 1]).map(drop)),
        Error::TooLong { expected: 0 }
    );
}
