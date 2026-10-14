use covalence_lib_cid::{Blake3, CtxKey, O256};

/// From the official BLAKE3 test vectors: inputs of length 0 and 1.
const VECTORS: [(&[u8], &str, &str, &str); 2] = [
    (
        b"",
        "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
        "92b2b75604ed3c761f9d6f62392c8a9227ad0ea3f09573e783f1498a4ed60d26",
        "2cc39783c223154fea8dfb7c1b1660f2ac2dcbd1c1de8277b0b0dd39b7e50d7d",
    ),
    (
        &[0],
        "2d3adedff11b61f14c886e35afa036736dcd87a74d27b5c1510225d0f592e213",
        "6d7878dfff2f485635d39013278ae14f1454b8c0a3a2d34bc1ab38228a80c95b",
        "b3e2e340a117a499c6cf2398a19ee0d29cca2bb7404c73063382693bf66cb06c",
    ),
];

const KEY: O256 = O256::from_array(*b"whats the Elvish word for friend");
const CONTEXT: &str = "BLAKE3 2019-12-27 16:29:52 test vectors context";

#[test]
fn blake3_vectors() {
    let ctx = CtxKey::derive(CONTEXT);
    for (input, hash, keyed, derived) in VECTORS {
        assert_eq!(O256::from_bytes(input).to_string(), hash);
        assert_eq!(O256::with_key(&KEY, input).to_string(), keyed);
        assert_eq!(O256::with_ctx(&ctx, input).to_string(), derived);
    }
}

#[test]
fn bytes_round_trip() {
    let id = O256::from_bytes(b"x");
    assert_eq!(O256::from_array(id.into_bytes()), id);
    assert_eq!(format!("{id:?}"), format!("O256({id})"));
    assert!(O256::from_array([0; 32]) < O256::from_array([1; 32]));
}

#[test]
fn blake3_cids_are_o256s() {
    let cid = Blake3::of(b"x");
    assert_eq!(cid.into_o256(), O256::from_bytes(b"x"));
    assert_eq!(O256::from(cid).as_bytes(), cid.digest());
}

#[cfg(feature = "rand")]
#[test]
fn random_guids_differ() {
    let mut rng = covalence_lib_rand::rng();
    assert_ne!(O256::random(&mut rng), O256::random(&mut rng));
}
