//! How many bytes range reads take from a database file. One test, alone in
//! its binary, so the process's read count is its own.
#![cfg(target_os = "linux")]

use std::path::Path;

use covalence_cas_sqlite::SqliteCas;
use covalence_cas_trait::{Get, Put};
use covalence_lib_cid::Cid;
use covalence_lib_sqlite::Connection;

const SIZE: usize = 32 << 20;
const PART: usize = 1 << 20;

/// Bytes this process has read through system calls, cached or not.
fn read_bytes() -> u64 {
    let io = std::fs::read_to_string("/proc/self/io").unwrap();
    let line = io.lines().find(|line| line.starts_with("rchar:")).unwrap();
    line["rchar:".len()..].trim().parse().unwrap()
}

fn auto_vacuum(path: &Path) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
        .unwrap()
}

/// Read every part of the block in order, check it, and return the bytes
/// read from the file for the last part alone and for all of them.
fn read_parts(path: &Path, cid: &Cid, data: &[u8]) -> (u64, u64) {
    let store = SqliteCas::open(path).unwrap();
    let parts = SIZE / PART;
    let before = read_bytes();
    let last = store
        .get_range_unchecked(cid, ((parts - 1) * PART) as u64, PART as u64)
        .unwrap()
        .unwrap();
    let after_last = read_bytes();
    assert_eq!(last, data[SIZE - PART..]);
    for part in 0..parts {
        let start = part * PART;
        let bytes = store
            .get_range_unchecked(cid, start as u64, PART as u64)
            .unwrap()
            .unwrap();
        assert_eq!(bytes, data[start..start + PART]);
    }
    (after_last - before, read_bytes() - after_last)
}

#[test]
fn ranges_read_about_their_own_length() {
    let data: Vec<u8> = (0..SIZE as u64)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let dir = tempfile::tempdir().unwrap();

    // A database this store creates keeps a pointer map.
    let path = dir.path().join("new");
    let cid: Cid = SqliteCas::open(&path)
        .unwrap()
        .put(data.clone().into())
        .unwrap();
    assert_eq!(auto_vacuum(&path), 2, "incremental");
    let (last, all) = read_parts(&path, &cid, &data);
    assert!(last < 2 * PART as u64, "the last part read {last} bytes");
    assert!(all < 2 * SIZE as u64, "all parts read {all} bytes");

    // One created without a map still reads correctly, from the block's
    // start, and opening it does not rewrite it.
    let path = dir.path().join("old");
    Connection::open(&path)
        .unwrap()
        .execute_batch(
            "PRAGMA journal_mode = WAL;
            CREATE TABLE blobs (
                blobid INTEGER PRIMARY KEY,
                storage TEXT NOT NULL DEFAULT '',
                data BLOB
            );",
        )
        .unwrap();
    let old: Cid = SqliteCas::open(&path)
        .unwrap()
        .put(data.clone().into())
        .unwrap();
    assert_eq!(old, cid);
    assert_eq!(auto_vacuum(&path), 0, "none");
    let (last, _) = read_parts(&path, &cid, &data);
    assert!(last >= SIZE as u64 - PART as u64, "{last}");

    // VACUUM adds the map, as the store's docs say.
    Connection::open(&path)
        .unwrap()
        .execute_batch("PRAGMA auto_vacuum = INCREMENTAL; VACUUM;")
        .unwrap();
    assert_eq!(auto_vacuum(&path), 2, "incremental");
    let (last, all) = read_parts(&path, &cid, &data);
    assert!(last < 2 * PART as u64, "the last part read {last} bytes");
    assert!(all < 2 * SIZE as u64, "all parts read {all} bytes");
}
