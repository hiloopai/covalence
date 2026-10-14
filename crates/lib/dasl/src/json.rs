//! JSON text for DASL values, in the ATProto form.
//!
//! Links are `{"$link": "<CID>"}` and bytes are `{"$bytes": "<base64>"}`,
//! unpadded both ways: padded base64 is rejected. Any other map with a
//! `$link` or `$bytes` key is rejected.
//!
//! ```
//! use covalence_lib_dasl::Value;
//! let value = Value::List(vec![Value::Integer(1), Value::Null]);
//! assert_eq!(value.to_json()?, "[1,null]");
//! assert_eq!(Value::from_json("[1,null]")?, value);
//! # Ok::<(), covalence_lib_dasl::Error>(())
//! ```

use std::{
    cell::Cell,
    collections::{BTreeMap, btree_map::Entry},
    fmt::{self, Write},
};

use covalence_lib_serde::serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use data_encoding::BASE64_NOPAD;

use crate::{Error, Event, Value, check};

const RESERVED: [&str; 2] = ["$link", "$bytes"];

/// A list or map being written. A map's `len` counts keys and values.
struct Open {
    map: bool,
    len: usize,
    done: usize,
}

impl Value {
    /// Write as JSON, with map keys in DRISL order.
    pub fn to_json(&self) -> Result<String, Error> {
        check(self)?;
        let mut out = String::new();
        let mut open: Vec<Open> = Vec::new();
        for event in self.events() {
            let top = open.last();
            if let Some(top) = top.filter(|top| top.done > 0) {
                out.push(if top.map && top.done % 2 == 1 {
                    ':'
                } else {
                    ','
                });
            }
            let key = top.is_some_and(|top| top.map && top.done % 2 == 0);
            match event {
                Event::Null => out.push_str("null"),
                Event::Bool(value) => write!(out, "{value}").expect("String"),
                Event::Integer(value) => write!(out, "{value}").expect("String"),
                Event::Bytes(value) => {
                    write!(out, r#"{{"$bytes":"{}"}}"#, BASE64_NOPAD.encode(value)).expect("String")
                }
                Event::String(value) if key && RESERVED.contains(&value) => {
                    return Err(json("a map key is $link or $bytes"));
                }
                Event::String(value) => {
                    out.push_str(&serde_json::to_string(value).expect("a string"))
                }
                Event::Link(cid) => write!(out, r#"{{"$link":"{cid}"}}"#).expect("String"),
                Event::List(0) => out.push_str("[]"),
                Event::Map(0) => out.push_str("{}"),
                Event::List(len) => {
                    out.push('[');
                    open.push(Open {
                        map: false,
                        len,
                        done: 0,
                    });
                    continue;
                }
                Event::Map(len) => {
                    out.push('{');
                    open.push(Open {
                        map: true,
                        len: 2 * len,
                        done: 0,
                    });
                    continue;
                }
            }
            // A key or value is complete: close the containers it completes.
            while let Some(top) = open.last_mut() {
                top.done += 1;
                if top.done < top.len {
                    break;
                }
                out.push(if top.map { '}' } else { ']' });
                open.pop();
            }
        }
        Ok(out)
    }

    /// Read one value from JSON.
    ///
    /// Numbers with a fraction or exponent, and `-0`, give [`Error::Float`].
    /// Integers outside i64 and duplicate keys give [`Error::Json`].
    pub fn from_json(text: &str) -> Result<Value, Error> {
        let error = Cell::new(None);
        let mut deserializer = serde_json::Deserializer::from_str(text);
        let value = Seed(&error)
            .deserialize(&mut deserializer)
            .and_then(|value| deserializer.end().map(|()| value))
            .map_err(|source| error.take().unwrap_or_else(|| json(source)))?;
        check(&value)?;
        Ok(value)
    }
}

fn json(message: impl ToString) -> Error {
    Error::Json {
        message: message.to_string(),
    }
}

/// Builds a [`Value`], keeping the first error that is not a JSON error.
#[derive(Clone, Copy)]
struct Seed<'a>(&'a Cell<Option<Error>>);

impl Seed<'_> {
    fn fail<E: de::Error>(self, error: Error) -> E {
        let message = error.to_string();
        self.0.set(Some(error));
        E::custom(message)
    }
}

impl<'de> DeserializeSeed<'de> for Seed<'_> {
    type Value = Value;

    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Seed<'_> {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a DASL value")
    }
    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }
    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Integer(value))
    }
    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        i64::try_from(value)
            .map(Value::Integer)
            .map_err(|_| E::custom(format_args!("integer {value} is outside i64")))
    }
    // serde_json reads integers outside u64 and i64 as f64 too. An exponent
    // past ±2^63 cannot be told apart from them.
    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        if value.fract() == 0.0 && value.abs() >= 2f64.powi(63) {
            return Err(E::custom("integer is outside i64"));
        }
        Err(self.fail(Error::Float))
    }
    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element_seed(self)? {
            items.push(item);
        }
        Ok(Value::List(items))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut entries = BTreeMap::new();
        while let Some(key) = map.next_key::<String>()? {
            match entries.entry(key) {
                Entry::Vacant(entry) => entry.insert(map.next_value_seed(self)?),
                Entry::Occupied(entry) => {
                    return Err(de::Error::custom(format_args!(
                        "duplicate key {:?}",
                        entry.key()
                    )));
                }
            };
        }
        if !entries.keys().any(|key| RESERVED.contains(&key.as_str())) {
            return Ok(Value::Map(entries));
        }
        let Some((key, Value::String(text))) = entries.pop_first().filter(|_| entries.is_empty())
        else {
            return Err(de::Error::custom(
                "$link and $bytes must be the only key and hold a string",
            ));
        };
        if key == "$link" {
            let cid = text
                .parse()
                .map_err(|source| self.fail(Error::Cid { source }))?;
            return Ok(Value::Link(cid));
        }
        BASE64_NOPAD
            .decode(text.as_bytes())
            .map(|bytes| Value::Bytes(bytes.into()))
            .map_err(de::Error::custom)
    }
}
