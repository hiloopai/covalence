"""Bounded file-object transfers against the installed native wheel."""

import base64
import hashlib
import io
import os
import tempfile
import threading
import time
import unittest
from http.server import ThreadingHTTPServer
from unittest import mock

from covalence import Cas, Cid, IntegrityError, Kv, StoreError
from test_api import FakeS3, MultipartFailureS3

PART_BYTES = 8 << 20
CALLBACK_BYTES = 1 << 20
U64_MAX = (1 << 64) - 1


def key_for(cid):
    digest = base64.b32encode(cid.digest).decode("ascii").lower().rstrip("=")
    return f"blake3/{digest}/bytes".encode("ascii")


class OwnedBytes(io.BytesIO):
    def __init__(self, data=b""):
        super().__init__(data)
        self.flushes = 0
        self.closes = 0

    def flush(self):
        self.flushes += 1
        return super().flush()

    def close(self):
        self.closes += 1
        return super().close()


class ShortSource(OwnedBytes):
    def __init__(self, data, *, mutable=False):
        super().__init__(data)
        self.mutable = mutable
        self.requests = []

    def read(self, count):
        self.requests.append(count)
        result = super().read(min(count, 3))
        return bytearray(result) if self.mutable else result


class MeasuredSource(OwnedBytes):
    def __init__(self, data):
        super().__init__(data)
        self.requests = []
        self.bytes_read = 0

    def read(self, count):
        self.requests.append(count)
        result = super().read(count)
        self.bytes_read += len(result)
        return result


class ChangedOnRewind(OwnedBytes):
    """Change only after the hash pass has observed EOF and seeks to start again."""

    def __init__(self, first, second):
        super().__init__(first)
        self.second = second
        self.saw_eof = False
        self.changed = False
        self.second_reads = 0

    def read(self, count):
        if self.changed:
            self.second_reads += 1
        result = super().read(count)
        if result == b"":
            self.saw_eof = True
        return result

    def seek(self, offset, whence=0):
        if self.saw_eof and not self.changed and offset == 0 and whence == 0:
            self.changed = True
            super().seek(0)
            super().truncate(0)
            super().write(self.second)
        return super().seek(offset, whence)


class PartialSink:
    def __init__(self, count=3):
        self.count = count
        self.data = bytearray()
        self.calls = 0
        self.flushes = 0
        self.closes = 0

    def write(self, data):
        self.calls += 1
        accepted = min(self.count, len(data))
        self.data.extend(data[:accepted])
        return accepted

    def flush(self):
        self.flushes += 1

    def close(self):
        self.closes += 1


class CallbackSink:
    def __init__(self, callback):
        self.callback = callback

    def write(self, data):
        return self.callback(data)


class FileObjectCidTests(unittest.TestCase):
    def test_hashes_one_complete_pass_from_zero_without_storage_or_source_ownership(
        self,
    ):
        data = b"\x00known raw BLAKE3 bytes\xff"
        source = MeasuredSource(data)
        source.seek(7)
        # This operation needs neither a CAS instance nor storage configuration.
        with mock.patch.dict(os.environ, {}, clear=True):
            cid = Cid.of_fileobj(source, max_bytes=len(data), expected_length=len(data))
        self.assertEqual(cid, Cid.of(data))
        self.assertEqual(
            (cid.codec.name, cid.hash.name, len(cid.digest)), ("raw", "blake3", 32)
        )
        self.assertEqual(source.bytes_read, len(data))
        self.assertEqual(source.tell(), len(data))
        self.assertEqual((source.closed, source.closes, source.flushes), (False, 0, 0))

    def test_large_source_hashes_one_pass_in_bounded_read_callbacks(self):
        data = bytes(range(256)) * (2 * CALLBACK_BYTES // 256) + b"tail"
        source = MeasuredSource(data)
        self.assertEqual(Cid.of_fileobj(source, max_bytes=len(data)), Cid.of(data))
        self.assertEqual(source.bytes_read, len(data))
        self.assertGreaterEqual(len(source.requests), 3)
        self.assertTrue(all(0 < count <= CALLBACK_BYTES for count in source.requests))
        self.assertEqual(source.tell(), len(data))

    def test_short_bytes_and_bytearray_reads_have_the_same_raw_cid(self):
        data = bytes(range(256)) * 17 + b"tail"
        for mutable in (False, True):
            with self.subTest(mutable=mutable):
                source = ShortSource(data, mutable=mutable)
                self.assertEqual(
                    Cid.of_fileobj(
                        source, max_bytes=len(data), expected_length=len(data)
                    ),
                    Cid.of(data),
                )
                self.assertEqual(source.tell(), len(data))
                self.assertEqual(
                    (source.closed, source.closes, source.flushes), (False, 0, 0)
                )

    def test_empty_zero_cap_and_exact_expected_lengths(self):
        for data, maximum, expected in (
            (b"", 0, None),
            (b"", 0, 0),
            (b"payload", 7, 7),
            (b"payload", 8, 7),
            (b"", U64_MAX, 0),
        ):
            with self.subTest(data=data, maximum=maximum, expected=expected):
                source = OwnedBytes(data)
                self.assertEqual(
                    Cid.of_fileobj(source, max_bytes=maximum, expected_length=expected),
                    Cid.of(data),
                )
                self.assertEqual(source.tell(), len(data))
                self.assertFalse(source.closed)

    def test_too_long_or_short_sources_fail_without_closing_the_caller(self):
        for data, maximum, expected in (
            (b"payload", 0, None),
            (b"payload", 6, None),
            (b"payload", 7, 6),
            (b"payload", 8, 8),
            (b"", 1, 1),
        ):
            with self.subTest(data=data, maximum=maximum, expected=expected):
                source = OwnedBytes(data)
                with self.assertRaises(ValueError):
                    Cid.of_fileobj(source, max_bytes=maximum, expected_length=expected)
                self.assertEqual(
                    (source.closed, source.closes, source.flushes), (False, 0, 0)
                )

    def test_invalid_bound_relationship_precedes_seek_and_read(self):
        class Uncalled:
            def read(self, count):
                raise AssertionError("read must not run")

            def seek(self, offset, whence=0):
                raise AssertionError("seek must not run")

        with self.assertRaises(ValueError):
            Cid.of_fileobj(Uncalled(), max_bytes=2, expected_length=3)

    def test_hash_read_and_seek_exceptions_preserve_identity(self):
        class FailingSource(OwnedBytes):
            def __init__(self, phase, failure):
                super().__init__(b"bytes")
                self.phase = phase
                self.failure = failure

            def read(self, count):
                if self.phase == "read":
                    raise self.failure
                return super().read(count)

            def seek(self, offset, whence=0):
                if self.phase == "seek":
                    raise self.failure
                return super().seek(offset, whence)

        for phase in ("read", "seek"):
            with self.subTest(phase=phase):
                failure = RuntimeError("hash source disconnected")
                source = FailingSource(phase, failure)
                with self.assertRaises(RuntimeError) as raised:
                    Cid.of_fileobj(source, max_bytes=5)
                self.assertIs(raised.exception, failure)
                self.assertEqual(
                    (source.closed, source.closes, source.flushes), (False, 0, 0)
                )

    def test_hash_reader_rejects_wrong_types_and_oversized_callback_results(self):
        class BadSource(OwnedBytes):
            def __init__(self, result):
                super().__init__()
                self.result = result

            def read(self, count):
                return self.result(count)

        for result in (
            lambda _n: None,
            lambda _n: "bytes",
            lambda _n: True,
            lambda _n: memoryview(b"x"),
            lambda n: b"x" * (n + 1),
        ):
            with self.subTest(result=result):
                source = BadSource(result)
                with self.assertRaises((TypeError, ValueError)):
                    Cid.of_fileobj(source, max_bytes=10)
                self.assertFalse(source.closed)

    def test_hash_bounds_are_keyword_only_unsigned_64_bit_values(self):
        for field in ("max_bytes", "expected_length"):
            for invalid in (-1, 1 << 64):
                with (
                    self.subTest(field=field, invalid=invalid),
                    self.assertRaises(OverflowError),
                ):
                    Cid.of_fileobj(
                        io.BytesIO(), **{"max_bytes": U64_MAX, field: invalid}
                    )
            for invalid in ("1", 1.5):
                with (
                    self.subTest(field=field, invalid=invalid),
                    self.assertRaises(TypeError),
                ):
                    Cid.of_fileobj(
                        io.BytesIO(), **{"max_bytes": U64_MAX, field: invalid}
                    )
        with self.assertRaises(TypeError):
            Cid.of_fileobj(io.BytesIO(), 0)


class BoundedFileObjectTests(unittest.TestCase):
    def stores(self):
        return {
            "memory": Cas.memory(),
            "sqlite": Cas.sqlite(),
            "kv-memory": Cas.kv(Kv.memory()),
        }

    def test_upload_resets_source_to_zero_and_leaves_it_open_at_eof(self):
        data = b"\x00binary\xff evidence"
        for name, cas in self.stores().items():
            with self.subTest(store=name):
                source = OwnedBytes(data)
                source.seek(5)
                cid = cas.put_fileobj(
                    source, max_bytes=len(data), expected_length=len(data)
                )
                self.assertEqual(cid, Cid.of(data))
                self.assertEqual(cas.get(cid), data)
                self.assertEqual(source.tell(), len(data))
                self.assertEqual(
                    (source.closed, source.closes, source.flushes), (False, 0, 0)
                )

    def test_upload_accepts_short_bytes_and_bytearray_reads(self):
        data = bytes(range(256)) * 31 + b"tail"
        for mutable in (False, True):
            for name, cas in self.stores().items():
                with self.subTest(store=name, mutable=mutable):
                    source = ShortSource(data, mutable=mutable)
                    self.assertEqual(
                        cas.put_fileobj(source, max_bytes=len(data)), Cid.of(data)
                    )
                    self.assertEqual(cas.get(Cid.of(data)), data)
                    self.assertTrue(source.requests)
                    self.assertTrue(all(count > 0 for count in source.requests))
                    self.assertEqual(source.tell(), len(data))
                    self.assertEqual(
                        (source.closed, source.closes, source.flushes), (False, 0, 0)
                    )

    def test_source_read_callbacks_are_bounded_to_one_mib(self):
        data = b"x" * (2 * CALLBACK_BYTES + 7)
        source = MeasuredSource(data)
        cas = Cas.memory()
        self.assertEqual(cas.put_fileobj(source, max_bytes=len(data)), Cid.of(data))
        self.assertTrue(source.requests)
        self.assertTrue(all(0 < count <= CALLBACK_BYTES for count in source.requests))
        self.assertEqual(source.tell(), len(data))

    def test_real_binary_files_keep_caller_ownership_and_sink_position(self):
        data = b"native file bytes\x00\xff"
        cas = Cas.memory()
        with (
            tempfile.TemporaryFile("w+b") as source,
            tempfile.TemporaryFile("w+b") as target,
        ):
            source.write(data)
            source.seek(3)
            cid = cas.put_fileobj(
                source, max_bytes=len(data), expected_length=len(data)
            )
            target.write(b"prefix/")
            self.assertEqual(cas.get_to(cid, target, max_bytes=len(data)), len(data))
            self.assertFalse(source.closed)
            self.assertFalse(target.closed)
            self.assertEqual(source.tell(), len(data))
            target.seek(0)
            self.assertEqual(target.read(), b"prefix/" + data)

    def test_missing_and_empty_are_distinct_and_neither_writes(self):
        for name, cas in self.stores().items():
            with self.subTest(store=name):
                source = OwnedBytes()
                empty = cas.put_fileobj(source, max_bytes=0, expected_length=0)
                self.assertEqual(empty, Cid.of(b""))
                sink = PartialSink()
                self.assertEqual(
                    cas.get_to(empty, sink, max_bytes=0, expected_length=0), 0
                )
                self.assertIsNone(
                    cas.get_to(Cid.of(b"missing"), sink, max_bytes=3, expected_length=3)
                )
                self.assertEqual(
                    (sink.calls, sink.data, sink.flushes, sink.closes), (0, b"", 0, 0)
                )
                self.assertFalse(source.closed)

    def test_download_handles_partial_writes(self):
        data = bytes(range(256)) * 33
        for name, cas in self.stores().items():
            cid = cas.put(data)
            with self.subTest(store=name):
                sink = PartialSink()
                self.assertEqual(
                    cas.get_to(
                        cid, sink, max_bytes=len(data), expected_length=len(data)
                    ),
                    len(data),
                )
                self.assertEqual(sink.data, data)
                self.assertGreater(sink.calls, 1)
                self.assertEqual((sink.flushes, sink.closes), (0, 0))

    def test_download_never_flushes_or_closes_owned_writer(self):
        cas = Cas.memory()
        data = b"verified"
        sink = OwnedBytes(b"prefix/")
        sink.seek(0, io.SEEK_END)
        self.assertEqual(
            cas.get_to(cas.put(data), sink, max_bytes=len(data)), len(data)
        )
        self.assertEqual(sink.getvalue(), b"prefix/" + data)
        self.assertEqual((sink.closed, sink.closes, sink.flushes), (False, 0, 0))

    def test_actual_download_limit_and_exact_expected_length_are_enforced(self):
        data = b"some bytes"
        for name, cas in self.stores().items():
            cid = cas.put(data)
            for maximum, expected in (
                (4, None),
                (len(data), len(data) - 1),
                (len(data) + 1, len(data) + 1),
            ):
                with self.subTest(store=name, maximum=maximum, expected=expected):
                    sink = PartialSink()
                    with self.assertRaises(ValueError):
                        cas.get_to(
                            cid, sink, max_bytes=maximum, expected_length=expected
                        )
                    self.assertLessEqual(len(sink.data), maximum)
                    self.assertEqual(bytes(sink.data), data[: len(sink.data)])
                    self.assertEqual((sink.flushes, sink.closes), (0, 0))

    def test_invalid_write_counts_are_rejected_without_implicit_success(self):
        cas = Cas.memory()
        cid = cas.put(b"payload")
        for result in (None, True, False, -1, 0, 8, 1.5, "7"):
            with self.subTest(result=result):
                calls = []

                def write(data, calls=calls, result=result):
                    calls.append(data)
                    return result

                with self.assertRaises((TypeError, ValueError, OSError)):
                    cas.get_to(cid, CallbackSink(write), max_bytes=7)
                self.assertEqual(len(calls), 1)

    def test_writer_exception_identity_survives_native_io(self):
        data = b"partial progress"
        for name, cas in self.stores().items():
            with self.subTest(store=name):
                failure = RuntimeError("writer disconnected")
                sink = PartialSink(2)

                def write(chunk, sink=sink, failure=failure):
                    if sink.calls:
                        raise failure
                    return sink.write(chunk)

                with self.assertRaises(RuntimeError) as raised:
                    cas.get_to(cas.put(data), CallbackSink(write), max_bytes=len(data))
                self.assertIs(raised.exception, failure)
                self.assertEqual(sink.data, data[:2])
                self.assertEqual((sink.flushes, sink.closes), (0, 0))

    def test_invalid_bound_relationship_precedes_every_callback(self):
        class Uncalled:
            def read(self, count):
                raise AssertionError("read must not run")

            def seek(self, offset, whence=0):
                raise AssertionError("seek must not run")

            def write(self, data):
                raise AssertionError("write must not run")

        cas = Cas.memory()
        with self.assertRaises(ValueError):
            cas.put_fileobj(Uncalled(), max_bytes=2, expected_length=3)
        with self.assertRaises(ValueError):
            cas.get_to(Cid.of(b"data"), Uncalled(), max_bytes=2, expected_length=3)

    def test_bounds_are_keyword_only_unsigned_64_bit_values(self):
        cas = Cas.memory()
        cid = cas.put(b"")
        for field in ("max_bytes", "expected_length"):
            for invalid in (-1, 1 << 64):
                arguments = {"max_bytes": U64_MAX, field: invalid}
                with self.subTest(field=field, invalid=invalid):
                    with self.assertRaises(OverflowError):
                        cas.put_fileobj(io.BytesIO(), **arguments)
                    with self.assertRaises(OverflowError):
                        cas.get_to(cid, io.BytesIO(), **arguments)
            for invalid in ("1", 1.5):
                arguments = {"max_bytes": U64_MAX, field: invalid}
                with self.subTest(field=field, invalid=invalid):
                    with self.assertRaises(TypeError):
                        cas.put_fileobj(io.BytesIO(), **arguments)
                    with self.assertRaises(TypeError):
                        cas.get_to(cid, io.BytesIO(), **arguments)
        self.assertEqual(
            cas.put_fileobj(io.BytesIO(), max_bytes=U64_MAX, expected_length=0), cid
        )
        self.assertEqual(
            cas.get_to(cid, io.BytesIO(), max_bytes=U64_MAX, expected_length=0), 0
        )
        with self.assertRaises(TypeError):
            cas.put_fileobj(io.BytesIO(), 0)
        with self.assertRaises(TypeError):
            cas.get_to(cid, io.BytesIO(), 0)

    def test_source_limit_and_expected_length_publish_no_block(self):
        data = b"payload"
        for maximum, expected in (
            (0, None),
            (len(data) - 1, None),
            (len(data), len(data) - 1),
            (len(data) + 1, len(data) + 1),
        ):
            for name, cas in self.stores().items():
                with self.subTest(store=name, maximum=maximum, expected=expected):
                    source = OwnedBytes(data)
                    with self.assertRaises(ValueError):
                        cas.put_fileobj(
                            source, max_bytes=maximum, expected_length=expected
                        )
                    self.assertFalse(cas.has(Cid.of(data)))
                    self.assertEqual(
                        (source.closed, source.closes, source.flushes), (False, 0, 0)
                    )

    def test_bad_read_returns_are_rejected_before_publication(self):
        class BadSource(OwnedBytes):
            def __init__(self, result):
                super().__init__()
                self.result = result

            def read(self, count):
                return self.result(count)

        for result in (
            lambda _n: None,
            lambda _n: "bytes",
            lambda _n: True,
            lambda _n: 3,
            lambda _n: memoryview(b"x"),
            lambda n: b"x" * (n + 1),
        ):
            with self.subTest(result=result):
                cas = Cas.memory()
                source = BadSource(result)
                with self.assertRaises((TypeError, ValueError)):
                    cas.put_fileobj(source, max_bytes=10)
                self.assertEqual(cas.stats(), {"blocks": 0, "bytes": 0})
                self.assertEqual(
                    (source.closed, source.closes, source.flushes), (False, 0, 0)
                )

    def test_read_and_seek_exception_identity_is_preserved(self):
        for phase in ("read", "seek"):
            with self.subTest(phase=phase):
                failure = OSError("source unavailable")

                class FailingSource(OwnedBytes):
                    def __init__(self, data, phase, failure):
                        super().__init__(data)
                        self.phase = phase
                        self.failure = failure

                    def read(self, count):
                        if self.phase == "read":
                            raise self.failure
                        return super().read(count)

                    def seek(self, offset, whence=0):
                        if self.phase == "seek":
                            raise self.failure
                        return super().seek(offset, whence)

                cas = Cas.memory()
                source = FailingSource(b"bytes", phase, failure)
                with self.assertRaises(OSError) as raised:
                    cas.put_fileobj(source, max_bytes=5)
                self.assertIs(raised.exception, failure)
                self.assertEqual(cas.stats(), {"blocks": 0, "bytes": 0})
                self.assertEqual(
                    (source.closed, source.closes, source.flushes), (False, 0, 0)
                )

    def test_invalid_seek_results_are_rejected_before_reading(self):
        class InvalidSeek(OwnedBytes):
            def __init__(self, result):
                super().__init__(b"payload")
                self.result = result
                self.reads = 0

            def seek(self, offset, whence=0):
                super().seek(offset, whence)
                return self.result

            def read(self, count):
                self.reads += 1
                return super().read(count)

        for result in (None, True, False, 1, -1, 0.0, "0"):
            with self.subTest(result=result):
                cas = Cas.memory()
                source = InvalidSeek(result)
                with self.assertRaises((TypeError, ValueError, OverflowError)):
                    cas.put_fileobj(source, max_bytes=7)
                self.assertEqual(source.reads, 0)
                self.assertEqual(cas.stats(), {"blocks": 0, "bytes": 0})

    def test_second_rewind_exception_is_preserved_before_publication(self):
        failure = RuntimeError("second rewind failed")

        class FailingRewind(ChangedOnRewind):
            def seek(self, offset, whence=0):
                if self.saw_eof:
                    raise failure
                return super().seek(offset, whence)

        cas = Cas.memory()
        source = FailingRewind(b"payload", b"payload")
        with self.assertRaises(RuntimeError) as raised:
            cas.put_fileobj(source, max_bytes=7)
        self.assertIs(raised.exception, failure)
        self.assertEqual(cas.stats(), {"blocks": 0, "bytes": 0})
        self.assertFalse(source.closed)

    def test_second_pass_changes_never_publish_a_new_block(self):
        first = b"first content"
        for second in (b"other content", first + b"!", first[:-1], b""):
            for name, cas in self.stores().items():
                with self.subTest(store=name, second=second):
                    source = ChangedOnRewind(first, second)
                    with self.assertRaises((IntegrityError, ValueError, OSError)):
                        cas.put_fileobj(
                            source, max_bytes=len(first) + 1, expected_length=len(first)
                        )
                    self.assertTrue(source.changed)
                    self.assertGreater(source.second_reads, 0)
                    self.assertFalse(cas.has(Cid.of(first)))
                    self.assertFalse(cas.has(Cid.of(second)))
                    self.assertEqual(
                        (source.closed, source.closes, source.flushes), (False, 0, 0)
                    )

    def test_deduplication_still_checks_the_entire_second_pass(self):
        first = b"already present"
        for second in (b"changed content", first + b"!", first[:-1]):
            for name, cas in self.stores().items():
                with self.subTest(store=name, second=second):
                    cid = cas.put(first)
                    source = ChangedOnRewind(first, second)
                    with self.assertRaises((IntegrityError, ValueError, OSError)):
                        cas.put_fileobj(
                            source, max_bytes=len(first) + 1, expected_length=len(first)
                        )
                    self.assertTrue(source.changed)
                    self.assertGreater(source.second_reads, 0)
                    self.assertEqual(cas.get(cid), first)
                    self.assertFalse(cas.has(Cid.of(second)))

    def test_empty_first_pass_cannot_grow_before_storage_or_deduplication(self):
        for present in (False, True):
            for name, cas in self.stores().items():
                with self.subTest(store=name, present=present):
                    empty = Cid.of(b"")
                    if present:
                        cas.put(b"")
                    source = ChangedOnRewind(b"", b"grew")
                    with self.assertRaises(ValueError):
                        cas.put_fileobj(source, max_bytes=4, expected_length=0)
                    self.assertTrue(source.changed)
                    self.assertEqual(cas.has(empty), present)
                    if present:
                        self.assertEqual(cas.get(empty), b"")
                    self.assertFalse(cas.has(Cid.of(b"grew")))
                    self.assertFalse(source.closed)

    def test_deduplicated_source_callback_failure_is_not_skipped(self):
        for name, cas in self.stores().items():
            with self.subTest(store=name):
                data = b"already present"
                cid = cas.put(data)
                failure = RuntimeError("second pass disconnected")

                class FailingSecondPass(ChangedOnRewind):
                    def read(self, count, failure=failure):
                        if self.changed:
                            raise failure
                        return super().read(count)

                source = FailingSecondPass(data, data)
                with self.assertRaises(RuntimeError) as raised:
                    cas.put_fileobj(source, max_bytes=len(data))
                self.assertIs(raised.exception, failure)
                self.assertEqual(cas.get(cid), data)
                self.assertFalse(source.closed)

    def test_valid_deduplication_finishes_the_second_source_pass(self):
        data = b"already present"
        for name, cas in self.stores().items():
            with self.subTest(store=name):
                cid = cas.put(data)
                source = ChangedOnRewind(data, data)
                self.assertEqual(cas.put_fileobj(source, max_bytes=len(data)), cid)
                self.assertTrue(source.changed)
                self.assertGreater(source.second_reads, 0)
                self.assertEqual(source.tell(), len(data))
                self.assertEqual(
                    (source.closed, source.closes, source.flushes), (False, 0, 0)
                )

    def test_corrupt_existing_kv_bytes_are_never_overwritten_or_trusted(self):
        kv = Kv.memory()
        cas = Cas.kv(kv)
        data = b"valid bytes"
        cid = Cid.of(data)
        key = key_for(cid)
        kv.put(key, b"wrong bytes")
        self.assertEqual(cas.put_fileobj(io.BytesIO(data), max_bytes=len(data)), cid)
        self.assertEqual(kv.get(key), b"wrong bytes")
        sink = OwnedBytes()
        with self.assertRaises(IntegrityError):
            cas.get_to(cid, sink, max_bytes=len(data), expected_length=len(data))
        self.assertEqual((sink.closed, sink.closes, sink.flushes), (False, 0, 0))


class RangedS3(FakeS3):
    def serve(self, body):
        # Count the controlled pre-response phase, not handler teardown after the client has
        # already received its bytes and may legitimately issue its next serial request.
        with self.server.lock:
            self.server.pending_headers += 1
            self.server.peak_before_headers = max(
                self.server.peak_before_headers, self.server.pending_headers
            )
        try:
            time.sleep(0.02)
        finally:
            with self.server.lock:
                self.server.pending_headers -= 1
        self.server.headers.append((self.command, self.headers.get("Range")))
        if self.command == "GET" and self.headers.get("Range") is None:
            self.send_response(400)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        return super().serve(body)


class DigestSink:
    def __init__(self):
        self.length = 0
        self.chunks = []
        self.digest = hashlib.sha256()
        self.flushes = 0
        self.closes = 0

    def write(self, data):
        self.length += len(data)
        self.chunks.append(len(data))
        self.digest.update(data)
        return len(data)

    def close(self):
        self.closes += 1

    def flush(self):
        self.flushes += 1


class S3BoundedDownloadTests(unittest.TestCase):
    def setUp(self):
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), RangedS3)
        self.server.objects, self.server.requests, self.server.denied = {}, [], set()
        self.server.headers = []
        self.server.pending_headers, self.server.peak_before_headers = 0, 0
        self.server.lock = threading.Lock()
        self.server.live, self.server.peak, self.server.delay = 0, 0, 0.01
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        endpoint = f"http://127.0.0.1:{self.server.server_address[1]}"
        environment = {
            "HOME": directory.name,
            "AWS_EC2_METADATA_DISABLED": "true",
            "AWS_ENDPOINT_URL": endpoint,
            "AWS_ACCESS_KEY_ID": "test-key",
            "AWS_SECRET_ACCESS_KEY": "test-secret",
        }
        patch = mock.patch.dict(os.environ, environment, clear=True)
        patch.start()
        self.addCleanup(patch.stop)
        self.cas = Cas.kv(Kv.s3("s3://bucket/p", region="us-east-1", concurrency=1))

    def seed(self, data, *, cid=None):
        key = Cid.of(data) if cid is None else cid
        path = "/bucket/p/" + key_for(key).decode("ascii")
        self.server.objects[path] = data
        return key

    def assert_ranged_only(self):
        self.assertTrue(self.server.headers)
        self.assertTrue(
            all(
                method == "GET" and value is not None
                for method, value in self.server.headers
            )
        )
        for method, _path, query in self.server.requests:
            self.assertEqual(method, "GET")
            self.assertIn("range", query["X-Amz-SignedHeaders"][0].split(";"))
        self.assertEqual(self.server.peak_before_headers, 1)

    def test_s3_streams_bounded_chunks_over_signed_serial_ranges(self):
        data = bytes(range(256)) * (3 * PART_BYTES // 256) + b"tail"
        cid = self.seed(data)
        sink = DigestSink()
        self.assertEqual(
            self.cas.get_to(cid, sink, max_bytes=len(data), expected_length=len(data)),
            len(data),
        )
        self.assertEqual(sink.length, len(data))
        self.assertEqual(sink.digest.digest(), hashlib.sha256(data).digest())
        self.assertGreaterEqual(len(sink.chunks), 4)
        self.assertLessEqual(max(sink.chunks), CALLBACK_BYTES)
        self.assertEqual((sink.flushes, sink.closes), (0, 0))
        self.assertEqual(
            [value for _method, value in self.server.headers],
            [
                f"bytes={index * PART_BYTES}-{(index + 1) * PART_BYTES - 1}"
                for index in range(4)
            ],
        )
        self.assert_ranged_only()

    def test_s3_actual_bytes_cannot_exceed_the_caller_sink_limit(self):
        data = b"x" * (2 * PART_BYTES + 11)
        cid = self.seed(data)
        maximum = PART_BYTES + 5
        sink = DigestSink()
        with self.assertRaises(ValueError):
            self.cas.get_to(cid, sink, max_bytes=maximum)
        self.assertLessEqual(sink.length, maximum)
        self.assertTrue(all(length <= CALLBACK_BYTES for length in sink.chunks))
        self.assertEqual((sink.flushes, sink.closes), (0, 0))
        self.assert_ranged_only()

    def test_s3_corrupt_or_truncated_existing_object_fails_integrity(self):
        expected = b"valid content"
        cid = Cid.of(expected)
        for data in (b"wrong content", expected[:-1]):
            with self.subTest(data=data):
                self.seed(data, cid=cid)
                sink = DigestSink()
                with self.assertRaises(IntegrityError):
                    self.cas.get_to(
                        cid,
                        sink,
                        max_bytes=len(expected),
                        expected_length=len(expected),
                    )
                self.assertEqual((sink.flushes, sink.closes), (0, 0))
        self.assert_ranged_only()

    def test_s3_invalid_bound_relationship_performs_no_http_or_callback(self):
        sink = DigestSink()
        with self.assertRaises(ValueError):
            self.cas.get_to(Cid.of(b"unused"), sink, max_bytes=1, expected_length=2)
        self.assertEqual(sink.length, 0)
        self.assertEqual(self.server.requests, [])
        self.assertEqual(self.server.headers, [])

    def test_s3_empty_and_missing_never_invoke_the_writer(self):
        empty = self.seed(b"")
        sink = DigestSink()
        self.assertEqual(
            self.cas.get_to(empty, sink, max_bytes=0, expected_length=0), 0
        )
        missing = Cid.of(b"missing")
        self.assertIsNone(self.cas.get_to(missing, sink, max_bytes=0))
        self.assertIsNone(
            self.cas.get_to(missing, sink, max_bytes=7, expected_length=7)
        )
        self.assertEqual(
            (sink.length, sink.chunks, sink.flushes, sink.closes), (0, [], 0, 0)
        )
        self.assert_ranged_only()

    def test_s3_second_pass_source_error_retains_multipart_cleanup_failure(self):
        server = ThreadingHTTPServer(("127.0.0.1", 0), MultipartFailureS3)
        server.requests = []
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        os.environ["AWS_ENDPOINT_URL"] = f"http://127.0.0.1:{server.server_address[1]}"
        cas = Cas.kv(Kv.s3("s3://bucket/p", region="us-east-1", concurrency=1))
        data = b"x" * (PART_BYTES + 1)
        for denied in (False, True):
            with self.subTest(abort_denied=denied):
                server.requests.clear()
                server.abort_denied = denied
                failure = RuntimeError("file-object source disconnected")

                class FailingSecondPass(ChangedOnRewind):
                    def read(self, count, failure=failure):
                        if self.changed:
                            raise failure
                        return super().read(count)

                source = FailingSecondPass(data, data)
                with self.assertRaises(
                    StoreError if denied else RuntimeError
                ) as raised:
                    cas.put_fileobj(
                        source, max_bytes=len(data), expected_length=len(data)
                    )
                if denied:
                    self.assertIs(raised.exception.__cause__, failure)
                    self.assertIn("failed-source-upload", str(raised.exception))
                    self.assertIn("403", str(raised.exception))
                else:
                    self.assertIs(raised.exception, failure)
                self.assertEqual(
                    [method for method, _path in server.requests], ["POST", "DELETE"]
                )
                self.assertEqual(
                    (source.closed, source.flushes, source.closes), (False, 0, 0)
                )

    def test_s3_file_object_upload_never_replaces_existing_corrupt_bytes(self):
        data = b"valid content"
        cid = Cid.of(data)
        self.seed(b"wrong content", cid=cid)
        source = OwnedBytes(data)
        self.assertEqual(self.cas.put_fileobj(source, max_bytes=len(data)), cid)
        self.assertEqual(source.tell(), len(data))
        self.assertFalse(source.closed)
        self.assertEqual(list(self.server.objects.values()), [b"wrong content"])
        with self.assertRaises(IntegrityError):
            self.cas.get_to(
                cid, DigestSink(), max_bytes=len(data), expected_length=len(data)
            )


if __name__ == "__main__":
    unittest.main()
