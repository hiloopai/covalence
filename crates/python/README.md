# Covalence for Python

Content identifiers (CIDs), DASL values, and content-addressed and key-value
stores, from Python.

`covalence.config` also provides application directories and TOML configuration.

```pycon
>>> from covalence import Cid, Value
>>> cid = Cid.of(b"payload", "sha256")
>>> Cid(str(cid)) == Cid(bytes(cid)) == cid
True
>>> cid.hash, cid.codec
(Hash('sha256'), Codec('raw'))
>>> value = Value({"a": cid})
>>> Value.decode(value.encode()) == value
True
>>> value.links() == [cid]
True

```

## Install

```sh
pip install covalence==0.1.0.dev6
```

- Pre-releases need an exact pin like this one, or `pip install --pre`.
- Wheels cover CPython 3.11 and later on Linux (x86_64 and aarch64, glibc
  2.28 or later) and macOS (arm64).
- Elsewhere pip builds the sdist, which needs Rust 1.98 or later.
- Each wheel and the sdist include `THIRD-PARTY-LICENSES.md`, the licenses of
  the Rust crates compiled into the package.

## Terms

- A **CID** ([content identifier](https://github.com/multiformats/cid)) names
  bytes by their hash. It holds a codec, which says how the bytes are
  encoded, a hash function, and the digest.
- A **multicodec** code is a number from the
  [multicodec table](https://github.com/multiformats/multicodec) that names a
  codec or a hash function.
- [**DASL**](https://dasl.ing) is a small set of specifications for CIDs and
  data. [**DRISL**](https://dasl.ing/drisl.html) is its deterministic CBOR
  encoding, which CIDs label with the `dag-cbor` codec.
- A **CAS** (content-addressed store) keeps bytes and finds them by their
  CIDs.

## Configuration

`covalence.config` selects application directories and reads TOML using the
same Rust implementation as `covalence-lib-config`.

```pycon
>>> from covalence import config
>>> str(config.config_dir("example", override_dir="settings"))
'settings'
>>> config.parse('enabled = true\nworkers = 2')
{'enabled': True, 'workers': 2}

```

- `config_dir`, `cache_dir`, `data_dir`, `state_dir` and `runtime_dir` return
  `pathlib.Path`. Each takes an application name and an optional keyword
  `override_dir`. An override is returned unchanged, including relative paths.
  Without an override, names must be a single non-empty directory component;
  `.`, `..`, separators, colons and NUL raise `ValueError`.
- Unix, including macOS, uses `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`,
  `XDG_DATA_HOME`, `XDG_STATE_HOME` and `XDG_RUNTIME_DIR`, respectively. Unset,
  empty and relative variables fall back to `~/.config`, `~/.cache`,
  `~/.local/share` and `~/.local/state`. Runtime falls back to state.
- Windows uses roaming application data for config, data, state and runtime,
  and local application data for cache. An unavailable home raises `OSError`.
- Directory lookups do not create paths or enforce permissions. A runtime
  fallback does not have session lifetime or permission guarantees.
- `parse(text)` returns a dict. `read(path)` reads a UTF-8 file and returns
  `{}` if it is missing, including a missing parent. `read_required(path)`
  raises `FileNotFoundError` instead, suitable for a user-supplied config path.
  Invalid TOML raises `ValueError`; other read failures raise `OSError`.
- TOML tables, arrays and scalars become dicts, lists, str, bool, signed 64-bit
  int and float. Date/time values become `datetime.datetime`, `datetime.date`
  or `datetime.time`, retaining offsets and truncating fractions to
  microseconds. Integers outside the signed 64-bit range, and dates Python
  cannot represent, raise `ValueError`.
- Files are read in full. This API adds no file-size limit, schema validation,
  interpolation, environment expansion or layer merging; callers validate
  their configuration fields. Normal filesystem symlink semantics apply.

## CIDs, hashes and codecs

- `Cid` is any CIDv1 with a digest of at most 64 bytes. `Cid(x)` takes text
  or binary; `Cid.from_text` and `Cid.from_bytes` take only one.
- Text is `b` and lowercase base32 only. Invalid CIDs raise `ValueError`.
- `Hash` and `Codec` wrap multicodec codes. `from_code` takes an int, and
  the constructors take names:
  - hashes: the multicodec names `"sha1"`, `"sha2-256"`, `"sha2-512"` and
    `"blake3"`, or the `hashlib` names `"sha256"` and `"sha512"`;
  - codecs: `"raw"`, `"dag-cbor"` (or `"drisl"`) and `"git-raw"`.
- `Cid.of` hashes with `"blake3"` and `"raw"` by default.
- `Cid.of_file(path)` is the raw BLAKE3 `Cid` of a file, as `Cas.put_file`
  returns it, without storing it or reading the file into memory.
- `Cid.of_fileobj(file, max_bytes=..., expected_length=...)` hashes an
  already-open, seekable binary file object once from offset zero, without
  storing it or holding it in memory. It uses the same bounded reader as
  `Cas.put_fileobj`, and leaves the caller's source open at EOF on success.
  Keep that source unchanged and exclusively owned until it returns.

## Values

- `Value` is a DASL value. `Value(...)` takes None, bool, int, str, bytes,
  `Cid`, `Value`, lists, tuples and dicts with str keys. It checks the same
  rules as `decode`:
  - links must be DASL CIDs;
  - integers outside i64, floats and nesting deeper than 126 raise
    `ValueError`.

  Other types raise `TypeError`.
- `Value.to_json` and `Value.from_json` use the ATProto JSON form of
  `covalence-lib-dasl`.
- `Value.cid()` is the DRISL BLAKE3 CID of the value's encoding.

## Key-value stores

- `Kv` is a byte key-value store:
  - `Kv.memory()` makes one in memory;
  - `Kv.s3("s3://bucket/prefix/")` (or `Kv.s3("bucket", "prefix")`) makes
    one in S3.
- S3 is configured as the AWS SDKs are:
  - the region is `region=`, else from `AWS_REGION`, the profile or EC2
    instance metadata, which can take a moment off EC2;
  - the endpoint is from `AWS_ENDPOINT_URL` or the profile's `endpoint_url`,
    else AWS's for the region, so other S3 services need `AWS_ENDPOINT_URL`;
  - credentials come from the SDKs' default chain: environment variables,
    profile files, web identity, ECS or EKS Pod Identity, or EC2 instance
    roles. They are fetched at the first request; missing or failed
    credentials raise `StoreError` then.
  - `profile=` picks the profile instead of `AWS_PROFILE` or `default`; its
    credentials win over environment variables.
- `get` returns `bytes` or `None`.
- `put` replaces any value under the key.
- `exists` says whether there is a value without reading it (a HEAD on S3).
- `Kv.s3` raises `ValueError` for an invalid location, a URL with a prefix
  argument too, or no region.
- Without `s3:ListBucket`, S3 answers 403 for a missing key, so reading one
  raises `StoreError` instead of returning `None` or `False`, here and
  through `Cas.kv`.
- S3 keys must be UTF-8 with non-empty `/`-separated segments other than `.`
  and `..`; other keys raise `StoreError`.
- On S3, values over 8 MiB go up as multipart uploads, and values come down
  in ranged GETs of 8 MiB, 16 parts or ranges at a time. Upload parts grow
  when needed to stay within 10,000 parts. Covalence checks
  [S3's limits](https://docs.aws.amazon.com/AmazonS3/latest/userguide/qfacts.html)
  before upload: at most 10,000 parts of 5 GiB each, subject to the platform's
  allocation limit.
- `Kv.s3(..., upload_buffer_bytes=n)` caps aggregate upload payload
  reservations across calls sharing that store. An upload reserves the smaller
  of its length and its part size times its worker count before allocating
  parts, reading its upload source or making requests. Insufficient capacity
  raises `StoreError` immediately; zero admits only empty uploads. The default
  is no budget. Reservations last through retries and cleanup.
- This budget excludes caller-owned sources, read buffers, allocator overhead,
  HTTP/TLS buffers and request metadata; it is not an RSS limit. Owned byte
  values still reserve their planned payload even though their parts share the
  original allocation. File helpers may hash their source before upload
  admission. A larger legal object can need a larger budget because its parts
  grow; rejection does not consume its upload source or create an upload.

## Content-addressed stores

```pycon
>>> from covalence import Cas
>>> cas = Cas.memory()
>>> cid = cas.put(b"hello")
>>> cas.size(cid)
5
>>> cas.get_range(cid, 1, 3), cas.get_range(cid, 4, 10)
(b'ell', b'o')

```

- `Cas` is a CAS. Make one with:
  - `Cas.memory()`;
  - `Cas.sqlite(path)`, in memory without a path;
  - `Cas.kv(kv, prefix)`, keeping blocks in a `Kv` under
    `prefix/blake3/<digest>/bytes`.
- `put` returns the raw BLAKE3 `Cid`, after checking that the store kept the
  data under it.
- `Cas.kv` never replaces a block its `Kv` already holds: on `Kv.s3` each
  write is conditional (`If-None-Match: *`), so concurrent writers of one
  block race safely and the first one's bytes stay.
- `put_value(value)` stores the DRISL encoding of a `Value` (or what
  `Value(...)` takes) and returns its DRISL CID, which is `value.cid()`.
- `get` returns the bytes and `get_fact` a `BlobFact`, both checked against
  the CID; a failed check raises `IntegrityError`.
- `has` and `missing(cids)` say which CIDs the store holds bytes for,
  without checking their integrity:
  - `Cas.sqlite` checks its index, without reading the bytes;
  - `Cas.kv` asks its `Kv` whether the key exists (a HEAD on S3);
  - `Cas.memory` looks the bytes up in memory.
- `info` and `stats` return native values.
- `size(cid)` and `get_range(cid, start, length)` return a byte count, or up
  to `length` bytes from `start`, as the store vouches for them.
  `size_unchecked` and `get_range_unchecked` skip any checking. No store
  checks sizes or ranges yet.
  - Missing values return `None`, even for a zero `length`.
  - Ranges are clipped at the end.
  - Stores without native ranges fetch the whole value and slice it.

### Files and streams

- `Cas.put_file(path)` stores a file and returns its raw BLAKE3 `Cid`, as
  `put` does.
- `Cas.put_from(cid, chunks, length)` stores a block from an iterable of
  bytes, such as a request body, and returns its raw BLAKE3 `Cid`. Before a
  new block is stored, its bytes must match `length` and the BLAKE3 `cid`,
  and `chunks` must end there. A store may return an existing block without
  consuming all the input; unread bytes and source errors are not checked.
  While consuming input, a hash mismatch (`IntegrityError`), too few or too
  many bytes (`ValueError`), or a source exception publish no new block.
  Source exceptions propagate unchanged unless cleanup also fails, in which
  case `StoreError` reports that failure with the source as its `__cause__`.
- `Cas.get_file(cid, path)` writes the block to a file and returns its
  length, or `None` if missing.
- `Cas.put_fileobj(file, max_bytes=..., expected_length=...)` stores a
  caller-owned, seekable binary file object. It reads from offset zero twice,
  checks the complete source on both passes even when deduplicating, and
  leaves it open at EOF on success. The caller must keep the source unchanged
  and exclusively owned until it returns; the method creates no snapshot.
  `max_bytes` caps the accepted source length; `expected_length`, if given,
  must match it exactly. A length violation raises `ValueError` before a new
  block is published. A pass may read one extra byte to check the end.
- `Cas.get_to(cid, writer, max_bytes=..., expected_length=...)` writes to a
  caller-owned binary file object and returns the length, or `None` if missing.
  It checks all bytes against the CID, caps the bytes the writer accepts, and
  checks the optional exact length. It handles short writes and never seeks,
  flushes or closes the writer. A failure may leave partial or unchecked bytes
  there: write to private staging and publish only after a successful return.
- These limits govern source length and output bytes, not the memory a backend
  uses before reading or writing. Python callback transfers are at most 1 MiB.
- `put_file`, `put_fileobj`, `put_from`, `get_file` and `get_to` stream on `Cas.kv` over `Kv.s3`,
  holding up to 16 part payloads at a time, or as many as
  `Kv.s3(..., concurrency=n)` says; blocks over 78 GiB go up in larger parts.
  Upload parts start at 8 MiB; read ranges stay at 8 MiB. Set
  `upload_buffer_bytes=` to enforce a shared upload payload budget.
  Other stores, including `Cas.kv(Kv.memory())`, hold the block in memory.
- A create-only write can reuse a block without checking its stored bytes.
  Returning a CID does not freshly verify an existing block: use a checked
  read, such as `get_to`, when that guarantee is needed.
- `put_file` reads the file twice, to hash it and to store it, and stores the
  bytes it hashed. If they changed in between it raises `IntegrityError`,
  and if the file got shorter `OSError`, storing nothing either way. Bytes
  appended in between are not stored.
- `get_file` checks the bytes against the CID in a private temporary file
  beside `path`. That file replaces `path` only once the check passes, so a
  missing block or a failed check (`IntegrityError`) leaves `path` as it
  was. The new file has the permissions of the file it replaces, or those
  the umask gives a new file.

Use an already-open file without exposing its path:

```pycon
>>> import io
>>> source = io.BytesIO(b"hello")
>>> cid = cas.put_fileobj(source, max_bytes=5, expected_length=5)
>>> sink = io.BytesIO()
>>> cas.get_to(cid, sink, max_bytes=5, expected_length=5)
5
>>> sink.getvalue()
b'hello'
>>> Cid.of_fileobj(sink, max_bytes=5, expected_length=5) == cid
True

```

An integer file descriptor can be wrapped with `os.fdopen(os.dup(fd), "wb",
buffering=0)` for downloads, or `"rb"` for uploads. Close that wrapper when
finished; the original descriptor remains open. Duplicates share a file
position, so the caller must also keep the original descriptor exclusively
owned during the operation.

## Blobs and facts

- `Blob` is bytes read whole or by range:
  - `Blob(data)` holds a snapshot of `data` in memory, and `Blob(blob)`
    shares `blob`'s source;
  - `size()`, `read_range(start, length)` and `read_all()` are not checked,
    and ranges are clipped at the end;
  - a `read_range` returning more than `length` bytes raises `StoreError`;
  - `slice(start, length)` is a blob over the same bytes;
  - copies keep their class and share the source.
- `Blob` can be subclassed, but its methods are not virtual: an override in
  Python changes only what Python callers see, while `fact` and `slice` read
  the Rust blob underneath.
- `Blob.fact(cid)` reads the blob once and returns a `BlobFact` for exactly
  `cid`, checked, or `None` if the bytes do not hash to it:
  - it checks SHA-1, SHA-256 and BLAKE3 CIDs;
  - a digest over 32 bytes, such as SHA-512's, raises `ValueError`;
  - another hash, or a digest length the hash does not give, raises
    `UnsupportedError`.
- `Fact` is an interface for bytes and the `Cid` they claim to hash to: `cid`
  and `value()`, which decodes the bytes as the CID's codec says. `BlobFact`
  is checked; `isinstance(x, Fact)` alone proves nothing.
- `BlobFact` is a `Fact` and a `Blob` of its bytes, with `data` too. It
  compares and hashes by fact, and cannot be subclassed.
- `BlobFact.of(data)` hashes `data` with BLAKE3. A fact asked for its own
  hash and digest under another codec is relabelled, not hashed again.

## Errors

- Bad input raises `ValueError` or `TypeError`, or `OverflowError` for an int
  below 0 or of 2**64 or more where a size, offset or code is expected.
- Other failures raise a subclass of `covalence.Error`:
  - `IntegrityError` (also a `ValueError`) when data does not match its CID;
  - `StoreError` (also an `OSError`) when a store's storage fails;
  - `UnsupportedError` (also a `NotImplementedError`) for an unsupported
    operation, such as hashing with an unknown function.
- A local file that cannot be read or written raises `OSError`.

## Developing

- `glu build` builds the `covalence` wheel into `target/glu/wheels/`.
- `glu test` installs it in a fresh venv and runs `tests/`.
- Classes are named after the Rust types they wrap.

## Documentation

The compiled module is the private `covalence._native`; users import
everything from `covalence`. PyO3's typing guide recommends a hand-written
stub; this package writes its stub by hand and checks it against `__doc__`:

- Rust `///` comments are the source. PyO3 makes them `__doc__`, with
  signatures from the Rust arguments. Python code in `covalence/` keeps its
  own docstrings.
- `_native.pyi` repeats each docstring word for word, since editors read the
  stub and cannot see a compiled module's `__doc__`. `tests/test_docs.py`
  fails if the two differ, or if the stub lacks a public member.
- Classes and exceptions set their module to `covalence` (`#[pyclass(module
  = "covalence")]`), so they show as `covalence.Name`, never
  `covalence._native.Name`. `_native` itself is not documented.
- Every class, method, property and exception in `covalence.__all__` has a
  docstring. Examples are `>>>` doctests. `tests/test_docs.py` runs them,
  and this README's, with `doctest.DocTestSuite` and `DocFileSuite`.
- [`docs/python`](https://github.com/hiloopai/covalence/tree/main/docs/python)
  builds the docstrings into HTML with Sphinx, for Read the Docs.

## Prebuilt wheels

`.github/workflows/python-wheels.yml` builds abi3 wheels (CPython 3.11 and
later) for manylinux_2_28 x86_64 and aarch64 and macOS arm64, and an sdist,
and runs `tests/` against each wheel. Pull requests and pushes keep them as
workflow artifacts. A release tag must match the workspace version, such as
`0.1.0-dev.6`; the files carry its PEP 440 form, `0.1.0.dev6`:

- `v<version>` attaches them to that GitHub release. In the public
  `hiloopai/covalence` repository it also publishes them to PyPI, and
  `.github/workflows/crates.yml` publishes the crates to crates.io.
- `python-v<version>` only attaches them to the GitHub release.

To publish `0.1.0-dev.6`:

```sh
git tag v0.1.0-dev.6 && git push origin v0.1.0-dev.6
```
