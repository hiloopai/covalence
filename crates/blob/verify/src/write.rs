//! A writer that refuses bytes past the expected size and checks what it
//! passed on.

use std::{io, time::Instant};

use crate::{Cancel, Error, Expected, Stop, Verified, Verifier};

/// A writer that passes bytes on to another writer, refusing any past the
/// expected size, and checks them once they are all written.
///
/// - Fails with [`Error::TooLong`], passing on none of a write, if the write
///   would go past the expected size.
/// - Hashes only the bytes `inner` accepts.
/// - Checks the [`Cancel`] flag and deadline, if set, before each write to
///   `inner`, failing with [`Error::Cancelled`] or [`Error::DeadlineExceeded`].
///   It cannot interrupt a write in progress, so blocked writes need an
///   outer timeout.
/// - After one of these errors, every later write fails with the same error.
///   Errors from `inner` are returned unchanged.
///
/// Unlike [`VerifyingReader`](crate::VerifyingReader), it cannot hold back
/// bytes: `inner` has every byte before the check. Write to a destination
/// that nobody reads yet, such as a temporary file, then flush, call
/// [`VerifyingWriter::verified`], and only then make it visible, for example
/// by renaming the file. On any error, discard the destination.
///
/// With the default `tokio` feature, it is also a Tokio `AsyncWrite` when
/// `inner` is, with the same behaviour.
///
/// ```
/// use std::io::Write;
///
/// use covalence_blob_verify::{Error, Expected, HashFn, VerifyingWriter};
///
/// let expected = Expected::of(HashFn::Blake3, b"hello");
///
/// let mut writer = VerifyingWriter::new(Vec::new(), expected);
/// writer.write_all(b"hel")?;
/// assert!(matches!(writer.verified(), Err(Error::TooShort { received: 3, .. })));
/// writer.write_all(b"lo")?;
/// writer.flush()?;
/// writer.verified()?;
/// assert_eq!(writer.into_inner(), b"hello");
///
/// let mut writer = VerifyingWriter::new(Vec::new(), expected);
/// let error = writer.write_all(b"hello!").unwrap_err();
/// assert_eq!(Error::of(&error), Some(&Error::TooLong { expected: 5 }));
/// assert!(writer.get_ref().is_empty());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct VerifyingWriter<W> {
    inner: W,
    verifier: Verifier,
    stop: Stop,
    failed: Option<Error>,
}

impl<W> VerifyingWriter<W> {
    /// A writer of the `expected` bytes to `inner`.
    pub fn new(inner: W, expected: Expected) -> Self {
        Self {
            inner,
            verifier: Verifier::new(expected),
            stop: Stop::default(),
            failed: None,
        }
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

    /// The verifier, with the bytes written so far.
    pub fn verifier(&self) -> &Verifier {
        &self.verifier
    }

    /// Check the bytes written so far.
    ///
    /// - The error that stopped an earlier write, if one did.
    /// - Otherwise, what [`Verifier::finish`] returns.
    ///
    /// It does not flush `inner`: flush first if it buffers.
    pub fn verified(&self) -> Result<Verified, Error> {
        match self.failed {
            Some(error) => Err(error),
            None => self.verifier.finish(),
        }
    }

    /// The inner writer.
    pub fn get_ref(&self) -> &W {
        &self.inner
    }

    /// The inner writer, giving up the check.
    pub fn into_inner(self) -> W {
        self.inner
    }

    fn fail(&mut self, error: Error) -> io::Error {
        self.failed = Some(error);
        error.into()
    }

    /// Whether a write of `len` bytes may go to the inner writer.
    fn admit(&mut self, len: usize) -> io::Result<()> {
        if let Some(error) = self.failed {
            return Err(error.into());
        }
        if let Err(error) = self.stop.check() {
            return Err(self.fail(error));
        }
        if len as u64 > self.verifier.remaining() {
            let error = Error::TooLong {
                expected: self.verifier.expected().size(),
            };
            return Err(self.fail(error));
        }
        Ok(())
    }

    /// Hash the first `written` bytes of `buf`, which the inner writer took.
    fn accepted(&mut self, buf: &[u8], written: usize) -> io::Result<usize> {
        let bytes = buf
            .get(..written)
            .ok_or_else(|| io::Error::other("the writer took more than it was given"))?;
        self.verifier
            .update(bytes)
            .expect("admitted writes fit in the remaining size");
        Ok(written)
    }
}

impl<W: io::Write> io::Write for VerifyingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.admit(buf.len())?;
        let written = self.inner.write(buf)?;
        self.accepted(buf, written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(feature = "tokio")]
mod tokio {
    use std::{
        io,
        pin::Pin,
        task::{Context, Poll, ready},
    };

    use tokio::io::AsyncWrite;

    use super::VerifyingWriter;

    impl<W: AsyncWrite + Unpin> AsyncWrite for VerifyingWriter<W> {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.admit(buf.len())?;
            let written = ready!(Pin::new(&mut this.inner).poll_write(cx, buf))?;
            Poll::Ready(this.accepted(buf, written))
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_flush(cx)
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
        }
    }
}
