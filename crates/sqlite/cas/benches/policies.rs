//! Measures what block sizes, fetch policies, caches and verifications cost.
//!
//! A store that counts what it is asked serves a generated database, or one
//! of yours with `--db` and `--sql`. Each workload runs against each
//! configuration twice: cold, on the first connection to a newly opened
//! database, and warm, on a second connection that finds the first one's
//! blocks in the cache.
//!
//! - The requests and bytes are exact and repeat from run to run.
//! - The seconds are those of this machine with the store in memory: the
//!   cost of SQLite, the cache and the hashing, with no network.
//! - Time over a network is modelled, not measured: each request costs a
//!   round trip, and each byte its share of the bandwidth, on top of the
//!   measured seconds. `--link` sets the round trip and the bandwidth.
//!   `--sleep` has the store really wait that long, to check the model.
//!
//! ```text
//! cargo bench -p covalence-sqlite-cas --bench policies -- --csv policies.csv
//! cargo bench -p covalence-sqlite-cas --bench policies -- --help
//! ```
//!
//! Without `--bench`, which `cargo bench` passes, a small database is run
//! once as a smoke test.

use std::{
    collections::VecDeque,
    error::Error,
    fs,
    io::Write,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    thread,
    time::{Duration, Instant},
};

use covalence_cas_trait::{
    AsyncGet, AsyncPut, Cas, CasFuture, Error as StoreError, Get, Put, ready,
};
use covalence_lib_cid::{Cid, Format, Multicodec};
use covalence_lib_data::Bytes;
use covalence_sqlite_cas::{
    Database, Options,
    cache::{FileCache, MemoryCache, NoCache},
    fetch::{Aligned, Exact, FetchPolicy, Miss, ReadAhead},
    rusqlite::{self, Connection, Row, types::ValueRef},
    verify::{Manifest, Verification},
};

type Outcome<T> = Result<T, Box<dyn Error>>;

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;

const USAGE: &str = "\
Measures what block sizes, fetch policies, caches and verifications cost.

  --rows N          rows of the generated database (400000; 2000 as a smoke test)
  --repeats N       runs of each case; the least seconds are kept (3)
  --csv PATH        also write every measurement to PATH
  --db PATH         measure this database file instead of a generated one
  --sql SQL         with --db: a workload of this statement; may be repeated
  --workload TEXT   only workloads whose name contains TEXT; may be repeated
  --config TEXT     only configurations whose name contains TEXT; may be repeated
  --link NAME:MS:MB a link to model: its round trip in milliseconds and its
                    bandwidth in megabytes a second; may be repeated
                    (local:0.2:1000, region:15:100 and wan:80:10)
  --sleep           the store waits as the first link would, for every request
  --trace           print the offset and length of every request to the store
  --list            print the workloads and configurations, and stop
";

/// A link to a store: what a request costs, and what a byte costs.
#[derive(Clone, Debug)]
struct Link {
    name: String,
    /// Seconds from a request to the first byte of its answer.
    round_trip: f64,
    /// Bytes a second.
    bandwidth: f64,
}

impl Link {
    fn new(name: &str, milliseconds: f64, megabytes: f64) -> Self {
        Self {
            name: name.to_owned(),
            round_trip: milliseconds / 1e3,
            bandwidth: megabytes * 1e6,
        }
    }

    /// `NAME:MS:MB`.
    fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split(':');
        let name = parts.next()?;
        let milliseconds = parts.next()?.parse().ok()?;
        let megabytes: f64 = parts.next()?.parse().ok()?;
        (parts.next().is_none() && megabytes > 0.0)
            .then(|| Self::new(name, milliseconds, megabytes))
    }

    /// The modelled seconds for `requests` that carry `bytes` in all.
    fn seconds(&self, requests: u64, bytes: u64) -> f64 {
        requests as f64 * self.round_trip + bytes as f64 / self.bandwidth
    }
}

/// A store of one value that counts the requests and bytes it serves.
struct Remote {
    bytes: Bytes,
    requests: AtomicU64,
    served: AtomicU64,
    /// The link whose time every request really waits.
    wait: Option<Link>,
    /// Whether to print every request.
    trace: bool,
}

impl Remote {
    fn serve(&self, start: u64, bytes: u64) {
        if self.trace {
            println!("request {start} {bytes}");
        }
        self.requests.fetch_add(1, Relaxed);
        self.served.fetch_add(bytes, Relaxed);
        if let Some(link) = &self.wait {
            thread::sleep(Duration::from_secs_f64(link.seconds(1, bytes)));
        }
    }

    /// The requests and the bytes served so far.
    fn counts(&self) -> (u64, u64) {
        (self.requests.load(Relaxed), self.served.load(Relaxed))
    }
}

impl Get for Remote {
    fn get_unchecked(&self, _key: &Cid) -> Result<Option<Bytes>, StoreError> {
        self.serve(0, self.bytes.len() as u64);
        Ok(Some(self.bytes.clone()))
    }

    fn size_unchecked(&self, _key: &Cid) -> Result<Option<u64>, StoreError> {
        self.serve(0, 0);
        Ok(Some(self.bytes.len() as u64))
    }

    fn get_range_unchecked(
        &self,
        _key: &Cid,
        start: u64,
        len: u64,
    ) -> Result<Option<Bytes>, StoreError> {
        let size = self.bytes.len() as u64;
        let (start, end) = (start.min(size), start.saturating_add(len).min(size));
        self.serve(start, end - start);
        Ok(Some(self.bytes.slice(start as usize..end as usize)))
    }
}

impl Put for Remote {
    fn put(&self, _bytes: Bytes) -> Result<Cid, StoreError> {
        Err(StoreError::Backend {
            source: "a database is never written to its store".into(),
        })
    }
}

impl AsyncGet for Remote {
    fn aget_unchecked<'a>(&'a self, key: &'a Cid) -> CasFuture<'a, Option<Bytes>> {
        ready(self.get_unchecked(key))
    }
}

impl AsyncPut for Remote {
    fn aput(&self, bytes: Bytes) -> CasFuture<'_, Cid> {
        ready(self.put(bytes))
    }
}

/// Downloads use the trait's own `get_to`, which checks the bytes as stores do.
impl Cas for Remote {}

/// The library's read-ahead with more to set: variants are measured here first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Trial {
    /// The first window, in bytes.
    first: u64,
    /// What a window is multiplied by each time its scan continues.
    growth: u64,
    /// The largest window, in bytes.
    max: u64,
}

struct TrialPolicy {
    trial: Trial,
    /// The last fetch of each scan followed, least recently continued first.
    scans: Mutex<VecDeque<Range<u64>>>,
}

impl FetchPolicy for TrialPolicy {
    fn plan(&self, miss: &Miss) -> Range<u64> {
        let Trial { first, growth, max } = self.trial;
        let blocks_in = |bytes: u64| (bytes / miss.block_size).max(1);
        let Range { start, end } = miss.need;
        let most = blocks_in(max).min(miss.most);
        let len = |fetch: &Range<u64>| fetch.end - fetch.start;
        let grown = |last: &Range<u64>| len(last).saturating_mul(growth).min(most);
        let mut scans = self.scans.lock().unwrap();
        let near =
            |last: &Range<u64>| start <= last.end + len(last) && last.start <= end + len(last);
        let nearest = scans.iter().enumerate().filter(|(_, last)| near(last));
        let continued = nearest.max_by_key(|(_, last)| len(last)).map(|(at, _)| at);
        let fetch = match continued.and_then(|at| scans.remove(at)) {
            Some(last) if last.end <= start => last.end..last.end + grown(&last),
            Some(last) if last.start <= start => start..start + len(&last).min(most),
            Some(last) => last.start.saturating_sub(grown(&last))..last.start,
            None => start..start + blocks_in(first).min(most),
        };
        let fetch = fetch.start.min(start)..fetch.end.min(miss.blocks).max(end);
        if scans.len() >= ReadAhead::SCANS {
            scans.pop_front();
        }
        scans.push_back(fetch.clone());
        fetch
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fetch {
    Exact,
    Aligned(u64),
    ReadAhead { first: u64, max: u64, scans: usize },
    Trial(Trial),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cache {
    None,
    Memory(u64),
    File(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verify {
    /// Ranges that the store is trusted for.
    Trusted,
    /// Ranges checked against a manifest with this hash function, whose
    /// chunks are the configuration's blocks.
    Manifest(Multicodec),
    /// All of it, checked, before the first read.
    Download,
}

/// One way to open the database.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Config {
    block: u64,
    fetch: Fetch,
    cache: Cache,
    verify: Verify,
}

/// `bytes` as a short size: `4K`, `1M`.
fn size(bytes: u64) -> String {
    match bytes {
        _ if bytes >= MIB && bytes.is_multiple_of(MIB) => format!("{}M", bytes / MIB),
        _ if bytes >= KIB && bytes.is_multiple_of(KIB) => format!("{}K", bytes / KIB),
        _ => bytes.to_string(),
    }
}

impl Config {
    /// What the library does when no option is set, with ranges trusted.
    const DEFAULT: Self = Self {
        block: Options::BLOCK_SIZE,
        fetch: Fetch::ReadAhead {
            first: ReadAhead::FIRST,
            max: ReadAhead::MAX,
            scans: ReadAhead::SCANS,
        },
        cache: Cache::Memory(Options::CACHE_BYTES),
        verify: Verify::Trusted,
    };

    /// The block size, the fetch policy, the cache and the verification, as text.
    fn columns(&self) -> [String; 4] {
        let fetch = match self.fetch {
            Fetch::Exact => "exact".to_owned(),
            Fetch::Aligned(bytes) => format!("aligned:{}", size(bytes)),
            Fetch::ReadAhead { first, max, scans } if scans == ReadAhead::SCANS => {
                format!("read-ahead:{}-{}", size(first), size(max))
            }
            Fetch::ReadAhead { first, max, scans } => {
                format!("read-ahead:{}-{}:scans={scans}", size(first), size(max))
            }
            Fetch::Trial(trial) => format!(
                "trial:{}-{}:x{}",
                size(trial.first),
                size(trial.max),
                trial.growth
            ),
        };
        let cache = match self.cache {
            Cache::None => "none".to_owned(),
            Cache::Memory(bytes) => format!("memory:{}", size(bytes)),
            Cache::File(bytes) => format!("file:{}", size(bytes)),
        };
        let verify = match self.verify {
            Verify::Trusted => "trusted".to_owned(),
            Verify::Manifest(Multicodec::BLAKE3) => "blake3".to_owned(),
            Verify::Manifest(hash) => format!("{hash:?}").to_lowercase(),
            Verify::Download => "download".to_owned(),
        };
        [size(self.block), fetch, cache, verify]
    }

    fn name(&self) -> String {
        let [block, fetch, cache, verify] = self.columns();
        format!("block={block} fetch={fetch} cache={cache} verify={verify}")
    }

    /// The options of this configuration, and the bytes of its manifest if it has one.
    fn options(&self, content: &[u8], manifests: &mut Vec<Manifest>) -> Outcome<(Options, u64)> {
        let options = Options::new().block_size(self.block);
        let options = match self.fetch {
            Fetch::Exact => options.fetch(Exact),
            Fetch::Aligned(bytes) => options.fetch(Aligned::new(bytes)),
            Fetch::ReadAhead { first, max, scans } => {
                options.fetch(ReadAhead::new(max).first(first).scans(scans))
            }
            Fetch::Trial(trial) => options.fetch(TrialPolicy {
                trial,
                scans: Mutex::default(),
            }),
        };
        let options = match self.cache {
            Cache::None => options.cache(NoCache),
            Cache::Memory(bytes) => options.cache(MemoryCache::new(bytes)),
            Cache::File(bytes) => options.cache(FileCache::new(bytes)),
        };
        Ok(match self.verify {
            Verify::Trusted => (options.verification(Verification::TrustedRanges), 0),
            Verify::Download => (options.verification(Verification::Download), 0),
            Verify::Manifest(hash) => {
                // Whoever publishes a database makes its manifest once.
                let made = manifests
                    .iter()
                    .position(|made| made.hash() == hash && made.chunk_size() == self.block);
                let manifest = match made {
                    Some(at) => manifests[at].clone(),
                    None => {
                        manifests.push(Manifest::of(hash, self.block, content)?);
                        manifests[manifests.len() - 1].clone()
                    }
                };
                let bytes = manifest.fact()?.bytes().len() as u64;
                (
                    options.verification(Verification::manifest(manifest)),
                    bytes,
                )
            }
        })
    }
}

/// The configurations to compare: each differs from the default in one or two ways.
fn configs() -> Vec<Config> {
    let default = Config::DEFAULT;
    let read_ahead = |first, max| Fetch::ReadAhead {
        first,
        max,
        scans: ReadAhead::SCANS,
    };
    let trial = |first, growth, max| Fetch::Trial(Trial { first, growth, max });
    let (first, max) = (ReadAhead::FIRST, ReadAhead::MAX);
    let blocks = [4 * KIB, 16 * KIB, 64 * KIB, 256 * KIB, MIB];
    let mut all = vec![default];
    // Block sizes, each with every kind of fetch policy.
    for block in blocks {
        for fetch in [Fetch::Exact, Fetch::Aligned(MIB), default.fetch] {
            all.push(Config {
                block,
                fetch,
                ..default
            });
        }
    }
    // Fetch sizes, with blocks of a page.
    let fetches = [
        Fetch::Aligned(16 * KIB),
        Fetch::Aligned(64 * KIB),
        Fetch::Aligned(256 * KIB),
        Fetch::Aligned(4 * MIB),
        // The largest window.
        read_ahead(first, 256 * KIB),
        read_ahead(first, MIB),
        read_ahead(first, 16 * MIB),
        // The first window.
        read_ahead(0, max),
        read_ahead(16 * KIB, max),
        read_ahead(256 * KIB, max),
        // Small windows, as for a store that answers fast.
        read_ahead(0, MIB),
        read_ahead(16 * KIB, MIB),
        Fetch::ReadAhead {
            first,
            max,
            scans: 1,
        },
        // How fast windows grow. The second is the library's policy made
        // here, to show that these trials are that policy but for the growth.
        trial(first, 2, max),
        trial(first, 4, max),
        trial(first, 8, max),
    ];
    all.extend(fetches.map(|fetch| Config { fetch, ..default }));
    // Verifications: manifests of each chunk size, another hash function, and a download.
    for block in blocks {
        all.push(Config {
            block,
            verify: Verify::Manifest(Multicodec::BLAKE3),
            ..default
        });
    }
    all.push(Config {
        block: 64 * KIB,
        verify: Verify::Manifest(Multicodec::SHA2_256),
        ..default
    });
    all.push(Config {
        verify: Verify::Download,
        ..default
    });
    // Caches.
    let caches = [
        Cache::None,
        Cache::Memory(MIB),
        Cache::Memory(8 * MIB),
        Cache::Memory(256 * MIB),
        Cache::File(32 * MIB),
        Cache::File(256 * MIB),
    ];
    all.extend(caches.map(|cache| Config { cache, ..default }));
    let mut distinct = Vec::new();
    for config in all {
        if !distinct.contains(&config) {
            distinct.push(config);
        }
    }
    distinct
}

/// A step of a workload.
enum Step {
    /// A statement, read to its end.
    Query(String),
    /// Two statements read a row at a time in turn, as a join reads its tables.
    Together(String, String),
}

struct Workload {
    name: String,
    steps: Vec<Step>,
}

/// A number that depends on every bit of `x`, the same on every run.
fn mix(x: u64) -> u64 {
    let x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn name_of(id: u64) -> String {
    format!("name-{:016x}", mix(id))
}

/// Write a database of `rows` rows to `path`, the same bytes on every run.
///
/// `items` has the columns `id` (the primary key), `kind` (one of 16),
/// `name` (in an order unlike that of `id`), `score` and `payload` (200
/// bytes), with an index on `name` and one on `(kind, score)`. It is vacuumed,
/// so each table and index is in page order.
fn generate(path: &Path, rows: u64) -> rusqlite::Result<()> {
    let mut connection = Connection::open(path)?;
    connection.execute_batch(
        "PRAGMA page_size = 4096;
         PRAGMA synchronous = OFF;
         CREATE TABLE items (
             id INTEGER PRIMARY KEY,
             kind INTEGER NOT NULL,
             name TEXT NOT NULL,
             score INTEGER NOT NULL,
             payload BLOB NOT NULL
         );",
    )?;
    let transaction = connection.transaction()?;
    {
        let mut insert = transaction.prepare("INSERT INTO items VALUES (?1, ?2, ?3, ?4, ?5)")?;
        for id in 1..=rows {
            let noise = mix(mix(id));
            let payload: Vec<u8> = (0..25)
                .flat_map(|word| mix(noise ^ word).to_le_bytes())
                .collect();
            let (kind, score) = (noise % 16, (noise >> 8) % 1_000_000);
            insert.execute((id as i64, kind as i64, name_of(id), score as i64, payload))?;
        }
    }
    transaction.commit()?;
    connection.execute_batch(
        "CREATE INDEX items_name ON items (name);
         CREATE INDEX items_kind_score ON items (kind, score);
         VACUUM;",
    )
}

/// The workloads for a generated database of `rows` rows.
fn workloads(rows: u64) -> Vec<Workload> {
    let ids = || (0..100).map(|lookup| 1 + mix(1_000 + lookup) % rows);
    let (tenth, hundredth) = ((rows / 10).max(1), (rows / 100).max(1));
    let middle = rows / 2;
    let one = |sql: String| vec![Step::Query(sql)];
    let all = [
        ("open", one("SELECT count(*) FROM sqlite_schema".to_owned())),
        (
            "point-lookups",
            ids()
                .map(|id| Step::Query(format!("SELECT name, score FROM items WHERE id = {id}")))
                .collect(),
        ),
        (
            "index-lookups",
            ids()
                .map(|id| {
                    let name = name_of(id);
                    Step::Query(format!("SELECT id, score FROM items WHERE name = '{name}'"))
                })
                .collect(),
        ),
        (
            "top-k",
            one(
                "SELECT id, name FROM items WHERE kind = 3 ORDER BY score DESC LIMIT 100"
                    .to_owned(),
            ),
        ),
        (
            "range-scan",
            one(format!(
                "SELECT count(*), sum(score), sum(length(payload)) FROM items \
                 WHERE id BETWEEN {middle} AND {}",
                middle + hundredth
            )),
        ),
        (
            "reverse-scan",
            one(format!(
                "SELECT id, score FROM items WHERE id > {} ORDER BY id DESC",
                rows - tenth
            )),
        ),
        (
            "index-walk",
            // One name in 256, and for each a row somewhere in the table.
            one("SELECT count(*), sum(score) FROM items \
                 WHERE name BETWEEN 'name-40' AND 'name-41'"
                .to_owned()),
        ),
        (
            "group-by",
            one("SELECT kind, count(*), max(score) FROM items GROUP BY kind".to_owned()),
        ),
        ("count", one("SELECT count(*) FROM items".to_owned())),
        (
            "two-scans",
            vec![Step::Together(
                format!("SELECT id, score FROM items WHERE id <= {tenth}"),
                "SELECT name FROM items WHERE name < 'name-2'".to_owned(),
            )],
        ),
        (
            "table-scan",
            one("SELECT count(*), sum(score), sum(length(payload)) FROM items".to_owned()),
        ),
    ];
    let named = all.into_iter().map(|(name, steps)| Workload {
        name: name.to_owned(),
        steps,
    });
    named.collect()
}

/// Add a row's values to `hasher`.
fn absorb(hasher: &mut blake3::Hasher, row: &Row<'_>, columns: usize) -> rusqlite::Result<()> {
    for column in 0..columns {
        match row.get_ref(column)? {
            ValueRef::Null => hasher.update(b"n"),
            ValueRef::Integer(value) => hasher.update(b"i").update(&value.to_le_bytes()),
            ValueRef::Real(value) => hasher.update(b"r").update(&value.to_le_bytes()),
            ValueRef::Text(text) => hasher.update(b"t").update(text).update(&[0]),
            ValueRef::Blob(blob) => hasher.update(b"b").update(blob).update(&[0]),
        };
    }
    Ok(())
}

/// Run `steps` on `connection`, and hash every row they give.
fn answer(connection: &Connection, steps: &[Step]) -> rusqlite::Result<blake3::Hash> {
    let mut hasher = blake3::Hasher::new();
    for step in steps {
        match step {
            Step::Query(sql) => {
                let mut statement = connection.prepare(sql)?;
                let columns = statement.column_count();
                let mut rows = statement.query([])?;
                while let Some(row) = rows.next()? {
                    absorb(&mut hasher, row, columns)?;
                }
            }
            Step::Together(first, second) => {
                let mut statements = [connection.prepare(first)?, connection.prepare(second)?];
                let columns = statements
                    .each_ref()
                    .map(|statement| statement.column_count());
                let [first, second] = &mut statements;
                let mut rows = [Some(first.query([])?), Some(second.query([])?)];
                while rows.iter().any(Option::is_some) {
                    for (rows, columns) in rows.iter_mut().zip(columns) {
                        let Some(open) = rows else { continue };
                        match open.next()? {
                            Some(row) => absorb(&mut hasher, row, columns)?,
                            None => *rows = None,
                        }
                    }
                }
            }
        }
    }
    Ok(hasher.finalize())
}

/// What one part of a run cost.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Cost {
    requests: u64,
    bytes: u64,
    /// The reads SQLite made, and their bytes.
    reads: u64,
    read_bytes: u64,
    seconds: f64,
}

impl Cost {
    /// The same counts, with the fewer seconds.
    fn least(self, other: Self, what: &str) -> Self {
        let counts = |cost: Self| (cost.requests, cost.bytes, cost.reads, cost.read_bytes);
        assert_eq!(counts(self), counts(other), "{what}: counts repeat");
        Self {
            seconds: self.seconds.min(other.seconds),
            ..self
        }
    }
}

/// A workload against a configuration: opening the database, then the
/// workload on a first connection and again on a second.
struct Run {
    open: Cost,
    cold: Cost,
    warm: Cost,
    answer: blake3::Hash,
}

struct Subject<'a> {
    content: &'a Bytes,
    cid: Cid,
    wait: Option<&'a Link>,
    trace: bool,
}

fn run(
    subject: &Subject<'_>,
    workload: &Workload,
    config: &Config,
    manifests: &mut Vec<Manifest>,
) -> Outcome<Run> {
    let (options, manifest_bytes) = config.options(subject.content, manifests)?;
    let remote = Arc::new(Remote {
        bytes: subject.content.clone(),
        requests: AtomicU64::new(0),
        served: AtomicU64::new(0),
        wait: subject.wait.cloned(),
        trace: subject.trace,
    });
    if manifest_bytes > 0 {
        // A manifest is one more value to get before the database opens.
        remote.serve(0, manifest_bytes);
    }
    let start = Instant::now();
    let database = Database::open(Arc::clone(&remote) as Arc<dyn Cas>, &subject.cid, options)?;
    let (requests, bytes) = remote.counts();
    let open = Cost {
        requests,
        bytes,
        seconds: start.elapsed().as_secs_f64(),
        ..Cost::default()
    };
    let pass = || -> Outcome<(Cost, blake3::Hash)> {
        let (requests, bytes) = remote.counts();
        let before = database.stats();
        let start = Instant::now();
        let connection = database.connect()?;
        let answer =
            answer(&connection, &workload.steps).map_err(|error| database.explain(error))?;
        drop(connection);
        let seconds = start.elapsed().as_secs_f64();
        let (after, counts) = (database.stats(), remote.counts());
        let cost = Cost {
            requests: counts.0 - requests,
            bytes: counts.1 - bytes,
            reads: after.reads - before.reads,
            read_bytes: after.read_bytes - before.read_bytes,
            seconds,
        };
        Ok((cost, answer))
    };
    let (cold, answer) = pass()?;
    let (warm, again) = pass()?;
    assert_eq!(answer, again, "{}: both connections agree", workload.name);
    Ok(Run {
        open,
        cold,
        warm,
        answer,
    })
}

#[derive(Default)]
struct Args {
    full: bool,
    rows: Option<u64>,
    repeats: Option<u32>,
    csv: Option<PathBuf>,
    db: Option<PathBuf>,
    sql: Vec<String>,
    workloads: Vec<String>,
    configs: Vec<String>,
    links: Vec<Link>,
    sleep: bool,
    trace: bool,
    list: bool,
}

impl Args {
    /// The arguments, or `None` if they ask for help.
    fn parse() -> Outcome<Option<Self>> {
        let mut args = Self::default();
        let mut words = std::env::args().skip(1);
        while let Some(word) = words.next() {
            let mut value = || words.next().ok_or(format!("{word} needs a value"));
            match word.as_str() {
                "--bench" => args.full = true,
                "--rows" => args.rows = Some(value()?.parse()?),
                "--repeats" => args.repeats = Some(value()?.parse()?),
                "--csv" => args.csv = Some(value()?.into()),
                "--db" => args.db = Some(value()?.into()),
                "--sql" => args.sql.push(value()?),
                "--workload" => args.workloads.push(value()?),
                "--config" => args.configs.push(value()?),
                "--link" => {
                    let text = value()?;
                    let link = Link::parse(&text).ok_or(format!("{text} is not NAME:MS:MB"))?;
                    args.links.push(link);
                }
                "--sleep" => args.sleep = true,
                "--trace" => args.trace = true,
                "--list" => args.list = true,
                "--help" | "-h" => return Ok(None),
                other => return Err(format!("unknown argument {other}\n\n{USAGE}").into()),
            }
        }
        if args.links.is_empty() {
            args.links = vec![
                Link::new("local", 0.2, 1000.0),
                Link::new("region", 15.0, 100.0),
                Link::new("wan", 80.0, 10.0),
            ];
        }
        Ok(Some(args))
    }
}

/// Whether `name` passes `filters`: any of them, or there are none.
fn chosen(filters: &[String], name: &str) -> bool {
    filters.is_empty() || filters.iter().any(|filter| name.contains(filter.as_str()))
}

fn main() -> Outcome<()> {
    let Some(args) = Args::parse()? else {
        print!("{USAGE}");
        return Ok(());
    };
    let rows = args.rows.unwrap_or(if args.full { 400_000 } else { 2_000 });
    let repeats = args.repeats.unwrap_or(if args.full { 3 } else { 1 }).max(1);
    let (content, workloads) = match &args.db {
        Some(path) => {
            let named = args.sql.iter().enumerate().map(|(at, sql)| Workload {
                name: format!("sql-{}", at + 1),
                steps: vec![Step::Query(sql.clone())],
            });
            (fs::read(path)?, named.collect::<Vec<_>>())
        }
        None => {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("generated.db");
            generate(&path, rows)?;
            (fs::read(path)?, workloads(rows))
        }
    };
    let workloads: Vec<_> = workloads
        .into_iter()
        .filter(|workload| chosen(&args.workloads, &workload.name))
        .collect();
    let configs: Vec<_> = configs()
        .into_iter()
        .filter(|config| chosen(&args.configs, &config.name()))
        .collect();
    if args.list {
        for workload in &workloads {
            println!("workload {}", workload.name);
        }
        for config in &configs {
            println!("config   {}", config.name());
        }
        return Ok(());
    }

    let content = Bytes::from(content);
    let format = Format {
        codec: Multicodec::RAW,
        hash: Multicodec::BLAKE3,
    };
    let subject = Subject {
        cid: Cid::new(format, blake3::hash(&content).as_bytes())?,
        content: &content,
        wait: args.links.first().filter(|_| args.sleep),
        trace: args.trace,
    };
    println!(
        "database: {} bytes; {} workloads, {} configurations, {repeats} repeats",
        content.len(),
        workloads.len(),
        configs.len()
    );
    let mut csv = args.csv.as_deref().map(fs::File::create).transpose()?;
    if let Some(csv) = &mut csv {
        writeln!(
            csv,
            "workload,block,fetch,cache,verify,pass,requests,bytes,reads,read_bytes,seconds,slept"
        )?;
    }
    let links: Vec<String> = (args.links.iter())
        .map(|link| format!("{:>10}", format!("{} ms", link.name)))
        .collect();
    println!(
        "{:<14} {:<70} {:>8} {:>9} {:>8} {} {:>8}",
        "workload",
        "configuration",
        "requests",
        "MiB",
        "cpu ms",
        links.join(" "),
        "warm req"
    );

    let mut manifests = Vec::new();
    for workload in &workloads {
        let mut expected = None;
        for config in &configs {
            let what = format!("{} with {}", workload.name, config.name());
            let mut kept = run(&subject, workload, config, &mut manifests)?;
            for _ in 1..repeats {
                let next = run(&subject, workload, config, &mut manifests)?;
                kept.open = kept.open.least(next.open, &what);
                kept.cold = kept.cold.least(next.cold, &what);
                kept.warm = kept.warm.least(next.warm, &what);
            }
            // No policy changes an answer.
            assert_eq!(*expected.get_or_insert(kept.answer), kept.answer, "{what}");

            let Run {
                open, cold, warm, ..
            } = kept;
            let (requests, bytes) = (open.requests + cold.requests, open.bytes + cold.bytes);
            let seconds = open.seconds + cold.seconds;
            let modelled: Vec<String> = (args.links.iter())
                .map(|link| {
                    // A store that slept has spent the link's time already.
                    let network = if args.sleep {
                        0.0
                    } else {
                        link.seconds(requests, bytes)
                    };
                    format!("{:>10.1}", (network + seconds) * 1e3)
                })
                .collect();
            println!(
                "{:<14} {:<70} {:>8} {:>9.2} {:>8.1} {} {:>8}",
                workload.name,
                config.name(),
                requests,
                bytes as f64 / MIB as f64,
                seconds * 1e3,
                modelled.join(" "),
                warm.requests
            );
            if let Some(csv) = &mut csv {
                let [block, fetch, cache, verify] = config.columns();
                let slept = subject.wait.map_or("", |link| link.name.as_str());
                for (pass, cost) in [("open", open), ("cold", cold), ("warm", warm)] {
                    writeln!(
                        csv,
                        "{},{block},{fetch},{cache},{verify},{pass},{},{},{},{},{:.6},{slept}",
                        workload.name,
                        cost.requests,
                        cost.bytes,
                        cost.reads,
                        cost.read_bytes,
                        cost.seconds
                    )?;
                }
            }
        }
    }
    Ok(())
}
