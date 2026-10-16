from collections.abc import Iterable, Sequence
from os import PathLike
from typing import Any, Literal, Self, TypeAlias, final

from typing_extensions import disjoint_base

from . import Fact

__all__ = [
    "__version__",
    "Codec",
    "Hash",
    "Cid",
    "Value",
    "Kv",
    "Cas",
    "Blob",
    "BlobFact",
    "Error",
    "IntegrityError",
    "StoreError",
    "UnsupportedError",
]

__version__: str

_BytesLike: TypeAlias = bytes | bytearray
_NativeInput: TypeAlias = None | bool | int | str | _BytesLike | Cid | Value | list[Any] | tuple[Any, ...] | dict[str, Any]
_Native: TypeAlias = None | bool | int | str | bytes | Cid | list["_Native"] | dict[str, "_Native"]

class Error(Exception):
    """The base class of IntegrityError, StoreError and UnsupportedError."""
class IntegrityError(Error, ValueError):
    """Data does not match its CID."""
class StoreError(Error, OSError):
    """The storage under a store failed."""
class UnsupportedError(Error, NotImplementedError):
    """Covalence or a store does not support the operation."""

@final
class Codec:
    """A CID's codec, which says how its bytes are encoded.

    `Codec(name)` takes "raw", "dag-cbor" (or "drisl") or "git-raw"; other
    names raise ValueError. `from_code` takes any multicodec code. Codecs
    compare and hash by code.

    >>> from covalence import Codec
    >>> Codec('drisl')
    Codec('dag-cbor')
    >>> Codec('raw').code
    85
    >>> Codec.from_code(0x99).name is None
    True
    """
    def __new__(cls, name: str) -> Self: ...
    @staticmethod
    def from_code(code: int) -> Codec:
        """The codec with multicodec code `code`, named or not.

        - A code from 2**63 to 2**64 - 1 raises ValueError.
        - A negative code, or one of 2**64 or more, raises OverflowError.
        """
    @property
    def name(self) -> str | None:
        """The codec's name, or None if Covalence has none for it."""
    @property
    def code(self) -> int:
        """The multicodec code."""
    def __eq__(self, other: object, /) -> bool: ...
    def __hash__(self) -> int: ...

@final
class Hash:
    """A CID's hash function.

    `Hash(name)` takes a multicodec name ("sha1", "sha2-256", "sha2-512",
    "blake3") or a `hashlib` name ("sha256", "sha512"); other names raise
    ValueError. `from_code` takes any multicodec code. Hashes compare and
    hash by code.

    >>> from covalence import Hash
    >>> Hash('sha2-256')
    Hash('sha256')
    >>> Hash('blake3').code
    30
    """
    def __new__(cls, name: str) -> Self: ...
    @staticmethod
    def from_code(code: int) -> Hash:
        """The hash with multicodec code `code`, named or not.

        - A code from 2**63 to 2**64 - 1 raises ValueError.
        - A negative code, or one of 2**64 or more, raises OverflowError.
        """
    @property
    def name(self) -> str | None:
        """The hash's name, or None if Covalence has none for it."""
    @property
    def code(self) -> int:
        """The multicodec code."""
    def __eq__(self, other: object, /) -> bool: ...
    def __hash__(self) -> int: ...

@final
class Cid:
    """A content identifier: a CIDv1 with a digest of at most 64 bytes.

    `Cid(cid)` parses text or binary, as `from_text` or `from_bytes` does.
    `str` gives the text and `bytes` the binary form. CIDs compare and hash
    by value, and match as `Cid(codec, hash, digest)`.

    >>> from covalence import Cid
    >>> cid = Cid.of(b'hello', 'sha256')
    >>> Cid(str(cid)) == Cid(bytes(cid)) == cid
    True
    >>> cid.codec, cid.hash
    (Codec('raw'), Hash('sha256'))
    """
    __match_args__ = ("codec", "hash", "digest")
    def __new__(cls, cid: str | _BytesLike) -> Self: ...
    @staticmethod
    def from_text(text: str) -> Cid:
        """Parse a CID's text: lowercase base32 with a `b` prefix.

        Invalid text raises ValueError.
        """
    @staticmethod
    def from_bytes(data: _BytesLike) -> Cid:
        """Parse a CID's binary form.

        Invalid bytes raise ValueError.
        """
    @staticmethod
    def from_parts(codec: Codec | str, hash: Hash | str, digest: _BytesLike) -> Cid:
        """The CID made of `codec`, `hash` and `digest`.

        The codec and hash may be objects or names. The digest is not checked
        against the hash; one over 64 bytes raises ValueError.
        """
    @staticmethod
    def of(data: _BytesLike, hash: Hash | str | None = None, codec: Codec | str | None = None) -> Cid:
        """The CID of `data`, hashed with `hash` and labelled with `codec`.

        The defaults are BLAKE3 and raw. Covalence computes SHA-1, SHA-256,
        SHA-512 and BLAKE3; another hash raises UnsupportedError.
        """
    @staticmethod
    def of_file(path: str | PathLike[str]) -> Cid:
        """The raw BLAKE3 CID of the file at `path`, as `Cas.put_file` stores it.

        Reads the file without holding it in memory.
        """
    def to_raw(self) -> Cid:
        """The CID with the same hash and digest and the raw codec."""
    def __str__(self) -> str: ...
    def __bytes__(self) -> bytes:
        """The binary form."""
    def __eq__(self, other: object, /) -> bool: ...
    def __hash__(self) -> int: ...
    @property
    def codec(self) -> Codec:
        """The codec."""
    @property
    def hash(self) -> Hash:
        """The hash function."""
    @property
    def digest(self) -> bytes:
        """The digest."""

@final
class Value:
    """A DASL value: null, bool, integer, bytes, string, list, map or link.

    Map keys are strings. `Value(value)` takes None, bool, int, str, bytes,
    `Cid`, `Value`, and lists, tuples and dicts with str keys of them. It
    checks the same rules as `decode`:

    - Links must be DASL CIDs: raw or DRISL, with a SHA-256 or BLAKE3 digest.
    - Integers outside i64, floats and nesting deeper than 126 raise
      ValueError.

    Other types raise TypeError. Values compare and hash by value.

    >>> from covalence import Value
    >>> value = Value({'a': [1, b'x']})
    >>> Value.decode(value.encode()) == value
    True
    >>> value.to_json()
    '{"a":[1,{"$bytes":"eA"}]}'
    """
    def __new__(cls, value: _NativeInput) -> Self: ...
    @property
    def kind(self) -> Literal["null", "bool", "integer", "bytes", "string", "list", "map", "link"]:
        """The kind of value, as a string.

        One of "null", "bool", "integer", "bytes", "string", "list", "map" or
        "link".

        >>> from covalence import Value
        >>> Value([1, 'a']).kind
        'list'
        """
    def to_native(self) -> _Native:
        """The value as None, bool, int, str, bytes, `Cid`, lists and dicts."""
    def encode(self) -> bytes:
        """The DRISL encoding."""
    @staticmethod
    def decode(data: _BytesLike) -> Value:
        """Decode exactly one DRISL value.

        Invalid DRISL raises ValueError.
        """
    def to_json(self) -> str:
        """The value as ATProto JSON.

        Links become `{"$link": "<CID>"}` and bytes `{"$bytes": "<base64>"}`.
        """
    @staticmethod
    def from_json(text: str) -> Value:
        """Parse ATProto JSON, as `to_json` writes it.

        Invalid JSON raises ValueError.

        >>> from covalence import Value
        >>> Value.from_json('{"a": {"$bytes": "eA"}}') == Value({'a': b'x'})
        True
        """
    def cid(self) -> Cid:
        """The CID of the DRISL encoding, hashed with BLAKE3.

        This is the CID `Cas.put_value` returns.
        """
    def links(self) -> list[Cid]:
        """The CIDs the value links to, in encoding order, with repeats."""
    def __eq__(self, other: object, /) -> bool: ...
    def __hash__(self) -> int: ...

@final
class Kv:
    """A store of byte values under byte keys.

    Build one with `memory` or `s3`. A failure of the storage underneath
    raises StoreError.

    >>> from covalence import Kv
    >>> kv = Kv.memory()
    >>> kv.put(b'key', b'value')
    >>> kv.get(b'key'), kv.get(b'other'), kv.exists(b'key')
    (b'value', None, True)
    """
    @staticmethod
    def memory() -> Kv:
        """A new, empty store in memory."""
    @staticmethod
    def s3(location: str, prefix: str | None = None, *, region: str | None = None, profile: str | None = None, concurrency: int | None = None) -> Kv:
        """A store in S3, configured as the AWS SDKs are.

        `location` is an `s3://bucket/prefix/` URL, or a bucket name with
        `prefix`. `region` and `profile` override the SDK configuration.
        `concurrency` is how many 8 MiB parts of one value move at once (16 by
        default; 0 counts as 1), which bounds the memory a streamed value
        holds.

        - An invalid location, a URL with `prefix` too, or no region raises
          ValueError.
        - Keys must be UTF-8 with non-empty `/`-separated segments other than
          `.` and `..`; other keys raise StoreError.
        - Credentials are fetched at the first request; missing or failed
          credentials raise StoreError then.
        - Without `s3:ListBucket`, S3 answers 403 for a missing key, so reading
          one raises StoreError instead of returning None or False.
        """
    def get(self, key: _BytesLike) -> bytes | None:
        """The value under `key`, or None if there is none."""
    def put(self, key: _BytesLike, value: _BytesLike) -> None:
        """Store `value` under `key`, replacing any value there."""
    def exists(self, key: _BytesLike) -> bool:
        """Whether there is a value under `key`, without reading it."""

@disjoint_base
class Blob:
    """Bytes read whole or by range, not checked except by `fact`.

    - `Blob(data)` holds a snapshot of `data` in memory.
    - `Blob(blob)` shares `blob`'s source.
    - Copies keep their class and share the source.

    Its methods are not virtual: a Python subclass's overrides change what
    Python callers see, but `fact` and `slice` read the Rust blob underneath.

    >>> from covalence import Blob
    >>> blob = Blob(b'hello world')
    >>> blob.size(), blob.slice(6, 5).read_all()
    (11, b'world')
    """
    def __new__(cls, data: _BytesLike | Blob) -> Self: ...
    def size(self) -> int:
        """The size in bytes, as the blob reports it."""
    def read_range(self, start: int, length: int) -> bytes:
        """Up to `length` bytes from `start`; fewer at the end, and none past it.

        >>> from covalence import Blob
        >>> blob = Blob(b'hello')
        >>> blob.read_range(1, 3)
        b'ell'
        >>> blob.read_range(4, 10)
        b'o'
        >>> blob.read_range(5, 1)
        b''
        """
    def read_all(self) -> bytes:
        """All the bytes."""
    def slice(self, start: int, length: int) -> Blob:
        """Up to `length` bytes from `start`, as a blob over the same bytes."""
    def fact(self, cid: Cid) -> BlobFact | None:
        """A checked fact for exactly `cid`, or None if the bytes do not hash to it.

        - A CID with a digest over 32 bytes, such as SHA-512's, raises
          ValueError.
        - Covalence checks SHA-1, SHA-256 and BLAKE3. Another hash, or a
          digest length the hash does not give, raises UnsupportedError.

        >>> from covalence import Blob, Cid
        >>> blob = Blob(b'hello')
        >>> fact = blob.fact(Cid.of(b'hello', 'sha256'))
        >>> fact.cid == Cid.of(b'hello', 'sha256'), fact.data
        (True, b'hello')
        >>> fact.fact(Cid.of(b'hello', 'sha256', 'drisl')).cid.codec.name
        'dag-cbor'
        >>> blob.fact(Cid.of(b'other')) is None
        True
        """
    def __copy__(self) -> Self:
        """A blob of the same class over the same source."""
    def __deepcopy__(self, memo: dict[int, Any], /) -> Self:
        """A blob of the same class over the same source: blobs are read-only."""

@final
class BlobFact(Blob, Fact):
    """Bytes and a CID they hash to: a `Fact`, and a blob of those bytes.

    Facts are equal when their CIDs and bytes are, and hash by CID. This
    class cannot be subclassed.

    >>> from covalence import BlobFact, Cid
    >>> fact = BlobFact.of(b'hello')
    >>> fact.cid == Cid.of(b'hello'), fact.data, fact.value()
    (True, b'hello', Value(b'hello'))
    """
    @staticmethod
    def of(data: _BytesLike) -> BlobFact:
        """The fact of `data` and its raw BLAKE3 CID."""
    @property
    def cid(self) -> Cid:
        """The CID the bytes hash to."""
    @property
    def data(self) -> bytes:
        """The bytes."""
    def value(self) -> Value:
        """The bytes decoded as the CID's codec says.

        - DRISL bytes decode to their value; invalid DRISL raises ValueError.
        - Raw bytes become a bytes `Value`.
        - Another codec raises UnsupportedError.
        """
    def __copy__(self) -> Self:
        """A fact over the same source."""
    def __deepcopy__(self, memo: dict[int, Any], /) -> Self:
        """A fact over the same source: facts are read-only."""
    def __eq__(self, other: object, /) -> bool: ...
    def __hash__(self) -> int: ...

@final
class Cas:
    """A content-addressed store, which finds bytes by their CIDs.

    Build one with `memory`, `sqlite` or `kv`. `put` returns the raw BLAKE3
    CID of the bytes, and a BLAKE3 CID with another codec finds the same
    bytes.

    - A CID with a digest over 32 bytes raises ValueError.
    - A failed check raises IntegrityError.
    - A failure of the storage underneath raises StoreError.

    >>> from covalence import Cas, Cid
    >>> cas = Cas.memory()
    >>> cid = cas.put(b'hello')
    >>> cid == Cid.of(b'hello'), cas.get(cid)
    (True, b'hello')
    >>> cas.missing([cid, Cid.of(b'other')]) == [Cid.of(b'other')]
    True
    """
    @staticmethod
    def memory() -> Cas:
        """A new, empty store in memory."""
    @staticmethod
    def sqlite(path: str | PathLike[str] | None = None) -> Cas:
        """A store in the SQLite database at `path`, or in memory without one."""
    @staticmethod
    def kv(kv: Kv, prefix: str | None = None) -> Cas:
        """A store of blocks in `kv`, each under `prefix/blake3/<digest>/bytes`.

        - Without a prefix, the key is `blake3/<digest>/bytes`.
        - The digest is the BLAKE3 digest in lowercase unpadded base32.
        - A trailing `/` on `prefix` is optional; an invalid prefix raises
          ValueError.
        """
    def put(self, data: _BytesLike) -> Cid:
        """Store `data` and return its raw BLAKE3 CID.

        Checks that the store keeps the data under that CID.
        """
    def put_value(self, value: _NativeInput) -> Cid:
        """Store the DRISL encoding of `value` and return its DRISL CID.

        The CID is hashed with BLAKE3, so it is `Value(value).cid()`.
        """
    def put_file(self, path: str | PathLike[str]) -> Cid:
        """Store the file at `path` and return its raw BLAKE3 CID.

        Checks that the store keeps the file under that CID.

        - Reads the file twice, to hash it and to store it, and stores only
          the bytes it hashed. If they changed in between it raises
          IntegrityError, and if the file got shorter OSError, storing nothing.
        - `Cas.kv` over `Kv.s3` streams the file; other stores read it into
          memory.
        """
    def put_from(self, cid: Cid, chunks: Iterable[_BytesLike], length: int) -> Cid:
        """Store `length` bytes from the iterable `chunks`, which must hash to `cid`, and return its raw BLAKE3 CID.

        For bytes that arrive in pieces, such as a request body, without
        holding them all at once.

        - `cid` must be a BLAKE3 CID of any codec; another hash raises
          ValueError.
        - `chunks` yields bytes or bytearray. It is read in order, and once
          more after `length` bytes, to check that it ends there.
        - Bytes that do not hash to `cid` raise IntegrityError; `chunks`
          holding fewer or more than `length` bytes raises ValueError; an
          exception from `chunks` propagates as it is. Each stores nothing.
          If cleanup also fails, raises StoreError with the source exception
          as its `__cause__` and the cleanup failure in its message.
        - `Cas.kv` over `Kv.s3` streams the bytes up in parts, holding about as
          many parts as upload at once; other stores hold them in memory.
        """
    def get(self, cid: Cid) -> bytes | None:
        """The bytes under `cid`, checked against it, or None if missing."""
    def get_file(self, cid: Cid, path: str | PathLike[str]) -> int | None:
        """Write the bytes under `cid` to the file at `path` and return their length.

        Returns None if missing.

        - Checks the bytes against `cid` in a temporary file beside `path`,
          which replaces `path` only once the check passes. A missing CID or a
          failure leaves `path` as it was.
        - `Cas.kv` over `Kv.s3` streams the bytes; other stores hold them in
          memory.
        """
    def size(self, cid: Cid) -> int | None:
        """The size in bytes of the value under `cid`, as the store vouches for it.

        None if missing. No store checks sizes yet.
        """
    def get_range(self, cid: Cid, start: int, length: int) -> bytes | None:
        """Up to `length` bytes from `start`, as the store vouches for them.

        None if missing. No store checks ranges yet.

        >>> from covalence import Cas
        >>> cas = Cas.memory()
        >>> cid = cas.put(b'hello')
        >>> cas.size(cid)
        5
        >>> cas.get_range(cid, 1, 3)
        b'ell'
        >>> cas.get_range(cid, 4, 10)
        b'o'
        >>> cas.get_range(cid, 5, 1)
        b''
        """
    def size_unchecked(self, cid: Cid) -> int | None:
        """The unchecked size in bytes of the value under `cid`, or None if missing."""
    def get_range_unchecked(self, cid: Cid, start: int, length: int) -> bytes | None:
        """Up to `length` bytes from `start`, unchecked, or None if missing."""
    def get_fact(self, cid: Cid) -> BlobFact | None:
        """The bytes under `cid` as a checked `BlobFact` for `cid`, or None if missing."""
    def has(self, cid: Cid) -> bool:
        """Whether the store holds bytes for `cid`, without checking their integrity.

        - `Cas.sqlite` checks its index, without reading the bytes.
        - `Cas.kv` asks its `Kv` whether the key exists (a HEAD on S3).
        - `Cas.memory` looks the bytes up in memory.
        """
    def missing(self, cids: Sequence[Cid]) -> list[Cid]:
        """Those of `cids` the store lacks, in order, as `has` finds them."""
    def info(self) -> _Native:
        """What the store says about itself, for debugging: None or a dict."""
    def stats(self) -> _Native:
        """Whatever the store counts, for debugging: None or a dict."""
