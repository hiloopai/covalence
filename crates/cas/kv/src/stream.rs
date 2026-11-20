//! A stream of chunks checked against an expected size and digest, which
//! holds back the last chunk until they pass.

use std::{
    error, fmt,
    future::poll_fn,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, ready},
};

use covalence_blob_verify::{Error as VerifyError, Expected, Verifier};
use covalence_lib_data::{ByteStream, Bytes, Stream};

/// The chunks of `inner`, which must be exactly the `expected` bytes and
/// then end.
///
/// - Passes each chunk on as it arrives, except the one that completes the
///   expected size. It holds that chunk back until the digest matches and
///   `inner` has ended.
/// - On a mismatch, a short or long stream, yields the [`VerifyError`] as an
///   [`io::Error`] in place of the held chunk, then ends.
/// - An error from `inner` is passed on, with its kind and message and the
///   error itself as its source, and the stream ends. [`Checked::finish`]
///   returns it again.
pub(crate) struct Checked {
    inner: ByteStream<'static>,
    verifier: Verifier,
    state: State,
}

/// How far a [`Checked`] stream has got.
enum State {
    /// More bytes are expected.
    Reading,
    /// Every byte arrived and matched; the last chunk waits for `inner` to end.
    Holding(Bytes),
    /// `inner` ended after exactly the expected bytes.
    Passed,
    /// The check failed.
    Refused(VerifyError),
    /// `inner` failed.
    Broken(Arc<io::Error>),
}

impl Checked {
    pub(crate) fn new(inner: ByteStream<'static>, expected: Expected) -> Self {
        Self {
            inner,
            verifier: Verifier::new(expected),
            state: State::Reading,
        }
    }

    /// The check that failed, if one did.
    pub(crate) fn failure(&self) -> Option<VerifyError> {
        match self.state {
            State::Refused(error) => Some(error),
            _ => None,
        }
    }

    /// Read the rest of the stream, checking it, and succeed only if every
    /// byte arrived and matched and the stream then ended.
    pub(crate) async fn finish(&mut self) -> io::Result<()> {
        while let Some(chunk) = poll_fn(|cx| Pin::new(&mut *self).poll_next(cx)).await {
            chunk?;
        }
        match &self.state {
            State::Passed => Ok(()),
            State::Refused(error) => Err((*error).into()),
            State::Broken(error) => Err(relay(error)),
            State::Reading | State::Holding(_) => unreachable!("a checked stream ends decided"),
        }
    }

    fn refuse(&mut self, error: VerifyError) -> Poll<Option<io::Result<Bytes>>> {
        self.state = State::Refused(error);
        Poll::Ready(Some(Err(error.into())))
    }
}

impl Stream for Checked {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if !matches!(this.state, State::Reading | State::Holding(_)) {
                return Poll::Ready(None);
            }
            match ready!(this.inner.as_mut().poll_next(cx)) {
                Some(Ok(chunk)) if chunk.is_empty() => {}
                Some(Ok(chunk)) => {
                    if let Err(error) = this.verifier.update(&chunk) {
                        return this.refuse(error);
                    }
                    if this.verifier.remaining() > 0 {
                        return Poll::Ready(Some(Ok(chunk)));
                    }
                    // Every byte is here: fail a mismatch now, but release
                    // the last chunk only once the source has ended.
                    if let Err(error) = this.verifier.finish() {
                        return this.refuse(error);
                    }
                    this.state = State::Holding(chunk);
                }
                Some(Err(error)) => {
                    let error = Arc::new(error);
                    let relayed = relay(&error);
                    this.state = State::Broken(error);
                    return Poll::Ready(Some(Err(relayed)));
                }
                None => {
                    let held = match std::mem::replace(&mut this.state, State::Passed) {
                        State::Holding(chunk) => Some(chunk),
                        _ => match this.verifier.finish() {
                            // Nothing was expected, and nothing came.
                            Ok(_) => None,
                            Err(error) => return this.refuse(error),
                        },
                    };
                    return Poll::Ready(held.map(Ok));
                }
            }
        }
    }
}

/// An error of the source, as an [`io::Error`] of the same kind and message
/// whose source is the error itself, so the stream can both pass it on and
/// keep it.
fn relay(error: &Arc<io::Error>) -> io::Error {
    io::Error::new(error.kind(), Relayed(error.clone()))
}

/// The source's [`io::Error`], shared: displayed as it is, and its
/// `source`.
#[derive(Debug)]
struct Relayed(Arc<io::Error>);

impl fmt::Display for Relayed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl error::Error for Relayed {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(&*self.0)
    }
}

/// A [`Checked`] lent to a store, which the lender takes back once the store
/// is done: the store's stream then ends, whether or not it still holds it.
///
/// Shared through `Arc<Mutex>` where futures must be `Send`, and through
/// `Rc<RefCell>` on `wasm32`, where they need not be and may hold
/// JavaScript values.
#[derive(Clone)]
pub(crate) struct Lent(Shared<Option<Checked>>);

#[cfg(not(target_arch = "wasm32"))]
type Shared<T> = std::sync::Arc<std::sync::Mutex<T>>;

#[cfg(target_arch = "wasm32")]
type Shared<T> = std::rc::Rc<std::cell::RefCell<T>>;

impl Lent {
    pub(crate) fn new(checked: Checked) -> Self {
        Self(Shared::new(Some(checked).into()))
    }

    /// The stream, unless already taken back.
    pub(crate) fn take(&self) -> Option<Checked> {
        self.with(Option::take)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn with<T>(&self, f: impl FnOnce(&mut Option<Checked>) -> T) -> T {
        f(&mut self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    #[cfg(target_arch = "wasm32")]
    fn with<T>(&self, f: impl FnOnce(&mut Option<Checked>) -> T) -> T {
        f(&mut self.0.borrow_mut())
    }
}

impl Stream for Lent {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.with(|lent| match lent.as_mut() {
            Some(checked) => Pin::new(checked).poll_next(cx),
            None => Poll::Ready(None),
        })
    }
}
