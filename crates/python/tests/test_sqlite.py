"""Read-only SQLite databases opened from a store, through the installed wheel."""

import base64
import collections
import os
import sqlite3
import tempfile
import threading
import types
import unittest
from http.server import ThreadingHTTPServer
from unittest import mock

from covalence import (
    Cas,
    ChunkManifest,
    Cid,
    Error,
    Hash,
    IntegrityError,
    Kv,
    SqliteConnection,
    SqliteCursor,
    SqliteDatabase,
    SqliteError,
    StoreError,
    UnsupportedError,
)
from test_api import FakeS3

ROWS = 3000
QUERIES = [
    "SELECT count(*), sum(id), sum(length(payload)) FROM items",
    "SELECT id, name, payload FROM items WHERE id IN (1, 2, 777, 2999)",
    "SELECT id FROM items WHERE name = 'name-1234'",
    "SELECT id, name FROM items ORDER BY name DESC LIMIT 5",
    "SELECT name, sql FROM sqlite_schema ORDER BY name",
    "PRAGMA integrity_check",
]


def sample(rows=ROWS):
    """The bytes of a database of `rows` rows with 200-byte payloads, and an index."""
    source = sqlite3.connect(":memory:")
    source.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL, payload BLOB NOT NULL)")
    source.executemany(
        "INSERT INTO items VALUES (?, ?, ?)",
        ((id, f"name-{id * 7919 % rows}", bytes([id % 251]) * 200) for id in range(1, rows + 1)),
    )
    source.execute("CREATE INDEX items_name ON items (name)")
    source.commit()
    data = source.serialize()
    source.close()
    return data


DATA = sample()


def expected(sql, parameters=()):
    """What Python's own SQLite answers for the sample."""
    source = sqlite3.connect(":memory:")
    source.deserialize(DATA)
    try:
        return source.execute(sql, parameters).fetchall()
    finally:
        source.close()


def kv_key(cid):
    """The key `Cas.kv` keeps the bytes of `cid` under."""
    return f"blake3/{base64.b32encode(cid.digest).decode().lower().rstrip('=')}/bytes".encode()


def flipped(data, at):
    """`data` with one bit of the byte at `at` flipped."""
    changed = bytearray(data)
    changed[at] ^= 1
    return bytes(changed)


class Stored(unittest.TestCase):
    """A store with the sample in it, and the CID of the sample."""

    def setUp(self):
        self.kv = Kv.memory()
        self.cas = Cas.kv(self.kv)
        self.cid = self.cas.put(DATA)

    def corrupt(self, at):
        """Flip a bit of the stored sample, keeping its length."""
        self.kv.put(kv_key(self.cid), flipped(DATA, at))


class OpenTests(Stored):
    def test_a_downloaded_database_answers_as_the_original(self):
        database = SqliteDatabase.open(self.cas, self.cid)
        self.assertEqual((database.cid, database.size), (self.cid, len(DATA)))
        self.assertEqual(repr(database), f"SqliteDatabase(cid=Cid('{self.cid}'), size={len(DATA)})")
        with database.connect() as connection:
            self.assertIsInstance(connection, SqliteConnection)
            for sql in QUERIES:
                with self.subTest(sql=sql):
                    self.assertEqual(connection.execute(sql).fetchall(), expected(sql))
        stats = database.stats()
        self.assertEqual(sorted(stats), ["fetched_bytes", "fetches", "read_bytes", "reads"])
        self.assertEqual((stats["fetches"], stats["fetched_bytes"]), (1, len(DATA)))
        self.assertGreater(stats["reads"], 1)

    def test_ranges_answer_as_the_original_in_every_configuration(self):
        manifest = ChunkManifest.of(DATA, chunk_size=8192)
        with tempfile.TemporaryDirectory() as dir:
            for verify in ("trusted-ranges", manifest):
                for fetch, fetch_bytes in (("read-ahead", None), ("read-ahead", 65536), ("aligned", 32768), ("exact", None)):
                    for cache in ("memory", "file", "none"):
                        with self.subTest(verify=verify, fetch=fetch, fetch_bytes=fetch_bytes, cache=cache):
                            database = SqliteDatabase.open(
                                self.cas, self.cid, verify=verify, fetch=fetch, fetch_bytes=fetch_bytes,
                                cache=cache, cache_bytes=1 << 20, dir=dir,
                            )
                            with database.connect() as connection:
                                for sql in QUERIES:
                                    self.assertEqual(connection.execute(sql).fetchall(), expected(sql))
            # The local files are private and temporary: nothing is left behind, or to clean up.
            self.assertEqual(os.listdir(dir), [])

    def test_ranges_fetch_only_what_queries_read(self):
        database = SqliteDatabase.open(self.cas, self.cid, verify="trusted-ranges", fetch="exact", cache="none")
        self.assertEqual(database.stats(), {"reads": 0, "read_bytes": 0, "fetches": 0, "fetched_bytes": 0})
        with database.connect() as connection:
            self.assertEqual(connection.execute("SELECT name FROM items WHERE id = 1500").fetchall(), [("name-1500",)])
        stats = database.stats()
        # Whole blocks, one request for each read, and a small part of the database.
        self.assertEqual(stats["fetched_bytes"], stats["fetches"] * 4096)
        self.assertEqual(stats["fetches"], stats["reads"])
        self.assertLessEqual(stats["read_bytes"], stats["fetched_bytes"])
        self.assertLess(stats["fetched_bytes"], len(DATA) // 10)

    def test_a_cache_that_keeps_nothing_is_fetched_only_what_is_read(self):
        fetched = set()
        for fetch, fetch_bytes in (("exact", None), ("aligned", 1 << 20), ("read-ahead", None)):
            database = SqliteDatabase.open(self.cas, self.cid, verify="trusted-ranges", fetch=fetch, fetch_bytes=fetch_bytes, cache="none")
            with database.connect() as connection:
                connection.execute(QUERIES[0])
            stats = database.stats()
            fetched.add((stats["fetches"], stats["fetched_bytes"]))
        self.assertEqual(len(fetched), 1)

    def test_the_fetch_policy_of_an_open_database_can_change(self):
        database = SqliteDatabase.open(self.cas, self.cid, verify="trusted-ranges", fetch="exact")
        with database.connect() as connection:
            # The index, a block at a time.
            connection.execute("SELECT count(*) FROM items WHERE name > ''")
            exact = database.stats()
            self.assertGreater(exact["fetches"], 10)
            self.assertEqual(exact["fetched_bytes"], exact["fetches"] * 4096)
            # The table, in one request for all of the database.
            database.set_fetch("aligned", 2 * len(DATA))
            connection.execute("SELECT sum(length(payload)) FROM items")
            aligned = database.stats()
            self.assertEqual(aligned["fetches"], exact["fetches"] + 1)
            self.assertEqual(aligned["fetched_bytes"], exact["fetched_bytes"] + len(DATA))
        database.set_fetch("read-ahead")
        database.set_fetch("exact", 4096)
        for fetch, fetch_bytes in (("aligned", None), ("everything", None), ("", 1)):
            with self.subTest(fetch=fetch), self.assertRaises(ValueError):
                database.set_fetch(fetch, fetch_bytes)

    def test_a_missing_database_is_none(self):
        other = Cid.of(b"not in the store")
        for verify in (None, "download", "trusted-ranges", ChunkManifest.of(DATA)):
            with self.subTest(verify=verify):
                self.assertIsNone(SqliteDatabase.open(self.cas, other, verify=verify))

    def test_options_that_cannot_be_used_raise_value_error(self):
        for options in (
            {"verify": "ranges"},
            {"verify": "trusted-ranges", "fetch": "aligned"},
            {"verify": "trusted-ranges", "fetch": "everything"},
            {"verify": "trusted-ranges", "cache": "disk"},
            {"verify": "trusted-ranges", "block_size": 0},
            {"verify": "trusted-ranges", "block_size": (1 << 30) + 1},
            {"max_bytes": len(DATA) - 1},
        ):
            with self.subTest(options=options), self.assertRaises(ValueError) as raised:
                SqliteDatabase.open(self.cas, self.cid, **options)
            self.assertNotIsInstance(raised.exception, Error)
        self.assertIsNotNone(SqliteDatabase.open(self.cas, self.cid, max_bytes=len(DATA)))
        for options in ({"verify": 1}, {"max_bytes": "1"}, {"dir": 1}):
            with self.subTest(options=options), self.assertRaises(TypeError):
                SqliteDatabase.open(self.cas, self.cid, **options)
        with self.assertRaises(OverflowError):
            SqliteDatabase.open(self.cas, self.cid, max_bytes=-1)

    def test_a_cid_no_store_can_hold_raises_value_error(self):
        with self.assertRaises(ValueError):
            SqliteDatabase.open(self.cas, Cid.of(DATA, "sha512"))

    def test_a_directory_that_is_not_there_raises_os_error(self):
        with tempfile.TemporaryDirectory() as dir:
            missing = os.path.join(dir, "missing")
            with self.assertRaises(OSError) as raised:
                SqliteDatabase.open(self.cas, self.cid, dir=missing)
            self.assertNotIsInstance(raised.exception, Error)

    def test_bytes_that_are_not_a_database_open_but_do_not_connect(self):
        cid = self.cas.put(b"not a database" * 1000)
        for verify in ("download", "trusted-ranges"):
            with self.subTest(verify=verify):
                database = SqliteDatabase.open(self.cas, cid, verify=verify)
                with self.assertRaises(SqliteError) as raised:
                    database.connect()
                self.assertIsInstance(raised.exception, Error)
                self.assertIn("not a database", str(raised.exception))

    def test_a_database_kept_in_a_sqlite_store_opens(self):
        cas = Cas.sqlite()
        cid = cas.put(DATA)
        for verify in ("download", "trusted-ranges", ChunkManifest.of(DATA)):
            with self.subTest(verify=verify):
                with SqliteDatabase.open(cas, cid, verify=verify).connect() as connection:
                    self.assertEqual(connection.execute(QUERIES[0]).fetchall(), expected(QUERIES[0]))


class TrustTests(Stored):
    def test_a_download_that_does_not_hash_to_the_cid_is_refused(self):
        self.corrupt(len(DATA) // 2)
        with self.assertRaises(IntegrityError) as raised:
            SqliteDatabase.open(self.cas, self.cid)
        self.assertIsInstance(raised.exception, ValueError)

    def test_blake3_ranges_are_unsupported_and_nothing_else_is_done(self):
        # Not even asking the store: a CID it lacks is refused, not None.
        for cid in (self.cid, Cid.of(b"not in the store")):
            with self.subTest(cid=cid), self.assertRaises(UnsupportedError) as raised:
                SqliteDatabase.open(self.cas, cid, verify="blake3-ranges")
            self.assertIsInstance(raised.exception, NotImplementedError)

    def test_a_download_needs_a_hash_function_that_can_be_computed(self):
        # The bytes could not be checked, so the store is not even asked: a
        # CID it lacks is refused, not None.
        cid = Cid.from_parts("raw", Hash.from_code(0xB220), bytes(32))
        with self.assertRaises(UnsupportedError) as raised:
            SqliteDatabase.open(self.cas, cid)
        self.assertIsInstance(raised.exception, NotImplementedError)
        # Ranges that are trusted hash nothing, and the store has no such CID.
        self.assertIsNone(SqliteDatabase.open(self.cas, cid, verify="trusted-ranges"))

    def test_trusted_ranges_are_not_checked(self):
        at = DATA.rindex(bytes([18]) * 200) + 100
        self.corrupt(at)
        database = SqliteDatabase.open(self.cas, self.cid, verify="trusted-ranges")
        with database.connect() as connection:
            payloads = connection.execute("SELECT DISTINCT payload FROM items WHERE id % 251 = 18").fetchall()
        self.assertEqual(len(payloads), 2)

    def test_ranges_checked_with_a_manifest_refuse_other_bytes(self):
        manifest = ChunkManifest.of(DATA, chunk_size=4096)
        at = DATA.rindex(bytes([18]) * 200) + 100
        self.corrupt(at)
        database = SqliteDatabase.open(self.cas, self.cid, verify=manifest, fetch="exact")
        scan = "SELECT count(DISTINCT payload) FROM items"
        with database.connect() as connection:
            # Chunks that are the database's still answer.
            self.assertEqual(connection.execute("SELECT name FROM items WHERE id = 1").fetchall(), [("name-1919",)])
            with self.assertRaises(IntegrityError) as raised:
                connection.execute(scan)
            self.assertIn(f"chunk {at // 4096} ", str(raised.exception))
            # The refused chunk was not kept: a store that recovers is read again.
            self.kv.put(kv_key(self.cid), DATA)
            self.assertEqual(connection.execute(scan).fetchall(), [(251,)])

    def test_a_manifest_of_other_content_opens_nothing(self):
        with self.assertRaises(IntegrityError):
            SqliteDatabase.open(self.cas, self.cid, verify=ChunkManifest.of(DATA + b"more"))
        database = SqliteDatabase.open(self.cas, self.cid, verify=ChunkManifest.of(flipped(DATA, 50)))
        with self.assertRaises(IntegrityError):
            database.connect()

    def test_a_store_that_gives_too_few_bytes_fails_the_read(self):
        for verify in ("trusted-ranges", ChunkManifest.of(DATA, chunk_size=4096)):
            with self.subTest(verify=verify):
                self.kv.put(kv_key(self.cid), DATA)
                database = SqliteDatabase.open(self.cas, self.cid, verify=verify, cache="none")
                with database.connect() as connection:
                    self.kv.put(kv_key(self.cid), DATA[: len(DATA) // 2])
                    with self.assertRaises(IntegrityError) as raised:
                        connection.execute(QUERIES[0])
                    self.assertIn("the store gave", str(raised.exception))
                    self.kv.put(kv_key(self.cid), DATA)
                    self.assertEqual(connection.execute(QUERIES[0]).fetchall(), expected(QUERIES[0]))


class S3Tests(unittest.TestCase):
    """The sample in a store over S3, as a local server speaks it."""

    def setUp(self):
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), FakeS3)
        self.server.objects, self.server.requests, self.server.denied = {}, [], set()
        self.server.lock, self.server.live, self.server.peak, self.server.delay = threading.Lock(), 0, 0, 0
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        home = tempfile.TemporaryDirectory()
        self.addCleanup(home.cleanup)
        endpoint = f"http://127.0.0.1:{self.server.server_address[1]}"
        # Only this environment: no real credentials, profiles or instance metadata.
        env = {
            "HOME": home.name, "AWS_EC2_METADATA_DISABLED": "true", "AWS_ENDPOINT_URL": endpoint,
            "AWS_ACCESS_KEY_ID": "key", "AWS_SECRET_ACCESS_KEY": "secret",
        }
        patch = mock.patch.dict(os.environ, env, clear=True)
        patch.start()
        self.addCleanup(patch.stop)
        self.cas = Cas.kv(Kv.s3("s3://bucket/p", region="us-east-1"))
        self.cid = self.cas.put(DATA)
        self.server.requests.clear()

    def gets(self):
        """The number of GET requests since the last call."""
        gets = sum(1 for method, _, _ in self.server.requests if method == "GET")
        self.server.requests.clear()
        return gets

    def test_a_query_fetches_a_few_ranges_of_the_object(self):
        database = SqliteDatabase.open(self.cas, self.cid, verify="trusted-ranges")
        self.assertEqual(self.gets(), 0)
        with database.connect() as connection:
            self.assertEqual(connection.execute("SELECT name FROM items WHERE id = 1500").fetchall(), [("name-1500",)])
            stats = database.stats()
            self.assertEqual(self.gets(), stats["fetches"])
            self.assertLess(stats["fetches"], 10)
            # Read-ahead starts with 64 KiB wherever a read begins.
            self.assertLess(stats["fetched_bytes"], len(DATA) // 4)
            # A scan reads ahead: far fewer requests than blocks.
            self.assertEqual(connection.execute(QUERIES[0]).fetchall(), expected(QUERIES[0]))
            self.assertLess(self.gets(), len(DATA) // 4096 // 4)

    def test_a_download_is_one_request(self):
        database = SqliteDatabase.open(self.cas, self.cid)
        self.assertEqual(self.gets(), 1)
        with database.connect() as connection:
            for sql in QUERIES:
                self.assertEqual(connection.execute(sql).fetchall(), expected(sql))
        self.assertEqual(self.server.requests, [])

    def test_a_store_that_fails_or_loses_the_database_fails_the_read(self):
        manifest = ChunkManifest.of(DATA)
        for verify in ("trusted-ranges", manifest):
            with self.subTest(verify=verify):
                database = SqliteDatabase.open(self.cas, self.cid, verify=verify, cache="none")
                with database.connect() as connection:
                    self.server.denied = {"GET"}
                    with self.assertRaises(StoreError) as raised:
                        connection.execute(QUERIES[0])
                    self.assertIsInstance(raised.exception, OSError)
                    self.server.denied = set()
                    self.assertEqual(connection.execute(QUERIES[0]).fetchall(), expected(QUERIES[0]))
                # SQLite's own cache answers the first connection: ask with another.
                objects, self.server.objects = self.server.objects, {}
                with self.assertRaises(StoreError) as raised:
                    database.connect()
                self.assertIn("has no bytes", str(raised.exception))
                self.server.objects = objects
        self.server.denied = {"GET", "HEAD"}
        for verify in ("download", "trusted-ranges", manifest):
            with self.subTest(verify=verify), self.assertRaises(StoreError):
                SqliteDatabase.open(self.cas, self.cid, verify=verify)


class ManifestTests(Stored):
    def test_a_manifest_describes_its_content(self):
        manifest = ChunkManifest.of(DATA)
        self.assertEqual((manifest.size, manifest.chunk_size), (len(DATA), 65536))
        self.assertEqual(manifest.chunks, -(-len(DATA) // 65536))
        self.assertEqual(manifest.hash, Hash("blake3"))
        self.assertEqual(repr(manifest), f"ChunkManifest(size={len(DATA)}, chunk_size=65536, hash='blake3')")
        empty = ChunkManifest.of(b"", chunk_size=1)
        self.assertEqual((empty.size, empty.chunks), (0, 0))
        self.assertEqual(ChunkManifest.of(bytearray(b"abc"), chunk_size=2).chunks, 2)

    def test_a_manifest_of_a_file_is_that_of_its_bytes(self):
        with tempfile.TemporaryDirectory() as dir:
            path = os.path.join(dir, "sample.db")
            with open(path, "wb") as file:
                file.write(DATA)
            for hash in ("blake3", "sha256", "sha512", "sha1", Hash("sha2-256")):
                with self.subTest(hash=hash):
                    of_file = ChunkManifest.of_file(path, chunk_size=10_000, hash=hash)
                    of_bytes = ChunkManifest.of(DATA, chunk_size=10_000, hash=hash)
                    self.assertEqual(of_file.put(self.cas), of_bytes.put(self.cas))
                    self.assertEqual(of_file.hash, Hash(hash) if isinstance(hash, str) else hash)
                    database = SqliteDatabase.open(self.cas, self.cid, verify=of_file)
                    with database.connect() as connection:
                        self.assertEqual(connection.execute(QUERIES[0]).fetchall(), expected(QUERIES[0]))
            with self.assertRaises(OSError):
                ChunkManifest.of_file(os.path.join(dir, "missing"))

    def test_a_manifest_kept_in_the_store_vouches_for_the_database(self):
        trusted = ChunkManifest.of(DATA, chunk_size=4096).put(self.cas)
        manifest = ChunkManifest.load(self.cas, trusted)
        self.assertEqual((manifest.size, manifest.chunk_size, manifest.chunks), (len(DATA), 4096, -(-len(DATA) // 4096)))
        database = SqliteDatabase.open(self.cas, self.cid, verify=manifest)
        with database.connect() as connection:
            self.assertEqual(connection.execute(QUERIES[0]).fetchall(), expected(QUERIES[0]))

    def test_a_manifest_that_cannot_be_loaded(self):
        self.assertIsNone(ChunkManifest.load(self.cas, Cid.of(b"not in the store")))
        with self.assertRaises(ValueError) as raised:
            ChunkManifest.load(self.cas, self.cas.put(b"not a manifest"))
        self.assertNotIsInstance(raised.exception, IntegrityError)
        trusted = ChunkManifest.of(DATA).put(self.cas)
        self.kv.put(kv_key(trusted), b"other bytes")
        with self.assertRaises(IntegrityError):
            ChunkManifest.load(self.cas, trusted)

    def test_chunk_sizes_and_hashes_that_cannot_be_used(self):
        for chunk_size in (0, (1 << 30) + 1):
            with self.subTest(chunk_size=chunk_size), self.assertRaises(ValueError) as raised:
                ChunkManifest.of(DATA, chunk_size=chunk_size)
            self.assertNotIsInstance(raised.exception, Error)
        with self.assertRaises(UnsupportedError):
            ChunkManifest.of(DATA, hash=Hash.from_code(0xB220))
        with self.assertRaises(ValueError):
            ChunkManifest.of(DATA, hash="md5")
        with self.assertRaises(TypeError):
            ChunkManifest.of("text")


class StatementTests(Stored):
    def setUp(self):
        super().setUp()
        self.database = SqliteDatabase.open(self.cas, self.cid)
        self.connection = self.database.connect()
        self.addCleanup(self.connection.close)

    def test_values_keep_their_types_both_ways(self):
        values = (None, True, False, 0, -(2**63), 2**63 - 1, 1.5, float("inf"), "", "text é中\U0001f600", b"", b"\x00\xff", bytearray(b"ab"))
        sql = "SELECT " + ", ".join("?" * len(values))
        (row,) = self.connection.execute(sql, values).fetchall()
        self.assertEqual(row, (None, 1, 0, 0, -(2**63), 2**63 - 1, 1.5, float("inf"), "", "text é中\U0001f600", b"", b"\x00\xff", b"ab"))
        self.assertEqual([type(value) for value in row], [type(None), int, int, int, int, int, float, float, str, str, bytes, bytes, bytes])
        self.assertEqual(row, expected(sql, values)[0])
        self.assertEqual(self.connection.execute("SELECT ?", [float("nan")]).fetchall(), [(None,)])
        self.assertEqual(self.connection.execute("SELECT 'a' || char(0) || 'b'").fetchall(), [("a\x00b",)])

    def test_parameters_fill_placeholders_by_position_or_name(self):
        execute = self.connection.execute
        self.assertEqual(execute("SELECT ?, ?", (1, 2)).fetchall(), [(1, 2)])
        self.assertEqual(execute("SELECT ?2, ?1", [1, 2]).fetchall(), [(2, 1)])
        self.assertEqual(execute("SELECT :a, @b, $c, :a", {"a": 1, "b": 2, "c": 3, "unused": 4}).fetchall(), [(1, 2, 3, 1)])
        self.assertEqual(execute("SELECT :a, :b", (1, 2)).fetchall(), [(1, 2)])
        self.assertEqual(execute("SELECT 1").fetchall(), [(1,)])
        self.assertEqual(execute("SELECT 1", None).fetchall(), [(1,)])
        self.assertEqual(execute("SELECT 1", {}).fetchall(), [(1,)])
        # Any sequence or mapping does, not only the built-in ones.
        self.assertEqual(execute("SELECT ?, ?", range(2)).fetchall(), [(0, 1)])
        self.assertEqual(execute("SELECT ?, ?", collections.UserList(["a", b"b"])).fetchall(), [("a", b"b")])
        self.assertEqual(execute("SELECT :a", types.MappingProxyType({"a": 1})).fetchall(), [(1,)])
        self.assertEqual(execute("SELECT :a, :b", collections.ChainMap({"a": 1}, {"b": 2})).fetchall(), [(1, 2)])
        for sql, parameters in (
            ("SELECT ?", ()),
            ("SELECT ?", (1, 2)),
            ("SELECT 1", (1,)),
            ("SELECT :a", {"b": 1}),
            ("SELECT :a", {":a": 1}),
            ("SELECT ?", {"a": 1}),
        ):
            with self.subTest(sql=sql, parameters=parameters), self.assertRaises(ValueError) as raised:
                execute(sql, parameters)
            self.assertNotIsInstance(raised.exception, Error)
        # A str or bytes is one value that was not wrapped, not a sequence of values.
        unordered = ({1}, iter([1]), (value for value in [1]))
        for parameters in ((object(),), ([1],), {"a": 1j}, {1: 1}, "a", b"a", bytearray(b"a"), 1, *unordered):
            with self.subTest(parameters=parameters), self.assertRaises(TypeError):
                execute("SELECT :a", parameters)
        for value in (2**63, -(2**63) - 1):
            with self.subTest(value=value), self.assertRaises(OverflowError):
                execute("SELECT ?", (value,))

    def test_one_statement_runs_at_a_time(self):
        execute = self.connection.execute
        self.assertEqual(execute("SELECT 1; -- and no more\n ").fetchall(), [(1,)])
        for sql in ("", "  ", "-- nothing", "SELECT 1; SELECT 2", "SELECT 1; DELETE FROM items"):
            with self.subTest(sql=sql), self.assertRaises(ValueError) as raised:
                execute(sql)
            self.assertNotIsInstance(raised.exception, Error)
        self.assertEqual(execute("SELECT count(*) FROM items").fetchall(), [(ROWS,)])

    def test_sql_that_sqlite_refuses_raises_sqlite_error(self):
        for sql in ("SELEC 1", "SELECT * FROM missing", "SELECT missing FROM items", "SELECT abs(1, 2)"):
            with self.subTest(sql=sql), self.assertRaises(SqliteError) as raised:
                self.connection.execute(sql)
            self.assertIsInstance(raised.exception, Error)
            self.assertNotIsInstance(raised.exception, (ValueError, OSError))

    def test_the_database_cannot_be_written(self):
        for sql in (
            "DELETE FROM items",
            "INSERT INTO items VALUES (0, 'zero', x'00')",
            "UPDATE items SET name = 'other' WHERE id = 1",
            "CREATE TABLE other (x)",
            "DROP TABLE items",
            "CREATE INDEX other ON items (payload)",
            "PRAGMA user_version = 7",
            "VACUUM",
        ):
            with self.subTest(sql=sql), self.assertRaises(SqliteError):
                self.connection.execute(sql)
        self.assertEqual(self.connection.execute("PRAGMA journal_mode = wal").fetchall(), [("delete",)])
        self.assertEqual(self.connection.execute("PRAGMA user_version").fetchall(), [(0,)])
        for sql in QUERIES:
            with self.subTest(sql=sql):
                self.assertEqual(self.connection.execute(sql).fetchall(), expected(sql))
        with self.database.connect() as other:
            self.assertEqual(other.execute(QUERIES[0]).fetchall(), expected(QUERIES[0]))

    def test_temporary_tables_can_be_written(self):
        execute = self.connection.execute
        self.assertIsNone(execute("CREATE TEMP TABLE picked AS SELECT id FROM items WHERE id < 4").description)
        execute("INSERT INTO picked VALUES (?)", (10,))
        self.assertEqual(execute("SELECT sum(id) FROM picked").fetchall(), [(16,)])
        with self.database.connect() as other, self.assertRaises(SqliteError):
            other.execute("SELECT * FROM picked")

    def test_a_connection_opens_no_other_database(self):
        with tempfile.TemporaryDirectory() as dir:
            path = os.path.join(dir, "other.db")
            other = sqlite3.connect(path)
            other.execute("CREATE TABLE t AS SELECT 7 AS x")
            other.commit()
            other.close()
            copy = os.path.join(dir, "copy.db")
            for sql, target in (
                ("ATTACH ? AS other", path),
                ("ATTACH ? AS other", f"file:{path}?vfs=unix"),
                ("ATTACH ? AS other", f"file:{path}?mode=ro"),
                ("ATTACH ? AS other", os.path.join(dir, "new.db")),
                ("ATTACH ? AS other", ":memory:"),
                ("ATTACH ? AS other", ""),
                ("VACUUM INTO ?", copy),
                ("VACUUM INTO ?", f"file:{copy}?vfs=unix"),
                ("VACUUM INTO ?", f"file:{copy}?vfs=unix&mode=rwc"),
            ):
                with self.subTest(sql=sql, target=target), self.assertRaises(SqliteError) as raised:
                    self.connection.execute(sql, (target,))
                self.assertIn("too many attached databases", str(raised.exception))
            self.assertEqual(os.listdir(dir), ["other.db"])
        self.assertEqual(self.connection.execute("PRAGMA database_list").fetchall()[0][:2], (0, "main"))
        self.assertEqual(self.connection.execute("SELECT count(*) FROM items").fetchall(), [(ROWS,)])


class CursorTests(Stored):
    def setUp(self):
        super().setUp()
        self.connection = SqliteDatabase.open(self.cas, self.cid).connect()
        self.addCleanup(self.connection.close)

    def test_a_cursor_gives_its_rows_once_in_order(self):
        cursor = self.connection.execute("SELECT id FROM items WHERE id <= 10 ORDER BY id")
        self.assertIsInstance(cursor, SqliteCursor)
        self.assertEqual(cursor.fetchone(), (1,))
        self.assertEqual(cursor.fetchmany(), [(2,)])
        self.assertEqual(cursor.fetchmany(3), [(3,), (4,), (5,)])
        self.assertEqual(cursor.fetchmany(0), [])
        self.assertIs(iter(cursor), cursor)
        self.assertEqual(next(cursor), (6,))
        self.assertEqual(list(cursor), [(7,), (8,), (9,), (10,)])
        self.assertEqual((cursor.fetchone(), cursor.fetchmany(5), cursor.fetchall(), list(cursor)), (None, [], [], []))
        with self.assertRaises(StopIteration):
            next(cursor)
        with self.assertRaises(OverflowError):
            cursor.fetchmany(-1)

    def test_a_cursor_describes_its_columns(self):
        cursor = self.connection.cursor()
        self.assertIsNone(cursor.description)
        self.assertEqual((cursor.fetchone(), cursor.fetchall()), (None, []))
        self.assertIs(cursor.execute("SELECT id, name AS n, 1 + 1, payload FROM items WHERE id < 0"), cursor)
        self.assertEqual(cursor.description, tuple((name, None, None, None, None, None, None) for name in ("id", "n", "1 + 1", "payload")))
        self.assertEqual(cursor.fetchall(), [])
        self.assertIsNone(cursor.execute("CREATE TEMP TABLE t (x)").description)

    def test_a_cursor_runs_statements_one_after_another(self):
        cursor = self.connection.execute("SELECT id FROM items ORDER BY id")
        self.assertEqual(cursor.fetchone(), (1,))
        self.assertEqual(cursor.execute("SELECT ?", ("next",)).fetchall(), [("next",)])
        # A statement that fails leaves the rows of the one before.
        cursor.execute("SELECT 1 UNION SELECT 2")
        with self.assertRaises(SqliteError):
            cursor.execute("SELECT * FROM missing")
        self.assertEqual(cursor.fetchall(), [(1,), (2,)])

    def test_closed_cursors_and_connections_raise_sqlite_error(self):
        cursor = self.connection.execute("SELECT 1")
        kept = self.connection.execute("SELECT id FROM items WHERE id < 3")
        cursor.close()
        cursor.close()
        for use in (cursor.fetchone, cursor.fetchmany, cursor.fetchall, lambda: cursor.description, lambda: next(cursor), lambda: cursor.execute("SELECT 1")):
            with self.assertRaises(SqliteError) as raised:
                use()
            self.assertIn("the cursor is closed", str(raised.exception))
        self.connection.close()
        self.connection.close()
        for use in (lambda: self.connection.execute("SELECT 1"), lambda: kept.execute("SELECT 1"), lambda: self.connection.cursor().execute("SELECT 1")):
            with self.assertRaises(SqliteError) as raised:
                use()
            self.assertIn("the connection is closed", str(raised.exception))
        # Rows fetched before the connection closed are still there.
        self.assertEqual(kept.fetchall(), [(1,), (2,)])

    def test_a_connection_closes_at_the_end_of_with(self):
        database = SqliteDatabase.open(self.cas, self.cid)
        with self.assertRaises(KeyError):
            with database.connect() as connection:
                self.assertEqual(connection.execute("SELECT 1").fetchone(), (1,))
                raise KeyError("left early")
        with self.assertRaises(SqliteError):
            connection.execute("SELECT 1")


class ThreadTests(Stored):
    def run_threads(self, count, work):
        failures = []

        def guarded(index):
            try:
                work(index)
            except BaseException as failure:  # noqa: BLE001 - reported below
                failures.append(failure)

        threads = [threading.Thread(target=guarded, args=(index,)) for index in range(count)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(60)
        self.assertFalse(any(thread.is_alive() for thread in threads), "a thread is stuck")
        self.assertEqual(failures, [])

    def test_one_connection_serves_many_threads(self):
        database = SqliteDatabase.open(self.cas, self.cid, verify="trusted-ranges", cache_bytes=64 << 10)
        want = expected(QUERIES[0])
        with database.connect() as connection:
            def work(index):
                for _ in range(20):
                    self.assertEqual(connection.execute(QUERIES[0]).fetchall(), want)
                    self.assertEqual(connection.execute("SELECT name FROM items WHERE id = ?", (index + 1,)).fetchall(), [(f"name-{(index + 1) * 7919 % ROWS}",)])

            self.run_threads(8, work)

    def test_connections_of_one_database_run_side_by_side(self):
        manifest = ChunkManifest.of(DATA, chunk_size=4096)
        database = SqliteDatabase.open(self.cas, self.cid, verify=manifest, cache_bytes=64 << 10)
        want = [expected(sql) for sql in QUERIES]

        def work(index):
            with database.connect() as connection:
                for _ in range(5):
                    self.assertEqual([connection.execute(sql).fetchall() for sql in QUERIES], want)

        self.run_threads(8, work)

    def test_a_connection_closed_by_another_thread_stops_statements(self):
        connection = SqliteDatabase.open(self.cas, self.cid).connect()
        closed = threading.Event()

        def work(index):
            if index == 0:
                connection.close()
                closed.set()
            else:
                closed.wait(60)
                with self.assertRaises(SqliteError):
                    connection.execute("SELECT 1")

        self.run_threads(4, work)


if __name__ == "__main__":
    unittest.main()
