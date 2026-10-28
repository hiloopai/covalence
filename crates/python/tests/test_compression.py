import base64
import gzip
import io
import tempfile
import unittest
import zlib
from pathlib import Path
from unittest import mock

from covalence import Cas, Cid, IntegrityError, Kv

HELLO = b"blake3/5khrmpntq2bjexseshc6ldklwnig56gbj23yvbxjbdcwestheahq/bytes.zst"
# Produced by Python's streaming compression.zstd.ZstdCompressor, with no content size.
LEGACY = bytes.fromhex("28b52ffd005829000068656c6c6f")
DEFLATE_HELLO = HELLO.removesuffix(b".zst") + b".deflate"
GZIP_HELLO = HELLO.removesuffix(b".zst") + b".gz"
BROTLI_HELLO = HELLO.removesuffix(b".zst") + b".br"
# Node.js 24.21.0 node:zlib.brotliCompressSync(Buffer.from("hello")), default options.
BROTLI_FRAME = bytes.fromhex("0b028068656c6c6f03")
CODECS = ("zstd", "deflate", "gzip", "br")
WRAPPED_CODECS = (
    ("deflate", DEFLATE_HELLO, zlib.compress, zlib.decompress),
    ("gzip", GZIP_HELLO, gzip.compress, gzip.decompress),
)


class CompressionTests(unittest.TestCase):
    def test_empty_objects_persist_and_read_across_all_codecs_and_ingestion_paths(self):
        empty = Cid.of(b"")
        digest = base64.b32encode(empty.digest).lower().rstrip(b"=")
        key = b"blake3/" + digest + b"/bytes"
        # Node.js 24.21.0 node:zlib Brotli/Zstandard encoders, default options.
        codecs = (
            ("identity", b"", b""),
            ("gzip", b".gz", gzip.compress(b"")),
            ("deflate", b".deflate", zlib.compress(b"")),
            ("br", b".br", bytes.fromhex("3b")),
            ("zstd", b".zst", bytes.fromhex("28b52ffd2000010000")),
        )
        for codec, suffix, independent in codecs:
            for ingestion in ("put", "put_from", "fileobj", "independent"):
                with self.subTest(codec=codec, ingestion=ingestion):
                    kv = Kv.memory()
                    cas = Cas.kv(kv, compression=codec)
                    self.assertFalse(cas.has(empty))
                    if ingestion == "put":
                        self.assertEqual(cas.put(b""), empty)
                    elif ingestion == "put_from":
                        self.assertEqual(cas.put_from(empty, [b"", b""], 0), empty)
                    elif ingestion == "fileobj":
                        source = io.BytesIO()
                        source.read = mock.Mock(wraps=source.read)
                        self.assertEqual(
                            cas.put_fileobj(source, max_bytes=0, expected_length=0), empty
                        )
                        self.assertFalse(source.closed)
                        self.assertEqual(source.tell(), 0)
                        self.assertTrue(source.read.called)
                        for call in source.read.call_args_list:
                            self.assertEqual(call.args, (1,))  # Bounded EOF probes only.
                    else:
                        kv.put(key + suffix, independent)
                    encoded = kv.get(key + suffix)
                    self.assertIsNotNone(encoded)
                    if codec == "identity":
                        self.assertEqual(encoded, b"")
                    else:
                        self.assertGreater(len(encoded), 0)
                    if ingestion == "independent":
                        self.assertEqual(encoded, independent)
                    reader = Cas.kv(kv, compression=codec)
                    self.assertTrue(reader.has(empty))
                    self.assertEqual(reader.get(empty), b"")
                    self.assertEqual(reader.size(empty), 0)
                    self.assertEqual(reader.get_range(empty, 0, 0), b"")
                    self.assertEqual(reader.get_range(empty, 0, 1), b"")
                    self.assertEqual(reader.get_range(empty, 1, 1), b"")
                    sink = mock.Mock()
                    sink.write.side_effect = AssertionError("empty blocks must not call write")
                    self.assertEqual(
                        reader.get_to(empty, sink, max_bytes=0, expected_length=0), 0
                    )
                    sink.write.assert_not_called()
                    sink.flush.assert_not_called()
                    sink.close.assert_not_called()

    def test_constructor_rejects_invalid_options(self):
        for options, error in (
            ({"compression": "other"}, ValueError),
            ({"compression": "zlib"}, ValueError),
            ({"compression": "raw"}, ValueError),
            ({"compression": "brotli"}, ValueError),
            ({"compression": 3}, TypeError),
            ({"compression": "zstd", "level": "3"}, TypeError),
            ({"compression": "zstd", "level": 1 << 31}, OverflowError),
        ):
            with self.subTest(options=options), self.assertRaises(error):
                Cas.kv(Kv.memory(), **options)

    def test_iterable_ingestion_checks_before_publication(self):
        def failing():
            yield b"thr"
            raise RuntimeError("the client went away")

        for compression in CODECS:
            with self.subTest(compression=compression):
                cas = Cas.kv(Kv.memory(), compression=compression)
                cid = Cid.of(b"hello")
                self.assertEqual(cas.put_from(cid, [b"hel", b"lo"], 5), cid)
                self.assertEqual(cas.get(cid), b"hello")
                pending = Cid.of(b"three")
                with self.assertRaises(IntegrityError):
                    cas.put_from(pending, [b"other"], 5)
                with self.assertRaisesRegex(RuntimeError, "went away"):
                    cas.put_from(pending, failing(), 5)
                self.assertFalse(cas.has(pending))

    def test_file_objects_preserve_existing_frames_and_use_logical_bytes(self):
        for compression, key, frame in (
            ("zstd", HELLO, LEGACY),
            ("deflate", DEFLATE_HELLO, zlib.compress(b"hello")),
            ("gzip", GZIP_HELLO, gzip.compress(b"hello")),
            ("br", BROTLI_HELLO, BROTLI_FRAME),
        ):
            with self.subTest(compression=compression):
                kv = Kv.memory()
                kv.put(key, frame)
                cas = Cas.kv(kv, compression=compression)
                cid = cas.put_fileobj(io.BytesIO(b"hello"), max_bytes=5, expected_length=5)
                self.assertEqual(cid, Cid.of(b"hello"))
                self.assertEqual(kv.get(key), frame)

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

    def test_identity_preserves_the_default_namespace(self):
        kv = Kv.memory()
        default = Cas.kv(kv)
        identity = Cas.kv(kv, compression="identity")
        alias = Cas.kv(kv, compression=None)
        cid = identity.put(b"hello")
        self.assertEqual(default.put(b"hello"), cid)
        self.assertEqual(kv.get(HELLO.removesuffix(b".zst")), b"hello")
        self.assertEqual(identity.get(cid), b"hello")
        self.assertEqual(identity.info(), default.info())
        self.assertEqual(alias.get(cid), b"hello")
        self.assertEqual(alias.info(), default.info())

    def test_http_codecs_interoperate_with_python_and_keep_namespaces_separate(self):
        for codec, key, encode, decode in WRAPPED_CODECS:
            for existing, streaming in (
                (None, False), (None, True), (encode(b"hello", 9), True),
            ):
                with self.subTest(codec=codec, existing=existing, streaming=streaming):
                    kv = Kv.memory()
                    if existing is not None:
                        kv.put(key, existing)
                    cas = Cas.kv(kv, compression=codec)
                    cid = cas.put_from(Cid.of(b"hello"), [b"he", b"llo"], 5) if streaming else cas.put(b"hello")
                    self.assertEqual(cid, Cid.of(b"hello"))
                    self.assertEqual(decode(kv.get(key)), b"hello")
                    if existing is not None:
                        self.assertEqual(kv.get(key), existing)
                    self.assertEqual(cas.get(cid), b"hello")
                    self.assertEqual(cas.size(cid), 5)
                    self.assertEqual(cas.get_range(cid, 1, 3), b"ell")
                    self.assertEqual(cas.get_range(cid, 5, 1), b"")
                    self.assertEqual(cas.info()["compression"], codec)
                    self.assertEqual(cas.info()["level"], 3)
                    for other_codec in ("identity", *CODECS):
                        if other_codec == codec:
                            continue
                        other = Cas.kv(kv, compression=other_codec)
                        self.assertFalse(other.has(cid))
                        self.assertEqual(other.put(b"hello"), cid)
                        self.assertEqual(other.get(cid), b"hello")

    def test_deflate_and_gzip_levels_clamp_and_zero_keeps_the_wrapper(self):
        for codec, key, _, decode in WRAPPED_CODECS:
            for requested, effective in ((-1, 0), (0, 0), (9, 9), (100, 9)):
                with self.subTest(codec=codec, level=requested):
                    kv = Kv.memory()
                    cas = Cas.kv(kv, "p/", compression=codec, level=requested)
                    self.assertEqual(cas.put(b"hello"), Cid.of(b"hello"))
                    self.assertEqual(decode(kv.get(b"p/" + key)), b"hello")
                    self.assertEqual(cas.info()["level"], effective)

    def test_brotli_reads_independent_frames_and_keeps_logical_cids(self):
        for streaming in (False, True):
            with self.subTest(streaming=streaming):
                kv = Kv.memory()
                kv.put(BROTLI_HELLO, BROTLI_FRAME)
                cas = Cas.kv(kv, compression="br")
                cid = Cid.of(b"hello")
                self.assertEqual(cas.get(cid), b"hello")
                self.assertEqual(cas.size(cid), 5)
                self.assertEqual(cas.get_range(cid, 1, 3), b"ell")
                stored = cas.put_from(cid, [b"he", b"llo"], 5) if streaming else cas.put(b"hello")
                self.assertEqual(stored, cid)
                self.assertEqual(kv.get(BROTLI_HELLO), BROTLI_FRAME)
                self.assertEqual(cas.info()["compression"], "br")
                self.assertEqual(cas.info()["level"], 3)
                for codec in ("identity", "zstd", "deflate", "gzip"):
                    other = Cas.kv(kv, compression=codec)
                    self.assertFalse(other.has(cid))
                    self.assertEqual(other.put(b"hello"), cid)
                    self.assertEqual(other.get(cid), b"hello")
                    self.assertEqual(cas.get(cid), b"hello")

    def test_brotli_levels_clamp(self):
        for requested, effective in ((-1, 0), (0, 0), (11, 11), (100, 11)):
            with self.subTest(level=requested):
                kv = Kv.memory()
                cas = Cas.kv(kv, "p/", compression="br", level=requested)
                cid = cas.put(b"hello")
                self.assertEqual(cid, Cid.of(b"hello"))
                self.assertTrue(kv.exists(b"p/" + BROTLI_HELLO))
                self.assertEqual(cas.get(cid), b"hello")
                self.assertEqual(cas.info()["level"], effective)

    def test_invalid_brotli_downloads_preserve_the_destination(self):
        for broken in (
            b"", b"not brotli", BROTLI_FRAME[:-1],
            BROTLI_FRAME + b"junk", BROTLI_FRAME + BROTLI_FRAME,
        ):
            with self.subTest(broken=broken), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                kv = Kv.memory()
                kv.put(BROTLI_HELLO, broken)
                cas = Cas.kv(kv, compression="br")
                cid = Cid.of(b"hello")
                for operation in (cas.get, cas.size, lambda cid: cas.get_range(cid, 0, 1)):
                    with self.assertRaises((IntegrityError, OSError)):
                        operation(cid)
                output = root / "output"
                output.write_bytes(b"keep")
                with self.assertRaises((IntegrityError, OSError)):
                    cas.get_file(cid, output)
                self.assertEqual(output.read_bytes(), b"keep")
                self.assertEqual(list(root.iterdir()), [output])

    def test_valid_brotli_with_wrong_content_fails_checked_reads(self):
        kv = Kv.memory()
        # Brotli has no checksum: this is a valid stream for the wrong content.
        kv.put(BROTLI_HELLO, BROTLI_FRAME.replace(b"hello", b"jello"))
        cas = Cas.kv(kv, compression="br")
        cid = Cid.of(b"hello")
        self.assertEqual(cas.size(cid), 5)  # Sizes/ranges do not verify the CID.
        self.assertEqual(cas.get_range(cid, 0, 1), b"j")
        with self.assertRaises(IntegrityError):
            cas.get(cid)
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            output.write_bytes(b"keep")
            with self.assertRaises(IntegrityError):
                cas.get_file(cid, output)
            self.assertEqual(output.read_bytes(), b"keep")
            self.assertEqual(list(Path(directory).iterdir()), [output])

    def test_invalid_wrapped_downloads_preserve_the_destination(self):
        for codec, key, encode, _ in WRAPPED_CODECS:
            frame = encode(b"hello")
            checksum = -4 if codec == "deflate" else -8
            wrong_checksum = frame[:checksum] + bytes([frame[checksum] ^ 1]) + frame[checksum + 1:]
            other_wrapper = gzip.compress(b"hello") if codec == "deflate" else zlib.compress(b"hello")
            for broken in (
                b"", b"not compressed", frame[:-1], wrong_checksum,
                frame + b"junk", frame + frame,
                zlib.compress(b"hello", wbits=-15), other_wrapper,
            ):
                with self.subTest(codec=codec, broken=broken), tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    kv = Kv.memory()
                    kv.put(key, broken)
                    cas = Cas.kv(kv, compression=codec)
                    cid = Cid.of(b"hello")
                    for operation in (cas.get, cas.size, lambda cid: cas.get_range(cid, 0, 1)):
                        with self.assertRaises((IntegrityError, OSError)):
                            operation(cid)
                    output = root / "output"
                    output.write_bytes(b"keep")
                    with self.assertRaises((IntegrityError, OSError)):
                        cas.get_file(cid, output)
                    self.assertEqual(output.read_bytes(), b"keep")
                    self.assertEqual(list(root.iterdir()), [output])

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
        for compression in CODECS:
            with self.subTest(compression=compression), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                kv = Kv.memory()
                cas = Cas.kv(kv, compression=compression)
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
