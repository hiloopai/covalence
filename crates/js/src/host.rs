//! The storage a host provides, as a key-value store.

use std::{cell::RefCell, io, rc::Rc};

use covalence_kv_trait::{Error, Kv, KvFuture, Op, ready};
use covalence_lib_data::{ByteStream, Bytes};
use js_sys::{Promise, Uint8Array};
use wasm_bindgen::{JsCast, prelude::*};
use wasm_bindgen_futures::JsFuture;
use web_sys::ReadableStream;

use crate::{
    MAX_LEN,
    errors::{Code, Thrown},
    stream, whole,
};

#[wasm_bindgen(typescript_custom_section)]
const HOST_STORAGE: &str = r#"
/**
 * Storage that a `Cas` keeps its blocks in, provided by the host: byte values
 * under string keys, such as objects in an R2 or S3 bucket.
 *
 * - A method may answer with a promise or with the value itself.
 * - A method fails by throwing or rejecting; the binding's error then has
 *   code `"store"` and what the method threw as its `cause`.
 * - An answer of the wrong type, a size that is not a whole number from 0
 *   to 2^53 - 1, or a locked body fails with code `"store"` too.
 */
export interface HostStorage {
  /** The value under `key`, or `null` if missing. */
  get(key: string): Promise<Uint8Array | null>;
  /** The size in bytes of the value under `key`, or `null` if missing. */
  size(key: string): Promise<number | null>;
  /**
   * Store `value` under `key` only if the key holds no value, atomically, and
   * resolve whether it stored.
   */
  putNew(key: string, value: Uint8Array): Promise<boolean>;
  /**
   * Store the `length` bytes of `body` under `key` only if the key holds no
   * value, atomically, and resolve whether it stored.
   *
   * - Reject, storing nothing, if `body` errors or does not yield exactly
   *   `length` bytes.
   * - Read `body` only as fast as it can be stored.
   * - If the key holds a value, `body` may be left unread.
   */
  putNewFrom(key: string, body: ReadableStream<Uint8Array>, length: number): Promise<boolean>;
  /** The value under `key` as a stream with its size, or `null` if missing. */
  getStream(key: string): Promise<SizedStream | null>;
}

/** A stream of bytes with its size. */
export interface SizedStream {
  /** The number of bytes `body` holds: a whole number from 0 to 2^53 - 1. */
  size: number;
  /** The bytes, in chunks. */
  body: ReadableStream<Uint8Array>;
}
"#;

#[wasm_bindgen]
extern "C" {
    /// Storage that a `Cas` keeps its blocks in, provided by the host; the
    /// TypeScript declarations describe its methods.
    #[wasm_bindgen(typescript_type = "HostStorage")]
    pub type HostStorage;

    // Imported as plain calls, not `async fn`, so a method that throws
    // before returning a promise fails the call rather than leaving it
    // pending; `answer` awaits what they return.
    #[wasm_bindgen(method, catch)]
    fn get(this: &HostStorage, key: &str) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch)]
    fn size(this: &HostStorage, key: &str) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch, js_name = putNew)]
    fn put_new(this: &HostStorage, key: &str, value: Uint8Array) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch, js_name = putNewFrom)]
    fn put_new_from(
        this: &HostStorage,
        key: &str,
        body: ReadableStream,
        length: f64,
    ) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch, js_name = getStream)]
    fn get_stream(this: &HostStorage, key: &str) -> Result<JsValue, JsValue>;
}

/// What a host method gave, awaited if it is a promise, or what it threw or
/// rejected with as a backend error.
async fn answer(call: Result<JsValue, JsValue>) -> Result<JsValue, Error> {
    let value = call.map_err(threw)?;
    JsFuture::from(Promise::resolve(&value))
        .await
        .map_err(threw)
}

/// A host's storage as a [`Kv`]. Only the async methods are supported, and
/// `aput` is not: blocks are only ever created.
pub(crate) struct JsKv(HostStorage);

impl JsKv {
    pub(crate) fn new(host: HostStorage) -> Self {
        Self(host)
    }
}

/// An answer the host should not have given.
fn misbehaved(message: impl Into<String>) -> Error {
    Error::Backend {
        source: message.into().into(),
    }
}

/// What the host threw, as the cause of a backend error.
fn threw(value: JsValue) -> Error {
    Error::Backend {
        source: Box::new(Thrown::new(value)),
    }
}

/// `key` as the host's string key.
fn text(key: &[u8]) -> Result<&str, Error> {
    std::str::from_utf8(key).map_err(|_| misbehaved("a key is not UTF-8"))
}

/// Whether the host answered `null` or `undefined`.
fn missing(value: &JsValue) -> bool {
    value.is_null() || value.is_undefined()
}

/// The size the host gave, or `None` for `null` or `undefined`.
fn size_of(value: &JsValue, method: &str) -> Result<Option<u64>, Error> {
    if missing(value) {
        return Ok(None);
    }
    match value.as_f64().and_then(whole) {
        Some(size) => Ok(Some(size)),
        None => Err(misbehaved(format!("host {method} gave an invalid size"))),
    }
}

fn stored(value: JsValue, method: &str) -> Result<bool, Error> {
    value
        .as_bool()
        .ok_or_else(|| misbehaved(format!("host {method} did not give a boolean")))
}

impl Kv for JsKv {
    fn get(&self, _: &[u8]) -> Result<Option<Bytes>, Error> {
        Err(Op::Get.unsupported())
    }

    fn put(&self, _: &[u8], _: Bytes) -> Result<(), Error> {
        Err(Op::Put.unsupported())
    }

    fn aget<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<Bytes>> {
        Box::pin(async move {
            let value = answer(self.0.get(text(key)?)).await?;
            if missing(&value) {
                return Ok(None);
            }
            let value: Uint8Array = value
                .dyn_into()
                .map_err(|_| misbehaved("host get did not give a Uint8Array"))?;
            Ok(Some(value.to_vec().into()))
        })
    }

    fn asize<'a>(&'a self, key: &'a [u8]) -> KvFuture<'a, Option<u64>> {
        Box::pin(async move {
            let size = answer(self.0.size(text(key)?)).await?;
            size_of(&size, "size")
        })
    }

    fn aput<'a>(&'a self, _: &'a [u8], _: Bytes) -> KvFuture<'a, ()> {
        ready(Err(Op::Put.unsupported()))
    }

    fn aput_new<'a>(&'a self, key: &'a [u8], value: Bytes) -> KvFuture<'a, bool> {
        Box::pin(async move {
            let value = Uint8Array::from(&value[..]);
            let result = answer(self.0.put_new(text(key)?, value)).await;
            stored(result?, "putNew")
        })
    }

    fn aput_new_from<'a>(
        &'a self,
        key: &'a [u8],
        stream: ByteStream<'static>,
        len: u64,
    ) -> KvFuture<'a, bool> {
        Box::pin(async move {
            let key = text(key)?;
            if len > MAX_LEN {
                return Err(misbehaved("a value is too large for a host"));
            }
            // The host sees only a JavaScript error; keep the stream's own.
            let failure: Rc<RefCell<Option<io::Error>>> = Rc::default();
            let body = stream::to_js(stream, Code::Io, Some(failure.clone()));
            let result = answer(self.0.put_new_from(key, body, len as f64)).await;
            if let Some(source) = failure.borrow_mut().take() {
                return Err(Error::Io { source });
            }
            stored(result?, "putNewFrom")
        })
    }

    fn aget_stream<'a>(
        &'a self,
        key: &'a [u8],
    ) -> KvFuture<'a, Option<(u64, ByteStream<'static>)>> {
        Box::pin(async move {
            let sized = answer(self.0.get_stream(text(key)?)).await?;
            if missing(&sized) {
                return Ok(None);
            }
            if !sized.is_object() {
                return Err(misbehaved("host getStream did not give an object"));
            }
            let field = |name: &str| js_sys::Reflect::get(&sized, &name.into());
            let size = size_of(&field("size").map_err(threw)?, "getStream")?
                .ok_or_else(|| misbehaved("host getStream gave no size"))?;
            let body: ReadableStream = field("body")
                .map_err(threw)?
                .dyn_into()
                .map_err(|_| misbehaved("host getStream gave no ReadableStream body"))?;
            let body = stream::from_js(body)
                .map_err(|_| misbehaved("host getStream gave a locked body"))?;
            Ok(Some((size, body)))
        })
    }
}
