"""Measure unknown streams without rewinding, buffering, or blessing their contents."""

import hashlib
import io
import unittest

from covalence import Cid, DigestAccumulator, digest_fileobj


class ShortReads:
    """A read-only source that never fills a large request."""

    def __init__(self, data):
        self.source = io.BytesIO(data)
        self.sizes = []

    def read(self, size):
        self.sizes.append(size)
        return self.source.read(min(size, 1023))


class DigestTests(unittest.TestCase):
    def assert_both(self, result, payload):
        self.assertEqual(result.len, len(payload))
        self.assertEqual(result.sha256, hashlib.sha256(payload).digest())
        self.assertEqual(result.blake3, Cid.of(payload, "blake3").digest)
        self.assertEqual(result.raw_blake3, Cid.from_bytes(b"\x01\x55\x1e\x20" + result.blake3))

    def test_incremental_modes_and_finish(self):
        payload = b"hello"
        accumulator = DigestAccumulator(max_len=len(payload))
        for chunk in (b"he", b"", b"l", b"lo"):
            accumulator.update(chunk)
        result = accumulator.finish()
        self.assert_both(result, payload)
        self.assertEqual(result.blake3.hex(), "ea8f163db38682925e4491c5e58d4bb3506ef8c14eb78a86e908c5624a67200f")
        with self.assertRaises(ValueError):
            accumulator.finish()
        with self.assertRaises(ValueError):
            accumulator.update(b"")

        sha = digest_fileobj(io.BytesIO(payload), algorithms="sha256")
        self.assertEqual(sha.sha256, hashlib.sha256(payload).digest())
        self.assertIsNone(sha.blake3)
        self.assertIsNone(sha.raw_blake3)
        blake = digest_fileobj(io.BytesIO(payload), algorithms="blake3")
        self.assertIsNone(blake.sha256)
        self.assertEqual(blake.blake3, result.blake3)
        self.assertEqual(blake.raw_blake3, result.raw_blake3)

    def test_binary_stream_short_reads_and_exact_limit(self):
        payload = bytes(range(251)) * 700
        source = ShortReads(payload)
        result = digest_fileobj(source, max_len=len(payload))
        self.assert_both(result, payload)
        self.assertEqual(max(source.sizes), 64 * 1024)
        self.assertEqual(source.sizes[-1], 1)
        self.assertFalse(source.source.closed)

        source = io.BytesIO(b"prefix" + payload)
        source.seek(len(b"prefix"))
        self.assert_both(digest_fileobj(source, max_len=len(payload)), payload)
        self.assertEqual(source.tell(), len(b"prefix") + len(payload))
        self.assertFalse(source.closed)

    def test_zero_limit_and_overrun_poisoning(self):
        self.assert_both(DigestAccumulator(max_len=0).finish(), b"")
        self.assert_both(digest_fileobj(io.BytesIO(), max_len=0), b"")
        for max_len in (0, 2):
            source = ShortReads(b"abcdef")
            with self.assertRaisesRegex(ValueError, f"limit of {max_len} bytes"):
                digest_fileobj(source, max_len=max_len)
            self.assertEqual(source.source.tell(), max_len + 1)
            self.assertEqual(source.sizes, [2, 1] if max_len else [1])

        accumulator = DigestAccumulator(max_len=3)
        accumulator.update(b"ab")
        with self.assertRaisesRegex(ValueError, "limit of 3 bytes"):
            accumulator.update(b"cd")
        with self.assertRaisesRegex(ValueError, "limit of 3 bytes"):
            accumulator.update(b"")
        with self.assertRaisesRegex(ValueError, "limit of 3 bytes"):
            accumulator.finish()

    def test_bytearray_inputs_and_result_value_semantics(self):
        accumulator = DigestAccumulator(max_len=5)
        chunk = bytearray(b"he")
        accumulator.update(chunk)
        chunk[:] = b"XX"
        accumulator.update(bytearray(b"llo"))
        from_acc = accumulator.finish()

        class ByteArrayReader:
            def __init__(self, data):
                self.source = io.BytesIO(data)

            def read(self, size):
                return bytearray(self.source.read(size))

        from_fileobj = digest_fileobj(ByteArrayReader(b"hello"), max_len=(1 << 64) - 1)
        self.assertEqual(from_acc, from_fileobj)
        self.assertEqual(hash(from_acc), hash(from_fileobj))
        self.assertEqual(len({from_acc, from_fileobj}), 1)
        self.assertNotEqual(from_acc, digest_fileobj(io.BytesIO(b"hello"), algorithms="sha256"))
        self.assertNotEqual(from_acc, digest_fileobj(io.BytesIO(b"world")))
        self.assertIn("DigestResult(len=5", repr(from_acc))
        self.assertIn(f"sha256={from_acc.sha256.hex()}", repr(from_acc))
        self.assertIn(f"blake3={from_acc.blake3.hex()}", repr(from_acc))

    def test_invalid_lengths_choices_and_streams(self):
        for max_len in (-1, 1 << 64):
            with self.assertRaises(OverflowError):
                DigestAccumulator(max_len=max_len)
        for algorithms in ("", "sha1"):
            with self.assertRaises(ValueError):
                digest_fileobj(io.BytesIO(), algorithms=algorithms)

        closed = io.BytesIO(b"payload")
        closed.close()
        with self.assertRaises(ValueError):
            digest_fileobj(closed)
        with self.assertRaises(TypeError):
            digest_fileobj(io.StringIO("text"))
        with self.assertRaises(AttributeError):
            digest_fileobj(object())
        for bad in ("text", memoryview(b"abc"), None):
            with self.assertRaises(TypeError):
                DigestAccumulator().update(bad)

        class ReturnsMemoryView:
            def read(self, size):
                return memoryview(b"x")

        with self.assertRaises(TypeError):
            digest_fileobj(ReturnsMemoryView())

        class Overreads:
            def read(self, size):
                return b"x" * (size + 1)

        with self.assertRaisesRegex(ValueError, "returned more bytes than requested"):
            digest_fileobj(Overreads(), max_len=1)

        failure = OSError("read failed")

        class BrokenStream:
            def read(self, size):
                raise failure

        with self.assertRaises(OSError) as captured:
            digest_fileobj(BrokenStream())
        self.assertIs(captured.exception, failure)
