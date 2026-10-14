use std::collections::BTreeMap;

use covalence_lib_cid::{Bdasl, Cid, Codec, HashFn};
use covalence_lib_dasl::{Builder, Error, Event, Links, MAX_DEPTH, Value};

fn build<'a>(events: impl IntoIterator<Item = Event<'a>>) -> Result<Value, Error> {
    let mut builder = Builder::default();
    for event in events {
        builder.push(event)?;
    }
    builder.finish()
}

#[test]
fn events_are_canonical_pre_order() {
    let cid = Cid::<Bdasl>::of(Codec::Drisl, HashFn::Blake3, b"x");
    let value = Value::Map(BTreeMap::from([
        (
            "b".into(),
            Value::List(vec![Value::Null, Value::Bool(true), Value::Link(cid)]),
        ),
        ("aa".into(), Value::Bytes(b"\x00".as_slice().into())),
        ("a".into(), Value::Integer(-1)),
        ("".into(), Value::Map(BTreeMap::new())),
    ]));
    let events: Vec<_> = value.events().collect();
    assert_eq!(
        events,
        [
            Event::Map(4),
            Event::String(""),
            Event::Map(0),
            Event::String("a"),
            Event::Integer(-1),
            Event::String("b"),
            Event::List(3),
            Event::Null,
            Event::Bool(true),
            Event::Link(cid),
            Event::String("aa"),
            Event::Bytes(b"\x00"),
        ]
    );
    let bytes: Vec<u8> = events.iter().flat_map(Event::encode).collect();
    assert_eq!(bytes, value.encode().unwrap());
    assert_eq!(build(events).unwrap(), value);
    for value in [
        Value::Integer(i64::MIN),
        Value::String("é".into()),
        Value::List(vec![]),
    ] {
        assert_eq!(build(value.events()).unwrap(), value);
    }
}

#[test]
fn rejects_malformed_events() {
    let invalid = [
        vec![],
        vec![Event::List(2), Event::Null],
        vec![Event::Null, Event::Null],
        vec![Event::Map(1), Event::Integer(1), Event::Null],
        vec![
            Event::Map(2),
            Event::String("b"),
            Event::Null,
            Event::String("a"),
            Event::Null,
        ],
        vec![
            Event::Map(2),
            Event::String("aa"),
            Event::Null,
            Event::String("b"),
            Event::Null,
        ],
        vec![
            Event::Map(2),
            Event::String("a"),
            Event::Null,
            Event::String("a"),
            Event::Null,
        ],
    ];
    for events in invalid {
        let error = build(events.clone()).unwrap_err();
        assert!(matches!(error, Error::Malformed { .. }), "{events:?}");
    }
    for container in [Event::List(1), Event::Map(0)] {
        let mut events = vec![Event::List(1); MAX_DEPTH];
        events.push(container);
        assert!(matches!(build(events), Err(Error::TooDeep)));
    }
}

#[test]
fn links_from_events() {
    let cids: Vec<_> = (0..3u8)
        .map(|i| Cid::<Bdasl>::of(Codec::Raw, HashFn::Sha256, &[i]))
        .collect();
    let value = Value::Map(BTreeMap::from([
        ("b".into(), Value::Link(cids[0])),
        (
            "a".into(),
            Value::List(vec![Value::Link(cids[1]), Value::Null]),
        ),
    ]));
    assert!(Links::new(value.events()).eq(value.links()));

    let events = [
        Event::List(4),
        Event::Link(cids[2]),
        Event::String("x"),
        Event::Map(1),
        Event::String("k"),
        Event::Link(cids[0]),
        Event::Link(cids[2]),
    ];
    assert!(Links::new(events).eq([cids[2], cids[0], cids[2]]));
}
