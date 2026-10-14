use covalence_lib_cid::{Bdasl, Cid, Codec, HashFn};
use covalence_lib_dasl::{Error, MAX_DEPTH, Value};

fn round_trip(value: &Value, text: &str) {
    assert_eq!(value.to_json().unwrap(), text);
    assert_eq!(&Value::from_json(text).unwrap(), value);
}

fn rejected(text: &str) -> Error {
    Value::from_json(text).expect_err(text)
}

fn map<const N: usize>(entries: [(&str, Value); N]) -> Value {
    Value::Map(entries.map(|(key, value)| (key.into(), value)).into())
}

#[test]
fn scalars_and_containers() {
    round_trip(&Value::Null, "null");
    round_trip(&Value::Bool(true), "true");
    round_trip(&Value::String("\"\\\n\u{1f}é".into()), r#""\"\\\n\u001fé""#);
    round_trip(&Value::List(vec![]), "[]");
    round_trip(&map([]), "{}");
    round_trip(
        &Value::List(vec![map([("a", Value::List(vec![]))]), Value::Null]),
        r#"[{"a":[]},null]"#,
    );
}

#[test]
fn keys_in_drisl_order() {
    let value = map([
        ("aa", Value::Integer(1)),
        ("b", Value::Integer(2)),
        ("", map([("ab", Value::Null), ("c", Value::Null)])),
    ]);
    round_trip(&value, r#"{"":{"c":null,"ab":null},"b":2,"aa":1}"#);
    assert_eq!(
        Value::from_json(r#"{"aa":1,"":{"ab":null,"c":null},"b":2}"#).unwrap(),
        value
    );
}

#[test]
fn numbers() {
    round_trip(&Value::Integer(i64::MIN), "-9223372036854775808");
    round_trip(&Value::Integer(i64::MAX), "9223372036854775807");
    for text in [
        "9223372036854775808",
        "-9223372036854775809",
        "18446744073709551616",
        "-18446744073709551617",
        "100000000000000000000000",
    ] {
        assert!(matches!(rejected(text), Error::Json { .. }), "{text}");
    }
    for text in ["-0", "-0.0", "0.0", "1.5", "1e3", r#"{"a":[1,1.5]}"#] {
        assert!(matches!(rejected(text), Error::Float), "{text}");
    }
    let text = r#"{"1\"9223372036854775808":"99999999999999999999"}"#;
    assert!(Value::from_json(text).is_ok());
}

#[test]
fn links_and_bytes() {
    let cid = Cid::<Bdasl>::of(Codec::Drisl, HashFn::Blake3, b"content");
    round_trip(&Value::Link(cid), &format!(r#"{{"$link":"{cid}"}}"#));
    round_trip(
        &Value::Bytes(b"hi".as_slice().into()),
        r#"{"$bytes":"aGk"}"#,
    );
    let upper = cid.to_string().to_uppercase();
    for text in [
        "not a cid".into(),
        format!("/ipfs/{cid}"),
        format!("z{}", &cid.to_string()[1..]),
        upper,
    ] {
        let text = format!(r#"{{"$link":"{text}"}}"#);
        assert!(matches!(rejected(&text), Error::Cid { .. }), "{text}");
    }
    for text in [
        r#"{"$bytes":"aGk="}"#,
        r#"{"$bytes":"aG=k"}"#,
        r#"{"$bytes":"aGl"}"#,
        r#"{"$bytes":"a!"}"#,
        r#"{"$bytes":1}"#,
        r#"{"$link":null}"#,
    ] {
        assert!(matches!(rejected(text), Error::Json { .. }), "{text}");
    }
}

#[test]
fn reserved_keys() {
    for key in ["$link", "$bytes"] {
        for value in [
            map([(key, Value::String("x".into()))]),
            map([(key, Value::Null), ("$type", Value::String("app".into()))]),
            Value::List(vec![map([(key, Value::Null)])]),
        ] {
            assert!(
                matches!(value.to_json(), Err(Error::Json { .. })),
                "{value:?}"
            );
        }
        let text = format!(r#"{{"{key}":"aGk","$type":"app"}}"#);
        assert!(matches!(rejected(&text), Error::Json { .. }), "{text}");
    }
    round_trip(&map([("$type", Value::Null)]), r#"{"$type":null}"#);
}

#[test]
fn duplicate_keys_and_trailing_text() {
    for text in [
        r#"{"a":1,"a":1}"#,
        r#"{"$link":"x","$link":"x"}"#,
        "1 2",
        "[",
    ] {
        assert!(matches!(rejected(text), Error::Json { .. }), "{text}");
    }
}

#[test]
fn depth_limit() {
    let mut value = Value::Link(Cid::<Bdasl>::of(Codec::Raw, HashFn::Sha256, b""));
    for _ in 0..MAX_DEPTH {
        value = Value::List(vec![value]);
    }
    let text = value.to_json().unwrap();
    assert_eq!(Value::from_json(&text).unwrap(), value);

    let value = Value::List(vec![value]);
    assert!(matches!(value.to_json(), Err(Error::TooDeep)));
    let text = format!(
        "{}0{}",
        "[".repeat(MAX_DEPTH + 1),
        "]".repeat(MAX_DEPTH + 1)
    );
    assert!(matches!(rejected(&text), Error::TooDeep));
    assert!(matches!(rejected(&"[".repeat(100_000)), Error::Json { .. }));
}
