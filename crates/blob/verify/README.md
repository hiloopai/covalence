# covalence-blob-verify

Byte streams measured or checked against an expected size and digest, in
constant memory.

A caller that knows how long a stream should be and what it should hash to
wraps the stream's reader or writer. The wrapper passes bytes through, hashes
them as they go, and fails the stream if it is too long, too short or has
another digest.

## Terms

- A digest is the 32-byte output of a hash function over the whole stream:
  [SHA-256](https://csrc.nist.gov/pubs/fips/180-4/upd1/final) or
  [BLAKE3](https://github.com/BLAKE3-team/BLAKE3-specs), as named by
  `HashFn` from `covalence-lib-cid`.
- A [CID](https://dasl.ing/cid.html) (content identifier) names bytes by a
  hash function, digest and codec. A raw CID does not assign an encoding to
  the bytes.
- `Expected` is a size in bytes, a hash function and a digest.
- `Verified` is proof that a stream had exactly the expected size and digest.
  Only a check that has seen every byte makes one.

## Pieces

- `Verifier`: the check alone, with no I/O. Feed it bytes in order, then ask
  it to finish.
- `VerifyingReader`: reads exactly the expected bytes from another reader,
  and withholds the last byte until they pass the check.
- `VerifyingWriter`: passes bytes on to another writer, refusing any past
  the expected size; the caller checks once every byte is written.
- `Cancel`: a flag that stops a reader or writer before its next read or
  write.

Readers and writers implement `std::io::Read` and `Write` and, with the
default `tokio` feature, Tokio's `AsyncRead` and `AsyncWrite`.

## Measuring without an expected digest

Use `DigestAccumulator` when the length or digest is not known in advance.
Choose SHA-256, BLAKE3 or both, and optionally cap the number of bytes. The
result describes the bytes read; it does not check them against a claim.

```rust
use covalence_blob_verify::{DigestAccumulator, DigestAlgorithms};

let source = &b"hello"[..];
let result = DigestAccumulator::new(DigestAlgorithms::Both, Some(5)).read(source)?;
assert_eq!(result.len(), 5);
assert_eq!(result.raw_blake3(), Some(covalence_lib_cid::Blake3::of(b"hello")));
# Ok::<(), std::io::Error>(())
```

- Feed chunks with `update` and consume the accumulator with `finish`, or
  use `read` (or `read_async` with Tokio) to hash through EOF and finish.
- An update that would exceed the inclusive limit hashes none of that chunk
  and permanently fails the accumulator. Reader helpers consume at most one
  extra byte to detect input beyond the limit.
- The result includes the exact byte count and only the requested digests.
  If BLAKE3 was requested, `raw_blake3` builds a CID without hashing again.

## What is guaranteed, and what the caller does

Read these limits before relying on the reader or writer:

- **The reader is not zero-unverified-byte publication.** It returns every
  byte but the last as it arrives, before the check. A consumer that
  forwards or stores bytes as they come has already passed on all but one
  unverified byte when the check fails.
- **The caller stages, checks, then commits.** Write to a destination nobody
  reads yet, such as a temporary file or an open multipart upload, and make
  it visible only after `verified()` returns proof. On any error, discard it.
- **Extra bytes are found only with `require_eof`.** Without it, the reader
  stops at the expected size and leaves any further bytes in the source
  unread and unchecked. Use `require_eof` whenever the source should end
  there, such as an HTTP body or a file.
- **Blocked I/O needs an outer timeout.** `Cancel` and deadlines are checked
  between reads and writes, never during one. Give the source or sink its
  own timeouts, or, with Tokio, wrap the future in `tokio::time::timeout`;
  dropping the future stops it.

The reader guarantees:

- A consumer that waits for every expected byte never gets them unless they
  passed the check. The last byte comes only after the digest matches and,
  with `require_eof`, the source has ended.
- An empty expected stream has no byte to withhold. It is checked by the
  first read into a non-empty buffer, which fails or returns 0, but
  `read_exact` of nothing and `take(0)` never read. Callers that may expect
  zero bytes read until 0 is returned, or check `verified()` before
  committing.
- It never reads past the expected size, except one read to see that the
  source ended, with `require_eof`.
- It holds at most one byte of the stream and reads only when it is read
  from, so a slow consumer slows the producer.

The writer guarantees that the inner writer is never given bytes past the
expected size. It cannot hold bytes back: the inner writer has every byte
before the check.

The caller is also responsible for authorization, ownership, retries and
retention. A failed check is not retried here; whether the source can be
read again, for example before a signed URL expires, is for the caller to
decide.

## Errors

Readers and writers return `Error` inside `std::io::Error`; `Error::of`
gets it back. After a check fails or a stream is stopped, every later read or
write fails with the same error. Errors from the wrapped reader or writer
pass through unchanged.
