//! The Tokio reader and writer: the same checks as the blocking ones, plus
//! wakeups, backpressure and cancellation by dropping the future.
#![cfg(feature = "tokio")]

mod common;

use std::{
    collections::VecDeque,
    io,
    path::Path,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use common::{Generated, pattern};
use covalence_blob_verify::{Error, Expected, HashFn, VerifyingReader, VerifyingWriter};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, ReadBuf};

/// Chunks as an HTTP body delivers them, pending once before each.
struct Chunks {
    chunks: VecDeque<Vec<u8>>,
    pending: bool,
}

impl Chunks {
    fn new(data: &[u8], size: usize) -> Self {
        Self {
            chunks: data.chunks(size).map(<[u8]>::to_vec).collect(),
            pending: true,
        }
    }
}

impl AsyncRead for Chunks {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if std::mem::take(&mut self.pending) {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        self.pending = true;
        let Some(mut chunk) = self.chunks.pop_front() else {
            return Poll::Ready(Ok(()));
        };
        let take = chunk.len().min(buf.remaining());
        buf.put_slice(&chunk[..take]);
        if take < chunk.len() {
            self.chunks.push_front(chunk.split_off(take));
        }
        Poll::Ready(Ok(()))
    }
}

async fn drain(reader: &mut (impl AsyncRead + Unpin)) -> (Vec<u8>, io::Result<()>) {
    let mut out = Vec::new();
    let mut buf = [0; 5];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => return (out, Ok(())),
            Ok(read) => out.extend_from_slice(&buf[..read]),
            Err(error) => return (out, Err(error)),
        }
    }
}

fn verification(result: io::Result<()>) -> Error {
    *Error::of(&result.unwrap_err()).expect("a verification error")
}

#[tokio::test]
async fn chunked_streams_are_checked() {
    for hash in [HashFn::Sha256, HashFn::Blake3] {
        let data = pattern(40);
        let expected = Expected::of(hash, &data);
        for size in [1, 3, 40] {
            let mut reader = VerifyingReader::new(Chunks::new(&data, size), expected).require_eof();
            let (out, result) = drain(&mut reader).await;
            result.unwrap();
            assert_eq!(out, data);
            assert!(reader.verified().is_some());

            let mut wrong = data.clone();
            wrong[39] ^= 1;
            let mut reader = VerifyingReader::new(Chunks::new(&wrong, size), expected);
            let (out, result) = drain(&mut reader).await;
            assert_eq!(verification(result), Error::Mismatch { hash });
            assert_eq!(out, data[..39]);

            let mut reader = VerifyingReader::new(Chunks::new(&data[..25], size), expected);
            let (out, result) = drain(&mut reader).await;
            assert_eq!(
                verification(result),
                Error::TooShort {
                    expected: 40,
                    received: 25
                }
            );
            assert_eq!(out, data[..25]);

            let long = [&data[..], b"more"].concat();
            let mut reader = VerifyingReader::new(Chunks::new(&long, size), expected).require_eof();
            let (out, result) = drain(&mut reader).await;
            assert_eq!(verification(result), Error::TooLong { expected: 40 });
            assert_eq!(out, data[..39]);
        }
    }
}

#[tokio::test]
async fn a_slow_consumer_holds_back_its_producer() {
    const LEN: u64 = 16 << 20;
    const PIPE: usize = 64 << 10;
    let expected = Generated::expected(HashFn::Blake3, LEN);
    let (mut tx, rx) = tokio::io::duplex(PIPE);
    let mut source = Generated::new(LEN);
    let produced = source.produced.clone();
    let producer = tokio::spawn(async move {
        let mut buf = vec![0; 16 << 10];
        loop {
            let read = std::io::Read::read(&mut source, &mut buf).unwrap();
            if read == 0 {
                break;
            }
            tx.write_all(&buf[..read]).await.unwrap();
        }
    });
    let mut reader = VerifyingReader::new(rx, expected).require_eof();
    let mut buf = vec![0; 8 << 10];
    let mut consumed = 0_u64;
    loop {
        let read = reader.read(&mut buf).await.unwrap();
        if read == 0 {
            break;
        }
        consumed += read as u64;
        if consumed % (1 << 20) < 8 << 10 {
            // Pause, as a slow consumer would: the producer can fill the pipe
            // and its own buffer, and no more.
            tokio::time::sleep(Duration::from_millis(2)).await;
            let ahead = produced.load(std::sync::atomic::Ordering::SeqCst) - consumed;
            assert!(
                ahead <= (PIPE + (16 << 10) + 1) as u64,
                "{ahead} bytes ahead"
            );
        }
    }
    producer.await.unwrap();
    assert_eq!(consumed, LEN);
    assert!(reader.verified().is_some());
}

/// Copy `source` into a temporary file beside `destination` and rename it
/// there once it passes, as a materializer would.
async fn materialize(
    source: impl AsyncRead + Unpin,
    expected: Expected,
    destination: &Path,
) -> io::Result<()> {
    let staged = tempfile::NamedTempFile::new_in(destination.parent().unwrap())?;
    let file = tokio::fs::File::from_std(staged.reopen()?);
    let mut writer = VerifyingWriter::new(file, expected);
    tokio::io::copy(&mut tokio::io::BufReader::new(source), &mut writer).await?;
    writer.flush().await?;
    writer.verified()?;
    writer.into_inner().sync_all().await?;
    staged
        .persist_noclobber(destination)
        .map_err(|error| error.error)?;
    Ok(())
}

#[tokio::test]
async fn files_are_published_only_when_verified() {
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("out");
    let entries = || std::fs::read_dir(dir.path()).unwrap().count();
    let data = pattern(5000);
    let expected = Expected::of(HashFn::Sha256, &data);

    let mut wrong = data.clone();
    wrong[0] ^= 1;
    let long = [&data[..], b"x"].concat();
    for (source, error) in [
        (
            &wrong[..],
            Error::Mismatch {
                hash: HashFn::Sha256,
            },
        ),
        (
            &data[..10],
            Error::TooShort {
                expected: 5000,
                received: 10,
            },
        ),
        (&long[..], Error::TooLong { expected: 5000 }),
    ] {
        let failed = materialize(Chunks::new(source, 700), expected, &destination)
            .await
            .unwrap_err();
        assert_eq!(Error::of(&failed), Some(&error));
        assert_eq!(entries(), 0);
    }

    materialize(Chunks::new(&data, 700), expected, &destination)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&destination).unwrap(), data);
}

#[tokio::test]
async fn a_timed_out_copy_is_dropped_and_cleaned_up() {
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("out");
    let data = pattern(100);
    // The source sends half and then stalls, as a stuck connection would.
    let (mut tx, rx) = tokio::io::duplex(1024);
    tx.write_all(&data[..50]).await.unwrap();
    let copy = materialize(rx, Expected::of(HashFn::Blake3, &data), &destination);
    tokio::time::timeout(Duration::from_millis(50), copy)
        .await
        .unwrap_err();
    // Dropping the future dropped the temporary file.
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    drop(tx);
}

#[tokio::test]
async fn an_http_body_is_checked_against_its_inventory() {
    // The shape of a client that streams a presigned GET into a sink: the
    // body must have exactly the inventory's size and SHA-256, and end there.
    let data = pattern(3000);
    let inventory = Expected::of(HashFn::Sha256, &data);
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut body = VerifyingReader::new(Chunks::new(&data, 1024), inventory)
        .require_eof()
        .with_deadline(deadline);
    let mut sink = Vec::new();
    tokio::io::copy(&mut body, &mut sink).await.unwrap();
    assert_eq!(body.verified().unwrap().expected(), &inventory);
    assert_eq!(sink, data);
}

#[tokio::test]
async fn the_async_writer_refuses_bytes_past_the_size() {
    let data = pattern(10);
    let mut writer = VerifyingWriter::new(Vec::new(), Expected::of(HashFn::Blake3, &data));
    writer.write_all(&data[..8]).await.unwrap();
    let error = writer.write_all(&data[..3]).await.unwrap_err();
    assert_eq!(Error::of(&error), Some(&Error::TooLong { expected: 10 }));
    assert_eq!(writer.get_ref(), &data[..8]);
    writer.shutdown().await.unwrap();
}
