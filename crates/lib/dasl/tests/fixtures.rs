use covalence_lib_dasl::{Builder, Value};
use data_encoding::HEXLOWER_PERMISSIVE;

/// Valid DRISL, but outside i64 or a float.
const REJECTED: [&str; 7] = [
    "largest CBOR integer, 2^64-1",
    "smallest CBOR integer, -(2^64)",
    "smallest unsigned bigint",
    "highest negative bigint",
    "64-bit floats only",
    "zero float64",
    "64-bit subnormal number",
];

#[test]
fn dasl_fixtures() {
    let fixtures = [
        include_str!("fixtures/map_keys.json"),
        include_str!("fixtures/utf8.json"),
        include_str!("fixtures/short_form.json"),
        include_str!("fixtures/integer_range.json"),
        include_str!("fixtures/cid.json"),
        include_str!("fixtures/tags.json"),
        include_str!("fixtures/floats.json"),
        include_str!("fixtures/indefinite.json"),
        include_str!("fixtures/concat.json"),
        include_str!("fixtures/simple.json"),
        include_str!("fixtures/numeric_reduction.json"),
    ];
    let mut checked = 0;
    for fixture in fixtures {
        let vectors: Vec<serde_json::Value> = serde_json::from_str(fixture).unwrap();
        for vector in vectors {
            let name = vector["name"].as_str().unwrap();
            let data = vector["data"].as_str().unwrap();
            let bytes = HEXLOWER_PERMISSIVE.decode(data.as_bytes()).unwrap();
            if REJECTED.contains(&name) {
                assert!(Value::decode(&bytes).is_err(), "accepted {name}");
                checked += 1;
                continue;
            }
            let tags = vector["tags"].as_array().unwrap();
            if !tags
                .iter()
                .any(|tag| matches!(tag.as_str(), Some("basic" | "dag-cbor" | "dasl-cid")))
            {
                continue;
            }
            match vector["type"].as_str().unwrap() {
                "roundtrip" => {
                    let value =
                        Value::decode(&bytes).unwrap_or_else(|error| panic!("{name}: {error}"));
                    assert_eq!(value.encode().unwrap(), bytes, "{name}");
                    let mut builder = Builder::default();
                    value
                        .events()
                        .try_for_each(|event| builder.push(event))
                        .unwrap();
                    assert_eq!(builder.finish().unwrap(), value, "{name}");
                    let text = value.to_json().unwrap();
                    assert_eq!(Value::from_json(&text).unwrap(), value, "{name}");
                }
                "invalid_in" => assert!(Value::decode(&bytes).is_err(), "accepted {name}"),
                "invalid_out" => continue,
                other => panic!("unknown fixture type {other}"),
            }
            checked += 1;
        }
    }
    assert_eq!(checked, 85);
}
