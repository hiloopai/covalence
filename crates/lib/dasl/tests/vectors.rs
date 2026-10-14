//! Replays the vectors in `tests/vectors`: Rust must accept the same inputs,
//! with the same outputs, and reject the rest with the same error class.

use std::collections::BTreeSet;

use covalence_lib_cid::{Bdasl, Cid, CidFormat, Packed32, Packed64, storage};
use covalence_lib_dasl::{Error, Value};
use data_encoding::HEXLOWER;
use serde_json::{Value as Json, json};

fn vectors(text: &str) -> Vec<Json> {
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn field<'a>(vector: &'a Json, key: &str) -> Option<&'a str> {
    vector.get(key).map(|value| value.as_str().unwrap())
}

fn hex(text: &str) -> Vec<u8> {
    HEXLOWER.decode(text.as_bytes()).unwrap()
}

/// The vector's input, as in the vector.
fn input(vector: &Json) -> (&str, &str) {
    match (field(vector, "input"), field(vector, "input_text")) {
        (Some(input), None) => ("input", input),
        (None, Some(text)) => ("input_text", text),
        _ => panic!("no input: {vector}"),
    }
}

/// The fields the model writes for parsing the input as `S`.
fn parse_cid<S: CidFormat>(vector: &Json) -> Json {
    let parsed = match input(vector) {
        ("input", input) => Cid::<S>::from_bytes(&hex(input)),
        (_, text) => text.parse(),
    };
    match parsed {
        Ok(cid) => json!({
            "text": cid.to_string(),
            "bytes": HEXLOWER.encode(&cid.to_bytes()),
            "codec": cid.codec().0,
            "hash": cid.hash().0,
        }),
        Err(covalence_lib_cid::Error::Text) => json!({"error": "Text"}),
        Err(covalence_lib_cid::Error::Malformed) => json!({"error": "Malformed"}),
        Err(covalence_lib_cid::Error::Unsupported { format, len }) => json!({
            "error": "Unsupported",
            "codec": format.codec.0,
            "hash": format.hash.0,
            "len": len,
        }),
        Err(error) => panic!("unexpected parse error {error:?}"),
    }
}

/// The model's fields, where Rust has no `Length` or `Header` error: those are
/// `Malformed` for bytes that are not one CIDv1 (as `cid64` says), else
/// `Unsupported`, whose codec, hash and length Rust also reports.
fn expected_cid(vector: &Json, cids: &BTreeSet<(&str, &str)>) -> Json {
    let mut expected = vector.as_object().unwrap().clone();
    for key in ["name", "format", "input", "input_text", "header"] {
        expected.remove(key);
    }
    if matches!(field(vector, "error"), Some("Length" | "Header")) {
        if cids.contains(&input(vector)) {
            return Json::Null;
        }
        expected = json!({"error": "Malformed"}).as_object().unwrap().clone();
    }
    Json::Object(expected)
}

#[test]
fn cid_vectors() {
    let vectors = vectors(include_str!("vectors/cid-vectors.jsonl"));
    // Inputs that are one CIDv1, whatever its digest length.
    let cids: BTreeSet<_> = vectors
        .iter()
        .filter(|vector| {
            field(vector, "format") == Some("cid64")
                && !matches!(
                    field(vector, "error"),
                    Some("Text" | "Length" | "Malformed")
                )
        })
        .map(input)
        .collect();
    let mut checked = 0;
    let mut failures = Vec::new();
    for vector in &vectors {
        let actual = match field(vector, "format").unwrap() {
            "dasl" => parse_cid::<Bdasl>(vector),
            "blake3_raw" => parse_cid::<storage::Blake3>(vector),
            "sha256_raw" => parse_cid::<storage::Sha256>(vector),
            "cid32" => parse_cid::<Packed32>(vector),
            "cid64" => parse_cid::<Packed64>(vector),
            // Git object IDs are not CID storages here.
            "git" => continue,
            other => panic!("unknown format {other}"),
        };
        let expected = expected_cid(vector, &cids);
        let agrees = match expected {
            Json::Null => actual["error"] == "Unsupported",
            expected => actual == expected,
        };
        if !agrees {
            failures.push(format!("{vector}\n  rust: {actual}"));
        }
        checked += 1;
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert_eq!(checked, 215);
}

fn error_class(error: &Error) -> &'static str {
    match error {
        Error::Malformed { .. } => "Malformed",
        Error::Cid { .. } => "Cid",
        Error::Float => "Float",
        Error::TooDeep => "TooDeep",
        Error::Json { .. } => "Json",
        _ => panic!("unexpected error {error:?}"),
    }
}

/// The fields the model writes for a decoded value.
fn value_fields(value: &Value) -> Json {
    let mut fields = json!({
        "encoded": HEXLOWER.encode(&value.encode().unwrap()),
        "links": value.links().map(|cid| cid.to_string()).collect::<Vec<_>>(),
    });
    match value.to_json() {
        Ok(text) => fields["canonical"] = text.into(),
        Err(error) => fields["json_error"] = error_class(&error).into(),
    }
    fields
}

#[test]
fn dasl_vectors() {
    let mut checked = 0;
    let mut failures = Vec::new();
    for vector in vectors(include_str!("vectors/dasl-vectors.jsonl")) {
        let decoded = match (field(&vector, "drisl"), field(&vector, "json")) {
            (Some(drisl), None) => Value::decode(&hex(drisl)),
            (None, Some(text)) => Value::from_json(text),
            _ => panic!("no input: {vector}"),
        };
        let actual = match decoded {
            Ok(value) => value_fields(&value),
            Err(error) => json!({"error": error_class(&error)}),
        };
        let mut expected = vector.as_object().unwrap().clone();
        for key in ["name", "drisl", "json"] {
            expected.remove(key);
        }
        if actual != Json::Object(expected) {
            failures.push(format!("{vector}\n  rust: {actual}"));
        }
        checked += 1;
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert_eq!(checked, 440);
}
