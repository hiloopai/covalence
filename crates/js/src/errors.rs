//! The binding's errors, as JavaScript `Error`s with a `name`, a `code` and,
//! where the host or a body threw, that value as the `cause`.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    error, fmt, io,
};

use js_sys::Reflect;
use wasm_bindgen::{JsCast, prelude::*};

#[wasm_bindgen(typescript_custom_section)]
const ERRORS: &str = r#"
/**
 * What an error the binding throws is about, in its `code`:
 *
 * - `"invalid"`: an argument is malformed, such as CID text, a length or a
 *   prefix. The `name` is `"InvalidError"`.
 * - `"integrity"`: bytes do not match their CID. `"IntegrityError"`.
 * - `"unsupported"`: the binding does not support the CID's hash or digest
 *   length. `"UnsupportedError"`.
 * - `"store"`: the host storage failed or gave an answer it should not
 *   have. `"StoreError"`.
 * - `"io"`: a body failed, or had too few or too many bytes. `"IoError"`.
 *
 * Where the host or a body threw, `cause` is the value it threw.
 */
export type ErrorCode = "invalid" | "integrity" | "unsupported" | "store" | "io";
"#;

/// What an error is about: its JavaScript `code` and `name`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Code {
    /// An argument is malformed.
    Invalid,
    /// Bytes do not match their CID.
    Integrity,
    /// The CID's hash or digest length is not supported.
    Unsupported,
    /// The host storage failed or misbehaved.
    Store,
    /// A body failed or had the wrong length.
    Io,
}

impl Code {
    fn code(self) -> &'static str {
        match self {
            Code::Invalid => "invalid",
            Code::Integrity => "integrity",
            Code::Unsupported => "unsupported",
            Code::Store => "store",
            Code::Io => "io",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Code::Invalid => "InvalidError",
            Code::Integrity => "IntegrityError",
            Code::Unsupported => "UnsupportedError",
            Code::Store => "StoreError",
            Code::Io => "IoError",
        }
    }
}

/// A JavaScript `Error` with `message`, the `name` and `code` of `code`, and
/// `cause` if given.
pub(crate) fn error(code: Code, message: &str, cause: Option<JsValue>) -> JsValue {
    let error = js_sys::Error::new(message);
    error.set_name(code.name());
    // Defining a property on a new, extensible `Error` cannot fail.
    let _ = Reflect::set(&error, &"code".into(), &code.code().into());
    if let Some(cause) = cause {
        error.set_cause(&cause);
    }
    error.into()
}

/// An invalid-argument error.
pub(crate) fn invalid(message: &str) -> JsValue {
    error(Code::Invalid, message, None)
}

/// The code of a CID error: mismatches are integrity errors, hashes this
/// crate cannot compute are unsupported, and the rest are invalid input.
fn cid_code(error: &covalence_lib_cid::Error) -> Code {
    match error {
        covalence_lib_cid::Error::Mismatch => Code::Integrity,
        covalence_lib_cid::Error::Hash { .. } => Code::Unsupported,
        _ => Code::Invalid,
    }
}

/// A CID error as a JavaScript error.
pub(crate) fn cid(error: covalence_lib_cid::Error) -> JsValue {
    self::error(cid_code(&error), &error.to_string(), None)
}

/// A store error as a JavaScript error, with what the host or a body threw
/// as its cause.
pub(crate) fn cas(error: covalence_cas_trait::Error) -> JsValue {
    use covalence_cas_trait::Error::*;
    let code = match &error {
        Mismatch => Code::Integrity,
        Check { source } => cid_code(source),
        Unsupported { .. } => Code::Unsupported,
        Io { .. } => Code::Io,
        _ => Code::Store,
    };
    self::error(code, &error.to_string(), thrown(&error))
}

/// A verification error as a JavaScript error: a mismatch is an integrity
/// error, a wrong length an I/O error.
pub(crate) fn verify(error: covalence_blob_verify::Error) -> JsValue {
    let code = match error {
        covalence_blob_verify::Error::Mismatch { .. } => Code::Integrity,
        _ => Code::Io,
    };
    self::error(code, &error.to_string(), None)
}

/// An error of a stream the binding hands to JavaScript: a failed check as
/// [`verify`] gives it, otherwise an error with `code` whose cause is what
/// the stream's source threw, if it threw.
pub(crate) fn stream(error: &io::Error, code: Code) -> JsValue {
    if let Some(failed) = covalence_blob_verify::Error::of(error) {
        return verify(*failed);
    }
    self::error(code, &error.to_string(), thrown(error))
}

thread_local! {
    static THROWN: RefCell<HashMap<u64, JsValue>> = RefCell::default();
    static NEXT: Cell<u64> = const { Cell::new(0) };
}

/// A value the host or a body threw, kept on this thread until the error
/// holding it is dropped.
///
/// Rust errors must be `Send` and `Sync`, which JavaScript values are not,
/// so the error holds a key to the value and its message.
#[derive(Debug)]
pub(crate) struct Thrown {
    key: u64,
    message: String,
}

impl Thrown {
    pub(crate) fn new(value: JsValue) -> Self {
        let message = message(&value);
        let key = NEXT.with(|next| {
            let key = next.get();
            next.set(key + 1);
            key
        });
        THROWN.with(|thrown| thrown.borrow_mut().insert(key, value));
        Self { key, message }
    }

    fn value(&self) -> Option<JsValue> {
        THROWN.with(|thrown| thrown.borrow().get(&self.key).cloned())
    }
}

impl Drop for Thrown {
    fn drop(&mut self) {
        THROWN.with(|thrown| thrown.borrow_mut().remove(&self.key));
    }
}

impl fmt::Display for Thrown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl error::Error for Thrown {}

/// The value thrown that caused `error`, if one did.
///
/// [`io::Error::source`] skips the error it wraps, so this also looks inside
/// each [`io::Error`].
fn thrown(error: &(dyn error::Error + 'static)) -> Option<JsValue> {
    let mut next = Some(error);
    while let Some(error) = next {
        if let Some(thrown) = error.downcast_ref::<Thrown>() {
            return thrown.value();
        }
        if let Some(inner) = error
            .downcast_ref::<io::Error>()
            .and_then(io::Error::get_ref)
            && let Some(value) = thrown(inner)
        {
            return Some(value);
        }
        next = error.source();
    }
    None
}

/// The message of a thrown value: an `Error`'s message, or its name if the
/// message is empty, or the value as text.
pub(crate) fn message(value: &JsValue) -> String {
    if let Some(error) = value.dyn_ref::<js_sys::Error>() {
        let message = String::from(error.message());
        if message.is_empty() {
            return error.name().into();
        }
        return message;
    }
    value.as_string().unwrap_or_else(|| format!("{value:?}"))
}
