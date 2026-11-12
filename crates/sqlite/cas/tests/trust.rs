//! What a store can and cannot get past each verification: wrong bytes, wrong
//! lengths, failures and absences.

mod common;

use std::sync::{Arc, Mutex};

use common::{Call, Fault, Store, answers, rows, sample};
use covalence_cas_memory::MemoryCas;
use covalence_cas_trait::Put;
use covalence_lib_cid::{Cid, Format, Multicodec};
use covalence_lib_error::BoxError;
use covalence_sqlite_cas::{
    Database, Error, MAX_BLOCK_SIZE, Options,
    cache::{MemoryCache, NoCache},
    fetch::{Aligned, Exact, ReadAhead},
    rusqlite::{self, Connection},
    verify::{Manifest, RangeVerifier, Verification},
};

const ROWS: u32 = 2_000;
/// The hash functions a manifest can use.
const HASHES: [Multicodec; 4] = [
    Multicodec::BLAKE3,
    Multicodec::SHA2_256,
    Multicodec::SHA2_512,
    Multicodec::SHA1,
];

/// Those whose digests a store's CIDs hold: at most 32 bytes.
const CID_HASHES: [Multicodec; 3] = [Multicodec::BLAKE3, Multicodec::SHA2_256, Multicodec::SHA1];

/// Reads every page of a sample database's tables and index.
const SCAN: &str = "SELECT (SELECT sum(length(payload)) FROM items)
    + (SELECT count(*) FROM items WHERE name > '')
    + (SELECT sum(length(hex(body))) FROM blobs)";

/// The answer to [`SCAN`] for a sample of [`ROWS`] rows.
const SCANNED: i64 = 400_000 + 2_000 + 600_000;

/// Counts the rows whose payload is 200 bytes of 0x18: eight of [`ROWS`],
/// those whose `id % 251` is 24.
fn payloads() -> String {
    format!(
        "SELECT count(*) FROM items WHERE payload = x'{}'",
        "18".repeat(200)
    )
}

/// The offset of a byte in the payload of the last such row in the file. It
/// is far from the pages that opening a connection reads.
fn payload_offset(bytes: &[u8]) -> u64 {
    let found = bytes.windows(200).rposition(|window| window == [0x18; 200]);
    let offset = found.expect("the sample has that payload") as u64 + 100;
    assert!(offset > 256 << 10, "{offset}");
    offset
}

fn ranges(verification: Verification) -> Options {
    // Every page SQLite reads is asked of the store, and nothing is kept.
    Options::new()
        .verification(verification)
        .fetch(Exact)
        .cache(NoCache)
}

fn scan(connection: &Connection) -> Result<i64, rusqlite::Error> {
    connection.query_row(SCAN, [], |row| row.get(0))
}

#[test]
fn a_download_is_checked_against_its_cid_whatever_the_store_checks() {
    let bytes = sample(ROWS);
    let size = bytes.len() as u64;
    // A careful store checks what it gives; a careless one does not.
    for store in [Store::new(bytes.clone()), Store::careless(bytes.clone())] {
        for hash in CID_HASHES {
            let cid = store.cid(hash);
            let open = || Database::open(store.clone(), &cid, Options::new());
            let database = open().unwrap();
            assert_eq!(scan(&database.connect().unwrap()).unwrap(), SCANNED);

            for fault in [
                Fault::Flip(0),
                Fault::Flip(size / 2),
                Fault::Flip(size - 1),
                Fault::Short(1),
                Fault::Short(size as usize),
                Fault::Long(1),
            ] {
                store.set(fault);
                let error = open().unwrap_err();
                assert!(
                    matches!(error, Error::Corrupt { offset: 0, .. }),
                    "{fault:?}: {error}"
                );
            }
            store.set(Fault::Missing);
            assert!(
                matches!(open().unwrap_err(), Error::Missing { cid: missing } if missing == cid)
            );
            store.set(Fault::Fail);
            let error = open().unwrap_err();
            assert!(matches!(error, Error::Store { .. }), "{error}");
            assert!(error.to_string().contains("the store is down"), "{error}");
            store.set(Fault::None);
        }
    }
}

#[test]
fn a_download_for_another_cid_or_of_another_length_is_refused() {
    let bytes = sample(10);
    let size = bytes.len() as u64;
    let store = Store::careless(bytes);
    let other = Store::new(sample(11)).blake3();
    let error = Database::open(store.clone(), &other, Options::new()).unwrap_err();
    assert!(matches!(error, Error::Corrupt { offset: 0, .. }), "{error}");
    assert!(error.to_string().contains(&other.to_string()), "{error}");

    // The bytes are right, but the store miscounts them.
    *store.size.lock().unwrap() = size + 1;
    let error = Database::open(store.clone(), &store.blake3(), Options::new()).unwrap_err();
    assert!(
        matches!(error, Error::Length { offset: 0, expected, actual } if expected == size + 1 && actual == size),
        "{error}"
    );
}

#[test]
fn a_download_stops_at_its_limit() {
    let bytes = sample(ROWS);
    let size = bytes.len() as u64;
    for store in [Store::new(bytes.clone()), Store::careless(bytes.clone())] {
        let open = |limit| {
            Database::open(
                store.clone(),
                &store.blake3(),
                Options::new().max_download(limit),
            )
        };
        for limit in [0, 1, size - 1] {
            let error = open(limit).unwrap_err();
            assert!(
                matches!(error, Error::TooLarge { limit: at } if at == limit),
                "{error}"
            );
        }
        assert_eq!(open(size).unwrap().size(), size);
        assert_eq!(open(u64::MAX).unwrap().size(), size);
    }
    // The default limit is 1 GiB.
    assert_eq!(Options::MAX_DOWNLOAD, 1 << 30);
}

#[test]
fn a_download_with_a_hash_that_cannot_be_computed_is_refused_unasked() {
    let store = Store::careless(sample(10));
    let format = Format {
        codec: Multicodec::RAW,
        hash: Multicodec(0xb220),
    };
    let cid = Cid::new(format, &[7; 32]).unwrap();
    let error = Database::open(store.clone(), &cid, Options::new()).unwrap_err();
    assert!(matches!(error, Error::Unsupported { .. }), "{error}");
    assert_eq!(store.take_calls(), []);
}

#[test]
fn a_download_leaves_no_file_behind() {
    let dir = tempfile::tempdir().unwrap();
    let files = || std::fs::read_dir(dir.path()).unwrap().count();
    let store = Store::careless(sample(ROWS));
    let options = Options::new().dir(dir.path());
    let database = Database::open(store.clone(), &store.blake3(), options.clone()).unwrap();
    let connection = database.connect().unwrap();
    // On Unix the file has no name, so the directory stays empty.
    if cfg!(unix) {
        assert_eq!(files(), 0);
    }
    // A connection keeps the file after its database is dropped.
    drop(database);
    assert_eq!(scan(&connection).unwrap(), SCANNED);
    // Everywhere, nothing is left once the last of them is closed.
    drop(connection);
    assert_eq!(files(), 0);
    store.set(Fault::Flip(5));
    Database::open(store.clone(), &store.blake3(), options.clone()).unwrap_err();
    assert_eq!(files(), 0);

    // A directory that does not exist fails the open: nothing is put elsewhere.
    store.set(Fault::None);
    let options = options.dir(dir.path().join("missing"));
    let error = Database::open(store.clone(), &store.blake3(), options).unwrap_err();
    assert!(matches!(error, Error::Io { .. }), "{error}");
}

#[test]
fn ranges_checked_with_blake3_proofs_are_unsupported_and_nothing_else_is_done() {
    let store = Store::new(sample(10));
    let options = Options::new().verification(Verification::Blake3Ranges);
    let error = Database::open(store.clone(), &store.blake3(), options).unwrap_err();
    assert!(matches!(error, Error::Unsupported { .. }), "{error}");
    assert!(error.to_string().contains("BLAKE3 proofs"), "{error}");
    // No fallback: the store was not asked for a size, a range or a download.
    assert_eq!(store.take_calls(), []);
}

#[test]
fn trusted_ranges_are_not_checked() {
    let bytes = sample(ROWS);
    let store = Store::new(bytes.clone());
    let database = Database::open(
        store.clone(),
        &store.blake3(),
        ranges(Verification::TrustedRanges),
    )
    .unwrap();
    assert_eq!(rows(&database.connect().unwrap(), &payloads()), ["8"]);
    // A store that changes a byte changes the answer, and nothing notices.
    store.set(Fault::Flip(payload_offset(&bytes)));
    assert_eq!(rows(&database.connect().unwrap(), &payloads()), ["7"]);
    // Even a CID that is not the content's opens.
    let other = Store::new(sample(11)).blake3();
    let database = Database::open(store.clone(), &other, ranges(Verification::TrustedRanges));
    assert_eq!(
        rows(&database.unwrap().connect().unwrap(), &payloads()),
        ["7"]
    );
}

#[test]
fn ranges_of_the_wrong_length_fail_the_read_that_asked() {
    let bytes = sample(ROWS);
    let manifest = Manifest::blake3(4096, &bytes).unwrap();
    for verification in [
        Verification::TrustedRanges,
        Verification::manifest(manifest),
    ] {
        let store = Store::new(bytes.clone());
        let database =
            Database::open(store.clone(), &store.blake3(), ranges(verification)).unwrap();
        let connection = database.connect().unwrap();
        for (fault, by) in [
            (Fault::Short(1), -1),
            (Fault::Short(4096), -4096),
            (Fault::Long(1), 1),
        ] {
            store.set(fault);
            // SQLite says only that reading failed; the database knows why.
            let failed = scan(&connection).unwrap_err();
            assert!(failed.to_string().contains("disk I/O error"), "{failed}");
            let error = database.explain(failed);
            assert!(
                matches!(error, Error::Length { offset, expected: 4096, actual }
                    if offset % 4096 == 0 && actual as i64 == 4096 + by),
                "{fault:?}: {error}"
            );
            // A connection cannot be opened either: its first read fails.
            let error = database.connect().unwrap_err();
            assert!(
                matches!(error, Error::Length { offset: 0, .. }),
                "{fault:?}: {error}"
            );
        }
        // Nothing wrong was kept: when the store recovers, so do the answers.
        store.set(Fault::None);
        assert_eq!(scan(&connection).unwrap(), SCANNED);
    }
}

#[test]
fn a_store_that_fails_or_loses_the_database_fails_the_read() {
    let bytes = sample(ROWS);
    let manifest = Manifest::blake3(4096, &bytes).unwrap();
    for verification in [
        Verification::TrustedRanges,
        Verification::manifest(manifest),
    ] {
        let store = Store::new(bytes.clone());
        let cid = store.blake3();
        let open = || Database::open(store.clone(), &cid, ranges(verification.clone()));
        let database = open().unwrap();
        let connection = database.connect().unwrap();

        store.set(Fault::Fail);
        let error = database.explain(scan(&connection).unwrap_err());
        assert!(matches!(error, Error::Store { .. }), "{error}");
        assert!(matches!(open().unwrap_err(), Error::Store { .. }));

        store.set(Fault::Missing);
        let error = database.explain(scan(&connection).unwrap_err());
        assert!(
            matches!(error, Error::Missing { cid: missing } if missing == cid),
            "{error}"
        );
        assert!(matches!(open().unwrap_err(), Error::Missing { .. }));

        store.set(Fault::None);
        assert_eq!(scan(&connection).unwrap(), SCANNED);
    }
}

#[test]
fn explain_keeps_errors_that_are_not_failed_reads() {
    let store = Store::new(sample(10));
    let database = Database::open(
        store.clone(),
        &store.blake3(),
        ranges(Verification::TrustedRanges),
    )
    .unwrap();
    let connection = database.connect().unwrap();
    let syntax = connection.execute_batch("SELEKT 1").unwrap_err();
    assert!(matches!(database.explain(syntax), Error::Sqlite { .. }));

    // A failed read is explained once, to the first I/O error that asks.
    store.set(Fault::Fail);
    let failed = scan(&connection).unwrap_err();
    let syntax = connection.execute_batch("SELEKT 1").unwrap_err();
    assert!(matches!(database.explain(syntax), Error::Sqlite { .. }));
    assert!(matches!(database.explain(failed), Error::Store { .. }));
    let failed = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR_READ),
        None,
    );
    assert!(matches!(database.explain(failed), Error::Sqlite { .. }));
}

#[test]
fn verified_ranges_refuse_bytes_that_are_not_the_contents() {
    let bytes = sample(ROWS);
    let flip = payload_offset(&bytes);
    for hash in HASHES {
        let policies: [Options; 3] = [
            Options::new().fetch(Exact).cache(NoCache),
            Options::new()
                .fetch(Aligned::new(64 << 10))
                .cache(MemoryCache::new(1 << 30)),
            Options::new()
                .fetch(ReadAhead::new(1 << 20))
                .cache(MemoryCache::new(1 << 30)),
        ];
        for options in policies {
            let store = Store::new(bytes.clone());
            let manifest = Manifest::of(hash, 4096, &bytes).unwrap();
            let options = options.verification(Verification::manifest(manifest));
            let database = Database::open(store.clone(), &store.blake3(), options).unwrap();
            let connection = database.connect().unwrap();

            store.set(Fault::Flip(flip));
            let error = database.explain(
                connection
                    .query_row(&payloads(), [], |_| Ok(()))
                    .unwrap_err(),
            );
            let chunk = flip / 4096;
            assert!(
                matches!(&error, Error::Corrupt { offset, .. } if *offset % 4096 == 0 && *offset <= flip),
                "{error}"
            );
            assert!(
                error
                    .to_string()
                    .contains(&format!("chunk {chunk} does not hash")),
                "{error}"
            );

            // The refused bytes reached neither SQLite nor the cache: the
            // answer is right as soon as the store is.
            store.set(Fault::None);
            assert_eq!(rows(&connection, &payloads()), ["8"]);
            assert_eq!(rows(&database.connect().unwrap(), &payloads()), ["8"]);
        }
    }
}

#[test]
fn a_manifest_of_other_content_opens_nothing() {
    let bytes = sample(ROWS);
    let store = Store::new(bytes.clone());

    // Content of the same size: the first read is refused.
    let mut other = bytes.clone();
    other[50] ^= 1;
    let manifest = Manifest::blake3(4096, &other).unwrap();
    let options = ranges(Verification::manifest(manifest));
    let database = Database::open(store.clone(), &store.blake3(), options).unwrap();
    let error = database.connect().unwrap_err();
    assert!(matches!(error, Error::Corrupt { offset: 0, .. }), "{error}");

    // Content of another size: the open is refused.
    let manifest = Manifest::blake3(4096, &bytes[..bytes.len() - 1]).unwrap();
    let options = ranges(Verification::manifest(manifest));
    let error = Database::open(store.clone(), &store.blake3(), options).unwrap_err();
    let size = bytes.len() as u64;
    assert!(
        matches!(error, Error::Length { offset: 0, expected, actual } if expected == size - 1 && actual == size),
        "{error}"
    );
}

/// A verifier that accepts everything and records what it is asked.
struct Recorder {
    size: u64,
    chunk_size: u64,
    asked: Mutex<Vec<(u64, usize)>>,
}

impl RangeVerifier for Recorder {
    fn size(&self) -> u64 {
        self.size
    }
    fn chunk_size(&self) -> u64 {
        self.chunk_size
    }
    fn verify(&self, chunk: u64, bytes: &[u8]) -> Result<(), BoxError> {
        self.asked.lock().unwrap().push((chunk, bytes.len()));
        Ok(())
    }
}

#[test]
fn a_verifier_of_ones_own_is_given_whole_chunks() {
    let bytes = sample(ROWS);
    let size = bytes.len() as u64;
    let store = Store::new(bytes);
    let recorder = Arc::new(Recorder {
        size,
        chunk_size: 10_000,
        asked: Mutex::default(),
    });
    let options = Options::new()
        .verification(Verification::VerifiedRanges(recorder.clone()))
        // The verifier's chunks are the blocks, whatever the options say.
        .block_size(512)
        .fetch(ReadAhead::new(100_000));
    let database = Database::open(store.clone(), &store.blake3(), options).unwrap();
    assert_eq!(store.take_calls(), [Call::Size]);
    answers(&database.connect().unwrap());
    let asked = recorder.asked.lock().unwrap();
    let fetched = store.take_ranges();
    assert_eq!(asked.len(), fetched.len());
    for ((chunk, len), (start, fetched)) in asked.iter().zip(fetched) {
        assert_eq!((chunk * 10_000, *len as u64), (start, fetched));
        let end = start + fetched;
        assert!(end % 10_000 == 0 || end == size, "{start} {fetched}");
    }
    // Every chunk was checked at least once before SQLite saw it.
    let mut seen = vec![false; size.div_ceil(10_000) as usize];
    for (chunk, len) in asked.iter() {
        for index in 0..len.div_ceil(10_000) {
            seen[*chunk as usize + index] = true;
        }
    }
    assert!(seen.iter().all(|seen| *seen));
}

#[test]
fn sizes_and_block_sizes_out_of_range_are_refused() {
    let store = Store::new(sample(10));
    let cid = store.blake3();
    for block_size in [0, MAX_BLOCK_SIZE + 1, u64::MAX] {
        let options = ranges(Verification::TrustedRanges).block_size(block_size);
        let error = Database::open(store.clone(), &cid, options).unwrap_err();
        assert!(matches!(error, Error::Options { .. }), "{error}");
        let verifier = Arc::new(Recorder {
            size: 20_480,
            chunk_size: block_size,
            asked: Mutex::default(),
        });
        let options = ranges(Verification::VerifiedRanges(verifier));
        let error = Database::open(store.clone(), &cid, options).unwrap_err();
        assert!(matches!(error, Error::Options { .. }), "{error}");
        // Download does not use blocks.
        Database::open(store.clone(), &cid, Options::new().block_size(block_size)).unwrap();
    }
    assert_eq!(store.take_calls(), [Call::GetTo; 3]);

    // SQLite addresses at most `i64::MAX` bytes.
    for size in [i64::MAX as u64 + 1, u64::MAX] {
        *store.size.lock().unwrap() = size;
        let error =
            Database::open(store.clone(), &cid, ranges(Verification::TrustedRanges)).unwrap_err();
        assert!(
            matches!(error, Error::TooLarge { limit } if limit == i64::MAX as u64),
            "{error}"
        );
        let verifier = Arc::new(Recorder {
            size,
            chunk_size: 4096,
            asked: Mutex::default(),
        });
        let options = ranges(Verification::VerifiedRanges(verifier));
        let error = Database::open(store.clone(), &cid, options).unwrap_err();
        assert!(matches!(error, Error::TooLarge { .. }), "{error}");
    }
    // The largest size opens, with blocks of any size: nothing overflows.
    *store.size.lock().unwrap() = i64::MAX as u64;
    for block_size in [1, 4096, MAX_BLOCK_SIZE] {
        let options = ranges(Verification::TrustedRanges).block_size(block_size);
        let database = Database::open(store.clone(), &cid, options).unwrap();
        assert_eq!(database.size(), i64::MAX as u64);
        // The store has far fewer bytes than it says.
        let error = database.connect().unwrap_err();
        assert!(
            matches!(error, Error::Length { .. } | Error::Sqlite { .. }),
            "{block_size}: {error}"
        );
    }
}

#[test]
fn a_manifest_kept_in_the_store_vouches_for_the_database() {
    // An untrusted store holds the database and its manifest; only the
    // manifest's CID comes from somewhere trusted.
    let bytes = sample(ROWS);
    let cas = Arc::new(MemoryCas::default());
    let database: Cid = cas.put(bytes.clone().into()).unwrap();
    let trusted = Manifest::blake3(64 << 10, &bytes)
        .unwrap()
        .put(&*cas)
        .unwrap();

    let manifest = Manifest::load(&*cas, &trusted).unwrap();
    assert_eq!(manifest.size(), bytes.len() as u64);
    let options = Options::new().verification(Verification::manifest(manifest));
    let database = Database::open(cas, &database, options).unwrap();
    assert_eq!(scan(&database.connect().unwrap()).unwrap(), SCANNED);
}
