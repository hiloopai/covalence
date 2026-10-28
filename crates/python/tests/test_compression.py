import io
import tempfile
import unittest
from pathlib import Path

from covalence import Cas, Cid, IntegrityError, Kv

HELLO = b"blake3/5khrmpntq2bjexseshc6ldklwnig56gbj23yvbxjbdcwestheahq/bytes.zst"
# Produced by Python's streaming compression.zstd.ZstdCompressor, with no content size.
LEGACY = bytes.fromhex("28b52ffd005829000068656c6c6f")


class CompressionTests(unittest.TestCase):
    def test_constructor_rejects_invalid_options(self):
        for options, error in (
            ({"compression": "other"}, ValueError),
            ({"compression": 3}, TypeError),
            ({"compression": "zstd", "level": "3"}, TypeError),
            ({"compression": "zstd", "level": 1 << 31}, OverflowError),
        ):
            with self.subTest(options=options), self.assertRaises(error):
                Cas.kv(Kv.memory(), **options)

    def test_iterable_ingestion_checks_before_publication(self):
        cas = Cas.kv(Kv.memory(), compression="zstd")
        cid = Cid.of(b"hello")
        self.assertEqual(cas.put_from(cid, [b"hel", b"lo"], 5), cid)
        self.assertEqual(cas.get(cid), b"hello")

        pending = Cid.of(b"three")

        def failing():
            yield b"thr"
            raise RuntimeError("the client went away")

        with self.assertRaises(IntegrityError):
            cas.put_from(pending, [b"other"], 5)
        with self.assertRaisesRegex(RuntimeError, "went away"):
            cas.put_from(pending, failing(), 5)
        self.assertFalse(cas.has(pending))

    def test_file_objects_preserve_existing_frames_and_use_logical_bytes(self):
        kv = Kv.memory()
        kv.put(HELLO, LEGACY)
        cas = Cas.kv(kv, compression="zstd")
        cid = cas.put_fileobj(io.BytesIO(b"hello"), max_bytes=5, expected_length=5)
        self.assertEqual(cid, Cid.of(b"hello"))
        self.assertEqual(kv.get(HELLO), LEGACY)

        output = io.BytesIO()
        self.assertEqual(cas.get_to(cid, output, max_bytes=5, expected_length=5), 5)
        self.assertEqual(output.getvalue(), b"hello")
        self.assertFalse(output.closed)

        with self.assertRaises(ValueError):
            cas.get_to(cid, io.BytesIO(), max_bytes=4)

        data = b"new logical content" * 100_000
        cid = cas.put_fileobj(io.BytesIO(data), max_bytes=len(data))
        output = io.BytesIO()
        self.assertEqual(cas.get_to(cid, output, max_bytes=len(data)), len(data))
        self.assertEqual(output.getvalue(), data)

    def test_legacy_compressed_objects_have_logical_cids_and_separate_raw_keys(self):
        kv = Kv.memory()
        kv.put(HELLO, LEGACY)
        cas = Cas.kv(kv, compression="zstd")
        cid = Cid.of(b"hello")
        self.assertEqual(cas.get(cid), b"hello")
        self.assertEqual(cas.size(cid), 5)
        self.assertEqual(cas.get_range(cid, 1, 3), b"ell")
        self.assertEqual(cas.get_range(cid, 5, 1), b"")
        self.assertEqual(cas.put(b"hello"), cid)
        self.assertFalse(Cas.kv(kv).has(cid))
        self.assertEqual(Cas.kv(kv).put(b"hello"), cid)
        self.assertEqual(kv.get(HELLO.removesuffix(b".zst")), b"hello")
        prefixed = Cas.kv(kv, "p/", compression="zstd", level=0)
        self.assertEqual(prefixed.put(b"hello"), cid)
        self.assertTrue(kv.exists(b"p/" + HELLO))
        self.assertEqual(cas.info()["compression"], "zstd")
        self.assertEqual(cas.info()["level"], 3)
        self.assertNotIn("compression", Cas.kv(kv).info())

    def test_compressed_files_are_checked_before_publication(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kv = Kv.memory()
            cas = Cas.kv(kv, compression="zstd")
            source = root / "source"
            source.write_bytes(b"hello" * 100_000)
            cid = cas.put_file(source)
            self.assertEqual(cid, Cid.of(source.read_bytes()))
            output = root / "output"
            self.assertEqual(cas.get_file(cid, output), source.stat().st_size)
            self.assertEqual(output.read_bytes(), source.read_bytes())
            self.assertIsNone(cas.get_file(Cid.of(b"missing"), output))
            self.assertEqual(output.read_bytes(), source.read_bytes())

    def test_corrupt_compressed_download_preserves_destination(self):
        for broken, error in (
            (b"not zstd", (IntegrityError, OSError)),
            (b"", (IntegrityError, OSError)),
            (LEGACY[:-1], (IntegrityError, OSError)),
            (LEGACY + b"junk", (IntegrityError, OSError)),
            (LEGACY[:-5] + b"wrong", IntegrityError),
        ):
            with self.subTest(broken=broken), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                kv = Kv.memory()
                kv.put(HELLO, broken)
                cas = Cas.kv(kv, compression="zstd")
                output = root / "output"
                output.write_bytes(b"keep")
                with self.assertRaises(error):
                    cas.get_file(Cid.of(b"hello"), output)
                self.assertEqual(output.read_bytes(), b"keep")
                self.assertEqual(list(root.iterdir()), [output])
