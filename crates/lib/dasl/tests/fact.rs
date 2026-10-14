use covalence_lib_cid::{Bdasl, Cid, Codec, Error as CidError, Fact, GitSha1, HashFn, Multicodec};
use covalence_lib_dasl::{Error, Value};
use covalence_lib_data::Bytes;

#[test]
fn decodes_drisl_and_raw() {
    let value = Value::List(vec![Value::Integer(1), Value::Null]);
    let drisl = Bytes::from(value.encode().unwrap());
    let cid = value.cid().unwrap();
    assert_eq!(cid, Cid::<Bdasl>::of(Codec::Drisl, HashFn::Blake3, &drisl));
    let fact = Fact::new(cid, drisl.clone()).unwrap();
    assert_eq!(Value::from_fact(&fact).unwrap(), value);
    assert_eq!(Value::from_cid_bytes(cid, drisl.clone()).unwrap(), value);

    let raw = Fact::new(cid.with_codec(Codec::Raw), drisl.clone()).unwrap();
    assert_eq!(Value::from_fact(&raw).unwrap(), Value::Bytes(drisl));
}

#[test]
fn rejects_wrong_bytes_and_invalid_drisl() {
    let cid = Value::Null.cid().unwrap();
    assert!(matches!(
        Fact::new(cid, Bytes::from_static(b"wrong")),
        Err(CidError::Mismatch)
    ));
    assert!(matches!(
        Value::from_cid_bytes(cid, Bytes::from_static(b"wrong")),
        Err(Error::Cid {
            source: CidError::Mismatch
        })
    ));

    let cid = Cid::<Bdasl>::of(Codec::Drisl, HashFn::Blake3, b"\xff");
    let fact = Fact::new(cid, Bytes::from_static(b"\xff")).unwrap();
    assert!(matches!(
        Value::from_fact(&fact),
        Err(Error::Malformed { .. })
    ));
}

#[test]
fn rejects_other_codecs() {
    let fact = Fact::<Cid<GitSha1>>::of(Bytes::from_static(b"blob 0\0"));
    assert!(matches!(
        Value::from_fact(&fact),
        Err(Error::Codec { codec }) if codec == Multicodec::GIT_RAW
    ));
}
