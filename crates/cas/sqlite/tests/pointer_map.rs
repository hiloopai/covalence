//! Which databases get SQLite's pointer map: only those the store creates.
//! Opening an existing database leaves its `auto_vacuum` mode alone.

use std::{path::Path, sync::Barrier, thread};

use covalence_cas_sqlite::{Sha256Policy, SqliteCas};
use covalence_cas_trait::{Get, Put};
use covalence_lib_cid::Cid;
use covalence_lib_sqlite::Connection;

fn auto_vacuum(path: &Path) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn a_new_database_gets_a_pointer_map() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let cid: Cid = SqliteCas::open(&path).unwrap().put("hello".into()).unwrap();
    assert_eq!(auto_vacuum(&path), 2, "incremental");
    // An empty file is a new database too.
    let empty = dir.path().join("empty");
    std::fs::File::create(&empty).unwrap();
    drop(SqliteCas::open(&empty).unwrap());
    assert_eq!(auto_vacuum(&empty), 2, "incremental");
    assert_eq!(
        SqliteCas::open(&path).unwrap().get(&cid).unwrap().unwrap(),
        "hello"
    );
}

#[test]
fn opening_keeps_an_existing_mode() {
    let dir = tempfile::tempdir().unwrap();
    for (name, mode) in [("none", 0), ("full", 1), ("incremental", 2)] {
        // A database as an earlier version made it: tables first, so no map,
        // then set to `mode` by its operator.
        let path = dir.path().join(name);
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
            CREATE TABLE blobs (
                blobid INTEGER PRIMARY KEY,
                storage TEXT NOT NULL DEFAULT '',
                data BLOB
            );",
        )
        .unwrap();
        db.execute_batch(&format!("PRAGMA auto_vacuum = {mode}; VACUUM;"))
            .unwrap();
        drop(db);
        assert_eq!(auto_vacuum(&path), mode, "{name} before");

        let cid: Cid = SqliteCas::open(&path).unwrap().put("hello".into()).unwrap();
        let store = SqliteCas::open_with(&path, Sha256Policy::Always).unwrap();
        assert_eq!(store.get(&cid).unwrap().unwrap(), "hello");
        drop(store);
        assert_eq!(auto_vacuum(&path), mode, "{name} after");
    }
}

#[test]
fn racing_opens_of_a_new_file_agree() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let barrier = Barrier::new(8);
    thread::scope(|scope| {
        for i in 0..8 {
            let (path, barrier) = (&path, &barrier);
            scope.spawn(move || {
                barrier.wait();
                let store = SqliteCas::open(path).unwrap();
                let cid: Cid = store.put(format!("block {i}").into()).unwrap();
                assert_eq!(store.get(&cid).unwrap().unwrap(), format!("block {i}"));
            });
        }
    });
    assert_eq!(auto_vacuum(&path), 2, "incremental");
}
