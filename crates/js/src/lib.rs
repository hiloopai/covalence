#![doc = include_str!("../README.md")]
#![deny(missing_docs)]
#![cfg(target_arch = "wasm32")]

mod digest;
mod errors;
mod host;
mod stream;

use covalence_blob_verify::{Expected, HashFn, Verifier as Check};
use covalence_cas_kv::KvCas;
use covalence_cas_trait::{AsyncGet, AsyncPut, Cas as _};
use covalence_kv_trait::Kv as _;
use covalence_lib_cid::{Blake3, Cid, Cid64, Format};
use errors::Code;
use js_sys::{Object, Reflect, Uint8Array};
use wasm_bindgen::prelude::*;
use web_sys::ReadableStream;

pub use digest::DigestAccumulator;
pub use host::HostStorage;

/// The largest whole number JavaScript numbers hold exactly: 2^53 - 1.
const MAX_LEN: u64 = (1 << 53) - 1;

/// `number` as a whole number from 0 to 2^53 - 1, if it is one.
fn whole(number: f64) -> Option<u64> {
    let whole = number >= 0.0 && number.fract() == 0.0 && number <= MAX_LEN as f64;
    whole.then_some(number as u64)
}

/// `length` as a byte count, or an invalid-argument error.
fn byte_count(length: f64) -> Result<u64, JsValue> {
    whole(length)
        .ok_or_else(|| errors::invalid("a length must be a whole number from 0 to 2^53 - 1"))
}

/// The CID `text`, with a digest of at most 64 bytes.
fn parse(text: &str) -> Result<Cid64, JsValue> {
    text.parse().map_err(errors::cid)
}

/// The CID `text` as a store key. A digest over 32 bytes is invalid input,
/// as no stored block has one.
fn key(text: &str) -> Result<Cid, JsValue> {
    let cid = parse(text)?;
    Cid::try_from(cid).map_err(|_| errors::invalid(&format!("{cid} has a digest over 32 bytes")))
}

/// A count as a JavaScript number, exact up to 2^53 - 1.
fn number(count: u64) -> JsValue {
    JsValue::from_f64(count as f64)
}

/// A content-addressed store whose blocks a `HostStorage` keeps.
///
/// It runs the Rust library's `KvCas` over the host storage, so block keys,
/// CIDs and checks are the Rust library's.
///
/// - Each block lies under `blake3/<digest>/bytes`, after the optional
///   prefix, where `<digest>` is its BLAKE3 digest in lowercase, unpadded
///   base32.
/// - Writes never replace a block.
/// - A CID with a 32-byte BLAKE3 digest finds its block whatever its codec;
///   other CIDs find nothing.
/// - Reads are checked against the CID, except `size`.
#[wasm_bindgen]
pub struct Cas(KvCas);

#[wasm_bindgen]
impl Cas {
    /// A store keeping blocks in `host`, under `prefix/` if given.
    ///
    /// Throws an `"invalid"` error unless `prefix` is `/`-separated, non-empty
    /// segments other than `.` and `..`, optionally ending in `/`.
    #[wasm_bindgen(constructor)]
    pub fn new(host: HostStorage, prefix: Option<String>) -> Result<Cas, JsValue> {
        let kv = host::JsKv::new(host).into_dyn();
        Ok(Cas(match prefix {
            None => KvCas::new(kv),
            Some(prefix) => KvCas::with_prefix(kv, &prefix)
                .map_err(|error| errors::invalid(&error.to_string()))?,
        }))
    }

    /// Store `bytes` through the host's `putNew` and return their raw BLAKE3
    /// CID.
    ///
    /// Throws a `"store"` error if the host fails.
    pub async fn put(&self, bytes: Vec<u8>) -> Result<String, JsValue> {
        let cid = self.0.aput(bytes.into()).await.map_err(errors::cas)?;
        Ok(cid.to_string())
    }

    /// The bytes under `cid`, checked, or `null` if missing.
    ///
    /// - Throws an `"integrity"` error if they do not match `cid`.
    /// - Throws an `"invalid"` error if `cid` is not CID text or has a digest
    ///   over 32 bytes, and a `"store"` error if the host fails.
    #[wasm_bindgen(unchecked_return_type = "Uint8Array | null")]
    pub async fn get(&self, cid: String) -> Result<JsValue, JsValue> {
        let cid = key(&cid)?;
        Ok(match self.0.aget(&cid).await.map_err(errors::cas)? {
            Some(bytes) => Uint8Array::from(&bytes[..]).into(),
            None => JsValue::NULL,
        })
    }

    /// The size the host reports for the block under `cid`, unchecked, or
    /// `null` if missing.
    ///
    /// Throws as `get` does, except that nothing is checked.
    #[wasm_bindgen(unchecked_return_type = "number | null")]
    pub async fn size(&self, cid: String) -> Result<JsValue, JsValue> {
        let cid = key(&cid)?;
        Ok(match self.0.asize(&cid).await.map_err(errors::cas)? {
            Some(size) => number(size),
            None => JsValue::NULL,
        })
    }

    /// Store the `length` bytes of `body`, whose CID is `cid`, through the
    /// host's `putNewFrom`, and return its raw BLAKE3 CID.
    ///
    /// - Reads and checks every byte, even if the block already exists, so
    ///   success shows the caller had the bytes, not only the CID.
    /// - `cid` must be a BLAKE3 CID with a 32-byte digest, of any codec.
    /// - Reads `body` only as the host reads, so a slow host slows the body.
    /// - Holds back the last chunk from the host until every byte has
    ///   arrived, `body` has ended and the bytes match `cid`, so the host
    ///   never stores other bytes.
    /// - If the host fails, cancels whatever of `body` it left unread.
    ///
    /// Throws, and the host stores nothing:
    ///
    /// - `"integrity"` if the bytes have another CID.
    /// - `"io"` if `body` errors, with what it threw as the `cause`, or does
    ///   not yield exactly `length` bytes, or yields a chunk that is not a
    ///   `Uint8Array`.
    /// - `"invalid"` if `cid` is not CID text or has a digest over 32 bytes,
    ///   `length` is not a whole number from 0 to 2^53 - 1, or `body` is
    ///   locked.
    /// - `"unsupported"` if `cid` is not a BLAKE3 CID with a 32-byte digest.
    /// - `"store"` if the host fails.
    ///
    /// `cid` comes first because the host key is derived from it. A caller
    /// that does not know it must learn it before calling: from a client that
    /// declares it, which this checks, or with a `Hasher` while the host
    /// stages the bytes. `putFrom` never stages or buffers a block to compute
    /// its CID.
    #[wasm_bindgen(js_name = putFrom)]
    pub async fn put_from(
        &self,
        cid: String,
        #[wasm_bindgen(unchecked_param_type = "ReadableStream<Uint8Array>")] body: ReadableStream,
        length: f64,
    ) -> Result<String, JsValue> {
        let cid = key(&cid)?;
        let claim = cid.raw_blake3().ok_or_else(|| {
            let message = format!("{cid} is not a BLAKE3 CID with a 32-byte digest");
            errors::error(Code::Unsupported, &message, None)
        })?;
        let len = byte_count(length)?;
        let body = stream::from_js(body).map_err(|_| errors::invalid("the body is locked"))?;
        let stored = self.0.aput_from(claim, body, len).await;
        Ok(stored.map_err(errors::cas)?.to_string())
    }

    /// The block under `cid` as a `SizedStream`, or `null` if missing.
    ///
    /// - `body` reads from the host only as it is read.
    /// - It holds back the last chunk until every byte has arrived and they
    ///   match `cid`. If they do not, it errors in place of the last chunk,
    ///   or as its only item for an empty block, with an `"integrity"` or
    ///   `"io"` error; discard what it yielded before.
    /// - If the host's stream errors, `body` errors with a `"store"` error
    ///   whose `cause` is what the host's stream threw.
    /// - Throws as `get` does.
    #[wasm_bindgen(js_name = getStream, unchecked_return_type = "SizedStream | null")]
    pub async fn get_stream(&self, cid: String) -> Result<JsValue, JsValue> {
        let cid = key(&cid)?;
        let Some((size, body)) = self.0.aget_stream(&cid).await.map_err(errors::cas)? else {
            return Ok(JsValue::NULL);
        };
        let sized = Object::new();
        // Defining properties on a new, extensible object cannot fail.
        let _ = Reflect::set(&sized, &"size".into(), &number(size));
        let body = stream::to_js(body, Code::Store, None);
        let _ = Reflect::set(&sized, &"body".into(), &body.into());
        Ok(sized.into())
    }
}

/// A check of a byte stream against a CID and length, fed chunk by chunk.
///
/// It holds only the hash state, never the bytes.
#[wasm_bindgen]
pub struct Verifier(Check);

#[wasm_bindgen]
impl Verifier {
    /// A check that the stream has `length` bytes and the digest of `cid`.
    ///
    /// - `cid` may have any codec; its hash must be BLAKE3 or SHA-256, with a
    ///   32-byte digest.
    /// - Throws an `"invalid"` error if `cid` is not CID text or `length` is
    ///   not a whole number from 0 to 2^53 - 1.
    /// - Throws an `"unsupported"` error for another hash or digest length.
    #[wasm_bindgen(constructor)]
    pub fn new(cid: &str, length: f64) -> Result<Verifier, JsValue> {
        let cid = parse(cid)?;
        let unsupported = || {
            let error = covalence_lib_cid::Error::Hash {
                hash: cid.hash(),
                len: cid.digest().len(),
            };
            errors::cid(error)
        };
        let hash = HashFn::from_code(cid.hash()).ok_or_else(unsupported)?;
        let digest = <[u8; 32]>::try_from(cid.digest()).map_err(|_| unsupported())?;
        let expected = Expected::new(byte_count(length)?, hash, digest);
        Ok(Verifier(Check::new(expected)))
    }

    /// Hash the next `chunk`.
    ///
    /// Throws an `"io"` error, hashing none of it, if it would make the
    /// stream longer than expected.
    pub fn update(&mut self, chunk: &[u8]) -> Result<(), JsValue> {
        self.0.update(chunk).map_err(errors::verify)
    }

    /// Check the bytes so far.
    ///
    /// Throws an `"io"` error if fewer than expected have been seen, or an
    /// `"integrity"` error if all have and the digest differs.
    pub fn finish(&self) -> Result<(), JsValue> {
        self.0.finish().map_err(errors::verify)?;
        Ok(())
    }

    /// The number of bytes seen so far.
    #[wasm_bindgen(getter)]
    pub fn received(&self) -> f64 {
        self.0.received() as f64
    }

    /// The number of bytes still expected.
    #[wasm_bindgen(getter)]
    pub fn remaining(&self) -> f64 {
        self.0.remaining() as f64
    }
}

/// The raw BLAKE3 CID of a stream of bytes, hashed chunk by chunk.
///
/// - For bytes whose CID is not known in advance: hash them as they pass,
///   for example while the host stages them, then store them with
///   `Cas.putFrom` under the CID it gives.
/// - It holds only the hash state, never the bytes. Staging is the host's
///   job; `Cas` never stages a block to compute its CID.
#[wasm_bindgen]
pub struct Hasher {
    hasher: blake3::Hasher,
    received: u64,
}

#[wasm_bindgen]
impl Hasher {
    /// A hasher that has seen no bytes.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Hasher {
        Hasher {
            hasher: blake3::Hasher::new(),
            received: 0,
        }
    }

    /// Hash the next `chunk`.
    pub fn update(&mut self, chunk: &[u8]) {
        self.hasher.update(chunk);
        self.received += chunk.len() as u64;
    }

    /// The raw BLAKE3 CID of the bytes so far, as `blake3Cid` gives for them
    /// whole.
    ///
    /// Later chunks may still be added.
    pub fn finish(&self) -> String {
        let digest = self.hasher.finalize();
        Blake3::new(Format::BLAKE3, digest.as_bytes())
            .expect("a BLAKE3 digest makes a raw BLAKE3 CID")
            .to_string()
    }

    /// The number of bytes hashed so far, exact up to 2^53 - 1.
    #[wasm_bindgen(getter)]
    pub fn received(&self) -> f64 {
        self.received as f64
    }
}

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

/// The binary form of the CID `text`.
///
/// Throws an `"invalid"` error unless `text` is a CID in the form `formatCid`
/// gives, with a digest of at most 64 bytes.
#[wasm_bindgen(js_name = parseCid)]
pub fn parse_cid(text: &str) -> Result<Uint8Array, JsValue> {
    Ok(parse(text)?.to_bytes().as_slice().into())
}

/// The text of the binary CID `bytes`: `b` and lowercase, unpadded base32.
///
/// Throws an `"invalid"` error unless `bytes` are exactly one CIDv1 with a
/// digest of at most 64 bytes.
#[wasm_bindgen(js_name = formatCid)]
pub fn format_cid(bytes: &[u8]) -> Result<String, JsValue> {
    Ok(Cid64::from_bytes(bytes).map_err(errors::cid)?.to_string())
}

/// The raw BLAKE3 CID of `bytes`, as `Cas.put` would return it.
#[wasm_bindgen(js_name = blake3Cid)]
pub fn blake3_cid(bytes: &[u8]) -> String {
    Blake3::of(bytes).to_string()
}
