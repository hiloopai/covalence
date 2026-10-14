use covalence_lib_cid::{
    Bdasl, Blake3, Cid, Cid32, Cid64, CidFormat, Codec, Dasl, Error, Format, HashFn, Multicodec,
    Sha256,
};
use data_encoding::BASE32_NOPAD;
use proptest::prelude::*;

/// The CIDs of the empty string, with independently computed text.
const EMPTY: [(Codec, HashFn, &str); 4] = [
    (
        Codec::Raw,
        HashFn::Sha256,
        "bafkreihdwdcefgh4dqkjv67uzcmw7ojee6xedzdetojuzjevtenxquvyku",
    ),
    (
        Codec::Drisl,
        HashFn::Sha256,
        "bafyreihdwdcefgh4dqkjv67uzcmw7ojee6xedzdetojuzjevtenxquvyku",
    ),
    (
        Codec::Raw,
        HashFn::Blake3,
        "bafkr4ifpcne3t5pzugtkaqcn5i3nzskjtpfslsnnyejlpte2spfoihzsmi",
    ),
    (
        Codec::Drisl,
        HashFn::Blake3,
        "bafyr4ifpcne3t5pzugtkaqcn5i3nzskjtpfslsnnyejlpte2spfoihzsmi",
    ),
];

/// The raw SHA-512 CID of the empty string, computed independently.
const EMPTY_SHA512: &str = "bafkrgqgpqpqtk7xpxc67cvbikdlg3aah2yqoibilk4k5za7uveq5g3hjzzd5buj4lwc7fmh7qmmnfb365qxwhojrxvduc6ubuu4de6xze7nd4";

const ALPHABET: &str = "abcdefghijklmnopqrstuvwxyz234567";

fn sample() -> Cid<Bdasl> {
    Cid::<Bdasl>::of(Codec::Raw, HashFn::Sha256, b"")
}

fn text_of(bytes: &[u8]) -> String {
    format!("b{}", BASE32_NOPAD.encode(bytes).to_lowercase())
}

/// Text, bytes and `Debug` all round-trip.
fn round_trips<S: CidFormat>(cid: Cid<S>, text: &str) {
    assert_eq!(cid.to_string(), text);
    assert_eq!(text.parse::<Cid<S>>().unwrap(), cid);
    assert_eq!(Cid::<S>::from_bytes(&cid.to_bytes()).unwrap(), cid);
    assert_eq!(format!("{cid:?}"), format!("Cid({text})"));
}

#[test]
fn known_vectors() {
    for (codec, hash, text) in EMPTY {
        let cid = Cid::<Bdasl>::of(codec, hash, b"");
        assert_eq!((cid.codec(), cid.hash()), (codec.code(), hash.code()));
        assert_eq!(*cid.digest(), hash.digest(b""));
        round_trips(cid, text);
        round_trips(Cid32::from(cid), text);
        round_trips(Cid64::from(cid), text);
    }
    round_trips(Sha256::of(b""), EMPTY[0].2);
    round_trips(Blake3::of(b""), EMPTY[2].2);
    let sha512 = EMPTY_SHA512.parse::<Cid64>().unwrap();
    assert_eq!(
        (sha512.codec(), sha512.hash()),
        (Multicodec::RAW, Multicodec::SHA2_512)
    );
    round_trips(sha512, EMPTY_SHA512);
}

#[test]
fn stores_only_the_bytes() {
    assert_eq!(size_of::<Cid<Bdasl>>(), 36);
    assert_eq!(size_of::<Blake3>(), 32);
    assert_eq!(size_of::<Cid32>(), 20 + 32);
    assert_eq!(size_of::<Cid64>(), 20 + 64);
    let digest: [u8; 32] = HashFn::Blake3.digest(b"x");
    assert_eq!(*Blake3::of(b"x").digest(), digest);
    let dasl = Cid::<Bdasl>::of(Codec::Drisl, HashFn::Blake3, b"x");
    assert_eq!(*dasl.digest(), digest);
    assert_eq!(dasl.stored_bytes()[4..], digest);
}

#[test]
fn with_codec_keeps_the_digest() {
    let raw = Cid::<Bdasl>::of(Codec::Raw, HashFn::Blake3, b"x");
    let drisl = raw.with_codec(Codec::Drisl);
    assert_eq!(drisl, Cid::<Bdasl>::of(Codec::Drisl, HashFn::Blake3, b"x"));
    assert_eq!(drisl.with_codec(Codec::Raw), raw);
}

#[test]
fn holds_unknown_codes() {
    let (codec, hash) = (Multicodec(0x0129), Multicodec(0xb220));
    let cid = Cid32::new(Format { codec, hash }, &[7; 32]).unwrap();
    assert_eq!(
        (cid.codec(), cid.hash(), cid.digest()),
        (codec, hash, &[7; 32][..])
    );
    assert_eq!(cid.to_bytes()[..6], [1, 0xa9, 0x02, 0xa0, 0xe4, 0x02]);
    round_trips(cid, &text_of(&cid.to_bytes()));
    assert_eq!(
        (codec.name(), format!("{hash:?}")),
        (None, "0xb220".to_owned())
    );
    let max = Multicodec((1 << 63) - 1);
    let cid = Cid32::new(
        Format {
            codec: max,
            hash: max,
        },
        &[],
    )
    .unwrap();
    round_trips(cid, &text_of(&cid.to_bytes()));
    assert!(matches!(
        Cid32::new(
            Format {
                codec: Multicodec(1 << 63),
                hash: max
            },
            &[]
        ),
        Err(Error::Unsupported { .. })
    ));
}

#[test]
fn names() {
    let known = [
        (Multicodec::RAW, "raw"),
        (Multicodec::DAG_CBOR, "dag-cbor"),
        (Multicodec::GIT_RAW, "git-raw"),
        (Multicodec::SHA1, "sha1"),
        (Multicodec::SHA2_256, "sha2-256"),
        (Multicodec::SHA2_512, "sha2-512"),
        (Multicodec::BLAKE3, "blake3"),
    ];
    for (code, name) in known {
        assert_eq!(code.name(), Some(name));
        assert_eq!(format!("{code:?}"), name);
    }
    assert_eq!(Multicodec(0xb220).name(), None);
}

#[test]
fn parses_codes_in_the_wrong_position() {
    let (codec, hash) = (Multicodec::SHA2_256, Multicodec::RAW);
    let cid = Cid32::new(Format { codec, hash }, &[0; 32]).unwrap();
    let cid = Cid32::from_bytes(&cid.to_bytes()).unwrap();
    assert_eq!((cid.codec(), cid.hash()), (codec, hash));
}

#[test]
fn rejects_malformed_headers() {
    for (index, byte) in [(0, 0), (0, 2), (3, 0x1f), (3, 0x21)] {
        let mut bytes = sample().to_bytes();
        bytes[index] = byte;
        assert!(
            matches!(Cid32::from_bytes(&bytes), Err(Error::Malformed)),
            "{bytes:?}"
        );
    }
    let bytes = sample().to_bytes();
    let v0 = [&[0x12, 0x20][..], sample().digest()].concat();
    let long = [&bytes[..], &[0]].concat();
    let non_minimal = [&[1, 0xd5, 0x00][..], &bytes[2..]].concat();
    let too_big = [&[1][..], &[0xff; 9], &[0x01, 0x12, 0x00]].concat();
    for bad in [&bytes[..35], &long, &v0, &non_minimal, &too_big, &[]] {
        assert!(
            matches!(Cid32::from_bytes(bad), Err(Error::Malformed)),
            "{bad:?}"
        );
        assert!(
            matches!(text_of(bad).parse::<Cid32>(), Err(Error::Malformed)),
            "{bad:?}"
        );
    }
}

#[test]
fn storages_reject_other_cids() {
    let unsupported = |result| matches!(result, Err(Error::Unsupported { .. }));
    for (index, byte) in [(1, 0x70), (2, 0x11)] {
        let mut bytes = sample().to_bytes();
        bytes[index] = byte;
        assert!(unsupported(Cid::<Bdasl>::from_bytes(&bytes).map(drop)));
        assert!(Cid32::from_bytes(&bytes).is_ok());
    }
    let sha512 = EMPTY_SHA512.parse::<Cid64>().unwrap();
    assert!(unsupported(Cid32::try_from(sha512).map(drop)));
    assert!(unsupported(EMPTY_SHA512.parse::<Cid32>().map(drop)));
    assert!(unsupported(
        Cid32::new(
            Format {
                codec: Multicodec::RAW,
                hash: Multicodec::SHA2_256
            },
            &[0; 33]
        )
        .map(drop)
    ));
    assert!(unsupported(Cid::<Bdasl>::try_from(sha512).map(drop)));
    let drisl = Cid::<Bdasl>::of(Codec::Drisl, HashFn::Blake3, b"");
    let blake3 = Cid::<Bdasl>::of(Codec::Raw, HashFn::Blake3, b"");
    assert!(unsupported(Blake3::try_from(drisl).map(drop)));
    assert!(unsupported(Sha256::try_from(blake3).map(drop)));
    assert!(unsupported(
        Blake3::from_bytes(&sample().to_bytes()).map(drop)
    ));
}

#[test]
fn conversions_keep_the_cid() {
    for (codec, hash, text) in EMPTY {
        let cid = Cid::<Bdasl>::of(codec, hash, b"");
        let cid32 = Cid32::from(cid);
        let cid64 = Cid64::from(cid32);
        assert_eq!(Cid32::try_from(cid64).unwrap(), cid32);
        assert_eq!(Cid::<Bdasl>::try_from(cid32).unwrap(), cid);
        assert_eq!(Cid::<Bdasl>::try_from(cid64).unwrap(), cid);
        assert_eq!(cid64.to_string(), text);
    }
    let blake3 = Blake3::of(b"x");
    let sha256 = Sha256::of(b"x");
    assert_eq!(
        Cid::<Bdasl>::from(blake3),
        Cid::<Bdasl>::of(Codec::Raw, HashFn::Blake3, b"x")
    );
    assert_eq!(
        Cid::<Bdasl>::from(sha256),
        Cid::<Bdasl>::of(Codec::Raw, HashFn::Sha256, b"x")
    );
    assert_eq!(
        Blake3::try_from(Cid::<Bdasl>::from(blake3)).unwrap(),
        blake3
    );
    assert_eq!(Sha256::try_from(Cid32::from(sha256)).unwrap(), sha256);
    assert_eq!(Sha256::try_from(Cid64::from(sha256)).unwrap(), sha256);
    assert_eq!(Cid32::from(blake3).to_bytes(), blake3.to_bytes());
    assert_eq!(Cid64::from(blake3).to_bytes(), blake3.to_bytes());
}

#[test]
fn rejects_bad_text() {
    let text = sample().to_string();
    let body = &text[1..];
    let last = body.chars().last().unwrap();
    let flipped = ALPHABET.as_bytes()[ALPHABET.find(last).unwrap() | 1] as char;
    let bad = [
        format!("B{body}"),
        format!("b{}", body.to_uppercase()),
        format!("z{body}"),
        format!("f{body}"),
        format!("m{body}"),
        format!("{text}======"),
        format!("{}{flipped}", &text[..text.len() - 1]),
        format!("/ipfs/{text}"),
        "QmRN6wdp1S2A5EtjW9A3M1vKSBuQQGcgvuhoMUoEz4iiT5".to_owned(),
        String::new(),
    ];
    for text in bad {
        assert!(
            matches!(text.parse::<Cid<Bdasl>>(), Err(Error::Text)),
            "{text}"
        );
        assert!(matches!(text.parse::<Cid64>(), Err(Error::Text)), "{text}");
    }
    assert!(matches!("b".parse::<Cid32>(), Err(Error::Malformed)));
}

fn dasl() -> impl Strategy<Value = Cid<Bdasl>> {
    let codec = prop_oneof![Just(Codec::Raw), Just(Codec::Drisl)];
    let hash = prop_oneof![Just(HashFn::Sha256), Just(HashFn::Blake3)];
    (codec, hash, any::<[u8; 32]>())
        .prop_map(|(codec, hash, digest)| Cid::<Bdasl>::from_digest(codec, hash, digest))
}

fn packed() -> impl Strategy<Value = Cid64> {
    let code = prop_oneof![0..0x100u64, 0..1u64 << 63];
    let digest = prop::collection::vec(any::<u8>(), 0..=64);
    (code.clone(), code, digest).prop_map(|(codec, hash, digest)| {
        Cid64::new(
            Format {
                codec: Multicodec(codec),
                hash: Multicodec(hash),
            },
            &digest,
        )
        .unwrap()
    })
}

fn fixed() -> impl Strategy<Value = Blake3> {
    any::<[u8; 32]>().prop_map(|digest| {
        Cid::new(
            Format {
                codec: Multicodec::RAW,
                hash: Multicodec::BLAKE3,
            },
            &digest,
        )
        .unwrap()
    })
}

proptest! {
    #[test]
    fn ord_is_byte_order(a in dasl(), b in dasl(), c in packed(), d in packed(), e in fixed(), f in fixed()) {
        prop_assert_eq!(a.cmp(&b), a.to_bytes().cmp(&b.to_bytes()));
        prop_assert_eq!(c.cmp(&d), c.to_bytes().cmp(&d.to_bytes()));
        prop_assert_eq!(e.cmp(&f), e.to_bytes().cmp(&f.to_bytes()));
        prop_assert_eq!(Cid64::from(a).cmp(&c), a.to_bytes().cmp(&c.to_bytes()));
    }

    #[test]
    fn packed_round_trips(cid in packed()) {
        prop_assert_eq!(Cid64::from_bytes(&cid.to_bytes()).unwrap(), cid);
        prop_assert_eq!(cid.to_string().parse::<Cid64>().unwrap(), cid);
        prop_assert_eq!(Cid32::try_from(cid).is_ok(), cid.digest().len() <= 32);
    }
}

#[test]
fn strict_dasl_is_sha256_only() {
    for codec in [Codec::Raw, Codec::Drisl] {
        let cid = Cid::<Dasl>::of(codec, b"x");
        let bdasl = Cid::<Bdasl>::of(codec, HashFn::Sha256, b"x");
        assert_eq!(Cid::<Bdasl>::from(cid), bdasl);
        assert_eq!(Cid::<Dasl>::try_from(bdasl).unwrap(), cid);
        assert_eq!(
            cid.with_codec(Codec::Raw),
            Cid::<Dasl>::of(Codec::Raw, b"x")
        );
        round_trips(cid, &bdasl.to_string());
    }
    let blake3 = Cid::<Bdasl>::of(Codec::Raw, HashFn::Blake3, b"x");
    assert!(matches!(
        Cid::<Dasl>::try_from(blake3),
        Err(Error::Unsupported { .. })
    ));
}

#[test]
fn casts_between_any_formats() {
    let sha256 = Sha256::of(b"x");
    let cid64: Cid64 = sha256.cast().unwrap();
    assert_eq!(
        cid64.cast::<Dasl>().unwrap(),
        Cid::<Dasl>::of(Codec::Raw, b"x")
    );
    assert_eq!(
        cid64.cast::<covalence_lib_cid::storage::Sha256>().unwrap(),
        sha256
    );
    assert!(matches!(
        cid64.cast::<covalence_lib_cid::storage::Blake3>(),
        Err(Error::Unsupported { .. })
    ));
}

#[test]
fn raw_blake3_and_sha256_discard_the_codec() {
    for codec in [Codec::Raw, Codec::Drisl] {
        let blake3 = Cid::<Bdasl>::of(codec, HashFn::Blake3, b"hello");
        assert_eq!(blake3.raw_blake3(), Some(Blake3::of(b"hello")));
        assert_eq!(blake3.raw_sha256(), None);
        let sha256 = Cid::<Bdasl>::of(codec, HashFn::Sha256, b"hello");
        assert_eq!(sha256.raw_sha256(), Some(Sha256::of(b"hello")));
        assert_eq!(sha256.raw_blake3(), None);
        let wide = Cid64::from(blake3);
        assert_eq!(wide.raw_blake3(), Some(Blake3::of(b"hello")));
    }
    let sha512: Cid64 = Cid::new(Format::SHA2_512, &[0; 64]).unwrap();
    assert_eq!(sha512.raw_blake3(), None);
    // Truncated digests of either hash are not raw BLAKE3 or SHA-256 CIDs.
    let short: Cid64 = Cid::new(Format::BLAKE3, &[0; 16]).unwrap();
    assert_eq!(short.raw_blake3(), None);
    let long: Cid64 = Cid::new(Format::SHA2_256, &[0; 64]).unwrap();
    assert_eq!(long.raw_sha256(), None);
}

#[test]
fn same_multihash_needs_the_hash_and_digest_not_the_codec() {
    let raw = Cid32::from(Blake3::of(b"hello"));
    let digest = raw.digest();
    let with = |codec, hash, digest: &[u8]| Cid32::new(Format { codec, hash }, digest).unwrap();
    let same = [
        raw,
        with(Multicodec::DAG_CBOR, Multicodec::BLAKE3, digest),
        with(Multicodec(0x0129), Multicodec::BLAKE3, digest),
    ];
    for cid in same {
        assert!(
            raw.same_multihash(&cid) && cid.same_multihash(&raw),
            "{cid}"
        );
    }
    let other = [
        Cid32::from(Blake3::of(b"hellp")),
        with(Multicodec::RAW, Multicodec::SHA2_256, digest),
        with(Multicodec::RAW, Multicodec::BLAKE3, &digest[..16]),
    ];
    for cid in other {
        assert!(
            !raw.same_multihash(&cid) && !cid.same_multihash(&raw),
            "{cid}"
        );
    }
}
