use covalence_lib_cid::{
    Bdasl, Blake3, Cid, Cid32, Cid64, Codec, Error, Fact, Format, HashFn, Multicodec, Packed32,
};
use covalence_lib_data::Bytes;

const PAIRS: [(Codec, HashFn); 4] = [
    (Codec::Raw, HashFn::Sha256),
    (Codec::Drisl, HashFn::Sha256),
    (Codec::Raw, HashFn::Blake3),
    (Codec::Drisl, HashFn::Blake3),
];

const HELLO: Bytes = Bytes::from_static(b"hello");

fn sha512(bytes: &[u8]) -> Cid64 {
    let digest = Multicodec::SHA2_512.digest(bytes).unwrap();
    Cid64::new(Format::SHA2_512, &digest).unwrap()
}

#[test]
fn checks_bytes() {
    for (codec, hash) in PAIRS {
        let cid = Cid::<Bdasl>::of(codec, hash, &HELLO);
        let fact = Fact::new(cid, HELLO).unwrap();
        assert_eq!((fact.claim(), fact.bytes()), (&cid, &HELLO));
        assert!(matches!(
            Fact::new(cid, Bytes::from_static(b"hellp")),
            Err(Error::Mismatch)
        ));
        Fact::new(Cid64::from(cid), HELLO).unwrap();
    }
    let fact = Fact::new(Blake3::of(&HELLO), HELLO).unwrap();
    assert_eq!(Fact::<Blake3>::of(HELLO), fact);
    Fact::new(sha512(&HELLO), HELLO).unwrap();
    assert!(matches!(
        Fact::new(sha512(b"x"), HELLO),
        Err(Error::Mismatch)
    ));
}

#[test]
fn ignores_the_codec() {
    let digest = HashFn::Blake3.digest(&HELLO);
    let cid = Cid32::new(
        Format {
            codec: Multicodec(0x0129),
            hash: Multicodec::BLAKE3,
        },
        &digest,
    )
    .unwrap();
    Fact::new(cid, HELLO).unwrap();
}

#[test]
fn rejects_unknown_and_truncated_hashes() {
    let digest = HashFn::Sha256.digest(&HELLO);
    for (hash, len) in [(Multicodec(0xb220), 32), (Multicodec::SHA2_256, 20)] {
        let cid = Cid32::new(
            Format {
                codec: Multicodec::RAW,
                hash,
            },
            &digest[..len],
        )
        .unwrap();
        assert!(
            matches!(Fact::new(cid, HELLO), Err(Error::Hash { hash: h, len: l }) if (h, l) == (hash, len))
        );
    }
}

#[test]
fn sha1_facts_hold() {
    let digest = Multicodec::SHA1.digest(&HELLO).unwrap();
    let cid = Cid32::new(Format::SHA1, &digest).unwrap();
    Fact::new(cid, HELLO).unwrap();
    assert!(matches!(
        Fact::new(cid, Bytes::from_static(b"hellp")),
        Err(Error::Mismatch)
    ));
}

#[test]
fn facts_cast_between_storages() {
    let fact = Fact::new(Blake3::of(&HELLO), HELLO).unwrap();
    let wide: Fact<Cid64> = fact.clone().widen();
    let back = wide
        .clone()
        .cast::<covalence_lib_cid::storage::Blake3>()
        .unwrap();
    assert_eq!(back, fact);
    assert_eq!(
        wide.cast::<Bdasl>().unwrap().claim(),
        &Cid::<Bdasl>::from(*fact.claim())
    );
    let sha512 = Fact::new(sha512(&HELLO), HELLO).unwrap();
    assert!(matches!(
        sha512.cast::<Packed32>(),
        Err(Error::Unsupported { .. })
    ));
}

#[test]
fn facts_move_to_raw_blake3() {
    let raw = Fact::new(Blake3::of(&HELLO), HELLO).unwrap();
    let drisl = Cid::new(Format::DRISL_BLAKE3, raw.claim().digest()).unwrap();
    let drisl: Fact<Cid32> = Fact::new(drisl, HELLO).unwrap();
    assert_eq!(drisl.raw_blake3(), Some(raw));
    let sha256 = Fact::new(
        Cid32::new(Format::SHA2_256, &HashFn::Sha256.digest(&HELLO)).unwrap(),
        HELLO,
    )
    .unwrap();
    assert_eq!(sha256.raw_blake3(), None);
}

#[test]
fn facts_change_codec() {
    for (codec, hash) in PAIRS {
        let original = Fact::new(Cid::<Bdasl>::of(codec, hash, &HELLO), HELLO).unwrap();
        for (new_codec, _) in PAIRS {
            let changed = original.clone().with_codec(new_codec.code()).unwrap();
            assert_eq!(changed.claim().codec(), new_codec.code());
            assert_eq!(changed.claim().hash(), original.claim().hash());
            assert_eq!(changed.claim().digest(), original.claim().digest());
            assert_eq!(changed.bytes(), &HELLO);
            assert_eq!(changed.bytes().as_ptr(), original.bytes().as_ptr());
            assert_eq!(changed.with_codec(codec.code()).unwrap(), original);
        }
    }
    assert!(matches!(
        Fact::new(Blake3::of(&HELLO), HELLO)
            .unwrap()
            .with_codec(Multicodec::DAG_CBOR),
        Err(Error::Unsupported { .. })
    ));
}
