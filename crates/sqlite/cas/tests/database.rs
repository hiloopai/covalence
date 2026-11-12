//! Databases opened from a store answer as the same bytes in a local file do,
//! under every verification, block size, fetch policy and cache.

mod common;

use std::{ops::Range, sync::Arc};

use common::{Call, QUERIES, Store, answers, rows, sample, sample_file};
use covalence_cas_memory::MemoryCas;
use covalence_cas_trait::Put;
use covalence_lib_cid::{Cid, Multicodec};
use covalence_sqlite_cas::{
    Database, Error, Options,
    cache::{BlockCache, FileCache, Layout, MemoryCache, NoCache},
    fetch::{Aligned, Exact, FetchPolicy, Miss, ReadAhead},
    rusqlite::Connection,
    verify::{Manifest, Verification},
};

const ROWS: u32 = 2_000;

/// The answers of a plain local file, to compare with.
fn expected() -> (Vec<u8>, Vec<Vec<String>>) {
    let (_dir, path) = sample_file(ROWS);
    let connection = Connection::open(&path).unwrap();
    let expected = answers(&connection);
    assert_eq!(expected[0], ["2000|1999000|400000"]);
    assert_eq!(expected.last().unwrap(), &["ok"]);
    drop(connection);
    (std::fs::read(path).unwrap(), expected)
}

#[test]
fn every_way_to_open_answers_as_a_local_file_does() {
    let (bytes, expected) = expected();
    let store = Store::new(bytes.clone());
    let cid = store.blake3();
    let manifest = |hash, chunk| Verification::manifest(Manifest::of(hash, chunk, &bytes).unwrap());
    let ways: Vec<(&str, Options)> = vec![
        ("download", Options::new()),
        (
            "trusted ranges",
            Options::new().verification(Verification::TrustedRanges),
        ),
        (
            "one page at a time, nothing kept",
            Options::new()
                .verification(Verification::TrustedRanges)
                .fetch(Exact)
                .cache(NoCache),
        ),
        (
            "blocks that are no multiple of a page, in a small cache",
            Options::new()
                .verification(Verification::TrustedRanges)
                .block_size(1_000)
                .fetch(Aligned::new(7_000))
                .cache(MemoryCache::new(50_000)),
        ),
        (
            "large blocks in a cache file",
            Options::new()
                .verification(Verification::TrustedRanges)
                .block_size(256 << 10)
                .fetch(ReadAhead::new(1 << 20))
                .cache(FileCache::new(1 << 20)),
        ),
        (
            "one block for everything",
            Options::new()
                .verification(Verification::TrustedRanges)
                .block_size(covalence_sqlite_cas::MAX_BLOCK_SIZE),
        ),
        (
            "ranges checked with a BLAKE3 manifest",
            Options::new().verification(manifest(Multicodec::BLAKE3, 4096)),
        ),
        (
            "ranges checked with a SHA-256 manifest of odd chunks",
            Options::new()
                .verification(manifest(Multicodec::SHA2_256, 10_007))
                .fetch(Exact)
                .cache(FileCache::new(100_000)),
        ),
        (
            "ranges checked with a manifest of one chunk",
            Options::new().verification(manifest(Multicodec::SHA1, 1 << 30)),
        ),
    ];
    for (way, options) in ways {
        let database = Database::open(store.clone(), &cid, options).unwrap();
        assert_eq!(
            (database.cid(), database.size()),
            (&cid, bytes.len() as u64),
            "{way}"
        );
        let connection = database.connect().unwrap();
        assert_eq!(answers(&connection), expected, "{way}");
        // A second connection shares the cache and answers the same.
        assert_eq!(answers(&database.connect().unwrap()), expected, "{way}");
        // Nothing asked for the whole value in memory.
        assert!(!store.take_calls().contains(&Call::Get), "{way}");
    }
}

#[test]
fn a_store_without_ranges_of_its_own_still_answers() {
    let (bytes, expected) = expected();
    let cas = Arc::new(MemoryCas::default());
    let cid: Cid = cas.put(bytes.clone().into()).unwrap();
    let manifest = Manifest::blake3(64 << 10, &bytes).unwrap();
    for verification in [
        Verification::Download,
        Verification::TrustedRanges,
        Verification::manifest(manifest),
    ] {
        let options = Options::new().verification(verification);
        let database = Database::open(cas.clone(), &cid, options).unwrap();
        assert_eq!(answers(&database.connect().unwrap()), expected);
    }
}

#[test]
fn an_empty_value_is_an_empty_database() {
    let store = Store::new(Vec::new());
    let manifest = Manifest::blake3(4096, &[]).unwrap();
    for verification in [
        Verification::Download,
        Verification::TrustedRanges,
        Verification::manifest(manifest),
    ] {
        let options = Options::new().verification(verification);
        let database = Database::open(store.clone(), &store.blake3(), options).unwrap();
        assert_eq!(database.size(), 0);
        let connection = database.connect().unwrap();
        assert_eq!(
            rows(&connection, "SELECT count(*) FROM sqlite_schema"),
            ["0"]
        );
    }
}

#[test]
fn bytes_that_are_no_database_fail_to_connect() {
    let store = Store::new(vec![0x5a; 20_000]);
    for verification in [Verification::Download, Verification::TrustedRanges] {
        let options = Options::new().verification(verification);
        let database = Database::open(store.clone(), &store.blake3(), options).unwrap();
        let error = database.connect().unwrap_err();
        assert!(matches!(error, Error::Sqlite { .. }), "{error}");
        assert!(error.to_string().contains("not a database"), "{error}");
    }
}

#[test]
fn the_database_cannot_be_written() {
    let (bytes, expected) = expected();
    let store = Store::new(bytes);
    for verification in [Verification::Download, Verification::TrustedRanges] {
        let options = Options::new().verification(verification);
        let database = Database::open(store.clone(), &store.blake3(), options).unwrap();
        let connection = database.connect().unwrap();
        for sql in [
            "INSERT INTO items VALUES (-1, 'new', x'00')",
            "UPDATE items SET name = 'changed' WHERE id = 1",
            "DELETE FROM items",
            "CREATE TABLE more (x)",
            "CREATE INDEX more ON items (payload)",
            "DROP TABLE blobs",
            "PRAGMA user_version = 7",
            "PRAGMA journal_mode = WAL; CREATE TABLE more (x)",
            "BEGIN IMMEDIATE; DELETE FROM blobs; COMMIT",
        ] {
            let error = connection.execute_batch(sql).unwrap_err();
            let text = error.to_string();
            assert!(
                text.contains("readonly") || text.contains("read-only"),
                "{sql}: {text}"
            );
            if !connection.is_autocommit() {
                connection.execute_batch("ROLLBACK").unwrap();
            }
        }
        // `VACUUM` fails sooner: it would attach a database to rebuild into.
        let error = connection.execute_batch("VACUUM").unwrap_err();
        assert!(error.to_string().contains("too many attached"), "{error}");
        // Work that needs scratch space still does: it goes to temporary files.
        connection
            .execute_batch(
                "CREATE TEMP TABLE scratch AS SELECT id FROM items WHERE id < 10;
                 DELETE FROM scratch WHERE id > 4;",
            )
            .unwrap();
        assert_eq!(rows(&connection, "SELECT count(*) FROM scratch"), ["5"]);
        assert_eq!(answers(&connection), expected);
        // The store was only read: its `put` panics.
        assert_eq!(answers(&database.connect().unwrap()), expected);
    }
}

#[test]
fn connections_outlive_their_database_and_work_across_threads() {
    let (bytes, expected) = expected();
    let store = Store::new(bytes);
    let options = Options::new()
        .verification(Verification::TrustedRanges)
        .cache(MemoryCache::new(64 << 10));
    let database = Database::open(store.clone(), &store.blake3(), options).unwrap();
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let database = database.clone();
            std::thread::spawn(move || {
                let connection = database.connect().unwrap();
                drop(database);
                (0..3).map(|_| answers(&connection)).collect::<Vec<_>>()
            })
        })
        .collect();
    let connection = database.connect().unwrap();
    drop(database);
    for thread in threads {
        for answers in thread.join().unwrap() {
            assert_eq!(answers, expected);
        }
    }
    // The cache is far smaller than the database, so this still reads the store.
    store.take_calls();
    assert_eq!(answers(&connection), expected);
    assert!(!store.take_calls().is_empty());
}

#[test]
fn stats_count_reads_and_fetches() {
    let (bytes, _) = expected();
    let store = Store::new(bytes.clone());
    let size = bytes.len() as u64;

    let download = Database::open(store.clone(), &store.blake3(), Options::new()).unwrap();
    let stats = download.stats();
    assert_eq!((stats.reads, stats.read_bytes), (0, 0));
    assert_eq!((stats.fetches, stats.fetched_bytes), (1, size));
    assert_eq!(store.take_calls(), [Call::GetTo]);
    answers(&download.connect().unwrap());
    let stats = download.stats();
    assert!(stats.reads > 100 && stats.read_bytes > size, "{stats:?}");
    assert_eq!((stats.fetches, stats.fetched_bytes), (1, size));
    // Once downloaded, nothing more is asked of the store.
    assert_eq!(store.take_calls(), []);

    let options = Options::new()
        .verification(Verification::TrustedRanges)
        .fetch(Exact)
        .cache(NoCache);
    let ranges = Database::open(store.clone(), &store.blake3(), options).unwrap();
    assert_eq!(ranges.stats(), Default::default());
    assert_eq!(store.take_calls(), [Call::Size]);
    let connection = ranges.connect().unwrap();
    rows(&connection, "SELECT count(*) FROM items");
    let stats = ranges.stats();
    // With no cache and exact fetches, every read is one fetch of its blocks.
    assert!(
        stats.reads > 10 && stats.read_bytes <= stats.fetched_bytes,
        "{stats:?}"
    );
    assert_eq!(
        (stats.fetches, stats.fetched_bytes),
        (stats.reads, stats.reads * 4096)
    );
    let fetched: u64 = store.take_ranges().iter().map(|(_, len)| len).sum();
    assert_eq!(fetched, stats.fetched_bytes);
}

/// A policy that records the misses it is asked about and answers with `plan`.
struct Recorded<F>(std::sync::Mutex<Vec<Miss>>, F);

impl<F: Fn(&Miss) -> Range<u64> + Send + Sync> FetchPolicy for Recorded<F> {
    fn plan(&self, miss: &Miss) -> Range<u64> {
        self.0.lock().unwrap().push(miss.clone());
        (self.1)(miss)
    }
}

/// A cache that keeps nothing and has no limit to say: every read is fetched,
/// as its policy plans it.
struct Forgetful;

impl BlockCache for Forgetful {
    fn read(&self, _block: u64, _offset: usize, _buf: &mut [u8]) -> bool {
        false
    }
    fn insert(&self, _block: u64, _bytes: &[u8]) {}
}

fn forgetful(_layout: Layout) -> std::io::Result<Box<dyn BlockCache>> {
    Ok(Box::new(Forgetful))
}

#[test]
fn fetch_policies_size_the_requests_and_can_be_swapped_while_open() {
    let (bytes, expected) = expected();
    let size = bytes.len() as u64;
    let store = Store::new(bytes);
    let options = Options::new()
        .verification(Verification::TrustedRanges)
        .fetch(Exact)
        .cache(forgetful);
    let database = Database::open(store.clone(), &store.blake3(), options).unwrap();
    let connection = database.connect().unwrap();
    store.take_calls();

    // Exact: whole blocks, and only those of the read.
    assert_eq!(answers(&connection), expected);
    let exact = store.take_ranges();
    assert!(
        exact
            .iter()
            .all(|(start, len)| start % 4096 == 0 && *len == 4096),
        "{exact:?}"
    );

    // Aligned: every request is one window, cut at the end of the database.
    database.set_fetch_policy(Aligned::new(64 << 10));
    assert_eq!(answers(&connection), expected);
    let aligned = store.take_ranges();
    for (start, len) in &aligned {
        assert_eq!(start % (64 << 10), 0, "{aligned:?}");
        assert_eq!(*len, (64 << 10).min(size - start), "{aligned:?}");
    }
    assert!(aligned.len() < exact.len());

    // A policy is told what is missing, and whatever it answers does no harm.
    let hostile: [fn(&Miss) -> Range<u64>; 4] = [
        |_| 0..0,
        |_| u64::MAX..u64::MAX,
        |miss| miss.need.end..miss.need.start,
        |_| 0..u64::MAX,
    ];
    for plan in hostile {
        let policy = Arc::new(Recorded(Default::default(), plan));
        database.set_fetch_policy(policy.clone());
        assert_eq!(answers(&connection), expected);
        let misses = policy.0.lock().unwrap();
        assert!(!misses.is_empty());
        for miss in misses.iter() {
            assert!(miss.need.start < miss.need.end && miss.need.end <= miss.blocks);
            assert_eq!((miss.blocks, miss.block_size), (size.div_ceil(4096), 4096));
        }
        for (start, len) in store.take_ranges() {
            assert!(len > 0 && start + len <= size);
        }
    }
}

#[test]
fn no_request_is_for_more_than_the_cache_can_hold() {
    let (bytes, expected) = expected();
    let store = Store::new(bytes);
    let ranges = |options: Options| {
        let options = options.verification(Verification::TrustedRanges);
        let database = Database::open(store.clone(), &store.blake3(), options).unwrap();
        assert_eq!(store.take_calls(), [Call::Size]);
        assert_eq!(answers(&database.connect().unwrap()), expected);
        store.take_ranges()
    };
    // A cache that keeps nothing: whatever the policy, only what is read.
    let exact = ranges(Options::new().fetch(Exact).cache(NoCache));
    let policies: [Arc<dyn FetchPolicy>; 3] = [
        Arc::new(Aligned::new(1 << 20)),
        Arc::new(ReadAhead::default()),
        Arc::new(Recorded(Default::default(), |_: &Miss| 0..u64::MAX)),
    ];
    for policy in policies {
        assert_eq!(ranges(Options::new().fetch(policy).cache(NoCache)), exact);
    }
    // A cache of 8 blocks: no more than 8 blocks at once, and fewer requests.
    let options = Options::new()
        .fetch(Aligned::new(1 << 20))
        .cache(MemoryCache::new(8 * 4096));
    let small = ranges(options);
    assert!(small.iter().all(|(_, len)| *len <= 8 * 4096), "{small:?}");
    assert!(small.iter().any(|(_, len)| *len == 8 * 4096), "{small:?}");
    assert!(small.len() < exact.len(), "{} {}", small.len(), exact.len());
}

#[test]
fn caches_save_fetches_and_each_database_has_its_own() {
    let (bytes, expected) = expected();
    let store = Store::new(bytes);
    let fetches = |options: Options| {
        let options = options
            .verification(Verification::TrustedRanges)
            .fetch(Exact);
        let database = Database::open(store.clone(), &store.blake3(), options).unwrap();
        // Two connections, so that SQLite's own page caches do not hide the reads.
        assert_eq!(answers(&database.connect().unwrap()), expected);
        assert_eq!(answers(&database.connect().unwrap()), expected);
        database.stats().fetches
    };
    let none = fetches(Options::new().cache(NoCache));
    let small = fetches(Options::new().cache(MemoryCache::new(64 << 10)));
    let memory = fetches(Options::new().cache(MemoryCache::new(64 << 20)));
    let file = fetches(Options::new().cache(FileCache::new(64 << 20)));
    assert!(memory < small && small < none, "{memory} {small} {none}");
    assert_eq!(file, memory);
    // With room for everything, each block is fetched once.
    assert!(memory <= store.blake3_blocks(4096), "{memory}");

    // One policy value opens many databases; their caches are separate.
    let options = Options::new()
        .verification(Verification::TrustedRanges)
        .cache(MemoryCache::new(64 << 20));
    let other = Store::new(sample(10));
    let first = Database::open(store.clone(), &store.blake3(), options.clone()).unwrap();
    let second = Database::open(other.clone(), &other.blake3(), options).unwrap();
    assert_eq!(answers(&first.connect().unwrap()), expected);
    assert_eq!(rows(&second.connect().unwrap(), QUERIES[0]), ["10|45|2000"]);
    assert_eq!(answers(&first.connect().unwrap()), expected);
}

#[cfg(unix)]
#[test]
fn sql_reaches_no_other_file_unless_the_connection_allows() {
    use covalence_sqlite_cas::rusqlite::limits::Limit;

    let (dir, path) = sample_file(10);
    let store = Store::new(sample(10));
    let database = Database::open(store.clone(), &store.blake3(), Options::new()).unwrap();
    let connection = database.connect().unwrap();
    let attach =
        |name: &str, target: &str| connection.execute(&format!("ATTACH ?1 AS {name}"), [target]);
    let vacuum = |target: &str| connection.execute("VACUUM INTO ?1", [target]);
    let uri = format!("file:{}?vfs=unix", path.display());
    let copy = dir.path().join("copy.db");
    let copy_uri = format!("file:{}?vfs=unix", copy.display());

    // As connected, nothing attaches and nothing is written, whatever the name.
    for target in [path.to_str().unwrap(), &uri, ":memory:", ""] {
        let error = attach("other", target).unwrap_err();
        assert!(error.to_string().contains("too many attached"), "{error}");
    }
    for target in [copy.to_str().unwrap(), &copy_uri] {
        let error = vacuum(target).unwrap_err();
        assert!(error.to_string().contains("too many attached"), "{error}");
    }
    assert!(!copy.exists());
    assert_eq!(rows(&connection, "SELECT count(*) FROM items"), ["10"]);

    // Code can allow it; SQL cannot.
    connection
        .set_limit(Limit::SQLITE_LIMIT_ATTACHED, 1)
        .unwrap();

    // A path is asked of this crate's VFS, which knows only its databases.
    attach("plain", path.to_str().unwrap()).unwrap_err();
    vacuum(copy.to_str().unwrap()).unwrap_err();
    assert!(!copy.exists());

    // A URI can name another VFS. The file opens read-only, as the
    // connection is, and cannot be opened for writing.
    attach("other", &uri).unwrap();
    assert_eq!(
        rows(&connection, "SELECT count(*) FROM other.items"),
        ["10"]
    );
    let error = connection
        .execute("DELETE FROM other.items", [])
        .unwrap_err();
    assert!(error.to_string().contains("readonly"), "{error}");
    connection.execute_batch("DETACH other").unwrap();
    for mode in ["rw", "rwc"] {
        attach("writable", &format!("{uri}&mode={mode}")).unwrap_err();
    }

    // Even a database in memory attaches read-only. Temporary tables are
    // the connection's place to write.
    attach("scratch", ":memory:").unwrap();
    let error = connection
        .execute_batch("CREATE TABLE scratch.t (n)")
        .unwrap_err();
    assert!(error.to_string().contains("readonly"), "{error}");
    connection.execute_batch("DETACH scratch").unwrap();
    connection
        .execute_batch("CREATE TEMP TABLE t AS SELECT count(*) AS n FROM main.items")
        .unwrap();
    assert_eq!(rows(&connection, "SELECT n FROM temp.t"), ["10"]);

    // And `VACUUM INTO` then writes a copy where a URI says: the reason
    // connections attach nothing unless code allows it.
    vacuum(&copy_uri).unwrap();
    let copied = Connection::open(&copy).unwrap();
    assert_eq!(rows(&copied, "SELECT count(*) FROM items"), ["10"]);
}
