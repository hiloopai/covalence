from collections.abc import Iterable, Mapping, Sequence
from os import PathLike
from typing import Any, BinaryIO, Literal, Self, TypeAlias, final

from _typeshed import SupportsWrite as _SupportsWrite

from typing_extensions import disjoint_base

from . import Fact
from . import config as config

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
    "ChunkManifest",
    "SqliteDatabase",
    "SqliteConnection",
    "SqliteCursor",
    "Error",
    "IntegrityError",
    "SqliteError",
    "StoreError",
    "UnsupportedError",
    "config",
]

__version__: str

_BytesLike: TypeAlias = bytes | bytearray
_NativeInput: TypeAlias = None | bool | int | str | _BytesLike | Cid | Value | list[Any] | tuple[Any, ...] | dict[str, Any]
_Native: TypeAlias = None | bool | int | str | bytes | Cid | list["_Native"] | dict[str, "_Native"]
_SqlValue: TypeAlias = None | int | float | str | bytes
_SqlParameters: TypeAlias = Sequence[_SqlValue | bytearray] | Mapping[str, _SqlValue | bytearray]

class Error(Exception):
    """The base class of IntegrityError, SqliteError, StoreError and UnsupportedError."""
class IntegrityError(Error, ValueError):
    """Data does not match its CID."""
class SqliteError(Error):
    """SQLite refused a statement or a database."""
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
    @staticmethod
    def of_fileobj(file: BinaryIO, *, max_bytes: int, expected_length: int | None = None) -> Cid:
        """The raw BLAKE3 CID of a seekable binary file object, without storing it.

        - Reads once from offset zero, requesting at most 1 MiB at once,
          without holding the complete file in memory.
        - `max_bytes` bounds the accepted source length. If given,
          `expected_length` must not exceed it and must match the source exactly.
          A length violation raises ValueError. Reads at most one extra byte
          to check the end.
        - Calls `seek(0)` and `read(n)`. `seek` must return zero; `read` must
          return bytes or bytearray, at most `n` bytes. Short reads are allowed;
          empty bytes end the source. Source exceptions propagate unchanged.
        - Never closes the source. On success its position is at EOF.
          The caller must keep it unchanged and exclusively owned until this
          returns. This does not create a filesystem snapshot.
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
    def s3(location: str, prefix: str | None = None, *, region: str | None = None, profile: str | None = None, concurrency: int | None = None, upload_buffer_bytes: int | None = None) -> Kv:
        """A store in S3, configured as the AWS SDKs are.

        `location` is an `s3://bucket/prefix/` URL, or a bucket name with
        `prefix`. `region` and `profile` override the SDK configuration.
        `concurrency` is how many parts or read ranges of one value move at
        once (16 by default; 0 counts as 1). Upload parts start at 8 MiB and
        grow when needed to keep to 10,000 parts; read ranges stay at 8 MiB.

        `upload_buffer_bytes` optionally caps aggregate upload payload
        reservations across calls sharing this store. An upload reserves the
        smaller of its length and its part size times its worker count before
        reading its source, allocating parts or sending requests. Insufficient
        capacity raises StoreError immediately; zero admits only empty uploads.
        Reservations last through retries and cleanup. Caller-owned sources,
        read buffers, allocator overhead, HTTP/TLS buffers and metadata are
        additional memory; this is not an RSS limit. Already owned values also
        reserve their planned payload, though their parts share that allocation.

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
    def kv(kv: Kv, prefix: str | None = None, *, compression: Literal["identity", "gzip", "deflate", "zstd", "br"] | None = "identity", level: int = 3) -> Cas:
        """A store of blocks in `kv`, with optional compression.

        - `compression="identity"` (the default) and `compression=None` store
          unchanged bytes under `prefix/blake3/<digest>/bytes`; without a prefix,
          the key is `blake3/<digest>/bytes`.
        - The digest is the BLAKE3 digest in lowercase unpadded base32.
        - A trailing `/` on `prefix` is optional; an invalid prefix raises
          ValueError.
        - `compression="zstd"` stores Zstandard frames under `bytes.zst`,
          keeping CIDs of the uncompressed bytes. `level` sets its compression
          level; zero selects the default and other levels clamp to the supported
          range.
        - `compression="deflate"` stores RFC 1950 zlib-wrapped DEFLATE under
          `bytes.deflate`, compatible with Python `zlib` and the browser's
          `CompressionStream("deflate")`. This is not raw DEFLATE.
        - `compression="gzip"` stores RFC 1952 gzip under `bytes.gz`, compatible
          with Python `gzip` and the browser's `CompressionStream("gzip")`.
        - `compression="br"` stores RFC 7932 Brotli under `bytes.br`. Its HTTP
          token is `br`; the Compression Streams API calls it `"brotli"`, where
          supported. Brotli levels clamp to 0 through 11; zero is the fastest.
        - Deflate and gzip levels clamp to 0 through 9; zero disables compression
          while retaining the wrapper. `level` defaults to 3 for every codec.
          Other compression names raise ValueError. Each codec has separate keys.
          Python wheels include all codecs; using one disabled in a custom Rust
          build raises UnsupportedError.
        - Deflate and gzip reads require one stream or member, validate checksums
          and reject trailing data, following the browser compression profile.
          Brotli reads require one complete stream and reject trailing data.
        - Compressed streaming writes stage a complete frame on temporary disk
          before storage. Reads stream through the backend; sizes and ranges
          decode the complete frame, retaining only the requested output.
        - Never replaces a block `kv` already holds: on `Kv.s3` each write is
          conditional, so concurrent writers of one block race safely.
        """
    def put(self, data: _BytesLike) -> Cid:
        """Store `data` and return its raw BLAKE3 CID.

        Checks that the store keeps the data under that CID.
        """
    def put_many(self, values: Sequence[_BytesLike]) -> list[Cid]:
        """Store each of `values` and return their raw BLAKE3 CIDs, in order.

        Checks that the store keeps each under its CID. A SQLite store writes
        them in one transaction; others store them one at a time. On an
        error, some may be stored.
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
    def put_fileobj(self, file: BinaryIO, *, max_bytes: int, expected_length: int | None = None) -> Cid:
        """Store a seekable binary file object and return its raw BLAKE3 CID.

        - Reads from offset zero twice, to hash and store the same source.
          The caller must keep the source unchanged and exclusively owned
          until this returns. This does not create a filesystem snapshot.
        - `max_bytes` bounds the accepted source length. If given,
          `expected_length` must not exceed it and must match the source exactly.
          A length violation raises ValueError before a new block is published.
          Reads at most one extra byte per pass to check the end.
        - Calls `seek(0)` and `read(n)`, requesting at most 1 MiB at once.
          `seek` must return zero; `read` must return bytes or bytearray,
          at most `n` bytes. Short reads are allowed; empty bytes end the source.
        - Checks both complete passes, even if the store reuses a block.
          Changed bytes raise IntegrityError. Reusing a block does not verify
          the bytes already stored there; use a checked read when that is needed.
        - Never closes the source. On success its position is at EOF.
          Source exceptions propagate unchanged; if storage cleanup also fails,
          raises StoreError with the source exception as its `__cause__`.
        - The byte limit does not bound backend memory. `Cas.kv` over `Kv.s3`
          streams with its configured concurrency; other stores may hold the
          complete block in memory.
        """
    def put_from(self, cid: Cid, chunks: Iterable[_BytesLike], length: int) -> Cid:
        """Store a block identified by `cid` from `chunks` and return its raw BLAKE3 CID.

        For bytes that arrive in pieces, such as a request body, without
        holding them all at once.

        - `cid` must be a BLAKE3 CID of any codec; another hash raises
          ValueError.
        - `chunks` yields bytes or bytearray. Before a new block is stored,
          its bytes must match `length` and `cid`, and the iterable must end there.
        - A store may return an existing block without consuming all of
          `chunks`. Unread bytes and source errors are not checked.
        - While consuming input, a hash mismatch raises IntegrityError; too
          few or too many bytes raises ValueError; a source exception propagates
          unchanged. These failures publish no new block.
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
    def get_to(self, cid: Cid, writer: _SupportsWrite[bytes], *, max_bytes: int, expected_length: int | None = None) -> int | None:
        """Write a block to a binary file object, checking its CID and byte limits.

        Returns the written length, or None if missing.

        - `max_bytes` bounds bytes accepted by `writer.write`. If given,
          `expected_length` must not exceed it and must match the block exactly.
          A length violation raises ValueError.
        - Calls `write(bytes)` with at most 1 MiB at once. It must return an
          integer from one to the offered length. Short writes are allowed.
          Never seeks, flushes or closes the writer.
        - A successful return checks all bytes against `cid`. On failure the
          writer may contain a partial or unchecked block; use private staging
          and publish it only after success. Writer exceptions propagate unchanged.
          If backend cleanup also fails, raises StoreError with the writer
          exception as its `__cause__`.
        - The byte limit bounds output, not backend memory. `Cas.kv` over
          `Kv.s3` streams with its configured concurrency; other stores may
          fetch the complete block before writing any bytes.
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

@final
class ChunkManifest:
    """The digests of the chunks of some bytes, to check ranges of the bytes with.

    A CID can be checked only with all of its bytes at hand. A manifest
    splits the bytes into chunks of `chunk_size` bytes and keeps the digest of
    each, so any chunk can be checked alone. `SqliteDatabase.open` takes one
    as `verify`, to read a database by range from a store it does not trust.

    A manifest vouches for the bytes, so it must come from somewhere trusted:
    make it from bytes that are, or `load` it by a CID that is.

    >>> from covalence import Cas, ChunkManifest
    >>> manifest = ChunkManifest.of(bytes(100_000), chunk_size=65536)
    >>> manifest
    ChunkManifest(size=100000, chunk_size=65536, hash='blake3')
    >>> cas = Cas.memory()
    >>> ChunkManifest.load(cas, manifest.put(cas)).chunks
    2
    """
    @staticmethod
    def of(data: _BytesLike, *, chunk_size: int = 65536, hash: Hash | str | None = None) -> ChunkManifest:
        """The manifest of `data` in chunks of `chunk_size` bytes.

        - `chunk_size` is from 1 to 2**30; another size raises ValueError.
          Reading with the manifest fetches and checks whole chunks.
        - `hash` is the hash function of the digests: "blake3" (the default),
          "sha256", "sha512" or "sha1". Another raises UnsupportedError.
        """
    @staticmethod
    def of_file(path: str | PathLike[str], *, chunk_size: int = 65536, hash: Hash | str | None = None) -> ChunkManifest:
        """The manifest of the file at `path`, as `of` makes it.

        The file is read in pieces, so it need not fit in memory. A file that
        cannot be read raises OSError.
        """
    @staticmethod
    def load(cas: Cas, cid: Cid) -> ChunkManifest | None:
        """The manifest that `cas` holds under `cid`, or None if missing.

        - Bytes that do not hash to `cid` raise IntegrityError.
        - Bytes that are not a manifest raise ValueError.
        """
    def put(self, cas: Cas) -> Cid:
        """Store the manifest in `cas` and return the CID that `load` takes.

        The manifest is stored as DRISL: a map of the hash function's code,
        the size, the chunk size and the digests.
        """
    @property
    def size(self) -> int:
        """The size in bytes of the content the manifest is of."""
    @property
    def chunk_size(self) -> int:
        """The size in bytes of each chunk but the last, which may be shorter."""
    @property
    def chunks(self) -> int:
        """The number of chunks."""
    @property
    def hash(self) -> Hash:
        """The hash function of the digests."""

@final
class SqliteDatabase:
    """A SQLite database held in a store, opened read-only.

    `SqliteDatabase.open` makes the bytes under a CID ready for SQLite, and
    `connect` gives connections that run SQL on them. The database is the
    bytes of its CID and never changes: every write to it raises SqliteError.

    >>> import sqlite3
    >>> from covalence import Cas, SqliteDatabase
    >>> source = sqlite3.connect(':memory:')
    >>> _ = source.execute('CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)')
    >>> _ = source.execute("INSERT INTO notes (body) VALUES ('first'), ('second')")
    >>> source.commit()
    >>> cas = Cas.memory()
    >>> cid = cas.put(source.serialize())
    >>> source.close()
    >>> database = SqliteDatabase.open(cas, cid)
    >>> with database.connect() as connection:
    ...     connection.execute('SELECT id, body FROM notes WHERE id > ?', (1,)).fetchall()
    [(2, 'second')]
    """
    @staticmethod
    def open(cas: Cas, cid: Cid, *, verify: Literal["download", "trusted-ranges", "blake3-ranges"] | ChunkManifest | None = None, max_bytes: int | None = None, block_size: int | None = None, fetch: Literal["read-ahead", "aligned", "exact"] = "read-ahead", fetch_bytes: int | None = None, cache: Literal["memory", "file", "none"] = "memory", cache_bytes: int | None = None, dir: str | PathLike[str] | None = None) -> SqliteDatabase | None:
        """Open the database whose bytes `cas` holds under `cid`, or None if missing.

        `verify` says how far the store is trusted:

        - "download", the default, does not trust it. All the bytes are
          downloaded to a private temporary file, and must hash to `cid` or
          IntegrityError is raised. At most `max_bytes` bytes are accepted,
          1 GiB by default; a larger database raises ValueError. A `cid` whose
          hash function is not BLAKE3, SHA-256 or SHA-1 raises UnsupportedError.
        - "trusted-ranges" reads byte ranges as queries need them, and trusts
          them: nothing checks the bytes against `cid`. Use it only for a
          store trusted as much as a local file.
        - A `ChunkManifest` reads byte ranges and checks every chunk against
          the manifest before SQLite sees it, so the store need not be
          trusted. What is read is the content the manifest is of; `cid` only
          says where to fetch. A stored size that is not the manifest's raises
          IntegrityError.
        - "blake3-ranges", ranges checked against a BLAKE3 `cid` itself, is
          not supported yet. It raises UnsupportedError before the store is
          asked anything, and never falls back to another mode.

        The other options matter only when ranges are read:

        - `block_size` is the size in bytes of the blocks that are fetched and
          cached: 4096 by default, from 1 to 2**30, or ValueError is raised.
          With a manifest the blocks are its chunks.
        - `fetch` sizes the request when a read misses the cache. "read-ahead",
          the default, fetches 64 KiB where reads begin, and four times as
          much each time reads go on from there, up to `fetch_bytes` (4 MiB by
          default). "aligned" fetches whole windows of `fetch_bytes` bytes,
          which it needs. "exact" fetches only what is read.
        - `cache` keeps fetched blocks for every connection: "memory" (the
          default), "file" for a private temporary file, or "none". It holds
          up to `cache_bytes` bytes, 32 MiB by default. A cache smaller than
          the database keeps a request to a quarter of what it holds, unless
          one read needs more: with "none", only what is read is fetched.

        `dir` is the directory of the local files, the download and the file
        cache: the system's temporary directory by default. The files are
        private: they have no name on Unix, and on Windows nothing else can
        read or write them. The system removes them once the database and its
        connections are gone.

        A store that fails raises StoreError. Whether the bytes are a SQLite
        database is found by `connect`.
        """
    def connect(self) -> SqliteConnection:
        """A new read-only connection to the database.

        - Connections share the database's cache, and each has SQLite's own
          page cache.
        - A connection opens no other database: `ATTACH` and `VACUUM INTO`
          raise SqliteError.
        - Bytes that do not start as a SQLite database does raise SqliteError;
          other damage raises it from the statement that reads it.
        - A read that fails raises its cause: IntegrityError for bytes that
          were refused, StoreError for a store that failed.
        """
    @property
    def cid(self) -> Cid:
        """The CID the database was opened with."""
    @property
    def size(self) -> int:
        """The size of the database in bytes."""
    def stats(self) -> dict[str, int]:
        """Counts of what has been read so far, by every connection.

        - "reads" and "read_bytes": the reads SQLite made of the database, and
          their bytes.
        - "fetches" and "fetched_bytes": the requests to the store for the
          database's bytes, and the bytes it gave. A download is one fetch.

        `reads - fetches` reads were answered by the cache or the local file.
        """
    def set_fetch(self, fetch: Literal["read-ahead", "aligned", "exact"], fetch_bytes: int | None = None) -> None:
        """Size requests from now on with `fetch` and `fetch_bytes`, as in `open`.

        This holds for every connection, and the cache keeps its blocks. It
        does nothing for a database that was downloaded.
        """

@final
class SqliteConnection:
    """A read-only connection to a `SqliteDatabase`, from `SqliteDatabase.connect`.

    `execute`, `cursor` and `close` are those of a DB-API connection (PEP
    249). There is nothing to commit: the database cannot change.

    - A connection can be used from any thread; it runs one statement at a
      time.
    - As a context manager it closes itself on exit.

    >>> import sqlite3
    >>> from covalence import Cas, SqliteDatabase
    >>> source = sqlite3.connect(':memory:')
    >>> _ = source.execute('CREATE TABLE t AS SELECT 1 AS x')
    >>> source.commit()
    >>> cas = Cas.memory()
    >>> connection = SqliteDatabase.open(cas, cas.put(source.serialize())).connect()
    >>> source.close()
    >>> list(connection.execute('SELECT x, :y FROM t', {'y': 'two'}))
    [(1, 'two')]
    >>> connection.close()
    """
    def execute(self, sql: str, parameters: _SqlParameters | None = None) -> SqliteCursor:
        """Run one SQL statement and return a cursor over its rows.

        - `parameters` fills the statement's placeholders: a sequence such as
          a tuple or a list for `?`, or a mapping such as a dict for `:name`,
          `@name` and `$name`, keyed by the name without its first character.
          Values are None, bool, int, float, str, bytes or bytearray. Another
          type raises TypeError, as do str and bytes in place of a sequence;
          an int outside 64 bits raises OverflowError.
        - A wrong number of values, a name without a value, more than one
          statement and no statement raise ValueError.
        - The statement runs to its end here. Its rows stay in memory until
          they are fetched: tuples of None, int, float, str and bytes.
        - SQL that SQLite refuses raises SqliteError, as do every write to
          the database, `ATTACH` and `VACUUM INTO`. Temporary tables can be
          written.
        - A read that fails raises its cause: IntegrityError for bytes that
          were refused, StoreError for a store that failed.
        """
    def cursor(self) -> SqliteCursor:
        """A new cursor, with no rows until it runs a statement."""
    def close(self) -> None:
        """Close the connection. Closing again does nothing.

        Statements raise SqliteError from then on. Cursors keep the rows they
        have.
        """
    def __enter__(self) -> Self:
        """The connection itself, for `with`."""
    def __exit__(self, *_exception: object) -> None:
        """Close the connection at the end of `with`, whatever was raised."""

@final
class SqliteCursor:
    """The rows of a statement, from `SqliteConnection.execute`.

    `description` and the `fetch` methods are those of a DB-API cursor (PEP
    249). Iterating a cursor gives its remaining rows.

    >>> import sqlite3
    >>> from covalence import Cas, SqliteDatabase
    >>> source = sqlite3.connect(':memory:')
    >>> _ = source.execute('CREATE TABLE t AS SELECT 1 AS x UNION SELECT 2 UNION SELECT 3')
    >>> source.commit()
    >>> cas = Cas.memory()
    >>> connection = SqliteDatabase.open(cas, cas.put(source.serialize())).connect()
    >>> source.close()
    >>> cursor = connection.execute('SELECT x, x * 1.5 AS y FROM t ORDER BY x')
    >>> [column[0] for column in cursor.description]
    ['x', 'y']
    >>> cursor.fetchone(), cursor.fetchall()
    ((1, 1.5), [(2, 3.0), (3, 4.5)])
    """
    def execute(self, sql: str, parameters: _SqlParameters | None = None) -> Self:
        """Run one SQL statement and return this cursor over its rows.

        The statement runs as in `SqliteConnection.execute`. The rows of the
        cursor's earlier statement are dropped, unless this one raises.
        """
    @property
    def description(self) -> tuple[tuple[str, None, None, None, None, None, None], ...] | None:
        """The columns of the last statement, or None if it has none.

        Each column is a tuple of its name and six None, as DB-API has it.
        It is None before any statement too.
        """
    def fetchone(self) -> tuple[_SqlValue, ...] | None:
        """The next row, or None when there are no more."""
    def fetchmany(self, size: int = 1) -> list[tuple[_SqlValue, ...]]:
        """The next `size` rows, or as many as are left."""
    def fetchall(self) -> list[tuple[_SqlValue, ...]]:
        """All the rows that are left."""
    def close(self) -> None:
        """Close the cursor and drop its rows. Closing again does nothing.

        The cursor raises SqliteError from then on.
        """
    def __iter__(self) -> Self: ...
    def __next__(self) -> tuple[_SqlValue, ...]: ...
