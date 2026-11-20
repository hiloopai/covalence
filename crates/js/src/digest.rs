use covalence_blob_verify::{DigestAccumulator as Accumulator, DigestAlgorithms, DigestError};
use js_sys::{Object, Reflect, Uint8Array};
use wasm_bindgen::{JsCast, prelude::*};

use crate::{
    MAX_LEN, byte_count,
    errors::{self, Code},
    number,
};

const CHUNK_BYTES: u64 = 64 * 1024;

#[wasm_bindgen(typescript_custom_section)]
const DIGEST_TYPES: &str = r#"
/** Which digests to compute in one pass. */
export type DigestAlgorithms = "sha256" | "blake3" | "both";

/** The observed byte length and requested digests, not proof of integrity. */
export interface DigestResult {
    /** The exact number of bytes hashed. */
    readonly len: number;
    /** The 32-byte SHA-256 digest, or null if not requested. */
    readonly sha256: Uint8Array | null;
    /** The 32-byte BLAKE3 digest, or null if not requested. */
    readonly blake3: Uint8Array | null;
    /** The raw BLAKE3 CID text, or null if BLAKE3 was not requested. */
    readonly rawBlake3: string | null;
}
"#;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = Uint8Array, js_name = "prototype.subarray.call", catch)]
    fn subarray(chunk: &Uint8Array, start: f64, end: f64) -> Result<JsValue, JsValue>;
}

struct State {
    digest: Accumulator,
    max_bytes: u64,
    buffer: Vec<u8>,
}

/// A bounded measurement of bytes fed in order, without taking over a stream.
#[wasm_bindgen]
pub struct DigestAccumulator(Option<State>);

#[wasm_bindgen]
impl DigestAccumulator {
    /// Start measuring with the chosen algorithms and an inclusive byte limit.
    ///
    /// Defaults to both algorithms and a limit of `2**53 - 1` bytes. Unknown
    /// algorithm names and limits outside the exact integer range throw an
    /// `"invalid"` error.
    #[wasm_bindgen(constructor)]
    #[allow(non_snake_case)]
    pub fn new(
        #[wasm_bindgen(unchecked_optional_param_type = "DigestAlgorithms")] algorithms: Option<
            JsValue,
        >,
        #[wasm_bindgen(unchecked_optional_param_type = "number")] maxBytes: Option<JsValue>,
    ) -> Result<Self, JsValue> {
        let algorithms = match algorithms.as_ref().map(JsValue::as_string) {
            None => DigestAlgorithms::Both,
            Some(Some(name)) => match name.as_str() {
                "sha256" => DigestAlgorithms::Sha256,
                "blake3" => DigestAlgorithms::Blake3,
                "both" => DigestAlgorithms::Both,
                _ => return Err(invalid_algorithms()),
            },
            Some(None) => return Err(invalid_algorithms()),
        };
        let max_bytes = match maxBytes {
            None => MAX_LEN,
            Some(value) => byte_count(value.as_f64().unwrap_or(f64::NAN))?,
        };
        Ok(Self(Some(State {
            digest: Accumulator::new(algorithms, Some(max_bytes)),
            max_bytes,
            buffer: Vec::new(),
        })))
    }

    /// Hash the next chunk, copying at most 64 KiB at a time into WebAssembly.
    ///
    /// If the chunk would exceed the byte limit, hashes none of it, closes
    /// the accumulator and throws an `"io"` error. A non-`Uint8Array` chunk
    /// or a call after closing throws an `"invalid"` error.
    pub fn update(
        &mut self,
        #[wasm_bindgen(unchecked_param_type = "Uint8Array")] chunk: &JsValue,
    ) -> Result<(), JsValue> {
        let chunk = chunk.dyn_ref::<Uint8Array>().ok_or_else(invalid_chunk)?;
        let mut state = self.0.take().ok_or_else(closed)?;
        let len = chunk_len(chunk)?;
        let remaining = state.digest.remaining().map_err(limit)?;
        if len > remaining {
            return Err(limit(DigestError::TooLong {
                max_len: state.max_bytes,
            }));
        }
        if len == 0 {
            if chunk.length() != 0 {
                return Err(invalid_chunk());
            }
            self.0 = Some(state);
            return Ok(());
        }
        let window = len.min(CHUNK_BYTES) as usize;
        if state.buffer.len() < window {
            state.buffer.resize(window, 0);
        }
        if len <= CHUNK_BYTES {
            let count = len as usize;
            if chunk.length() as usize != count {
                return Err(invalid_chunk());
            }
            chunk.copy_to(&mut state.buffer[..count]);
            state.digest.update(&state.buffer[..count]).map_err(limit)?;
        } else {
            let mut start = 0;
            while start < len {
                let end = (start + CHUNK_BYTES).min(len);
                let view = subarray(chunk, start as f64, end as f64).map_err(|cause| {
                    errors::error(Code::Invalid, "failed to slice a digest chunk", Some(cause))
                })?;
                let view = view.dyn_ref::<Uint8Array>().ok_or_else(invalid_chunk)?;
                let count = end - start;
                if chunk_len(view)? != count || u64::from(view.length()) != count {
                    return Err(invalid_chunk());
                }
                let count = count as usize;
                view.copy_to(&mut state.buffer[..count]);
                state.digest.update(&state.buffer[..count]).map_err(limit)?;
                start = end;
            }
        }
        self.0 = Some(state);
        Ok(())
    }

    /// Return the measurement and close the accumulator.
    ///
    /// Throws an `"invalid"` error if the accumulator is already closed.
    #[wasm_bindgen(unchecked_return_type = "DigestResult")]
    pub fn finish(&mut self) -> Result<JsValue, JsValue> {
        let result = self
            .0
            .take()
            .ok_or_else(closed)?
            .digest
            .finish()
            .map_err(limit)?;
        let value = Object::new();
        for (key, field) in [
            ("len", number(result.len())),
            ("sha256", digest_bytes(result.sha256())),
            ("blake3", digest_bytes(result.blake3())),
            (
                "rawBlake3",
                result
                    .raw_blake3()
                    .map_or(JsValue::NULL, |cid| cid.to_string().into()),
            ),
        ] {
            Reflect::set(&value, &key.into(), &field)?;
        }
        Ok(value.into())
    }
}

fn chunk_len(chunk: &Uint8Array) -> Result<u64, JsValue> {
    let value = Reflect::get(chunk, &"length".into()).map_err(|cause| {
        errors::error(
            Code::Invalid,
            "a digest chunk must be a Uint8Array",
            Some(cause),
        )
    })?;
    byte_count(value.as_f64().unwrap_or(f64::NAN))
}

fn digest_bytes(digest: Option<&[u8; 32]>) -> JsValue {
    digest.map_or(JsValue::NULL, |bytes| Uint8Array::from(&bytes[..]).into())
}

fn invalid_algorithms() -> JsValue {
    errors::invalid("algorithms must be 'sha256', 'blake3' or 'both'")
}

fn invalid_chunk() -> JsValue {
    errors::invalid("a digest chunk must be a Uint8Array")
}

fn closed() -> JsValue {
    errors::invalid("the digest accumulator is closed")
}

fn limit(error: DigestError) -> JsValue {
    errors::error(Code::Io, &error.to_string(), None)
}
