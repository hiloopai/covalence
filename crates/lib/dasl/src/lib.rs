#![doc = include_str!("../README.md")]
#![deny(missing_docs)]
//!
//! ## Example
//!
//! [`Value::from_fact`] decodes a checked [`Fact`], and [`Value::cid`] names a
//! value.
//!
//! ```
//! use covalence_lib_cid::Fact;
//! use covalence_lib_dasl::Value;
//! let value = Value::List(vec![Value::Integer(42), Value::String("hello".into())]);
//! assert_eq!(Value::decode(&value.encode()?)?, value);
//! let fact = Fact::new(value.cid()?, value.encode()?.into())?;
//! assert_eq!(Value::from_fact(&fact)?, value);
//! # Ok::<(), covalence_lib_dasl::Error>(())
//! ```

mod drisl;
mod event;
#[cfg(feature = "json")]
mod json;

use std::{collections::BTreeMap, io::Write};

use covalence_lib_cid::{Bdasl, Cid, CidFormat, Codec, Fact, HashFn, Multicodec};
use covalence_lib_data::Bytes;
use covalence_lib_error::snafu::{Snafu, ensure};

pub use event::{Builder, Event, Events, Links};

/// The deepest nesting of lists and maps that encoding and decoding accept.
/// A scalar has depth 0 and `[]` has depth 1.
pub const MAX_DEPTH: usize = 126;

/// A DASL value. There are no floats.
///
/// Dropping a hand-built value nested far deeper than [`MAX_DEPTH`] can
/// overflow the stack. Decoded values are always within the limit.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Value {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// An integer. DRISL allows -2^64 to 2^64 - 1; [`Value::decode`] rejects
    /// integers outside i64.
    Integer(i64),
    /// A byte string.
    Bytes(Bytes),
    /// A UTF-8 string.
    String(String),
    /// A list of values.
    List(Vec<Value>),
    /// A map with string keys. Encoding writes keys in DRISL order: shorter
    /// keys first, then bytewise.
    Map(BTreeMap<String, Value>),
    /// A link to other data, by its DASL CID.
    Link(Cid<Bdasl>),
}

impl Value {
    /// Encode as DRISL bytes.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        self.encode_to(&mut out)?;
        Ok(out)
    }

    /// Encode as DRISL bytes into `out`, after checking the whole value.
    pub fn encode_to(&self, mut out: impl Write) -> Result<(), Error> {
        check(self)?;
        for event in self.events() {
            drisl::write(&mut out, event)?;
        }
        Ok(())
    }

    /// Decode exactly one DRISL value.
    pub fn decode(bytes: &[u8]) -> Result<Value, Error> {
        let mut builder = Builder::default();
        for event in drisl::Reader::new(bytes) {
            builder.push(event?)?;
        }
        builder.finish()
    }

    /// Decode a fact: DRISL bytes decode to their value, and raw bytes are
    /// [`Value::Bytes`].
    pub fn from_fact<S: CidFormat>(fact: &Fact<Cid<S>>) -> Result<Value, Error> {
        match Codec::from_code(fact.claim().codec()) {
            Some(Codec::Drisl) => Value::decode(fact.bytes()),
            Some(Codec::Raw) => Ok(Value::Bytes(fact.bytes().clone())),
            None => CodecSnafu {
                codec: fact.claim().codec(),
            }
            .fail(),
        }
    }

    /// Check that `bytes` hash to `cid`, then decode them as
    /// [`Value::from_fact`] does.
    pub fn from_cid_bytes<S: CidFormat>(cid: Cid<S>, bytes: Bytes) -> Result<Value, Error> {
        Value::from_fact(&Fact::new(cid, bytes)?)
    }

    /// The BLAKE3 DRISL CID of the encoding.
    pub fn cid(&self) -> Result<Cid<Bdasl>, Error> {
        let bytes = self.encode()?;
        Ok(Cid::<Bdasl>::of(Codec::Drisl, HashFn::Blake3, &bytes))
    }
}

/// Errors from encoding and decoding values.
#[derive(Debug, Snafu)]
#[snafu(crate_root(covalence_lib_error::snafu))]
#[non_exhaustive]
pub enum Error {
    /// The bytes are not one DRISL value, or the events pushed to a
    /// [`Builder`] are not one value.
    #[snafu(display("invalid DRISL: {message}"))]
    Malformed {
        /// What is wrong.
        message: &'static str,
    },
    /// A link is not a DASL CID, or bytes do not hash to their CID.
    #[snafu(display("invalid CID"), context(false))]
    Cid {
        /// The CID error.
        source: covalence_lib_cid::Error,
    },
    /// A fact's codec is neither DRISL nor raw.
    #[snafu(display("cannot decode codec {codec:?}"))]
    Codec {
        /// The fact's codec.
        codec: Multicodec,
    },
    /// The input holds a float.
    #[snafu(display("floats are not supported"))]
    Float,
    /// Writing the encoding failed.
    #[snafu(display("could not write DRISL"), context(false))]
    Io {
        /// The write error.
        source: std::io::Error,
    },
    /// A value is nested deeper than [`MAX_DEPTH`].
    #[snafu(display("value is nested deeper than {MAX_DEPTH}"))]
    TooDeep,
    /// The JSON is invalid or not a DASL value, or a value cannot be written
    /// as JSON because a map has a `$link` or `$bytes` key.
    #[cfg(feature = "json")]
    #[snafu(display("invalid JSON: {message}"))]
    Json {
        /// What is wrong.
        message: String,
    },
}

fn check(value: &Value) -> Result<(), Error> {
    let mut stack = vec![(value, 0)];
    while let Some((value, depth)) = stack.pop() {
        match value {
            Value::List(items) => {
                ensure!(depth < MAX_DEPTH, TooDeepSnafu);
                stack.extend(items.iter().map(|item| (item, depth + 1)));
            }
            Value::Map(map) => {
                ensure!(depth < MAX_DEPTH, TooDeepSnafu);
                stack.extend(map.values().map(|item| (item, depth + 1)));
            }
            _ => {}
        }
    }
    Ok(())
}
