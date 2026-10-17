"""Acceptance tests for the installed wheel; run from outside the source tree."""

import base64
import copy
import hashlib
import importlib.resources
import math
import os
import pathlib
import sqlite3
import tempfile
import unittest
import threading
import uuid
from collections import UserDict
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest import mock
from urllib.parse import parse_qs, urlsplit

from covalence import Blob, BlobFact, Cas, Cid, Codec, Error, Fact, Hash, IntegrityError, Kv, StoreError, UnsupportedError, Value

MAX_DEPTH = 126
EMPTY_BLAKE3 = "bafkr4ifpcne3t5pzugtkaqcn5i3nzskjtpfslsnnyejlpte2spfoihzsmi"


class ApiCase(unittest.TestCase):
    def assert_fails(self, operation, *args):
        with self.assertRaises(ValueError):
            operation(*args)


class InstalledWheelTests(ApiCase):
    def test_type_information_is_packaged(self):
        stub = importlib.resources.files("covalence").joinpath("_native.pyi").read_text(encoding="utf-8")
        for name in ("class Cid:", "class Codec:", "class Hash:", "def from_text(", "def of_file(", "class Value:", "def encode(self)", "class Kv:", "def s3(", "def exists(", "class Cas:", "def kv(", "def put_value(", "def put_file(", "def get_file(", "def has(", "def missing(", "class Blob:", "class BlobFact(Blob, Fact):"):
            self.assertIn(name, stub)


class ErrorTests(ApiCase):
    def test_classes(self):
        for error, base in ((IntegrityError, ValueError), (StoreError, OSError), (UnsupportedError, NotImplementedError)):
            with self.subTest(error=error.__name__):
                self.assertEqual(error.__mro__[1:3], (Error, base))
                self.assertEqual(error.__module__, "covalence")
                self.assertTrue(issubclass(error, Error))
        self.assertEqual((Error.__module__, Error.__bases__), ("covalence", (Exception,)))

    def test_bad_input_is_not_a_covalence_error(self):
        with self.assertRaises(ValueError) as raised:
            Cid("not a CID")
        self.assertNotIsInstance(raised.exception, Error)


class ValueTests(ApiCase):
    def test_all_value_kinds_and_known_wire_encodings(self):
        vectors = [
            (None, "null", "f6"),
            (False, "bool", "f4"),
            (True, "bool", "f5"),
            (0, "integer", "00"),
            (23, "integer", "17"),
            (24, "integer", "1818"),
            (255, "integer", "18ff"),
            (256, "integer", "190100"),
            (65535, "integer", "19ffff"),
            (65536, "integer", "1a00010000"),
            (2**32, "integer", "1b0000000100000000"),
            (2**63 - 1, "integer", "1b7fffffffffffffff"),
            (-1, "integer", "20"),
            (-24, "integer", "37"),
            (-25, "integer", "3818"),
            (-(2**63), "integer", "3b7fffffffffffffff"),
            (b"", "bytes", "40"),
            (b"\x00\xff", "bytes", "4200ff"),
            ("", "string", "60"),
            ("é", "string", "62c3a9"),
            ("e\u0301", "string", "6365cc81"),
            ([], "list", "80"),
            ([1, True, None], "list", "8301f5f6"),
            ({}, "map", "a0"),
            ({"aa": 3, "b": 2, "a": 1}, "map", "a361610161620262616103"),
        ]
        for native, kind, wire in vectors:
            with self.subTest(native=native, kind=kind):
                expected = bytes.fromhex(wire)
                value = Value(native)
                self.assertEqual(value.kind, kind)
                self.assertEqual(value.to_native(), native)
                self.assertIs(type(value.to_native()), type(native))
                self.assertEqual(value.encode(), expected)
                decoded = Value.decode(expected)
                self.assertEqual(decoded, value)
                self.assertEqual(decoded.kind, kind)
                self.assertEqual(decoded.to_native(), native)
                self.assertIs(type(decoded.to_native()), type(native))

    def test_tuples_become_lists_and_nested_values_are_accepted(self):
        value = Value({"tuple": (Value(1), Value([2, 3]))})
        self.assertEqual(value.to_native(), {"tuple": [1, [2, 3]]})
        self.assertEqual(Value.decode(value.encode()), value)

    def test_equality_and_hashing_follow_the_data_model(self):
        cid = Cid.of(b"x", "blake3", "drisl")
        self.assertEqual(Value({"a": [1, cid]}), Value({"a": (Value(1), cid)}))
        self.assertEqual(Value.from_json('{"a":1}'), Value.decode(Value({"a": 1}).encode()))
        for left, right in ((0, False), (b"a", "a"), ([], {}), (cid, cid.to_raw())):
            with self.subTest(left=left, right=right):
                self.assertNotEqual(Value(left), Value(right))
        self.assertNotEqual(Value(1), 1)
        self.assertEqual(hash(Value({"a": [1, cid]})), hash(Value({"a": (1, cid)})))
        self.assertEqual({Value([1]): "present"}[Value((1,))], "present")

    def test_repr_shows_the_value(self):
        cid = Cid.of(b"x")
        self.assertEqual(repr(Value({"a": [1, b"\x00"]})), "Value({'a': [1, b'\\x00']})")
        self.assertEqual(repr(Value(cid)), f"Value(Cid('{cid}'))")

    def test_floats_raise_value_error(self):
        for number in (1.5, 0.0, -0.0, math.inf, math.nan):
            with self.subTest(number=number):
                self.assert_fails(Value, number)
        self.assert_fails(Value.decode, bytes.fromhex("fb3ff8000000000000"))
        self.assertNotEqual(Value(False).encode(), Value(0).encode())

    def test_integers_outside_i64_raise_value_error(self):
        for number in (-(2**63) - 1, 2**63, -(2**200), 2**200):
            with self.subTest(number=number):
                self.assert_fails(Value, number)
        for data in ("1b8000000000000000", "3b8000000000000000", "1bffffffffffffffff"):
            with self.subTest(data=data):
                self.assert_fails(Value.decode, bytes.fromhex(data))

    def test_python_inputs_and_outputs_do_not_alias(self):
        original = {"items": [bytearray(b"before"), {"number": 1}]}
        value = Value(original)
        original["items"][0][:] = b"after!"
        original["items"][1]["number"] = 9
        original["items"].append(None)
        expected = {"items": [b"before", {"number": 1}]}
        self.assertEqual(value.to_native(), expected)
        output = value.to_native()
        output["items"][1]["number"] = 2
        output["items"].append(False)
        self.assertEqual(value.to_native(), expected)

    def test_frozen_objects_cannot_change_or_gain_attributes(self):
        for obj, attribute, replacement in (
            (Value(None), "kind", "integer"),
            (Cid.of(b"x"), "codec", Codec("dag-cbor")),
        ):
            with self.subTest(type=type(obj).__name__):
                with self.assertRaises(AttributeError):
                    setattr(obj, attribute, replacement)
                with self.assertRaises(AttributeError):
                    obj.extra = True

    def test_unsupported_types_and_protocols_are_not_coerced(self):
        class Iterable:
            def __iter__(self):
                raise AssertionError("user iteration must not run")

        class ByteCoercion:
            def __bytes__(self):
                raise AssertionError("user bytes conversion must not run")

        for value in (object(), {1, 2}, UserDict({"x": 1}), Iterable(), ByteCoercion()):
            with self.subTest(type=type(value).__name__), self.assertRaises(TypeError) as raised:
                Value(value)
            self.assertIn(type(value).__name__, str(raised.exception))
        for value in ({1: "x"}, {True: "x"}, {b"x": 1}):
            with self.subTest(value=value), self.assertRaises(TypeError):
                Value(value)
        with self.assertRaises(UnicodeEncodeError):
            Value("\ud800")


class LinkTests(ApiCase):
    def test_dasl_links_round_trip(self):
        for hash in ("sha256", "blake3"):
            for codec in ("raw", "drisl"):
                with self.subTest(hash=hash, codec=codec):
                    cid = Cid.of(b"x", hash, codec)
                    self.assertEqual(Value.decode(Value(cid).encode()).to_native(), cid)
                    self.assertEqual(Value.from_json(Value(cid).to_json()).to_native(), cid)

    def test_other_links_are_rejected(self):
        for binary in (
            bytes.fromhex("01551114") + b"\x00" * 20,
            bytes.fromhex("1220") + hashlib.sha256(b"x").digest(),
            bytes.fromhex("01701220") + hashlib.sha256(b"x").digest(),
            bytes.fromhex("01551340") + b"\x00" * 64,
        ):
            with self.subTest(binary=binary):
                self.assert_fails(Value.decode, bytes.fromhex("81d82a58") + bytes([len(binary) + 1, 0]) + binary)
                text = "b" + base64.b32encode(binary).decode("ascii").lower().rstrip("=")
                self.assert_fails(Value.from_json, '[{"$link":"' + text + '"}]')
                if binary[0] == 1:
                    self.assert_fails(Value, Cid.from_bytes(binary))

    def test_links_are_leaves_and_never_fetch_target_data(self):
        cid = Cid.of(b"not valid CBOR", "sha256", "drisl")
        wire = Value(cid).encode()
        self.assertEqual(wire, bytes.fromhex("d82a582500") + bytes(cid))
        value = Value({"items": [cid, {"again": cid}]})
        self.assertEqual(value.links(), [cid, cid])
        self.assertEqual(Value(cid).kind, "link")
        self.assertEqual(Value.decode(wire).to_native(), cid)
        self.assert_fails(Value.decode, bytes.fromhex("d82a582600") + bytes(cid) + b"\x00")


class CidTests(ApiCase):
    def test_cid_matches_independent_header_hash_and_base32(self):
        digest = hashlib.sha256(b"hello").digest()
        binary = bytes.fromhex("01551220") + digest
        text = "b" + base64.b32encode(binary).decode("ascii").lower().rstrip("=")
        cid = Cid.of(b"hello", "sha256")
        self.assertEqual(bytes(cid), binary)
        self.assertEqual(str(cid), text)
        self.assertEqual(repr(cid), f"Cid('{text}')")
        self.assertEqual((cid.codec, cid.hash, cid.digest), (Codec("raw"), Hash("sha256"), digest))
        self.assertEqual(Cid(text), cid)
        self.assertEqual(Cid(binary), cid)
        self.assertEqual(Cid(bytearray(binary)), cid)
        self.assertEqual(Cid.from_text(text), cid)
        self.assertEqual(Cid.from_bytes(binary), cid)
        for operation, wrong in ((Cid.from_text, binary), (Cid.from_bytes, text), (Cid, 1)):
            with self.subTest(operation=operation, wrong=wrong), self.assertRaises(TypeError):
                operation(wrong)
        self.assert_fails(Cid, text.upper())
        self.assert_fails(Cid, "b" + text[1:].upper())
        self.assertEqual(hash(Cid(text)), hash(cid))
        self.assertEqual({cid: "present"}[Cid.from_bytes(binary)], "present")

    def test_any_cid_with_a_short_digest_is_accepted(self):
        for binary in (
            bytes.fromhex("01551114") + b"\x00" * 20,
            bytes.fromhex("01550003") + b"abc",
            bytes.fromhex("01550000"),
            bytes.fromhex("0155f00104") + b"abcd",
            bytes.fromhex("01701220") + b"\x00" * 32,
            bytes.fromhex("01551340") + b"\x00" * 64,
        ):
            with self.subTest(binary=binary):
                text = "b" + base64.b32encode(binary).decode("ascii").lower().rstrip("=")
                self.assertEqual(bytes(Cid.from_bytes(binary)), binary)
                self.assertEqual(str(Cid(text)), text)
        long = bytes.fromhex("01551341") + b"\x00" * 65
        self.assert_fails(Cid.from_bytes, long)

    def test_cid_v0_and_malformed_input(self):
        v0 = bytes.fromhex("1220") + hashlib.sha256(b"x").digest()
        valid = bytes(Cid.of(b"x", "sha256"))
        for malformed in (b"", b"\x01", v0, valid[:-1], valid + b"\x00", b"\x81\x00" + valid[1:]):
            with self.subTest(malformed=malformed):
                self.assert_fails(Cid.from_bytes, malformed)
        text = str(Cid.of(b"x", "sha256"))
        v0_text = "QmRN6wdp1S2A5EtjW9A3M1vKSBuQQGcgvuhoMUoEz4iiT5"
        for malformed in ("not a CID", "", "b", text + "a", text + "=", "/ipfs/" + text, "z" + text[1:], v0_text):
            with self.subTest(malformed=malformed):
                self.assert_fails(Cid, malformed)


class CodeTests(ApiCase):
    def test_names_follow_hashlib_and_multicodec(self):
        for name, code in (("sha1", 0x11), ("sha256", 0x12), ("sha512", 0x13), ("blake3", 0x1E)):
            with self.subTest(name=name):
                self.assertEqual((Hash(name).name, Hash(name).code), (name, code))
                self.assertEqual(Hash.from_code(code), Hash(name))
        self.assertEqual(Hash("sha2-256"), Hash("sha256"))
        for name, code in (("raw", 0x55), ("dag-cbor", 0x71), ("git-raw", 0x78)):
            with self.subTest(name=name):
                self.assertEqual((Codec(name).name, Codec(name).code), (name, code))
        self.assertEqual(Codec("drisl"), Codec("dag-cbor"))
        self.assertEqual((str(Hash("sha256")), repr(Codec("raw"))), ("sha256", "Codec('raw')"))
        unknown = Codec.from_code(0x70)
        self.assertEqual((unknown.name, unknown.code, repr(unknown)), (None, 0x70, "Codec.from_code(0x70)"))
        self.assertEqual(len({Hash("sha256"), Hash("sha2-256")}), 1)

    def test_bad_names_and_codes(self):
        for operation, value in ((Hash, "md5"), (Hash, "SHA256"), (Codec, "dag-pb"), (Hash.from_code, 1 << 63)):
            with self.subTest(value=value):
                self.assert_fails(operation, value)
        for operation in (Hash, Codec):
            with self.subTest(operation=operation), self.assertRaises(TypeError):
                operation(0x12)


class CidOfTests(ApiCase):
    def test_known_vectors(self):
        self.assertEqual(str(Cid.of(b"")), EMPTY_BLAKE3)
        self.assertEqual(Cid.of(b"").digest, bytes.fromhex("af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"))
        for name in ("sha1", "sha256", "sha512"):
            with self.subTest(name=name):
                self.assertEqual(Cid.of(b"hello", name).digest, hashlib.new(name, b"hello").digest())
        self.assertEqual(bytes(Cid.of(b"", Hash("sha512")))[:4], bytes.fromhex("01551340"))
        self.assertTrue(str(Cid.of(b"", "blake3", "drisl")).startswith("bafyr4i"))
        self.assertEqual(Cid.of(bytearray(b"x")), Cid.of(b"x"))

    def test_parts_match_and_to_raw(self):
        digest = hashlib.sha256(b"x").digest()
        cid = Cid.from_parts("dag-cbor", Hash("sha256"), digest)
        self.assertEqual(cid, Cid.of(b"x", "sha256", Codec("dag-cbor")))
        match cid:
            case Cid(codec, hash, found):
                self.assertEqual((codec.name, hash.name, found), ("dag-cbor", "sha256", digest))
            case _:
                self.fail("no match")
        self.assertEqual(cid.to_raw(), Cid.of(b"x", "sha256"))

    def test_bad_parts_and_hashes(self):
        self.assert_fails(Cid.from_parts, "raw", "sha256", bytes(65))
        self.assert_fails(Cid.from_parts, "raw", "md5", bytes(16))
        cid = Cid.from_parts(Codec.from_code(0x70), Hash.from_code(0x11), bytes(20))
        self.assertEqual((cid.codec.code, cid.hash.name), (0x70, "sha1"))
        with self.assertRaises(UnsupportedError):
            Cid.of(b"x", Hash.from_code(0xB220))
        for args in ((0x55, "sha256", bytes(32)), ("raw", 0x12, bytes(32))):
            with self.subTest(args=args), self.assertRaises(TypeError):
                Cid.from_parts(*args)
        for code in (1 << 64, -1):
            with self.subTest(code=code), self.assertRaises(OverflowError):
                Hash.from_code(code)


class BufferTests(ApiCase):
    def test_bytearray_inputs_are_snapshots(self):
        cid = Cid.of(b"original", "sha256")
        self.assertEqual(Cid.from_bytes(bytearray(bytes(cid))), cid)
        self.assertEqual(Cid.from_parts("raw", "sha256", bytearray(cid.digest)), cid)
        original = bytearray(b"original")
        value = Value(original)
        original[:] = b"mutated!"
        self.assertEqual(value.to_native(), b"original")
        original = bytearray(Value({"bytes": b"original"}).encode())
        value = Value.decode(original)
        original[:] = b"\x00" * len(original)
        self.assertEqual(value.to_native(), {"bytes": b"original"})

    def test_memoryview_is_not_accepted(self):
        view = memoryview(bytes(Cid.of(b"x")))
        for operation in (Value, Value.decode, Cid, Cid.from_bytes, Cid.of):
            with self.subTest(operation=operation), self.assertRaises(TypeError):
                operation(view)

    def test_subclass_overrides_cannot_replace_data(self):
        class LyingBytes(bytes):
            def __len__(self):
                return 0

            def __bytes__(self):
                raise AssertionError("bytes override must not be called")

        class HostileBytearray(bytearray):
            def __len__(self):
                raise AssertionError("length override must not be called")

            def __bytes__(self):
                raise AssertionError("bytes override must not be called")

            def __buffer__(self, flags):
                raise AssertionError("buffer override must not be called")

        for constructor in (LyingBytes, HostileBytearray):
            with self.subTest(type=constructor.__name__):
                value = constructor(b"payload")
                self.assertEqual(Value(value).to_native(), b"payload")
                self.assertEqual(Cid.of(value), Cid.of(b"payload"))
                self.assertIsNone(Value.decode(constructor(b"\xf6")).to_native())


class ResourceTests(ApiCase):
    def test_size_and_item_count_are_not_limited(self):
        large = b"x" * (16 * 1024 * 1024 + 1)
        wire = Value(large).encode()
        self.assertEqual(Value.decode(wire).to_native(), large)
        self.assertEqual(Cid.of(large, "sha256").digest, hashlib.sha256(large).digest())
        items = [None] * 1_000_001
        self.assertEqual(Value.decode(Value(items).encode()).to_native(), items)
        self.assertEqual(Value.from_json(Value(items).to_json()).to_native(), items)

    def test_deep_nesting_raises_instead_of_overflowing_the_stack(self):
        cyclic_list = []
        cyclic_list.append(cyclic_list)
        cyclic_map = {}
        cyclic_map["self"] = cyclic_map
        deep = None
        for _ in range(100_000):
            deep = [deep]
        for value in (cyclic_list, cyclic_map, deep):
            with self.subTest(value=type(value).__name__):
                self.assert_fails(Value, value)
        wire = b"\x81" * MAX_DEPTH + b"\xf6"
        value = Value.decode(wire)
        self.assertEqual(value.encode(), wire)
        for depth in (MAX_DEPTH + 1, 100_000):
            with self.subTest(depth=depth):
                self.assert_fails(Value.decode, b"\x81" * depth + b"\xf6")
        inner = Value.decode(wire[1:])
        self.assertEqual(Value([inner]).encode(), wire)
        self.assert_fails(Value, [value])
        self.assert_fails(Value, [value.to_native()])
        self.assert_fails(Value.from_json, "[" * 100_000 + "]" * 100_000)

    def test_malformed_cbor_raises_value_error(self):
        for data in (
            b"", bytes.fromhex("1817"), bytes.fromhex("f6f6"), bytes.fromhex("61ff"),
            bytes.fromhex("a2616101616102"), bytes.fromhex("a262616101616202"),
            bytes.fromhex("9fff"), bytes.fromhex("f93e00"), bytes.fromhex("fb8000000000000000"),
            bytes.fromhex("fb7ff0000000000000"), bytes.fromhex("d82af6"),
        ):
            with self.subTest(data=data):
                self.assert_fails(Value.decode, data)
        wire = Value({"some": ["nested", 1_000_000]}).encode()
        for length in range(len(wire)):
            with self.subTest(length=length):
                self.assert_fails(Value.decode, wire[:length])


class JsonTests(ApiCase):
    def test_atproto_wrappers_and_plain_values(self):
        cid = Cid.of(b"payload")
        native = {"$type": "org.example.record", "link": cid, "bytes": b"\x00\xff", "items": [None, True, "é", 42]}
        json = Value(native).to_json()
        self.assertIn('"$bytes":"AP8"', json)
        self.assertIn('"$link":"' + str(cid) + '"', json)
        self.assertEqual(Value.from_json(json).to_native(), native)
        self.assertEqual(Value.from_json('{"$bytes":""}').to_native(), b"")
        self.assertEqual(Value({"aa": 1, "b": 2}).to_json(), '{"b":2,"aa":1}')

    def test_numbers(self):
        for number in (-(2**63), 2**63 - 1):
            with self.subTest(number=number):
                self.assertEqual(Value.from_json(Value(number).to_json()).to_native(), number)
        self.assertEqual(Value.from_json("1").kind, "integer")
        for text in ("1.5", "1e0", "-0", "-0.0", "1e10000", "9223372036854775808", "18446744073709551616"):
            with self.subTest(text=text):
                self.assert_fails(Value.from_json, text)

    def test_reserved_keys_and_bad_input(self):
        cid = Cid.of(b"x", "sha256")
        for native in ({"$bytes": "AA"}, {"$link": str(cid)}, {"$bytes": "AA", "extra": 1}):
            with self.subTest(native=native):
                self.assertEqual(Value.decode(Value(native).encode()).to_native(), native)
                self.assert_fails(Value(native).to_json)
        for text in (
            '{"$link":null}', '{"$bytes":42}', '{"$bytes":"__8"}', '{"$bytes":"A==="}', '{"$bytes":"AP8="}',
            '{"$bytes":"AA","extra":1}', '{"a":1,"a":2}', '{"a":1,"\\u0061":2}', '{"a":1} null', '"\\ud800"',
        ):
            with self.subTest(text=text):
                self.assert_fails(Value.from_json, text)


class KvTests(ApiCase):
    def test_put_and_get(self):
        kv = Kv.memory()
        self.assertIsNone(kv.get(b"a"))
        self.assertFalse(kv.exists(b"a"))
        kv.put(b"a", b"one")
        self.assertTrue(kv.exists(bytearray(b"a")))
        kv.put(bytearray(b"a"), bytearray(b"two"))
        self.assertEqual(kv.get(b"a"), b"two")
        for key in (b"", b"\xff", b"a//b"):
            kv.put(key, key)
            self.assertEqual(kv.get(key), key)

    def test_inputs_are_snapshots(self):
        kv = Kv.memory()
        value = bytearray(b"one")
        kv.put(b"a", value)
        value[:] = b"two"
        self.assertEqual(kv.get(b"a"), b"one")

    def test_bad_arguments_raise_type_error(self):
        kv = Kv.memory()
        with self.assertRaises(TypeError):
            kv.get("a")
        with self.assertRaises(TypeError):
            kv.put(b"a", "one")
        with self.assertRaises(TypeError):
            Kv()


class FakeS3(BaseHTTPRequestHandler):
    """Path-style S3 objects in `server.objects`, recording each request's
    method, path and query in `server.requests`, refusing the methods in
    `server.denied` with 403, and answering a GET's `Range` with its bytes."""

    def handle_request(self, body):
        url = urlsplit(self.path)
        self.server.requests.append((self.command, url.path, parse_qs(url.query)))
        if self.command in self.server.denied:
            self.send_response(403)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if self.command == "PUT":
            body = self.rfile.read(int(self.headers["Content-Length"]))
            if self.headers["If-None-Match"] == "*" and url.path in self.server.objects:
                self.send_response(412)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            self.server.objects[url.path] = body
            data = b""
        elif url.path in self.server.objects:
            data = self.server.objects[url.path]
        else:
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        status, headers = 200, {}
        if self.command == "GET" and (requested := self.headers["Range"]):
            first, last = (int(n) for n in requested.removeprefix("bytes=").split("-"))
            if first >= len(data):
                status, data = 416, b""
            else:
                size, data = len(data), data[first : last + 1]
                status, headers = 206, {"Content-Range": f"bytes {first}-{first + len(data) - 1}/{size}", "ETag": '"fake"'}
        self.send_response(status)
        for name, value in headers.items():
            self.send_header(name, value)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        if body:
            self.wfile.write(data)

    def do_GET(self):
        self.handle_request(True)

    def do_HEAD(self):
        self.handle_request(False)

    def do_PUT(self):
        self.handle_request(True)

    def log_message(self, *args):
        pass


class S3Tests(ApiCase):
    def setUp(self):
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), FakeS3)
        self.server.objects, self.server.requests, self.server.denied = {}, [], set()
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        home = tempfile.TemporaryDirectory()
        self.addCleanup(home.cleanup)
        self.home = home.name
        endpoint = f"http://127.0.0.1:{self.server.server_address[1]}"
        # Only this environment: no real credentials, profiles or instance metadata.
        env = {"HOME": self.home, "AWS_EC2_METADATA_DISABLED": "true", "AWS_ENDPOINT_URL": endpoint}
        patch = mock.patch.dict(os.environ, env, clear=True)
        patch.start()
        self.addCleanup(patch.stop)

    def credential(self):
        """The access key that signed the last request, and its session token."""
        _, _, query = self.server.requests[-1]
        return query["X-Amz-Credential"][0].split("/")[0], query.get("X-Amz-Security-Token", [None])[0]

    def test_environment_credentials(self):
        os.environ.update(AWS_ACCESS_KEY_ID="env-key", AWS_SECRET_ACCESS_KEY="env-secret", AWS_SESSION_TOKEN="env-token")
        url, bucket = Kv.s3("s3://bucket/p/q/", region="us-east-1"), Kv.s3("bucket", "p/q", region="us-east-1")
        self.assertIsNone(url.get(b"a/b"))
        self.assertFalse(url.exists(b"a/b"))
        url.put(b"a/b", b"one")
        self.assertTrue(url.exists(b"a/b"))
        self.assertEqual(self.server.requests[-1][0], "HEAD")
        self.assertEqual(self.credential(), ("env-key", "env-token"))
        self.assertEqual(bucket.get(b"a/b"), b"one")
        self.assertEqual(self.credential(), ("env-key", "env-token"))
        self.assertEqual(list(self.server.objects), ["/bucket/p/q/a/b"])

    def test_profile_wins_over_environment(self):
        config, credentials = (os.path.join(self.home, name) for name in ("config", "credentials"))
        with open(config, "w", encoding="utf-8") as file:
            file.write("[profile p]\nregion = eu-central-1\n")
        with open(credentials, "w", encoding="utf-8") as file:
            file.write("[p]\naws_access_key_id = profile-key\naws_secret_access_key = profile-secret\n")
        os.environ.update(AWS_CONFIG_FILE=config, AWS_SHARED_CREDENTIALS_FILE=credentials)
        os.environ.update(AWS_ACCESS_KEY_ID="env-key", AWS_SECRET_ACCESS_KEY="env-secret")
        kv = Kv.s3("s3://bucket", profile="p")
        kv.put(b"a", b"one")
        self.assertEqual(self.credential(), ("profile-key", None))
        _, _, query = self.server.requests[-1]
        self.assertIn("/eu-central-1/s3/", query["X-Amz-Credential"][0])

    def test_cas_never_replaces_over_s3(self):
        os.environ.update(AWS_ACCESS_KEY_ID="key", AWS_SECRET_ACCESS_KEY="secret")
        cas = Cas.kv(Kv.s3("s3://bucket/p", region="us-east-1"))
        cid = Cid.of(b"one")
        path = f"/bucket/p/blake3/{base64.b32encode(cid.digest).decode().lower().rstrip('=')}/bytes"
        self.server.objects[path] = b"wrong"
        self.assertEqual(cas.put(b"one"), cid)
        self.assertEqual(self.server.objects, {path: b"wrong"})
        with self.assertRaises(IntegrityError):
            cas.get(cid)

    def test_cas_over_s3(self):
        os.environ.update(AWS_ACCESS_KEY_ID="key", AWS_SECRET_ACCESS_KEY="secret")
        cas = Cas.kv(Kv.s3("s3://bucket/p", region="us-east-1"), "blocks")
        cid = cas.put(b"one")
        self.assertEqual(cas.put(b"one"), cid)
        self.assertEqual([method for method, _, _ in self.server.requests], ["PUT", "PUT"])
        self.assertTrue(cas.has(cid))
        self.assertEqual(cas.missing([cid, Cid.of(b"two")]), [Cid.of(b"two")])
        self.assertEqual(self.server.requests[-1][0], "HEAD")
        self.assertEqual(cas.get_fact(cid), BlobFact.of(b"one"))
        digest = base64.b32encode(cid.digest).decode().lower().rstrip("=")
        self.assertEqual(list(self.server.objects), [f"/bucket/p/blocks/blake3/{digest}/bytes"])

    def test_cas_put_needs_only_write_access(self):
        # Without s3:ListBucket, S3 answers a HEAD for a missing key with 403.
        os.environ.update(AWS_ACCESS_KEY_ID="key", AWS_SECRET_ACCESS_KEY="secret")
        self.server.denied = {"HEAD"}
        cas = Cas.kv(Kv.s3("s3://bucket/p", region="us-east-1"))
        cid = cas.put(b"one")
        self.assertEqual(cas.get(cid), b"one")
        with self.assertRaises(StoreError):
            cas.has(cid)

    def test_cas_files_over_s3(self):
        os.environ.update(AWS_ACCESS_KEY_ID="key", AWS_SECRET_ACCESS_KEY="secret")
        cas = Cas.kv(Kv.s3("s3://bucket/p", region="us-east-1"))
        with tempfile.TemporaryDirectory() as directory:
            source, target = pathlib.Path(directory, "source"), pathlib.Path(directory, "target")
            source.write_bytes(b"one")
            cid = cas.put_file(source)
            self.assertEqual(cid, Cid.of(b"one"))
            self.assertEqual(cas.get_file(cid, target), 3)
            self.assertEqual(target.read_bytes(), b"one")
            self.assertIsNone(cas.get_file(Cid.of(b"two"), target))
            self.assertEqual(target.read_bytes(), b"one")
        self.assertEqual([method for method, _, _ in self.server.requests], ["PUT", "GET", "GET"])

    def test_cas_files_over_s3_in_ranges(self):
        # Large enough to come down in several ranges at once.
        data = bytes(range(256)) * (80 << 10) + b"tail"
        cid = Cid.of(data)
        path = f"/bucket/p/blake3/{base64.b32encode(cid.digest).decode().lower().rstrip('=')}/bytes"
        self.server.objects[path] = data
        os.environ.update(AWS_ACCESS_KEY_ID="key", AWS_SECRET_ACCESS_KEY="secret")
        cas = Cas.kv(Kv.s3("s3://bucket/p", region="us-east-1"))
        with tempfile.TemporaryDirectory() as directory:
            target = pathlib.Path(directory, "target")
            self.assertEqual(cas.get_file(cid, target), len(data))
            self.assertEqual(target.read_bytes(), data)
            self.assertGreater([method for method, _, _ in self.server.requests].count("GET"), 2)
            tampered = bytearray(data)
            tampered[len(data) // 2] ^= 1
            self.server.objects[path] = bytes(tampered)
            with self.assertRaises(IntegrityError):
                cas.get_file(cid, target)
            self.assertEqual(target.read_bytes(), data)
            self.assertEqual(os.listdir(directory), ["target"])

    def test_failures(self):
        self.assert_fails(Kv.s3, "s3://bucket/p", "q")
        self.assert_fails(Kv.s3, "bucket/p")
        self.assert_fails(Kv.s3, "https://bucket")
        with self.assertRaises(ValueError):
            Kv.s3("s3://bucket")  # no region anywhere
        kv = Kv.s3("s3://bucket", region="us-east-1")  # no credentials anywhere
        with self.assertRaises(StoreError):
            kv.get(b"a")
        with self.assertRaises(StoreError):  # Kv reports keys a store cannot hold as backend errors
            kv.get(b"a//b")
        self.assertEqual(self.server.requests, [])


@unittest.skipUnless(os.environ.get("COVALENCE_TEST_S3"), "set COVALENCE_TEST_S3 to an s3:// scratch prefix")
class RealS3Tests(ApiCase):
    """Writes under a fresh prefix below $COVALENCE_TEST_S3, with the ambient
    AWS configuration, and leaves the objects there."""

    def test_round_trip(self):
        location = os.environ["COVALENCE_TEST_S3"].rstrip("/") + f"/{uuid.uuid4()}/"
        kv = Kv.s3(location)
        self.assertIsNone(kv.get(b"kv/a"))
        kv.put(b"kv/a", b"one")
        self.assertEqual(kv.get(b"kv/a"), b"one")
        cas = Cas.kv(kv, "cas")
        cid = cas.put(b"two")
        self.assertEqual((cas.get(cid), cas.get_fact(cid)), (b"two", BlobFact.of(b"two")))
        self.assertEqual(cas.put(b"two"), cid)
        value = Value({"a": [cid]})
        self.assertEqual(cas.put_value(value), value.cid())
        self.assertEqual(cas.get_fact(value.cid()).value(), value)
        self.assertEqual(cas.missing([cid, value.cid(), Cid.of(b"three")]), [Cid.of(b"three")])


class CasTests(ApiCase):
    def stores(self):
        return {"memory": Cas.memory(), "sqlite": Cas.sqlite(), "kv": Cas.kv(Kv.memory()), "kv-prefix": Cas.kv(Kv.memory(), "p/q")}

    def test_kv_layout(self):
        kv = Kv.memory()
        cid = Cas.kv(kv, "p/").put(b"")
        self.assertEqual(kv.get(b"p/blake3/v4jutopv7gq2nicajxvdnxgjjgn4wjojvxarfn6mtkj4vza7gjra/bytes"), b"")
        self.assertEqual(str(cid), EMPTY_BLAKE3)
        for prefix in ("", "/p", "p//q", ".."):
            with self.subTest(prefix=prefix):
                self.assert_fails(Cas.kv, kv, prefix)

    def test_untrusted_sizes_and_ranges(self):
        for name, cas in self.stores().items():
            with self.subTest(store=name):
                missing = Cid.of(b'missing')
                for size, get_range in ((cas.size, cas.get_range), (cas.size_unchecked, cas.get_range_unchecked)):
                    self.assertIsNone(size(missing))
                    self.assertIsNone(get_range(missing, 0, 1))
                    self.assertIsNone(get_range(missing, 0, 0))
                    cid = cas.put(b'hello')
                    self.assertEqual(size(cid), 5)
                    for start, length, expected in ((1, 3, b'ell'), (4, 10, b'o'), (5, 1, b''), (2, (1 << 64) - 1, b'llo'), ((1 << 64) - 1, 2, b''), (0, 0, b'')):
                        self.assertEqual(get_range(cid, start=start, length=length), expected)
                    empty = cas.put(b'')
                    self.assertEqual(size(empty), 0)
                    self.assertEqual(get_range(empty, 0, 1), b'')

    def test_range_input_validation(self):
        cas = Cas.memory()
        cid = cas.put(b'hello')
        for operation, args in (
            (cas.size, ('bad',)),
            (cas.get_range, ('bad', 0, 1)),
            (cas.size_unchecked, ('bad',)),
            (cas.get_range_unchecked, ('bad', 0, 1)),
        ):
            with self.subTest(operation=operation), self.assertRaises(TypeError):
                operation(*args)
        for operation in (cas.get_range, cas.get_range_unchecked):
            for start, length in ((-1, 1), (0, -1), (1 << 64, 1), (0, 1 << 64)):
                with self.subTest(operation=operation, start=start, length=length), self.assertRaises(OverflowError):
                    operation(cid, start, length)

    def test_put_and_get(self):
        for name, cas in self.stores().items():
            with self.subTest(store=name):
                self.assertIsNone(cas.get(Cid.of(b"one")))
                cid = cas.put(b"one")
                self.assertEqual(cid, Cid.of(b"one"))
                self.assertEqual(cas.put(bytearray(b"one")), cid)
                self.assertEqual(cas.get(cid), b"one")
                self.assertEqual(cas.get_fact(cid), BlobFact.of(b"one"))
                self.assertEqual(cas.get(Cid.from_parts("dag-cbor", "blake3", cid.digest)), b"one")
                self.assertIsNone(cas.get(Cid.of(b"one", "sha256")))
                self.assertIsNone(cas.get_fact(Cid.of(b"two")))
                self.assert_fails(cas.get, Cid.of(b"one", "sha512"))

    def test_files(self):
        data = bytes(range(256)) * 4099
        umask = os.umask(0)
        os.umask(umask)
        with tempfile.TemporaryDirectory() as directory:
            source = pathlib.Path(directory, "source")
            source.write_bytes(data)
            for name, cas in self.stores().items():
                with self.subTest(store=name):
                    cid = cas.put_file(str(source))
                    self.assertEqual(cid, Cid.of(data))
                    self.assertEqual(Cid.of_file(source), cid)
                    self.assertEqual(Cid.of_file(str(source)), cid)
                    self.assertEqual(cas.get(cid), data)
                    target = pathlib.Path(directory, name)
                    self.assertEqual(cas.get_file(cid, target), len(data))
                    self.assertEqual(target.read_bytes(), data)
                    self.assertEqual(target.stat().st_mode & 0o777, 0o666 & ~umask)
                    self.assertIsNone(cas.get_file(Cid.of(b"missing"), pathlib.Path(directory, "missing")))
                    empty = pathlib.Path(directory, "empty")
                    empty.write_bytes(b"")
                    self.assertEqual(cas.put_file(empty), Cid.of(b""))
                    self.assertEqual(Cid.of_file(empty), Cid.of(b""))
                    self.assertEqual(cas.get_file(Cid.of(b""), empty), 0)
            self.assertEqual(sorted(os.listdir(directory)), sorted(["empty", "source", *self.stores()]))

    def test_get_file_keeps_the_permissions_it_replaces(self):
        cas = Cas.memory()
        cid = cas.put(b"one")
        with tempfile.TemporaryDirectory() as directory:
            target = pathlib.Path(directory, "target")
            target.write_bytes(b"old")
            target.chmod(0o600)
            self.assertEqual(cas.get_file(cid, target), 3)
            self.assertEqual((target.read_bytes(), target.stat().st_mode & 0o777), (b"one", 0o600))

    def test_get_file_checks_before_replacing(self):
        kv = Kv.memory()
        cas = Cas.kv(kv)
        cid = cas.put(b"one")
        kv.put(f"blake3/{base64.b32encode(cid.digest).decode().lower().rstrip('=')}/bytes".encode(), b"two")
        with tempfile.TemporaryDirectory() as directory:
            target = pathlib.Path(directory, "target")
            target.write_bytes(b"old")
            with self.assertRaises(IntegrityError):
                cas.get_file(cid, target)
            self.assertEqual(target.read_bytes(), b"old")
            self.assertEqual(os.listdir(directory), ["target"])

    def test_file_errors(self):
        cas = Cas.memory()
        cid = cas.put(b"one")
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(FileNotFoundError):
                cas.put_file(os.path.join(directory, "missing"))
            with self.assertRaises(FileNotFoundError):
                Cid.of_file(os.path.join(directory, "missing"))
            with self.assertRaises(IsADirectoryError):
                Cid.of_file(directory)
            with self.assertRaises(FileNotFoundError):
                cas.get_file(cid, os.path.join(directory, "missing", "target"))
            with self.assertRaises(IsADirectoryError):
                cas.put_file(directory)
        with self.assertRaises(TypeError):
            cas.put_file(b"one")
        with self.assertRaises(TypeError):
            Cid.of_file(b"one")
        with self.assertRaises(TypeError):
            cas.get_file(str(cid), "target")

    def test_get_checks_kv_blocks(self):
        kv = Kv.memory()
        cas = Cas.kv(kv)
        cid = cas.put(b"one")
        key = f"blake3/{base64.b32encode(cid.digest).decode().lower().rstrip('=')}/bytes".encode()
        self.assertEqual(kv.get(key), b"one")
        kv.put(key, b"two")
        for get in (cas.get, cas.get_fact):
            with self.subTest(get=get.__name__), self.assertRaises(IntegrityError):
                get(cid)
        self.assertTrue(cas.has(cid))  # has does not read or check
        cas.put(b"one")  # put never replaces a block
        self.assertEqual(kv.get(key), b"two")

    def test_has_and_missing(self):
        one, two, sha256 = Cid.of(b"one"), Cid.of(b"two"), Cid.of(b"one", "sha256")
        for name, cas in self.stores().items():
            with self.subTest(store=name):
                self.assertFalse(cas.has(one))
                cas.put(b"one")
                self.assertTrue(cas.has(one))
                self.assertTrue(cas.has(Cid.from_parts("dag-cbor", "blake3", one.digest)))
                self.assertFalse(cas.has(sha256))
                self.assertEqual(cas.missing([two, one, sha256]), [two, sha256])
                self.assertEqual(cas.missing(()), [])
                self.assert_fails(cas.has, Cid.of(b"one", "sha512"))
                with self.assertRaises(TypeError):
                    cas.missing([str(one)])

    def test_put_value(self):
        native = {"a": [1, b"x", Cid.of(b"one")], "b": None}
        value = Value(native)
        for name, cas in self.stores().items():
            with self.subTest(store=name):
                cid = cas.put_value(value)
                # The DRISL CID of the value, not the raw CID of its bytes.
                self.assertEqual(cid, value.cid())
                self.assertEqual((cid.codec, cid.hash), (Codec("dag-cbor"), Hash("blake3")))
                self.assertNotEqual(cid, Cid.of(value.encode()))
                self.assertEqual(cas.put_value(native), cid)
                self.assertEqual(cas.get(cid), value.encode())
                self.assertEqual(cas.get_fact(cid).value(), value)
                self.assertTrue(cas.has(cid))
                with self.assertRaises(ValueError):
                    cas.put_value(1.5)
                with self.assertRaises(TypeError):
                    cas.put_value(object())

    def test_inputs_are_snapshots(self):
        cas = Cas.memory()
        data = bytearray(b"one")
        cid = cas.put(data)
        data[:] = b"two"
        self.assertEqual(cas.get(cid), b"one")

    def test_sqlite_file_reopens_and_checks_reads(self):
        with tempfile.TemporaryDirectory() as directory:
            path = os.path.join(directory, "cas.db")
            cid = Cas.sqlite(path).put(b"one")
            cas = Cas.sqlite(path)
            self.assertEqual(cas.get(cid), b"one")
            self.assertEqual(cas.stats(), {"blocks": 1, "bytes": 3})
            with sqlite3.connect(path) as db:
                db.execute("UPDATE blobs SET data = ?", (b"two",))
            db.close()
            self.assertEqual(cas.size(cid), 3)
            self.assertEqual(cas.get_range(cid, 0, 2), b'tw')
            for get in (cas.get, cas.get_fact):
                with self.subTest(get=get.__name__), self.assertRaises(IntegrityError):
                    get(cid)

    def test_storage_failures_raise_store_error(self):
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(StoreError) as raised:
            Cas.sqlite(directory)
        self.assertIsInstance(raised.exception, OSError)

    def test_info_and_stats(self):
        memory, sqlite = Cas.memory(), Cas.sqlite()
        self.assertEqual(memory.info(), {"name": "memory"})
        self.assertEqual(sqlite.info(), {"name": "sqlite", "id": None})
        self.assertEqual(repr(memory), "Cas({'name': 'memory'})")
        for cas in (memory, sqlite):
            self.assertEqual(cas.stats(), {"blocks": 0, "bytes": 0})
            cas.put(b"one")
            cas.put(b"one")
            cas.put(b"three")
            self.assertEqual(cas.stats(), {"blocks": 2, "bytes": 8})

    def test_facts(self):
        value = Value({"a": [1, b"x"]})
        data = value.encode()
        self.assertEqual(value.cid(), Cid.of(data, "blake3", "drisl"))
        fact = BlobFact.of(data)
        self.assertEqual((fact.cid, fact.data, fact.value()), (Cid.of(data), data, Value(data)))
        self.assertEqual(repr(fact), f"BlobFact(cid={fact.cid!r}, len={len(data)})")
        for name, cas in self.stores().items():
            with self.subTest(store=name):
                self.assertEqual(cas.get_fact(cas.put(data)), fact)
        # SQLite answers with a fact for the CID asked for, codec included.
        cas = Cas.sqlite()
        cas.put(data)
        drisl = cas.get_fact(value.cid())
        self.assertEqual((drisl.cid, drisl.data, drisl.value()), (value.cid(), data, value))
        self.assertNotEqual(drisl, fact)
        with self.assertRaises(UnsupportedError):
            cas.get_fact(Cid.from_parts("git-raw", "blake3", fact.cid.digest)).value()
        cas.put(b"\xff")
        with self.assertRaises(ValueError):
            cas.get_fact(Cid.of(b"\xff", "blake3", "drisl")).value()
        for constructor in (Fact, BlobFact, lambda: BlobFact(Cid.of(b"x"), b"y")):
            with self.subTest(constructor=constructor), self.assertRaises(TypeError):
                constructor()
        with self.assertRaises(AttributeError):
            fact.data = b"other"

    def test_bad_arguments_raise_type_error(self):
        cas = Cas.memory()
        for operation, argument in ((cas.get, str(Cid.of(b""))), (cas.get_fact, bytes(Cid.of(b""))), (cas.put, "one")):
            with self.subTest(operation=operation.__name__), self.assertRaises(TypeError):
                operation(argument)
        with self.assertRaises(TypeError):
            Cas()


def range_edges(size):
    """Start and length pairs at every edge of a blob of `size` bytes."""
    top = (1 << 64) - 1
    return [
        (0, 0), (0, 3), (2, 5), (max(size - 2, 0), 10), (size, 1), (size + 1, 1),
        (0, top), (1, top), (top, 1), (top, top),
    ]


def hashlib_digest(hash, data):
    """The digest of `data` from hashlib, or None for BLAKE3, which it lacks."""
    return None if hash == "blake3" else hashlib.new(hash, data).digest()


# Every hash and codec a blob can check, as Cid.of names them.
FORMATS = [
    ("blake3", "raw"), ("sha256", "raw"), ("sha1", "raw"),
    ("blake3", "drisl"), ("sha256", "drisl"), ("sha1", "git-raw"), ("sha256", "git-raw"),
]


class BlobTests(ApiCase):
    def blobs(self, data):
        """Blobs of `data`: in memory, as facts, and as slices."""
        padded = Blob(b"ab" + data + b"cd")
        return {
            "bytes": Blob(data),
            "bytearray": Blob(bytearray(data)),
            "fact": BlobFact.of(data),
            "slice": padded.slice(2, len(data)),
            "slice of slice": padded.slice(1, len(data) + 1).slice(1, len(data)),
            "whole slice": Blob(data).slice(0, (1 << 64) - 1),
            "slice of fact": BlobFact.of(b"ab" + data).slice(2, (1 << 64) - 1),
        }

    def test_range_contract(self):
        for data in (b"hello world", b"x", b""):
            for name, blob in self.blobs(data).items():
                with self.subTest(data=data, blob=name):
                    self.assertEqual((blob.size(), blob.read_all()), (len(data), data))
                    for start, length in range_edges(len(data)):
                        expected = data[min(start, len(data)):min(start + length, len(data))]
                        self.assertEqual(blob.read_range(start, length), expected, (start, length))
                        self.assertEqual(blob.read_range(start=start, length=length), expected)

    def test_slices_clip_when_read(self):
        blob = Blob(b"hello world")
        for start, length, expected in (
            (6, 100, b"world"), (11, 1, b""), (20, 5, b""), ((1 << 64) - 1, (1 << 64) - 1, b""), (0, 0, b""),
        ):
            with self.subTest(start=start, length=length):
                part = blob.slice(start, length)
                self.assertEqual((part.size(), part.read_all()), (len(expected), expected))
        world = blob.slice(6, 5)
        self.assertEqual(world.slice(1, 3).read_all(), b"orl")
        self.assertEqual(world.slice(3, 100).read_all(), b"ld")
        self.assertEqual(world.read_range(4, (1 << 64) - 1), b"d")
        self.assertEqual(type(BlobFact.of(b"x").slice(0, 1)), Blob)

    def test_inputs_are_snapshots(self):
        data = bytearray(b"original")
        blob = Blob(data)
        data[:] = b"mutated!"
        self.assertEqual(blob.read_all(), b"original")
        self.assertEqual(blob.fact(Cid.of(b"original")), BlobFact.of(b"original"))
        self.assertIsNone(blob.fact(Cid.of(b"mutated!")))
        fact = BlobFact.of(data)
        data[:] = b"original"
        self.assertEqual((fact.read_all(), fact.data), (b"mutated!", b"mutated!"))


    def test_every_supported_hash_checks_matching_bytes(self):
        for data in (b"", b"x", bytes(range(256)) * 3):
            for hash, codec in FORMATS:
                cid = Cid.of(data, hash, codec)
                expected = hashlib_digest(hash, data)
                if expected is not None:
                    self.assertEqual(cid.digest, expected)
                for name, blob in self.blobs(data).items():
                    with self.subTest(data=len(data), hash=hash, codec=codec, blob=name):
                        fact = blob.fact(cid)
                        self.assertIs(type(fact), BlobFact)
                        self.assertEqual((fact.cid, fact.data, fact.read_all()), (cid, data, data))
                        self.assertEqual(fact, BlobFact.of(data).fact(cid))

    def test_every_supported_hash_rejects_other_bytes(self):
        data = bytes(range(100))
        for hash, codec in FORMATS:
            for other in (data[:-1], data + b"\x00", b"", b"x"):
                cid = Cid.of(other, hash, codec)
                for name, blob in self.blobs(data).items():
                    with self.subTest(hash=hash, codec=codec, other=other[:3], blob=name):
                        self.assertIsNone(blob.fact(cid))

    def test_facts_relabel_to_the_codec_asked_for(self):
        for data in (b"", Value({"a": 1}).encode()):
            raw = BlobFact.of(data)
            for codec in ("drisl", "git-raw", Codec.from_code(0x0129)):
                with self.subTest(data=data, codec=codec):
                    cid = Cid.from_parts(codec, "blake3", raw.cid.digest)
                    relabelled = raw.fact(cid)
                    self.assertEqual((relabelled.cid, relabelled.data), (cid, data))
                    self.assertNotEqual(relabelled, raw)
                    self.assertEqual(relabelled.fact(raw.cid), raw)
                    self.assertEqual(Blob(data).fact(cid), relabelled)
        value = Value({"a": [1, b"x"]})
        fact = BlobFact.of(value.encode()).fact(value.cid())
        self.assertEqual((fact.cid, fact.value()), (value.cid(), value))

    def test_facts_check_their_bytes_for_other_hashes(self):
        for data in (b"", b"payload"):
            fact = BlobFact.of(data)
            for hash, codec in FORMATS:
                with self.subTest(data=data, hash=hash, codec=codec):
                    cid = Cid.of(data, hash, codec)
                    self.assertEqual(fact.fact(cid).cid, cid)
                    self.assertIsNone(fact.fact(Cid.of(b"other", hash, codec)))

    def test_slices_of_facts_hash_their_own_bytes(self):
        data = b"hello world"
        fact = BlobFact.of(data)
        part = fact.slice(6, 5)
        self.assertIsNone(part.fact(fact.cid))
        self.assertEqual(part.fact(Cid.of(b"world")), BlobFact.of(b"world"))
        self.assertEqual(fact.slice(0, (1 << 64) - 1).fact(fact.cid), fact)
        self.assertIsNone(fact.slice(0, len(data) - 1).fact(fact.cid))

    def test_unsupported_hashes_raise(self):
        digest = Cid.of(b"").digest
        for cid in (
            Cid.from_parts("raw", Hash.from_code(0x1B), digest),
            Cid.from_parts("raw", "blake3", digest[:16]),
            Cid.from_parts("raw", "sha256", digest[:20]),
            Cid.from_parts("raw", "sha1", digest[:19]),
        ):
            for name, blob in self.blobs(b"").items():
                with self.subTest(cid=str(cid), blob=name), self.assertRaises(UnsupportedError):
                    blob.fact(cid)
        for blob in self.blobs(b"x").values():
            with self.assertRaises(ValueError) as raised:
                blob.fact(Cid.of(b"x", "sha512"))
            self.assertNotIsInstance(raised.exception, Error)

    def test_facts_are_blobs(self):
        self.assertTrue(issubclass(BlobFact, Blob))
        data = b"hello"
        fact = BlobFact.of(data)
        self.assertIsInstance(fact, Blob)
        self.assertEqual((fact.size(), fact.read_range(1, 3)), (5, b"ell"))
        for name, cas in {"memory": Cas.memory(), "sqlite": Cas.sqlite()}.items():
            with self.subTest(store=name):
                stored = cas.get_fact(cas.put(data))
                self.assertIsInstance(stored, Blob)
                self.assertEqual((stored, stored.read_all()), (fact, data))
                self.assertEqual(stored.fact(Cid.of(data, "sha256")).cid, Cid.of(data, "sha256"))

    def test_python_overrides_do_not_change_facts(self):
        class Lying(Blob):
            def size(self):
                return 0

            def read_all(self):
                return b"other"

            def read_range(self, start, length):
                return b"other"

        blob = Lying(b"data")
        self.assertEqual(blob.read_all(), b"other")
        self.assertEqual(blob.fact(Cid.of(b"data")), BlobFact.of(b"data"))
        self.assertIsNone(blob.fact(Cid.of(b"other")))
        self.assertEqual(blob.slice(0, 4).fact(Cid.of(b"data")), BlobFact.of(b"data"))
        with self.assertRaises(TypeError):
            class SubFact(BlobFact):
                pass

    def test_subclass_overrides_cannot_replace_data(self):
        class LyingBytes(bytes):
            def __len__(self):
                return 0

            def __bytes__(self):
                raise AssertionError("bytes override must not be called")

        blob = Blob(LyingBytes(b"payload"))
        self.assertEqual((blob.size(), blob.read_all()), (7, b"payload"))

    def test_frozen(self):
        for blob in (Blob(b"x"), BlobFact.of(b"x")):
            with self.subTest(type=type(blob).__name__), self.assertRaises(AttributeError):
                blob.data = b"y"

    def test_bad_arguments(self):
        blob = Blob(b"hello")
        for operation, args in (
            (Blob, ()), (Blob, ("text",)), (Blob, (memoryview(b"x"),)), (Blob, (None,)),
            (blob.fact, (str(Cid.of(b"hello")),)), (blob.fact, (bytes(Cid.of(b"hello")),)),
            (blob.read_range, ("0", 1)), (blob.slice, (0,)),
        ):
            with self.subTest(operation=operation, args=args), self.assertRaises(TypeError):
                operation(*args)
        for operation in (blob.read_range, blob.slice):
            for start, length in ((-1, 1), (0, -1), (1 << 64, 1), (0, 1 << 64)):
                with self.subTest(operation=operation, start=start, length=length), self.assertRaises(OverflowError):
                    operation(start, length)

    def test_large_blobs(self):
        data = bytes(range(256)) * (64 * 1024 + 1)
        blob = Blob(data)
        cid = Cid.of(data, "sha256")
        self.assertEqual(cid.digest, hashlib.sha256(data).digest())
        self.assertEqual(blob.fact(cid).data, data)
        self.assertEqual(blob.read_range(len(data) - 3, 10), data[-3:])

    def test_blob_facts_are_facts(self):
        self.assertTrue(issubclass(BlobFact, Fact))
        self.assertFalse(issubclass(Blob, Fact))
        self.assertNotIsInstance(Blob(b"x"), Fact)
        cas = Cas.memory()
        cid = cas.put(b"x")
        for name, fact in {
            "of": BlobFact.of(b"x"),
            "Blob.fact": Blob(b"x").fact(cid),
            "get_fact": cas.get_fact(cid),
        }.items():
            with self.subTest(fact=name):
                self.assertIs(type(fact), BlobFact)
                self.assertIsInstance(fact, Blob)
                self.assertIsInstance(fact, Fact)
                self.assertEqual((fact.cid, fact.value()), (cid, Value(b"x")))
        with self.assertRaises(TypeError):
            Fact()
        self.assertFalse(hasattr(Fact, "of"))

    def test_blobs_share_sources(self):
        fact = BlobFact.of(b"hello")
        for source in (Blob(b"hello"), fact, fact.slice(0, 5)):
            with self.subTest(source=source):
                blob = Blob(source)
                self.assertIs(type(blob), Blob)
                self.assertEqual(blob.read_all(), b"hello")
                self.assertEqual(blob.fact(fact.cid), fact)

    def test_copies_keep_the_class_and_share_the_source(self):
        class Tagged(Blob):
            def read_all(self):
                return b"tagged"

        tagged = Tagged(b"hello")
        tagged.tags = ["a"]
        tagged.me = tagged
        for blob in (Blob(b"hello"), BlobFact.of(b"hello"), tagged):
            for copier in (copy.copy, copy.deepcopy):
                with self.subTest(type=type(blob).__name__, copier=copier.__name__):
                    other = copier(blob)
                    self.assertIsNot(other, blob)
                    self.assertIs(type(other), type(blob))
                    self.assertEqual(other.read_all(), blob.read_all())
                    self.assertEqual(other.read_range(1, 3), b"ell")
                    self.assertEqual(other.fact(Cid.of(b"hello")), BlobFact.of(b"hello"))
        shallow, deep = copy.copy(tagged), copy.deepcopy(tagged)
        self.assertIs(shallow.tags, tagged.tags)
        self.assertIs(shallow.me, tagged)
        self.assertEqual(deep.tags, ["a"])
        self.assertIsNot(deep.tags, tagged.tags)
        self.assertIs(deep.me, deep)
        fact = BlobFact.of(b"hello")
        self.assertEqual((copy.copy(fact), copy.deepcopy(fact)), (fact, fact))

    def test_facts_compare_and_hash_by_fact(self):
        fact = BlobFact.of(b"hello")
        same = Blob(b"hello").fact(Cid.of(b"hello"))
        relabelled = fact.fact(Cid.from_parts("drisl", "blake3", fact.cid.digest))
        self.assertIsNot(same, fact)
        self.assertEqual(same, fact)
        self.assertEqual(hash(same), hash(fact))
        self.assertEqual(len({fact, same, copy.copy(fact)}), 1)
        self.assertNotEqual(relabelled, fact)
        self.assertNotEqual(BlobFact.of(b"other"), fact)
        self.assertEqual(len({fact, relabelled, BlobFact.of(b"other")}), 3)
        # Plain blobs compare by identity, and never equal a fact.
        blob = Blob(b"hello")
        self.assertNotEqual(blob, Blob(b"hello"))
        self.assertNotEqual(blob, fact)
        self.assertNotEqual(fact, b"hello")
        self.assertEqual({blob: 1}[blob], 1)

    def test_get_fact(self):
        for name, cas in {"memory": Cas.memory(), "sqlite": Cas.sqlite()}.items():
            for data in (b"", b"hello", Value({"a": 1}).encode()):
                raw = cas.put(data)
                for cid in (raw, Cid.from_parts("drisl", "blake3", raw.digest)):
                    with self.subTest(store=name, data=data, cid=str(cid)):
                        fact = cas.get_fact(cid)
                        self.assertEqual((fact.cid, fact.data), (cid, data))
            with self.subTest(store=name, data="missing"):
                missing = Cid.of(b"missing")
                self.assertIsNone(cas.get_fact(missing))


if __name__ == "__main__":
    unittest.main()
