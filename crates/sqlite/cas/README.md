# covalence-sqlite-cas

Read-only SQLite databases opened from a content-addressed store.

A database file is kept in a store under the hash of its bytes. This crate
opens it for SQLite without ever making it writable: either after downloading
and checking all of it, or by fetching only the byte ranges that queries read.

## Terms

- A content-addressed store (CAS) keeps bytes under a key computed from the
  bytes. Here it is anything that implements `covalence_cas_trait::Cas`.
- A [CID](https://dasl.ing/cid.html) is such a key: a hash function and the
  digest of the bytes.
- A [VFS](https://www.sqlite.org/vfs.html) is SQLite's interface to files.
  This crate registers one, built on `covalence_lib_sqlite::vfs`, whose files
  are the databases opened here.
- [DRISL](https://dasl.ing/drisl.html) is the deterministic CBOR of
  [DASL](https://dasl.ing/). Chunk manifests are stored in it.
- A block is the unit that is fetched and cached. A chunk is the unit that a
  range verifier checks. With verified ranges they are the same.

## Open a database

`Database::open` takes a store, a CID and `Options`. `Database::connect` gives
`rusqlite` connections. The default options download the database to a local
file with no name and check it against the CID before SQLite reads a byte.

```rust
use std::sync::Arc;

use covalence_cas_memory::MemoryCas;
use covalence_cas_trait::Put;
use covalence_lib_cid::Cid;
use covalence_sqlite_cas::{Database, Options, rusqlite::Connection};

// Make a database file and put its bytes in a store.
let dir = tempfile::tempdir()?;
let path = dir.path().join("example.db");
Connection::open(&path)?.execute_batch(
    "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT);
     INSERT INTO notes (body) VALUES ('first'), ('second');",
)?;
let cas = Arc::new(MemoryCas::default());
let cid: Cid = cas.put(std::fs::read(&path)?.into())?;

// Open it from the store, and query it.
let database = Database::open(cas, &cid, Options::default())?;
let connection = database.connect()?;
let body: String =
    connection.query_row("SELECT body FROM notes WHERE id = 2", [], |row| row.get(0))?;
assert_eq!(body, "second");

// Nothing can change it.
assert!(connection.execute("DELETE FROM notes", []).is_err());
# Ok::<(), Box<dyn std::error::Error>>(())
```

## How far the store is trusted

A CID is the hash of all the bytes, so it can be checked only with all the
bytes at hand. `verify::Verification` says what to do about that:

- `Download`, the default, does not trust the store. It downloads everything
  to a local file, hashes the bytes itself whatever the store checks, and
  fails unless they hash to the CID. It accepts at most
  `Options::max_download` bytes, 1 GiB unless set.
- `TrustedRanges` reads ranges and trusts them. Nothing checks the bytes
  against the CID. Use it only for a store trusted as much as a local file.
- `VerifiedRanges` reads ranges and checks each with a `verify::RangeVerifier`
  before SQLite or a cache sees it. The store is not trusted; the verifier
  must come from somewhere that is.
- `Blake3Ranges`, ranges checked against a BLAKE3 CID with proofs from its
  hash tree, is not supported yet. Opening fails with `Error::Unsupported`
  before the store is asked anything, and never falls back to another mode.

`verify::Manifest` is a range verifier for any hash function this crate
computes (BLAKE3, SHA-256, SHA-512, SHA-1): the digests of the database's
fixed-size chunks. It is small, one digest for each chunk, and can be kept in
the store itself, under a CID that is checked when it is loaded. A reader who
trusts the manifest's CID can then read ranges from an untrusted store:

```rust
use std::sync::Arc;

use covalence_cas_memory::MemoryCas;
use covalence_cas_trait::Put;
use covalence_lib_cid::Cid;
use covalence_sqlite_cas::{
    Database, Options,
    cache::MemoryCache,
    fetch::ReadAhead,
    rusqlite::Connection,
    verify::{Manifest, Verification},
};

# let dir = tempfile::tempdir()?;
# let path = dir.path().join("example.db");
# Connection::open(&path)?.execute_batch(
#     "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT);
#      INSERT INTO notes (body) VALUES ('first'), ('second');",
# )?;
# let bytes = std::fs::read(&path)?;
// The publisher stores the database, and a manifest of its 64 KiB chunks.
let cas = Arc::new(MemoryCas::default());
let database: Cid = cas.put(bytes.clone().into())?;
let trusted: Cid = Manifest::blake3(64 << 10, &bytes)?.put(&*cas)?;

// The reader trusts only the manifest's CID.
let manifest = Manifest::load(&*cas, &trusted)?;
let options = Options::new()
    .verification(Verification::manifest(manifest))
    .fetch(ReadAhead::new(4 << 20))
    .cache(MemoryCache::new(64 << 20));
let database = Database::open(cas, &database, options)?;
let count: i64 =
    database.connect()?.query_row("SELECT count(*) FROM notes", [], |row| row.get(0))?;
assert_eq!(count, 2);
# Ok::<(), Box<dyn std::error::Error>>(())
```

Other proofs fit the same seam: implement `RangeVerifier` for a Merkle tree
or a signed index, and pass it in `Verification::VerifiedRanges`.

## Fetching and caching

With ranges, the database is read in blocks. Three small parts decide what
that costs, and each can be replaced or measured alone:

- `Options::block_size` sets the block size: 4 KiB unless set, or the
  verifier's chunk size.
- A `fetch::FetchPolicy` chooses the blocks of one request when a read misses
  the cache. `Database::set_fetch_policy` swaps the policy of an open
  database, such as before a scan.
  - `Exact` fetches only what is read.
  - `Aligned` fetches the fixed window that holds the read.
  - `ReadAhead`, the default, fetches 64 KiB where reads begin, and four
    times as much each time they go on from there, up to 4 MiB. It follows
    scans towards either end of the database, and several at once.
- A `cache::CachePolicy` builds the cache that keeps fetched blocks for one
  database: `NoCache`, `MemoryCache` (the default, 32 MiB), `FileCache`, or a
  `ClockCache` over storage of your own, such as a memory-mapped file.
  SQLite keeps the pages it has read in its own page cache, so this cache
  mostly holds what was fetched ahead and serves other connections. A cache
  smaller than the database keeps one request to a quarter of what it holds,
  so that blocks fetched ahead are still there when they are read.

A policy only chooses what to fetch or keep. It cannot change what SQLite
reads, and with verified ranges every fetched chunk is checked whatever the
policy asked for. `Database::stats` counts reads and fetches.

### Measure the choices

Fetching ahead spends bytes to save round trips, so the best sizes depend on
the store and on the queries. The `policies` benchmark runs queries against
each choice and counts the requests and the bytes, for a database that it
generates or for one of yours:

```text
cargo bench -p covalence-sqlite-cas --bench policies -- --help
cargo bench -p covalence-sqlite-cas --bench policies -- \
    --db my.db --sql "SELECT count(*) FROM notes" --link store:15:100
```

`--link` describes a store by its round trip in milliseconds and its
bandwidth in megabytes a second. The time that the benchmark gives for a link
is the time it measured with the store in memory, plus a round trip for each
request and the time of the bytes at that bandwidth. It is a model, not a
measurement of a network.

These are the requests, and the bytes they fetch, of the first connection to
the generated database of 109 MiB, read as trusted ranges in blocks of 4 KiB
with the default cache:

| Query                   | `Exact`        | `Aligned::new(1 << 20)` | `ReadAhead::default()` |
| ----------------------- | -------------- | ----------------------- | ---------------------- |
| 100 rows by primary key | 147, 0.6 MiB   | 84, 83 MiB              | 138, 11 MiB            |
| 1% of a table, in order | 241, 0.9 MiB   | 4, 3 MiB                | 6, 1.4 MiB             |
| A whole table           | 23,592, 92 MiB | 94, 93 MiB              | 28, 93 MiB             |

- Scattered lookups need about one request each whatever is fetched, and
  `ReadAhead` fetches its first window for each. `ReadAhead::first` sets that
  window: with 0 a lookup fetches what `Exact` does, and a scan takes a few
  more requests to reach its largest window.
- A scan makes fewer requests the larger the largest window is, and fetches
  up to one such window beyond where it stops. `ReadAhead::new` sets it.

## Errors

SQLite reports a failed read only as a disk I/O error. `Database::explain`
turns that error into its cause, such as `Error::Corrupt` for bytes a verifier
refused, `Error::Length` for a short answer, or `Error::Store`.

## Limits

- Read-only: every write to the database fails. Temporary tables still work;
  SQLite keeps them in local temporary files or memory.
- The schema comes from the database, so connections open with
  `trusted_schema` off and `cell_size_check` on, as SQLite advises for files
  from elsewhere.
- No other database can be attached. SQLite lets `ATTACH` and `VACUUM INTO`
  name another VFS in a URI, and so read a local file or write a new one.
  Connections therefore open with `SQLITE_LIMIT_ATTACHED` at 0, and both
  statements fail. Code that wants to attach raises the limit with
  `Connection::set_limit`; SQL cannot.
- This keeps the database and other files as they are. It does not bound the
  time, memory or temporary disk space that a statement uses.
- A store without ranges of its own answers each range by reading the whole
  value. Reading ranges from such a store costs more than one download.
- Two reads that miss the same block at the same time both fetch it.
- One request fetches at most 1 GiB (`MAX_BLOCK_SIZE`), which is also the
  largest block and chunk.
- A download can be checked only if this crate computes the CID's hash
  function, and a CID holds digests of at most 32 bytes: BLAKE3, SHA-256 and
  SHA-1 work.
- SQLite addresses at most `i64::MAX` bytes; a larger value fails to open.

## Snapshots and writes

A `Database` is one immutable snapshot, named by its CID. Writing is out of
scope here, and nothing in the API assumes it never comes: a writable layer
would keep changed pages apart from the snapshot, store the result as new
content, and name it with a new CID to open as any other.
