//! Archives decode to the bytes the pinned `leantar -x` writes for them.
//!
//! `fixtures/` holds archives packed by the pinned `leantar`, and the digests
//! below are of the files `leantar -x -j` wrote for them with input hash
//! `0123456789abcdef`:
//!
//! - `module.ltar`: the outputs of `Init.Data.Int.Bitwise.Basic` from Lean
//!   `nightly-2026-10-30` (Apache-2.0, <https://github.com/leanprover/lean4>),
//!   a stand-in `.c` file and a module trace, so the trace is compact output
//!   hashes.
//! - `logged.ltar`: a trace with a log, so the trace is Zstandard JSON.
//! - `patched.ltar`: written by hand; its olean uses opcodes `0xa0` and `0xb0`.

use covalence_lean_ltar::{Archive, Dir, Error, pack, unpack};
use sha2::{Digest, Sha256};

const HASH: u64 = 0x0123_4567_89ab_cdef;

fn digests(archive: &Archive) -> Vec<(Dir, &str, String)> {
    std::iter::once(&archive.trace)
        .chain(&archive.files)
        .map(|entry| {
            let digest = Sha256::digest(&entry.bytes);
            let hex = digest.iter().map(|b| format!("{b:02x}")).collect();
            (entry.dir, entry.path.as_str(), hex)
        })
        .collect()
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

#[test]
fn module_outputs_match_leantar() {
    let archive = unpack(&fixture("module.ltar"), HASH).unwrap();
    let m = "Init/Data/Int/Bitwise/Basic";
    let expected = [
        (
            Dir::Lib,
            "trace",
            "fd22a9aed6c6e0a66e1f6ade5b788a797b0070a9f96a7df8f29d2c8f71a47135",
        ),
        (
            Dir::Lib,
            "olean",
            "8bd4dc6c3b0284b267eb4ca00adc3dbeca8760186e417502f74dbdad29447ed9",
        ),
        (
            Dir::Lib,
            "olean.server",
            "220288ebf89e51c26440e79e0721380ba2b6475fc0e6552445ae50c7adc101d2",
        ),
        (
            Dir::Lib,
            "olean.private",
            "56fdffabb33b3577a93cacf83ccad49a84bf1c33d2b653c8d651516f9026e9ea",
        ),
        (
            Dir::Lib,
            "ilean",
            "926206d4f7e9c4c69551e6d22e5da3cee967932fcb968516324bb4fafa6b9f3a",
        ),
        (
            Dir::Lib,
            "ir.sig",
            "1afd5e0951235a430da48477a9e4d3f7ea78738e684f5a4a78ef1626ee2a2f12",
        ),
        (
            Dir::Lib,
            "ir",
            "69b21787cb3d7902c49ed1da0b4705fa410e3389cf84052fde03f9a4fb2a24ed",
        ),
        (
            Dir::Ir,
            "c",
            "bd4fb1a0abcd98488b5f5414c9847a6bb6dbc530416338290f7d320cbc72fef3",
        ),
    ];
    let expected: Vec<_> = expected
        .iter()
        .map(|(dir, extension, digest)| (*dir, format!("{m}.{extension}"), digest.to_string()))
        .collect();
    let actual: Vec<_> = digests(&archive)
        .into_iter()
        .map(|(dir, path, digest)| (dir, path.to_owned(), digest))
        .collect();
    assert_eq!(actual, expected);
    assert_eq!(
        String::from_utf8(archive.trace.bytes).unwrap(),
        r#"{"schemaVersion":"2025-09-10","depHash":"0123456789abcdef","outputs":{"m":true,"o":["1111111111111111.olean","2222222222222222.olean.server","3333333333333333.olean.private"],"i":"4444444444444444.ilean","rs":"5555555555555555.ir.sig","r":"6666666666666666.ir","c":"7777777777777777.c"}}"#
    );
}

#[test]
fn logged_trace_gains_the_hash_with_sorted_keys() {
    let archive = unpack(&fixture("logged.ltar"), HASH).unwrap();
    assert_eq!(
        String::from_utf8(archive.trace.bytes.clone()).unwrap(),
        r#"{"depHash":"0123456789abcdef","log":[{"level":"warning","message":"warning: \u0000 unused"}],"outputs":{"c":"7777777777777777.c","i":"4444444444444444.ilean","o":["1111111111111111.olean"]},"schemaVersion":"2025-09-10"}"#
    );
    assert_eq!(
        digests(&archive),
        [
            (
                Dir::Lib,
                "Logged.trace",
                "701843323d97e3a67b4c9c4d890358785510940cead36f5ce14028c5a8fb9909".into()
            ),
            (
                Dir::Lib,
                "Logged.olean",
                "dbf7cc7fe131c6e38ff5578d5ac2ae72178fad53bba2ba7d25cb3fe251bd7953".into()
            ),
            (
                Dir::Lib,
                "Logged.ilean",
                "ca3d163bab055381827226140568f3bef7eaac187cebd76878e0b63e9e442356".into()
            ),
            (
                Dir::Ir,
                "Logged.c",
                "7c725f30854a46033dd94f728ac6b08caf10845993cd3ed48e40079cdb0a76a6".into()
            ),
        ]
    );
}

#[test]
fn sized_strings_and_scalar_arrays_decode() {
    // A constructor with two fields: the string "a\0b" (0xb0) and a scalar
    // array of two u32s (0xa0), as `leantar -x` writes it.
    let olean = "6f6c65616e0201342e33372e30000000000000000000000000000000000000000000000000000000\
                 30303030303030303030303030303030303030303030303030303030303030303030303030303030\
                 0000010000000000a80001000000000000000000010000f904000000000000000400000000000000\
                 0300000000000000610062000000000000000000010004f802000000000000000200000000000000\
                 0100000002000000000000001800020060000100000000008800010000000000";
    let archive = unpack(&fixture("patched.ltar"), HASH).unwrap();
    let [file] = &archive.files[..] else {
        panic!("expected one file, got {:?}", archive.files)
    };
    assert_eq!(file.path, "T.olean");
    let hex: String = file.bytes.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, olean);
}

#[test]
fn malformed_archives_fail() {
    let module = fixture("module.ltar");
    for end in [0, 3, 5, 40, module.len() / 2, module.len() - 1] {
        assert!(
            unpack(&module[..end], HASH).is_err(),
            "prefix of {end} bytes"
        );
    }
    assert!(matches!(
        unpack(b"LTR3\0", HASH),
        Err(Error::Unsupported { .. })
    ));
    let mut escape = b"LTR4\0../T.trace\0\x06".to_vec();
    escape.extend([0; 25]);
    assert!(matches!(
        unpack(&escape, HASH),
        Err(Error::Malformed { .. })
    ));
}

#[test]
fn packing_gives_back_leantars_archives() {
    // `leantar -s` packed these from the files they unpack to.
    for name in ["module.ltar", "logged.ltar"] {
        let original = fixture(name);
        let packed = pack(&unpack(&original, HASH).unwrap()).unwrap();
        assert!(packed == original, "{name} packs to different bytes");
    }
}

#[test]
fn packed_archives_unpack_to_their_files() {
    for name in ["module.ltar", "logged.ltar", "patched.ltar"] {
        let archive = unpack(&fixture(name), HASH).unwrap();
        let again = unpack(&pack(&archive).unwrap(), HASH).unwrap();
        assert_eq!(again, archive, "{name}");
    }
}
