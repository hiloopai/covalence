//! Byte streams between Rust and JavaScript `ReadableStream`s, each read
//! only as the other side reads.

use std::{cell::RefCell, io, rc::Rc};

use covalence_lib_data::{ByteStream, Bytes};
use futures_util::StreamExt;
use js_sys::Uint8Array;
use wasm_bindgen::{JsCast, prelude::*};
use web_sys::ReadableStream;

use crate::errors::{self, Code, Thrown};

/// The chunks of `body`, which must be `Uint8Array`s. Fails if `body` is
/// locked to another reader.
///
/// What `body` errors with is the source of the item's [`io::Error`], so it
/// reaches JavaScript again as the `cause` of the binding's error.
pub(crate) fn from_js(body: ReadableStream) -> Result<ByteStream<'static>, JsValue> {
    let chunks = wasm_streams::ReadableStream::from_raw(body)
        .try_into_stream()
        .map_err(|(error, _)| JsValue::from(error))?;
    Ok(Box::pin(chunks.map(|chunk| {
        let chunk = chunk.map_err(|error| io::Error::other(Thrown::new(error)))?;
        let chunk: Uint8Array = chunk.dyn_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "a chunk is not a Uint8Array")
        })?;
        Ok(Bytes::from(chunk.to_vec()))
    })))
}

/// `stream` as a JavaScript stream of `Uint8Array`s, which errors where
/// `stream` fails: with a failed check's error, or else one with `code`. The
/// first failure is also kept in `failure`, if given.
pub(crate) fn to_js(
    stream: ByteStream<'static>,
    code: Code,
    failure: Option<Rc<RefCell<Option<io::Error>>>>,
) -> ReadableStream {
    let chunks = stream.map(move |chunk| match chunk {
        Ok(chunk) => Ok(Uint8Array::from(&chunk[..]).into()),
        Err(error) => {
            let thrown = errors::stream(&error, code);
            if let Some(failure) = &failure {
                failure.borrow_mut().get_or_insert(error);
            }
            Err(thrown)
        }
    });
    wasm_streams::ReadableStream::from_stream(chunks).into_raw()
}
