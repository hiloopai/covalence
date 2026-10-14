use covalence_lib_cid::{
    Cid, Cid32, Cid64, Error, Fact, Format, Git, GitSha1, GitSha256, Multicodec,
};
use covalence_lib_data::Bytes;

const EMPTY_BLOB: &[u8] = b"blob 0\0";

/// The SHAttered pair: different bytes with the same SHA-1.
const SHATTERED: [&[u8]; 2] = [
    include_bytes!("shattered/1.bin"),
    include_bytes!("shattered/2.bin"),
];

#[test]
fn empty_blob() {
    let sha1 = Cid::<Git>::sha1(EMPTY_BLOB);
    let sha256 = Cid::<Git>::sha256(EMPTY_BLOB);
    assert_eq!(sha1.to_string(), "baf4bcfhgtxrjxmwr2zbuxczjvz3vvwgc4sgfhei");
    assert_eq!(
        sha256.to_string(),
        "baf4beichhihuyo7ive3idith4oy6tj643iiykq3p4fa7o5erecrqg4qycm"
    );
    assert_eq!(
        sha1.digest(),
        b"\xe6\x9d\xe2\x9b\xb2\xd1\xd6\x43\x4b\x8b\x29\xae\x77\x5a\xd8\xc2\xe4\x8c\x53\x91"
    );
    for cid in [sha1, sha256] {
        assert_eq!(cid.codec(), Multicodec::GIT_RAW);
        assert_eq!(cid.to_string().parse::<Cid<Git>>().unwrap(), cid);
        assert_eq!(Cid::<Git>::from_bytes(&cid.to_bytes()).unwrap(), cid);
        assert_eq!(Cid::<Git>::try_from(Cid32::from(cid)).unwrap(), cid);
        assert_eq!(Cid::<Git>::try_from(Cid64::from(cid)).unwrap(), cid);
        Fact::new(cid, Bytes::from_static(EMPTY_BLOB)).unwrap();
    }
    assert!(sha1 < sha256);
    assert_eq!(
        (Multicodec::SHA1.name(), Multicodec::GIT_RAW.name()),
        (Some("sha1"), Some("git-raw"))
    );
}

#[test]
fn rejects_other_cids() {
    let cases = [
        (Multicodec::RAW, Multicodec::SHA1, 20),
        (Multicodec::GIT_RAW, Multicodec::SHA1, 32),
        (Multicodec::GIT_RAW, Multicodec::SHA2_256, 20),
        (Multicodec::GIT_RAW, Multicodec::BLAKE3, 32),
    ];
    for (codec, hash, len) in cases {
        let cid = Cid32::new(Format { codec, hash }, &[0; 32][..len]).unwrap();
        assert!(matches!(
            Cid::<Git>::try_from(cid),
            Err(Error::Unsupported { .. })
        ));
    }
}

#[test]
fn shattered_facts_share_a_cid() {
    let [first, second] = SHATTERED;
    assert_ne!(first, second);
    let facts = SHATTERED.map(|bytes| {
        let cid = Cid::<Git>::sha1(bytes);
        Fact::new(cid, Bytes::from_static(bytes)).unwrap()
    });
    assert_eq!(facts[0].claim(), facts[1].claim());
    assert_ne!(facts[0].bytes(), facts[1].bytes());
    let sha256 = SHATTERED.map(Cid::<Git>::sha256);
    assert_ne!(sha256[0], sha256[1]);
}

#[test]
fn typed_git_ids() {
    let sha1 = Cid::<GitSha1>::of(EMPTY_BLOB);
    let sha256 = Cid::<GitSha256>::of(EMPTY_BLOB);
    assert_eq!(size_of_val(sha1.digest()), 20);
    assert_eq!(size_of_val(sha256.digest()), 32);
    assert_eq!(Cid::<Git>::from(sha1), Cid::<Git>::sha1(EMPTY_BLOB));
    assert_eq!(Cid::<Git>::from(sha256), Cid::<Git>::sha256(EMPTY_BLOB));
    assert_eq!(
        Cid::<GitSha1>::try_from(Cid::<Git>::from(sha1)).unwrap(),
        sha1
    );
    assert!(matches!(
        Cid::<GitSha1>::try_from(Cid::<Git>::from(sha256)),
        Err(Error::Unsupported { .. })
    ));
    assert_eq!(Cid32::from(sha256).cast::<GitSha256>().unwrap(), sha256);
    assert_eq!(Cid64::from(sha1).to_string(), sha1.to_string());
}
