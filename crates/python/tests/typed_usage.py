"""Typed use of the public API, which `mypy --strict` checks in test_stubs.py."""

from typing import assert_type
import io

from covalence import Blob, BlobFact, Cas, Cid, EvictionCandidate, EvictionPolicy, Codec, Error, Fact, Flush, Hash, IntegrityError, Kv, Value


def use_cids() -> None:
    cid = Cid.of(b"hello", hash="sha2-256", codec=Codec("raw"))
    assert_type(cid, Cid)
    assert_type(Cid(str(cid)), Cid)
    assert_type(Cid(bytes(cid)), Cid)
    assert_type(cid.codec.name, str | None)
    assert_type(cid.hash, Hash)
    assert_type(cid.digest, bytes)
    assert_type(Hash.from_code(0x12).code, int)


def use_values(cid: Cid) -> None:
    value = Value({"name": "a", "link": cid, "items": [1, b"x", None]})
    assert_type(value.kind == "map", bool)
    assert_type(Value.decode(value.encode()), Value)
    assert_type(value.links(), list[Cid])
    native = value.to_native()
    if isinstance(native, dict) and isinstance(native["link"], Cid):
        assert_type(native["link"], Cid)


def use_stores(path: str) -> None:
    cas = Cas.kv(Kv.memory(), prefix="blobs/")
    cid = cas.put(bytearray(b"data"))
    assert_type(cas.get(cid), bytes | None)
    assert_type(cas.get_range(cid, 0, 2), bytes | None)
    assert_type(cas.missing([cid]), list[Cid])
    assert_type(Cas.sqlite(path).put_value([1, 2]), Cid)
    fact = cas.get_fact(cid)
    if fact is not None:
        assert_type(fact.value(), Value)
        checked: Fact = fact
        assert_type(checked.cid, Cid)


def use_blobs() -> None:
    blob = Blob(b"hello").slice(1, 3)
    assert_type(blob, Blob)
    assert_type(blob.read_all(), bytes)
    assert_type(blob.fact(Cid.of(b"ell")), BlobFact | None)


def use_streams(path: str) -> None:
    cas = Cas.kv(Kv.directory(path), compression="zstd")
    cid = cas.put_fileobj(io.BytesIO(b"hello"), max_bytes=5, expected_length=5)
    assert_type(cid, Cid)
    assert_type(Cid.of_fileobj(io.BytesIO(b"hello"), max_bytes=5), Cid)
    assert_type(cas.get_to(cid, io.BytesIO(), max_bytes=5), int | None)
    policy = EvictionPolicy.from_callable(lambda candidate: candidate.stored_size > 10)
    assert_type(policy, EvictionPolicy)
    candidate = EvictionCandidate(cid, 5)
    assert_type(candidate.cid, Cid)
    assert_type(candidate.stored_size, int)
    assert_type(candidate.modified_at, float | None)
    cas.flush(Flush.PERSIST, policy=policy)
    assert_type(cas.remote(), Cas)


def handle_errors(cas: Cas, cid: Cid) -> bytes | None:
    try:
        return cas.get(cid)
    except IntegrityError as error:
        assert_type(error, IntegrityError)
        raise ValueError("corrupt") from error
    except Error:
        return None
