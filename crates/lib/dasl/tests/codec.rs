use std::{
    collections::BTreeMap,
    io::{self, Write},
};

use covalence_lib_cid::{Bdasl, Cid, Codec, Format, HashFn};
use covalence_lib_dasl::{Error, MAX_DEPTH, Value};
use data_encoding::HEXLOWER;

fn hex(text: &str) -> Vec<u8> {
    HEXLOWER.decode(text.as_bytes()).unwrap()
}

fn decode(text: &str) -> Result<Value, Error> {
    Value::decode(&hex(text))
}

fn nested(depth: usize) -> (Value, Vec<u8>) {
    let mut value = Value::Integer(0);
    for _ in 0..depth {
        value = Value::List(vec![value]);
    }
    let mut bytes = vec![0x81; depth];
    bytes.push(0);
    (value, bytes)
}

/// Tag 42 around a byte string holding `payload`.
fn tag42(payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0xd8, 0x2a, 0x58, payload.len() as u8];
    bytes.extend(payload);
    bytes
}

#[test]
fn hand_vectors() {
    let value = Value::Map(BTreeMap::from([
        (
            "b".into(),
            Value::List(vec![Value::Null, Value::Bool(true)]),
        ),
        ("aa".into(), Value::Bytes(b"\x00".as_slice().into())),
        ("a".into(), Value::Integer(-1)),
        ("".into(), Value::String("é".into())),
    ]));
    let bytes = "a46062c3a9616120616282f6f56261614100";
    assert_eq!(value.encode().unwrap(), hex(bytes));
    assert_eq!(decode(bytes).unwrap(), value);
}

#[test]
fn links() {
    for codec in [Codec::Raw, Codec::Drisl] {
        for hash in [HashFn::Sha256, HashFn::Blake3] {
            let cid = Cid::<Bdasl>::of(codec, hash, b"content");
            let value = Value::Link(cid);
            let bytes = value.encode().unwrap();
            let mut payload = vec![0];
            payload.extend(cid.to_bytes());
            assert_eq!(bytes, tag42(&payload));
            assert_eq!(Value::decode(&bytes).unwrap(), value);
        }
    }
    let cid = Cid::<Bdasl>::of(Codec::Raw, HashFn::Sha256, b"").to_bytes();
    let mut long = vec![0, 0x01, 0x55, 0x13, 0x40];
    long.extend([0; 64]);
    let mut sha1 = vec![0, 0x01, 0x55, 0x11, 0x14];
    sha1.extend([0; 20]);
    let mut codec = vec![0, 0x01, 0x70, 0x12, 0x20];
    codec.extend([0; 32]);
    for payload in [long, sha1, codec, [&[0][..], &cid, &[0]].concat()] {
        assert!(
            matches!(Value::decode(&tag42(&payload)), Err(Error::Cid { .. })),
            "{payload:02x?}"
        );
    }
    let malformed = [
        tag42(&[[1].as_slice(), &cid].concat()),
        [&[0xd8, 0x2b, 0x58, 0x25, 0][..], &cid].concat(),
        [&[0xd8, 0x2a, 0x78, 0x25, 0][..], &cid].concat(),
        [&[0xd8, 0x2a, 0xd8, 0x2a][..], &tag42(&cid)].concat(),
        hex("d82a40"),
    ];
    for bytes in malformed {
        assert!(
            matches!(Value::decode(&bytes), Err(Error::Malformed { .. })),
            "{bytes:02x?}"
        );
    }
}

#[test]
fn links_in_order() {
    let cids: Vec<Cid<Bdasl>> = (0..4u8)
        .map(|i| Cid::<Bdasl>::of(Codec::Raw, HashFn::Sha256, &[i]))
        .collect();
    let link = |i: usize| Value::Link(cids[i]);
    let value = Value::List(vec![
        link(0),
        Value::Map(BTreeMap::from([
            ("aa".into(), link(3)),
            ("b".into(), Value::List(vec![link(1), Value::Null, link(2)])),
        ])),
    ]);
    assert!(value.links().eq(cids.iter().copied()));

    let mut value = link(0);
    for _ in 0..100_000 {
        value = Value::List(vec![Value::Null, value]);
    }
    assert!(value.links().eq(cids[..1].iter().copied()));
    // Take it apart by hand, since a normal drop this deep overflows the stack.
    while let Value::List(mut items) = value {
        value = items.pop().unwrap();
    }
}

#[test]
fn numbers() {
    for value in [i64::MIN, -1, 0, 23, 24, i64::MAX] {
        let value = Value::Integer(value);
        assert_eq!(Value::decode(&value.encode().unwrap()).unwrap(), value);
    }
    assert_eq!(
        decode("3b7fffffffffffffff").unwrap(),
        Value::Integer(i64::MIN)
    );
    for bytes in ["1b8000000000000000", "3b8000000000000000"] {
        assert!(
            matches!(decode(bytes), Err(Error::Malformed { .. })),
            "{bytes}"
        );
    }
    for bytes in ["f93e00", "fa3fc00000", "fb3ff8000000000000", "fb"] {
        assert!(matches!(decode(bytes), Err(Error::Float)), "{bytes}");
    }
}

#[test]
fn rejects_malformed_input() {
    let malformed = [
        // Empty input, truncation and trailing bytes.
        "",
        "62",
        "6261",
        "8201",
        "a161",
        "f6f6",
        "0000",
        // Heads that are not minimal.
        "1817",
        "190017",
        "1a0000ffff",
        "1b00000000ffffffff",
        "5800",
        "9800",
        // Indefinite lengths and reserved heads.
        "9fff",
        "5f40ff",
        "1c",
        // Simple values other than false, true and null.
        "f7",
        "e0",
        "f820",
        "ff",
        // Tags other than 42.
        "c074",
        // Strings that are not UTF-8.
        "61ff",
        // Map keys that are not strings, unsorted or duplicated.
        "a101f6",
        "a2616200616100",
        "a262616100616200",
        "a2616100616100",
    ];
    for bytes in malformed {
        assert!(
            matches!(decode(bytes), Err(Error::Malformed { .. })),
            "{bytes}"
        );
    }
    // A declared length is not allocated before its items arrive.
    assert!(matches!(
        decode("9bffffffffffffffff"),
        Err(Error::Malformed { .. })
    ));
}

#[test]
fn depth_limit() {
    let (value, bytes) = nested(MAX_DEPTH);
    assert_eq!(value.encode().unwrap(), bytes);
    assert_eq!(Value::decode(&bytes).unwrap(), value);

    let (value, bytes) = nested(MAX_DEPTH + 1);
    assert!(matches!(value.encode(), Err(Error::TooDeep)));
    assert!(matches!(Value::decode(&bytes), Err(Error::TooDeep)));

    let mut bytes = vec![0xa1, 0x60];
    bytes.extend([0x81; MAX_DEPTH - 1]);
    bytes.push(0xa0);
    assert!(matches!(Value::decode(&bytes), Err(Error::TooDeep)));

    let mut bytes = vec![0x81; 100_000];
    bytes.push(0);
    assert!(matches!(Value::decode(&bytes), Err(Error::TooDeep)));
}

#[test]
fn max_depth_on_small_stack() {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(|| {
            let mut value = Value::Link(Cid::<Bdasl>::of(Codec::Raw, HashFn::Blake3, b""));
            for _ in 0..MAX_DEPTH {
                value = Value::Map(BTreeMap::from([("key".into(), value)]));
            }
            let bytes = value.encode().unwrap();
            let decoded = Value::decode(&bytes).unwrap();
            assert_eq!(decoded.clone(), value);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn builds_large_values_incrementally() {
    let value = Value::List(
        (0..1_000_000)
            .map(|i| Value::List(vec![Value::Integer(i)]))
            .collect(),
    );
    let bytes = value.encode().unwrap();
    assert_eq!(Value::decode(&bytes).unwrap(), value);
}

/// SHA-256 as an `io::Write`, which sha2 0.11 does not provide.
struct Sha256Writer(sha2::Sha256);

impl Write for Sha256Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        sha2::Digest::update(&mut self.0, bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn encode_to_hashers() {
    let link = Value::Link(Cid::<Bdasl>::of(Codec::Raw, HashFn::Blake3, b"leaf"));
    let values = [
        Value::Null,
        Value::String("x".repeat(300)),
        Value::Map(BTreeMap::from([
            ("link".into(), link.clone()),
            (
                "nested".into(),
                Value::Map(BTreeMap::from([
                    ("bb".into(), Value::List(vec![Value::Integer(-7), link])),
                    ("a".into(), Value::Bytes(b"\x01\x02".as_slice().into())),
                ])),
            ),
        ])),
    ];
    for value in values {
        let bytes = value.encode().unwrap();

        let mut blake3 = blake3::Hasher::new();
        value.encode_to(&mut blake3).unwrap();
        let cid = Cid::<Bdasl>::new(Format::DRISL_BLAKE3, blake3.finalize().as_bytes());
        assert_eq!(
            cid.unwrap(),
            Cid::<Bdasl>::of(Codec::Drisl, HashFn::Blake3, &bytes)
        );

        let mut sha256 = Sha256Writer(sha2::Digest::new());
        value.encode_to(&mut sha256).unwrap();
        let cid = Cid::<Bdasl>::new(Format::DRISL_SHA256, &sha2::Digest::finalize(sha256.0));
        assert_eq!(
            cid.unwrap(),
            Cid::<Bdasl>::of(Codec::Drisl, HashFn::Sha256, &bytes)
        );
    }
}

#[test]
fn encode_to_errors() {
    let mut out = Vec::new();
    let (value, _) = nested(MAX_DEPTH + 1);
    assert!(matches!(value.encode_to(&mut out), Err(Error::TooDeep)));
    assert!(out.is_empty());

    let mut out = [0; 1];
    let value = Value::String("too long".into());
    assert!(matches!(
        value.encode_to(&mut out[..]),
        Err(Error::Io { .. })
    ));
}
