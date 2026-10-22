"""Python key-value backend errors, streamed transfers and file publication."""

from pathlib import Path
import shutil
import tempfile
import tracemalloc
import unittest

from covalence import Cas, Kv, StoreError


class Backend:
    def __init__(self):
        self.values = {}

    def get(self, key):
        return self.values.get(key)

    def put(self, key, value):
        self.values[key] = value


class KvBackendTests(unittest.TestCase):
    def test_byte_methods_and_optional_method_fallbacks(self):
        backend = Backend()
        kv = Kv.from_backend(backend)
        kv.put(b"key", b"value")
        self.assertEqual(kv.get(b"key"), b"value")
        self.assertTrue(kv.exists(b"key"))
        self.assertIsNone(kv.get(b"missing"))
        self.assertFalse(kv.exists(b"missing"))
        backend.values[b"key"] = [1, 2, 3]
        with self.assertRaises(StoreError):
            kv.get(b"key")
        backend.get = lambda key: (_ for _ in ()).throw(PermissionError("denied"))
        with self.assertRaisesRegex(StoreError, "denied"):
            kv.exists(b"key")

    def test_required_methods_are_callable(self):
        for backend in (object(), type("Bad", (), {"get": 1, "put": 2})()):
            with self.subTest(backend=backend), self.assertRaises((AttributeError, TypeError)):
                Kv.from_backend(backend)

    def test_atomic_creation_is_not_emulated_with_a_racy_check(self):
        backend = Backend()
        with self.assertRaises(StoreError):
            Cas.kv(Kv.from_backend(backend)).put(b"value")
        self.assertEqual(backend.values, {})

    def test_file_methods_work_with_native_and_byte_only_backends(self):
        for kv in (Kv.memory(), Kv.from_backend(Backend())):
            with self.subTest(kv=kv), tempfile.TemporaryDirectory() as directory:
                source, target = Path(directory) / "source", Path(directory) / "target"
                source.write_bytes(b"value")
                kv.put_file(b"key", source)
                self.assertIsNone(kv.get_file(b"missing", target))
                self.assertFalse(target.exists())
                target.write_bytes(b"old")
                target.chmod(0o600)
                self.assertIsNone(kv.get_file(b"missing", target))
                self.assertEqual(target.read_bytes(), b"old")
                self.assertEqual(kv.get_file(b"key", target), 5)
                self.assertEqual(target.read_bytes(), b"value")
                self.assertEqual(target.stat().st_mode & 0o777, 0o600)
                self.assertEqual(sorted(path.name for path in Path(directory).iterdir()), ["source", "target"])

    def test_file_callbacks_do_not_fall_back_to_whole_values(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, target, stored = root / "source", root / "target", root / "stored"
            length = 32 * 1024**2
            with source.open("wb") as stream:
                stream.truncate(length)
            callbacks = []

            class Files(Backend):
                def get(self, key):
                    raise AssertionError("whole-value read")

                def put(self, key, value):
                    raise AssertionError("whole-value write")

                def put_file(self, key, path):
                    callbacks.append(Path(path))
                    self.copy(path, stored)

                def get_file(self, key, path):
                    callbacks.append(Path(path))
                    self.copy(stored, path)
                    return length

                @staticmethod
                def copy(source, target):
                    with open(source, "rb") as reader, open(target, "wb") as writer:
                        shutil.copyfileobj(reader, writer, length=64 * 1024)

            kv = Kv.from_backend(Files())
            tracemalloc.start()
            try:
                kv.put_file(b"key", source)
                self.assertEqual(kv.get_file(b"key", target), length)
                _, peak = tracemalloc.get_traced_memory()
            finally:
                tracemalloc.stop()
            self.assertEqual(target.stat().st_size, length)
            self.assertLess(peak, 2 * 1024**2)
            self.assertEqual(len(callbacks), 2)
            self.assertTrue(all(not path.exists() for path in callbacks))

    def test_failed_or_incorrect_download_preserves_destination_and_cleans_temporary_files(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "target"
            target.write_bytes(b"old")
            callbacks = []

            class Files(Backend):
                result = 4

                def get_file(self, key, path):
                    callbacks.append(Path(path))
                    Path(path).write_bytes(b"partial")
                    if isinstance(self.result, Exception):
                        raise self.result
                    return self.result

            backend = Files()
            kv = Kv.from_backend(backend)
            for result in (4, -1, PermissionError("denied")):
                backend.result = result
                with self.subTest(result=result), self.assertRaises(StoreError):
                    kv.get_file(b"key", target)
                self.assertEqual(target.read_bytes(), b"old")
                self.assertEqual(list(Path(directory).iterdir()), [target])
                self.assertFalse(callbacks[-1].exists())
            backend.result = None
            self.assertIsNone(kv.get_file(b"missing", target))
            self.assertEqual(target.read_bytes(), b"old")
            self.assertEqual(list(Path(directory).iterdir()), [target])
            self.assertFalse(callbacks[-1].exists())


if __name__ == "__main__":
    unittest.main()
