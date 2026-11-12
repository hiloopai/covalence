//! Read-only VFSes: what SQLite reads, what it may not do, and what happens
//! when a file misbehaves. Registrations last for the process, so each test
//! registers a VFS of its own name.

use std::{
    fs, io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering::SeqCst},
    },
    thread,
};

use covalence_lib_sqlite::{
    Connection,
    rusqlite::{self, ErrorCode, OpenFlags, types::Value},
    vfs::{self, Error, File, Registered},
};

/// A file in memory.
struct Memory(Arc<Vec<u8>>);

impl File for Memory {
    fn size(&self) -> io::Result<u64> {
        Ok(self.0.len() as u64)
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let start = offset as usize;
        buf.copy_from_slice(&self.0[start..start + buf.len()]);
        Ok(buf.len())
    }
}

/// A VFS named `name` that serves `bytes` as the file `db`.
fn serve(name: &str, bytes: Vec<u8>) -> Registered {
    let bytes = Arc::new(bytes);
    vfs::register(name, move |file: &str| {
        Ok((file == "db").then(|| Box::new(Memory(bytes.clone())) as Box<dyn File>))
    })
    .unwrap()
}

/// The bytes of a database file made by running `setup`.
fn database(setup: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch(setup).unwrap();
    drop(connection);
    fs::read(&path).unwrap()
}

/// Tables with indices and rows that overflow their pages.
const SHOP: &str = "
    CREATE TABLE customer (id INTEGER PRIMARY KEY, name TEXT NOT NULL, city TEXT);
    CREATE TABLE purchase (id INTEGER PRIMARY KEY, customer INTEGER NOT NULL, cents INTEGER, note BLOB);
    CREATE INDEX purchase_customer ON purchase (customer, cents);
    CREATE INDEX customer_city ON customer (city);
    WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000)
      INSERT INTO customer SELECT i, 'name ' || i, 'city ' || (i % 37) FROM n;
    WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 6000)
      INSERT INTO purchase SELECT i, 1 + (i * 7919) % 2000, (i * 31) % 10007,
        CASE WHEN i % 500 = 0 THEN randomblob(20000) ELSE zeroblob(i % 40) END FROM n;
";

/// Every row of `sql`, as text.
fn rows(connection: &Connection, sql: &str) -> Vec<String> {
    let mut statement = connection.prepare(sql).unwrap();
    let columns = statement.column_count();
    let rows = statement.query_map([], |row| {
        let values: Result<Vec<Value>, _> = (0..columns).map(|i| row.get(i)).collect();
        Ok(format!("{:?}", values?))
    });
    rows.unwrap().map(Result::unwrap).collect()
}

/// SQLite's error code for `error`.
fn code<T: std::fmt::Debug>(result: Result<T, rusqlite::Error>) -> ErrorCode {
    result.unwrap_err().sqlite_error_code().unwrap()
}

#[test]
fn answers_match_the_unix_vfs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch(SHOP).unwrap();
    drop(connection);
    let local = Connection::open_with_flags(
        format!("file:{}?immutable=1", path.display()),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .unwrap();
    let served = serve("test-differential", fs::read(&path).unwrap())
        .open("db")
        .unwrap();
    for sql in [
        "PRAGMA integrity_check",
        "SELECT count(*), sum(cents), sum(length(note)) FROM purchase",
        "SELECT * FROM customer WHERE id IN (1, 77, 2000, 2001)",
        "SELECT city, count(*) FROM customer GROUP BY city ORDER BY city",
        "SELECT c.name, p.cents FROM purchase p JOIN customer c ON c.id = p.customer
           WHERE p.customer BETWEEN 100 AND 120 ORDER BY p.cents DESC, p.id",
        "SELECT id, hex(substr(note, 19000, 16)) FROM purchase WHERE id % 500 = 0 ORDER BY id",
        "SELECT name FROM sqlite_schema ORDER BY name",
    ] {
        let expected = rows(&local, sql);
        assert!(!expected.is_empty(), "{sql}");
        assert_eq!(rows(&served, sql), expected, "{sql}");
    }
}

#[test]
fn writes_fail_and_no_local_file_appears() {
    let dir = tempfile::tempdir().unwrap();
    let registered = serve(
        "test-read-only",
        database("CREATE TABLE t (x); INSERT INTO t VALUES (1)"),
    );
    let connection = registered.open("db").unwrap();
    for sql in [
        "INSERT INTO t VALUES (2)",
        "CREATE TABLE u (y)",
        "DELETE FROM t",
        "VACUUM",
    ] {
        assert_eq!(
            code(connection.execute_batch(sql)),
            ErrorCode::ReadOnly,
            "{sql}"
        );
    }
    assert_eq!(rows(&connection, "SELECT x FROM t"), ["[Integer(1)]"]);

    // Asking SQLite for a writable connection, by name or by URI, gives a
    // read-only one, and never creates a local file.
    let old = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    let rw = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE;
    let by_name = Connection::open_with_flags_and_vfs("db", rw, "test-read-only").unwrap();
    let uri = "file:db?vfs=test-read-only";
    let by_uri = Connection::open_with_flags(uri, rw | OpenFlags::SQLITE_OPEN_URI).unwrap();
    for connection in [by_name, by_uri] {
        let insert = connection.execute_batch("INSERT INTO t VALUES (2)");
        assert_eq!(code(insert), ErrorCode::ReadOnly);
        assert_eq!(rows(&connection, "SELECT x FROM t"), ["[Integer(1)]"]);
    }
    std::env::set_current_dir(old).unwrap();
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn short_files_are_empty_or_not_databases() {
    let empty = serve("test-empty", Vec::new()).open("db").unwrap();
    assert_eq!(
        rows(&empty, "SELECT count(*) FROM sqlite_schema"),
        ["[Integer(0)]"]
    );
    let create = empty.execute_batch("CREATE TABLE t (x)");
    assert_eq!(code(create), ErrorCode::ReadOnly);

    // Bytes that are no database header.
    let garbage = serve("test-garbage", b"not a database, only some text".repeat(9));
    let connection = garbage.open("db").unwrap();
    let query = connection.query_row("SELECT count(*) FROM t", [], |row| row.get::<_, i64>(0));
    assert_eq!(code(query), ErrorCode::NotADatabase);

    // A database cut inside its header or its first page.
    let bytes = database("CREATE TABLE t (x)");
    for (name, len) in [("test-short-50", 50), ("test-short-1000", 1000)] {
        let connection = serve(name, bytes[..len].to_vec()).open("db").unwrap();
        let query = connection.query_row("SELECT count(*) FROM t", [], |row| row.get::<_, i64>(0));
        let code = code(query);
        let damaged = [ErrorCode::NotADatabase, ErrorCode::DatabaseCorrupt].contains(&code);
        assert!(damaged, "{len}: {code:?}");
    }
    // A file cut between pages: the header's page count no longer fits.
    let bytes = database(SHOP);
    let cut = serve("test-cut", bytes[..bytes.len() / 2].to_vec())
        .open("db")
        .unwrap();
    let scan = cut.query_row("SELECT sum(length(note)) FROM purchase", [], |row| {
        row.get::<_, i64>(0)
    });
    assert_eq!(code(scan), ErrorCode::DatabaseCorrupt);
}

#[test]
fn reads_databases_in_wal_mode() {
    let bytes = database("PRAGMA journal_mode = WAL; CREATE TABLE t (x); INSERT INTO t VALUES (7)");
    assert_eq!(bytes[18..20], [2, 2]);
    let connection = serve("test-wal", bytes).open("db").unwrap();
    assert_eq!(rows(&connection, "SELECT x FROM t"), ["[Integer(7)]"]);
}

#[test]
fn names_reach_the_vfs_as_given() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let registered = vfs::register("test-names", move |name: &str| {
        log.lock().unwrap().push(name.to_owned());
        Ok(None)
    })
    .unwrap();
    assert_eq!(registered.name(), "test-names");
    let names = [
        "db",
        "a b/c?d=e#f",
        "bafkr4i/../x",
        "ünïcode",
        "-journal",
        "Memory:",
    ];
    for name in names {
        assert!(
            matches!(registered.open(name), Err(Error::Open { .. })),
            "{name}"
        );
    }
    assert_eq!(*seen.lock().unwrap(), names);

    // Names SQLite would not pass to the VFS, or that do not fit, are refused.
    for name in ["", ":memory:", "file:db", "FILE:db?vfs=unix", "a\0b"] {
        assert!(
            matches!(registered.open(name), Err(Error::Name { .. })),
            "{name}"
        );
    }
    let long = "x".repeat(2000);
    let Err(Error::Open { source }) = registered.open(&long) else {
        panic!("a 2000-byte name opened");
    };
    assert_eq!(source.sqlite_error_code(), Some(ErrorCode::CannotOpen));
    assert_eq!(seen.lock().unwrap().len(), names.len());
}

#[test]
fn taken_and_malformed_vfs_names_are_refused() {
    /// A VFS that counts its drops.
    struct Counted(Arc<AtomicUsize>);
    impl vfs::Vfs for Counted {
        fn open(&self, _name: &str) -> io::Result<Option<Box<dyn File>>> {
            Ok(None)
        }
    }
    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_add(1, SeqCst);
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let new = || Counted(drops.clone());
    vfs::register("test-taken", new()).unwrap();
    assert!(matches!(
        vfs::register("test-taken", new()),
        Err(Error::Taken { .. })
    ));
    assert!(matches!(
        vfs::register("unix", new()),
        Err(Error::Taken { .. })
    ));
    assert!(matches!(vfs::register("", new()), Err(Error::Name { .. })));
    assert!(matches!(
        vfs::register("a\0b", new()),
        Err(Error::Name { .. })
    ));
    // The registered VFS lives on; the four refused ones were dropped.
    assert_eq!(drops.load(SeqCst), 4);
    // SQLite's default VFS is still the default.
    let dir = tempfile::tempdir().unwrap();
    let local = Connection::open(dir.path().join("local")).unwrap();
    local.execute_batch("CREATE TABLE t (x)").unwrap();
}

/// A file that misbehaves in one way.
struct Faulty {
    bytes: Arc<Vec<u8>>,
    fault: Fault,
}

#[derive(Clone, Copy, Debug)]
enum Fault {
    /// `size` fails.
    SizeError,
    /// `size` is more than SQLite can address.
    Huge,
    /// `size` claims a page more than there is, and reads there come short.
    Longer,
    /// Reads fail.
    ReadError,
    /// Reads past the first page fail.
    LateReadError,
    /// Reads say they read one byte fewer than they did.
    Fewer,
    /// Reads say they read one byte more than the buffer holds.
    More,
    /// `size` panics.
    SizePanic,
    /// Reads panic.
    ReadPanic,
    /// Dropping the file panics.
    DropPanic,
}

impl File for Faulty {
    fn size(&self) -> io::Result<u64> {
        match self.fault {
            Fault::SizeError => Err(io::Error::other("no size")),
            Fault::Huge => Ok(i64::MAX as u64 + 1),
            Fault::Longer => Ok(self.bytes.len() as u64 + 4096),
            Fault::SizePanic => panic!("size"),
            _ => Ok(self.bytes.len() as u64),
        }
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let start = (offset as usize).min(self.bytes.len());
        let end = (start + buf.len()).min(self.bytes.len());
        buf[..end - start].copy_from_slice(&self.bytes[start..end]);
        match self.fault {
            Fault::ReadError => Err(io::Error::other("no bytes")),
            Fault::LateReadError if offset >= 4096 => Err(io::Error::other("no bytes")),
            Fault::Fewer => Ok(buf.len() - 1),
            Fault::More => Ok(buf.len() + 1),
            Fault::ReadPanic => panic!("read"),
            _ => Ok(end - start),
        }
    }
}

impl Drop for Faulty {
    fn drop(&mut self) {
        if matches!(self.fault, Fault::DropPanic) {
            panic!("drop");
        }
    }
}

#[test]
fn faults_and_panics_become_errors() {
    let bytes = Arc::new(database(SHOP));
    let registered = vfs::register("test-faults", {
        let bytes = bytes.clone();
        move |name: &str| {
            let fault = match name {
                "missing" => return Ok(None),
                "open-error" => return Err(io::Error::other("no file")),
                "open-panic" => panic!("open"),
                "size-error" => Fault::SizeError,
                "huge" => Fault::Huge,
                "longer" => Fault::Longer,
                "read-error" => Fault::ReadError,
                "late-read-error" => Fault::LateReadError,
                "fewer" => Fault::Fewer,
                "more" => Fault::More,
                "size-panic" => Fault::SizePanic,
                "read-panic" => Fault::ReadPanic,
                "drop-panic" => Fault::DropPanic,
                _ => unreachable!(),
            };
            let bytes = bytes.clone();
            Ok(Some(Box::new(Faulty { bytes, fault }) as Box<dyn File>))
        }
    })
    .unwrap();
    let open_code = |name: &str| match registered.open(name) {
        Err(Error::Open { source }) => source.sqlite_error_code().unwrap(),
        other => panic!("{name}: {other:?}"),
    };
    assert_eq!(open_code("missing"), ErrorCode::CannotOpen);
    for name in [
        "open-error",
        "open-panic",
        "size-error",
        "huge",
        "size-panic",
    ] {
        assert_eq!(open_code(name), ErrorCode::SystemIoFailure, "{name}");
    }
    // SQLite reads the database header when it opens a file, so a file whose
    // every read fails does not open.
    for name in ["read-error", "fewer", "more", "read-panic"] {
        assert_eq!(open_code(name), ErrorCode::SystemIoFailure, "{name}");
    }
    // Later failures are errors of the statement that needed the bytes.
    let connection = registered.open("late-read-error").unwrap();
    let scan = "SELECT count(*) FROM purchase";
    let query = connection.query_row(scan, [], |row| row.get::<_, i64>(0));
    assert_eq!(code(query), ErrorCode::SystemIoFailure);
    // A file that claims a page it cannot read: the page past the end is
    // never asked for, since the database's own header gives its page count.
    let connection = registered.open("longer").unwrap();
    assert_eq!(
        rows(&connection, "PRAGMA integrity_check"),
        ["[Text(\"ok\")]"]
    );
    // A file whose drop panics still answers, and closing it is an error
    // that `rusqlite` reports, not a crash.
    let connection = registered.open("drop-panic").unwrap();
    assert_eq!(
        rows(&connection, "PRAGMA integrity_check"),
        ["[Text(\"ok\")]"]
    );
    let _ = connection.close();
    // The VFS still works after every fault.
    assert_eq!(open_code("missing"), ErrorCode::CannotOpen);
}

/// A file that records the reads it serves and checks what SQLite hands it.
struct Recording {
    bytes: Arc<Vec<u8>>,
    reads: Arc<Mutex<Vec<(u64, usize)>>>,
    dropped: Arc<AtomicUsize>,
}

impl File for Recording {
    fn size(&self) -> io::Result<u64> {
        Ok(self.bytes.len() as u64)
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        assert!(!buf.is_empty());
        assert!(buf.iter().all(|&byte| byte == 0));
        assert!(offset + buf.len() as u64 <= self.bytes.len() as u64);
        self.reads.lock().unwrap().push((offset, buf.len()));
        Memory(self.bytes.clone()).read_at(offset, buf)
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, SeqCst);
    }
}

#[test]
fn reads_stay_inside_the_file_and_files_close() {
    let mut bytes = database(SHOP);
    // A size that is no multiple of the page size: the last page comes short.
    bytes.truncate(bytes.len() - 100);
    let size = bytes.len();
    let bytes = Arc::new(bytes);
    let reads = Arc::new(Mutex::new(Vec::new()));
    let dropped = Arc::new(AtomicUsize::new(0));
    let registered = vfs::register("test-recording", {
        let (reads, dropped) = (reads.clone(), dropped.clone());
        move |_: &str| {
            let (bytes, reads, dropped) = (bytes.clone(), reads.clone(), dropped.clone());
            Ok(Some(Box::new(Recording {
                bytes,
                reads,
                dropped,
            }) as Box<dyn File>))
        }
    })
    .unwrap();
    let connection = registered.open("db").unwrap();
    // Too small a page cache to hold the overflow chains.
    connection.execute_batch("PRAGMA cache_size = 10").unwrap();
    // Whole values, so SQLite reads their overflow pages.
    let mut statement = connection
        .prepare("SELECT note FROM purchase ORDER BY id")
        .unwrap();
    let notes = statement
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .unwrap();
    // The last page is cut, so SQLite may report damage once it gets there.
    let read = notes.take_while(Result::is_ok).count();
    assert!(read > 3000, "{read}");
    drop(statement);
    let reads = reads.lock().unwrap().clone();
    // The header read, then whole pages, parts of overflow pages, and one
    // read cut at the end of the file.
    assert_eq!(reads[0], (0, 100));
    assert!(reads.iter().any(|&(_, len)| len == 4096));
    assert!(reads.iter().any(|&(_, len)| len > 100 && len < 4096));
    assert!(
        reads
            .iter()
            .all(|&(offset, len)| offset as usize + len <= size)
    );
    assert_eq!(dropped.load(SeqCst), 0);
    drop(connection);
    assert_eq!(dropped.load(SeqCst), 1);
}

#[test]
fn sorts_spill_to_temporary_files() {
    let connection = serve("test-temp", database(SHOP)).open("db").unwrap();
    connection
        .execute_batch("PRAGMA temp_store = FILE; PRAGMA cache_size = 10;")
        .unwrap();
    let sorted = rows(
        &connection,
        "SELECT note, cents FROM purchase ORDER BY cents, note",
    );
    assert_eq!(sorted.len(), 6000);
    // Temporary tables live outside the database, so they may be written.
    connection
        .execute_batch("CREATE TEMP TABLE scratch AS SELECT id FROM customer WHERE id < 10")
        .unwrap();
    assert_eq!(
        rows(&connection, "SELECT count(*) FROM scratch"),
        ["[Integer(9)]"]
    );
}

#[test]
fn threads_share_a_file() {
    let registered = serve("test-threads", database(SHOP));
    let expected = rows(
        &registered.open("db").unwrap(),
        "SELECT sum(cents) FROM purchase",
    );
    thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                let connection = registered.open("db").unwrap();
                for _ in 0..20 {
                    assert_eq!(
                        rows(&connection, "SELECT sum(cents) FROM purchase"),
                        expected
                    );
                }
            });
        }
    });
    // A connection may move to another thread.
    let connection = registered.open("db").unwrap();
    let moved = thread::spawn(move || rows(&connection, "SELECT sum(cents) FROM purchase"));
    assert_eq!(moved.join().unwrap(), expected);
}
