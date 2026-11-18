//! A reader that hands over the last byte of a stream only once the whole
//! stream has passed its check.

use std::{io, time::Instant};

use crate::{Cancel, Error, Expected, Stop, Verified, Verifier};

/// A reader of exactly the expected bytes from another reader, which withholds
/// the last byte until they pass their check.
///
/// It returns every byte but the last before the check, so it does not stop
/// unverified bytes from being published: stage what it returns and commit
/// only after [`VerifyingReader::verified`]. See the [crate] docs for what
/// the caller must do.
///
/// - Reads from `inner` at most the bytes still expected, so it never reads
///   past the end of the expected stream.
/// - Fails with [`Error::TooShort`] if `inner` ends early.
/// - Holds back the final byte, then checks the digest. A mismatch fails
///   with [`Error::Mismatch`], and the last byte is never returned.
/// - With [`VerifyingReader::require_eof`], also reads `inner` once more
///   before releasing the last byte, and fails with [`Error::TooLong`] if it
///   is not at its end. Without it, extra bytes in `inner` are left unread
///   and unchecked.
/// - Checks the [`Cancel`] flag and deadline, if set, before each read from
///   `inner`, failing with [`Error::Cancelled`] or [`Error::DeadlineExceeded`].
///   It cannot interrupt a read in progress, so blocked reads need an outer
///   timeout: give `inner` its own, or, with Tokio, wrap the future in
///   `tokio::time::timeout`.
///
/// An empty expected stream has no byte to withhold: the first read into a
/// non-empty buffer checks it. A caller that may expect zero bytes should read
/// until a read returns 0, or check [`VerifyingReader::verified`] before it
/// commits, since `read_exact` of nothing never reads.
///
/// After one of these errors, every later read fails with the same error.
/// Errors from `inner` are returned unchanged and leave the reader as it was,
/// so a read that was interrupted can be retried.
///
/// Holds no bytes besides the one withheld, so memory stays constant however
/// long the stream. It reads from `inner` only when it is read from, so a
/// slow consumer slows the producer.
///
/// With the default `tokio` feature, it is also a Tokio `AsyncRead` when
/// `inner` is, with the same behaviour.
///
/// ```
/// use std::io::Read;
///
/// use covalence_blob_verify::{Error, Expected, HashFn, VerifyingReader};
///
/// let expected = Expected::of(HashFn::Sha256, b"hello");
///
/// let mut reader = VerifyingReader::new(&b"hello"[..], expected);
/// let mut bytes = Vec::new();
/// reader.read_to_end(&mut bytes)?;
/// assert_eq!(bytes, b"hello");
/// assert!(reader.verified().is_some());
///
/// // A wrong last byte is never returned.
/// let mut reader = VerifyingReader::new(&b"hellO"[..], expected);
/// let mut bytes = Vec::new();
/// let error = reader.read_to_end(&mut bytes).unwrap_err();
/// assert_eq!(Error::of(&error), Some(&Error::Mismatch { hash: HashFn::Sha256 }));
/// assert_eq!(bytes, b"hell");
///
/// // Trailing bytes are left unread, unless the end is required.
/// let mut reader = VerifyingReader::new(&b"hello!"[..], expected).require_eof();
/// let error = reader.read_to_end(&mut Vec::new()).unwrap_err();
/// assert_eq!(Error::of(&error), Some(&Error::TooLong { expected: 5 }));
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug)]
pub struct VerifyingReader<R> {
    inner: R,
    verifier: Verifier,
    stop: Stop,
    require_eof: bool,
    state: State,
}

/// Where a [`VerifyingReader`] is in its stream.
#[derive(Clone, Copy, Debug)]
enum State {
    /// More bytes are expected from the inner reader.
    Reading,
    /// Every expected byte has been read; the last one, if any, is withheld
    /// until the check passes.
    Finishing(Option<u8>),
    /// The stream passed its check and every byte was returned.
    Done,
    /// The stream failed its check or was stopped.
    Failed(Error),
}

/// The next step of a read.
enum Step {
    /// Read up to this many bytes from the inner reader.
    Read(u64),
    /// Check the stream and release the withheld byte.
    Finish,
    /// Report the end of the stream.
    Eof,
}

impl<R> VerifyingReader<R> {
    /// A reader of the `expected` bytes from `inner`.
    pub fn new(inner: R, expected: Expected) -> Self {
        let state = match expected.size() {
            0 => State::Finishing(None),
            _ => State::Reading,
        };
        Self {
            inner,
            verifier: Verifier::new(expected),
            stop: Stop::default(),
            require_eof: false,
            state,
        }
    }

    /// Also require `inner` to end with the expected bytes; see
    /// [`VerifyingReader`].
    pub fn require_eof(mut self) -> Self {
        self.require_eof = true;
        self
    }

    /// Stop with [`Error::Cancelled`] once `cancel` is set.
    pub fn with_cancel(mut self, cancel: Cancel) -> Self {
        self.stop.cancel = Some(cancel);
        self
    }

    /// Stop with [`Error::DeadlineExceeded`] at `deadline`.
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.stop.deadline = Some(deadline);
        self
    }

    /// The verifier, with the bytes read so far.
    pub fn verifier(&self) -> &Verifier {
        &self.verifier
    }

    /// The proof that the stream passed its check, once every byte has been
    /// returned.
    pub fn verified(&self) -> Option<Verified> {
        match self.state {
            State::Done => self.verifier.finish().ok(),
            _ => None,
        }
    }

    /// The error that stopped the stream, if one did.
    ///
    /// Useful when the reader was handed to code that reports its errors in
    /// its own way, such as an upload client that wraps them.
    pub fn error(&self) -> Option<Error> {
        match self.state {
            State::Failed(error) => Some(error),
            _ => None,
        }
    }

    /// The inner reader.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// The inner reader, giving up the check.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Record `error`, so later reads fail with it too.
    fn fail(&mut self, error: Error) -> io::Error {
        self.state = State::Failed(error);
        error.into()
    }

    /// The result of a read into an empty buffer: nothing, or the error.
    fn empty(&self) -> io::Result<()> {
        match self.state {
            State::Failed(error) => Err(error.into()),
            _ => Ok(()),
        }
    }

    fn stop(&mut self) -> io::Result<()> {
        self.stop.check().map_err(|error| self.fail(error))
    }

    fn step(&mut self) -> io::Result<Step> {
        match self.state {
            State::Failed(error) => Err(error.into()),
            State::Done => Ok(Step::Eof),
            State::Finishing(_) => Ok(Step::Finish),
            State::Reading => {
                self.stop()?;
                Ok(Step::Read(self.verifier.remaining()))
            }
        }
    }

    /// Take `filled`, just read from the inner reader, and return how many of
    /// its bytes to return now. Zero means the read reached the end: finish.
    fn received(&mut self, filled: &[u8]) -> io::Result<usize> {
        let Some((&last, _)) = filled.split_last() else {
            let error = Error::TooShort {
                expected: self.verifier.expected().size(),
                received: self.verifier.received(),
            };
            return Err(self.fail(error));
        };
        if let Err(error) = self.verifier.update(filled) {
            return Err(self.fail(error));
        }
        if self.verifier.remaining() > 0 {
            return Ok(filled.len());
        }
        self.state = State::Finishing(Some(last));
        Ok(filled.len() - 1)
    }

    /// Check the digest, before the withheld byte is released.
    fn check(&mut self) -> io::Result<()> {
        match self.verifier.finish() {
            Ok(_) => Ok(()),
            Err(error) => Err(self.fail(error)),
        }
    }

    /// Take the length of a read past the expected bytes.
    fn probed(&mut self, len: usize) -> io::Result<()> {
        if len == 0 {
            return Ok(());
        }
        let error = Error::TooLong {
            expected: self.verifier.expected().size(),
        };
        Err(self.fail(error))
    }

    /// The withheld byte, if any, now that the stream passed its check.
    fn release(&mut self) -> Option<u8> {
        let State::Finishing(last) = self.state else {
            unreachable!("released only while finishing");
        };
        self.state = State::Done;
        last
    }
}

/// The most to read into a buffer of `buffer` bytes with `remaining` expected.
fn cap(buffer: usize, remaining: u64) -> usize {
    usize::try_from(remaining).map_or(buffer, |remaining| buffer.min(remaining))
}

impl<R: io::Read> io::Read for VerifyingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return self.empty().map(|()| 0);
        }
        loop {
            match self.step()? {
                Step::Eof => return Ok(0),
                Step::Read(remaining) => {
                    let max = cap(buf.len(), remaining);
                    let read = self.inner.read(&mut buf[..max])?;
                    if read > max {
                        return Err(io::Error::other(
                            "the reader returned more than it was asked",
                        ));
                    }
                    let ready = self.received(&buf[..read])?;
                    if ready > 0 {
                        return Ok(ready);
                    }
                }
                Step::Finish => {
                    self.check()?;
                    if self.require_eof {
                        self.stop()?;
                        let read = self.inner.read(&mut [0])?;
                        self.probed(read)?;
                    }
                    return Ok(match self.release() {
                        Some(last) => {
                            buf[0] = last;
                            1
                        }
                        None => 0,
                    });
                }
            }
        }
    }
}

#[cfg(feature = "tokio")]
mod tokio {
    use std::{
        io,
        pin::Pin,
        task::{Context, Poll, ready},
    };

    use tokio::io::{AsyncRead, ReadBuf};

    use super::{Step, VerifyingReader, cap};

    impl<R: AsyncRead + Unpin> AsyncRead for VerifyingReader<R> {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            if buf.remaining() == 0 {
                return Poll::Ready(this.empty());
            }
            loop {
                match this.step()? {
                    Step::Eof => return Poll::Ready(Ok(())),
                    Step::Read(remaining) => {
                        let max = cap(buf.remaining(), remaining);
                        let mut part = ReadBuf::new(buf.initialize_unfilled_to(max));
                        ready!(Pin::new(&mut this.inner).poll_read(cx, &mut part))?;
                        let ready = this.received(part.filled())?;
                        buf.advance(ready);
                        if ready > 0 {
                            return Poll::Ready(Ok(()));
                        }
                    }
                    Step::Finish => {
                        this.check()?;
                        if this.require_eof {
                            this.stop()?;
                            let mut byte = [0];
                            let mut probe = ReadBuf::new(&mut byte);
                            ready!(Pin::new(&mut this.inner).poll_read(cx, &mut probe))?;
                            this.probed(probe.filled().len())?;
                        }
                        if let Some(last) = this.release() {
                            buf.put_slice(&[last]);
                        }
                        return Poll::Ready(Ok(()));
                    }
                }
            }
        }
    }
}
