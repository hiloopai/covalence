"""Streaming, durability and Python backend authority boundaries."""

import io
from pathlib import Path
import tempfile
import unittest

from covalence import Cas, Cid, EvictionCandidate, EvictionPolicy, Flush, IntegrityError, Kv, StoreError, UnsupportedError


class StreamsTests(unittest.TestCase):
    def test_legacy_compressed_layout_and_durability(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "store"
            cas = Cas.kv(Kv.directory(root), compression="zstd")
            self.assertFalse(root.exists())
            length = 5000
            cid = cas.put_fileobj(io.BytesIO(b"hello" * 1000), max_bytes=length, expected_length=length)
            self.assertEqual((cid, length), (Cid.of(b"hello" * 1000), 5000))
            files = list(root.glob("blake3/*/bytes.zst"))
            self.assertEqual(len(files), 1)
            self.assertLess(files[0].stat().st_size, 100)
            self.assertEqual(cas.get_range(cid, 3, 4), b"lohe")
            self.assertEqual(cas.size(cid), length)
            out = io.BytesIO()
            self.assertEqual(cas.get_to(cid, out, max_bytes=length), length)
            self.assertEqual(out.getvalue(), b"hello" * 1000)
            cas.flush()
            cas.flush(0)
            with self.assertRaises(UnsupportedError):
                cas.flush(Flush.REMOTE | Flush.EVICT)
            with self.assertRaises(UnsupportedError):
                cas.remote()
            files[0].write_bytes(b"broken zstd")
            with self.assertRaises(IntegrityError):
                cas.get_to(cid, io.BytesIO(), max_bytes=length)

    def test_compressed_upload_source_failure_never_publishes_a_frame(self):
        failure = LookupError("second pass failed")

        class FailingSecondPass(io.BytesIO):
            seeks = 0

            def seek(self, offset, whence=0):
                self.seeks += 1
                return super().seek(offset, whence)

            def read(self, size=-1):
                if self.seeks == 2:
                    raise failure
                return super().read(size)

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "store"
            cas = Cas.kv(Kv.directory(root), compression="zstd")
            with self.assertRaises(LookupError) as raised:
                cas.put_fileobj(FailingSecondPass(b"hello"), max_bytes=5, expected_length=5)
            self.assertIs(raised.exception, failure)
            self.assertFalse(root.exists())

    def test_expected_file_cid_is_checked_before_publication(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "source"
            path.write_bytes(b"after")
            cas = Cas.kv(Kv.directory(Path(directory) / "store"), compression="zstd")
            with self.assertRaises(IntegrityError):
                cas.put_file(path, expected=Cid.of(b"before"))
            self.assertFalse(cas.has(Cid.of(b"after")))
            self.assertEqual(cas.put_file(path, expected=Cid.of(b"after")), Cid.of(b"after"))

    def test_python_backend_checks_replies_and_atomic_downloads(self):
        class Backend:
            def __init__(self):
                self.values = {}

            def get(self, key):
                return self.values.get(key)

            def put(self, key, value):
                self.values[key] = value

        backend = Backend()
        kv = Kv.from_backend(backend)
        kv.put(b"key", b"value")
        self.assertEqual(kv.get(b"key"), b"value")
        self.assertTrue(kv.exists(b"key"))
        self.assertFalse(kv.exists(b"missing"))
        backend.values[b"key"] = [1, 2, 3]
        with self.assertRaises(StoreError):
            kv.get(b"key")
        backend.values[b"key"] = b"value"
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "out"
            path.write_bytes(b"old")
            self.assertIsNone(kv.get_file(b"missing", path))
            self.assertEqual(path.read_bytes(), b"old")
            self.assertEqual(kv.get_file(b"key", path), 5)
            self.assertEqual(path.read_bytes(), b"value")
            kv.put_file(b"file", path)
            self.assertEqual(kv.get(b"file"), b"value")

    def test_eviction_metadata_is_immutable_and_non_eviction_never_calls_policy(self):
        cid = Cid.of(b"candidate")
        candidate = EvictionCandidate(cid, 17, -1.25)
        self.assertEqual((candidate.cid, candidate.stored_size, candidate.modified_at), (cid, 17, -1.25))
        self.assertIsNone(EvictionCandidate(cid, 0).modified_at)
        for field, value in (("cid", cid), ("stored_size", 0), ("modified_at", None)):
            with self.subTest(field=field), self.assertRaises(AttributeError):
                setattr(candidate, field, value)
        for invalid in (float("nan"), float("inf"), 1e100):
            with self.subTest(timestamp=invalid), self.assertRaises(ValueError):
                EvictionCandidate(cid, 0, invalid)
        calls = []
        policy = EvictionPolicy.from_callable(lambda candidate: calls.append(candidate) is None)
        self.assertIsInstance(policy, EvictionPolicy)
        cas = Cas.memory()
        cas.flush(0, policy=policy)
        with self.assertRaises(UnsupportedError):
            cas.flush(Flush.EVICT, policy=policy)
        with tempfile.TemporaryDirectory() as directory:
            Cas.kv(Kv.directory(directory)).flush(Flush.PERSIST, policy=policy)
        self.assertEqual(calls, [])

    def test_default_unsupported_and_unknown_flush_requests(self):
        cas = Cas.memory()
        cas.flush(0)
        with self.assertRaises(UnsupportedError):
            cas.flush()
        with self.assertRaises(ValueError):
            cas.flush(8)
        with self.assertRaises(UnsupportedError):
            cas.remote()


if __name__ == "__main__":
    unittest.main()
