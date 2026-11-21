# Checked ranges and streaming

**Proposal.** Add exact content-addressed ranges in four stages: CID evidence,
checked content-addressed store (CAS) requests, authenticated streaming, and an
in-memory proof producer. Keep raw range reads and existing streams unchanged.
Names and signatures below are sketches, not available APIs.

## Evidence and scope

- [`Fact::new`](../crates/lib/cid/src/fact.rs) checks whole bytes. For a
  content identifier (CID), it checks the digest, not the codec.
  [`GetExt::get_fact` and `AsyncGetExt::aget_fact`](../crates/cas/trait/src/lib.rs)
  check the returned fact's multihash and use the requested codec. Unsuffixed
  `get_range` is what the store vouches for, not a cryptographic check. Current
  `Cas::get_to` and `Cas::aget_stream` can output provisional bytes before the
  whole-object check succeeds.
- Use Silmaril's [wire integration at `fad3de0`][lean-wire], built on the
  [range kernel][lean-kernel], [right-edge extent check][lean-extent], and
  [Bao/Iroh formats][lean-formats]. `step?` authenticates opened cells at
  the **claimed** geometry. An interior opening can pass with a false extent.
  Do not turn it into a public range fact until the same root and extent are
  supported by a right-edge proof or independent trusted evidence.
- `CompleteBody.trustedRange?` and `verifiedRange?` distinguish the two
  sources of extent evidence, and `Prefix.completed` is separate from both.
  `ExactRange.soundTrusted` and `soundEOF` give a reference slice or a
  concrete compression collision **against a supplied named payload**. They
  do not prove collision resistance or the existence of bytes behind opaque
  chaining values. The Lean outer wrappers buffer frames, the outboard
  adapter is eager, and the stepper assumes a plan is already built. These
  are references, not Rust equivalence, bounded-memory or transport proofs.

## 1. CID: range and extent facts

Keep `Fact` as the whole-object type. Add `crates/lib/cid/src/range.rs` for
coordinates and opaque evidence, `range/blake3.rs` for the verifier, and
private format adapters under `range/wire/`. Connect whole-fact slicing in
`fact.rs`. Sketch, for any CID storage format `S: CidFormat`:

```text
ByteRange::new(start: u64, end: u64) -> Result<ByteRange, BoundsError>
ByteRange::from_start_len(start: u64, len: u64) -> Result<ByteRange, BoundsError>
Fact<Cid<S>>::extent(&self) -> ExtentFact<S>
Fact<Cid<S>>::slice(&self, absolute: ByteRange) -> Result<RangeFact<S>, BoundsError>
RangeFact<S>::slice(&self, absolute: ByteRange) -> Result<RangeFact<S>, BoundsError>
RangeFact<S>::extent(&self) -> &ExtentFact<S>
RangeFact<S>::range(&self) -> ByteRange
RangeFact<S>::bytes(&self) -> &Bytes
ExtentFact<S>::cid(&self) -> &Cid<S>
ExtentFact<S>::len(&self) -> u64
ProofLimits = { max_extent, max_proof_bytes, max_return_bytes, max_items, max_work }

verify_blake3_extent(cid: &Cid<S>, tail: TailProof<'_>, limits: &ProofLimits)
    -> Result<ExtentFact<S>, ProofError>
verify_blake3_range(extent: &ExtentFact<S>, range: ByteRange,
                    proof: RangeProof<'_>, limits: &ProofLimits)
    -> Result<RangeFact<S>, RangeCheckError>
```

- `RangeFact` owns its immutable bytes, exact half-open absolute range and
  extent fact. `ExtentFact` binds a CID to exact length; it does not establish
  completeness or availability at any store. Keep fields and constructors
  private; no unchecked public constructor, deserialization into evidence,
  or adapter callback that can mint a fact. Match multihash for evidence reuse
  and preserve the requested CID label without certifying codec semantics.
- Reject inverted ranges and arithmetic overflow. Nested absolute slicing
  stays within the parent even for zero-length slices. It uses shared bytes,
  no tree proof or rehash, and works for every whole fact's hash. Empty ranges
  are valid at `start <= extent`, including EOF; they still need an extent
  fact and carry no data. No implicit clipping. A caller can explicitly clip
  both ends against an already authenticated extent. Don't promote a partial
  fact to `Fact`; defer any no-rehash whole-fact conversion.
- In v1 an extent fact comes only from a whole `Fact` or checked BLAKE3 tail,
  not a caller-supplied integer. For the canonical right edge, validate the
  true final chunk; for length zero, check the zero-byte **root** chunk.
  In a combined proof that carries its own tail, an internal path may create
  the extent fact before returning a buffered range. For standalone proofs,
  run the tail check first and require the body header to match it.
- In the private kernel use the original 1 KiB BLAKE3 leaves and absolute
  chunk counters. Compute a canonical shape from the bounded extent and
  validate the reached node, byte offset, width and orientation at every
  step. Check a parent pair (two 32-byte chaining values) before installing
  expected child values. Distinguish the BLAKE3 root digest from a non-root
  chaining value (CV). Check disclosed data at its reached subtree before
  trimming it to caller bytes. A 16 KiB wire group can fold up to sixteen
  original chunks, including a short final group; it is never one BLAKE3 chunk.
  Reject opaque roots; an opaque child CV is only an expected value, not data.
  Only accept this proof for a 32-byte, unkeyed BLAKE3 CID digest. Reuse the
  `Cid::raw_blake3` eligibility check without erasing the requested label.

### Wire adapters and dependencies

Treat every external header, pair, data field and request as untrusted.
Choose the profile from local context, not by sniffing input. Give adapters
an internal schedule and bounded parser interface: produce only raw pair/data
fields for the kernel's expected slot. Parsers cannot assert coverage, root
authentication, length or successful completion.

| Profile | Adapter contract |
| --- | --- |
| Official Bao combined | Eight-byte little-endian (`LE64`) unencoded extent and pre-order pairs/data; covers the whole object, even when the caller keeps a smaller window. |
| Official Bao slice | Same length header and pruned pre-order body; byte offset/count supplied out of band. Normalize the upstream slice's witness semantics internally, not the public exact-range semantics. A tail adapter chooses the final chunk from the bounded claim and verifies that it is the canonical edge. |
| Official Bao outboard | LE64 extent plus pre-order pairs and a separate source for the original bytes. Start with two bounded byte slices to match Lean `checkBaoOutboard?`. An eventual seek adapter reads checked offsets through a consistent read-at snapshot and validates selected paths; it is not the same as accepting the full outboard projection. No format called an official outboard slice. |
| bao-tree group-log 4 and Iroh raw Get | Use fixed log 4 (16 KiB) and LE64 extent for the first CAS proof profile, `Grouped16`. Derive the body selection by rounding the requested bytes outward to 1 KiB chunks, not 16 KiB groups; use bao-tree's last-chunk selection for the tail. A raw Iroh Get adapter additionally checks the canonical one-root request (`[(0, query), (1, [])]`), compares the independent digest and exact selection and consumes one reply. No transport dependency; no `HashSeq`, collection, push, observe, keyed mode, or portable Iroh outboards. |

Build the small audited kernel around the workspace's BLAKE3 1.8.7, using
its [`hazmat` subtree and merge operations][blake3] (no new compression
implementation). A non-root subtree has an aligned absolute input offset
and finalizes to a CV; singleton/root data and root parents use the root
operations. Pin [Bao 0.13.1][bao] and [bao-tree 0.17.0][bao-tree] as
differential oracles; use the latter for production **encoding** only in the
example's opt-in feature. Pin the Iroh raw request schema to the audited
[iroh-blobs v0.105.0 source][iroh], not just `/iroh-bytes/4`. No normal
`iroh`, Tokio or Postcard dependency in CID. A restricted canonical request
decoder is only needed when accepting serialized Iroh requests.

Reusing an upstream decoder saves traversal and I/O maintenance, and is a
reasonable later choice for a profile with a narrow audited adapter. It must
still establish absolute positions, consistent extent, empty-root checking,
coverage and exact frame consumption before minting facts. Bao tolerates a
trailing suffix; bao-tree `ResponseDecoder::Done` can follow an error. Neither
automatically implements the CAS boundary. Start with the private kernel and
the **one** CAS profile; keep official adapters and serialized Iroh request
handling independently reviewable, using the same checked seam. Don't ship a
profile solely because its Lean checker accepts a fixture. Require differential
coverage of supported schedules and documented rejections before exposing it.

## 2. CAS: offer, fetch, verify

Add optional, object-safe hooks to `Cas`, in `crates/cas/trait/src/lib.rs`,
with proof/request types and the blanket checked extension in `range.rs`.
Use the existing `CasFuture`/`MaybeSend` convention; async hooks must not
default to a blocking backend. Forward the hooks through `Arc<dyn Cas>`.
Start with a single selected kind rather than a general negotiation protocol.

```text
Selection = Exact(ByteRange) | Whole
RangeQuery = { selection: Selection, kind: ProofKind, limits: ProofLimits }
ProofReply<T> = Found(T) | Absent | Unavailable | Unsupported(Reason)
RangeWireProof = { tail_frame: Bytes, body_frame: Option<Bytes> }
Fallback = Never | WholeObject { max_bytes: u64 }

Cas::offer_range_proof(&self, cid: &Cid, query: &RangeQuery)
    -> Result<ProofReply<()>, cas::Error>
Cas::fetch_range_proof_unchecked(&self, cid: &Cid, query: &RangeQuery)
    -> Result<ProofReply<RangeWireProof>, cas::Error>
Cas::afetch_range_proof_unchecked<'a>(&'a self, cid: &'a Cid, query: &'a RangeQuery)
    -> CasFuture<'a, ProofReply<RangeWireProof>>
CheckedRangeExt::get_range_fact(&self, cid: &Cid, query: &RangeQuery, fallback: Fallback)
    -> Result<Option<RangeFact>, CheckedRangeError>
CheckedRangeExt::aget_range_fact<'a>(&'a self, cid: &'a Cid, query: &'a RangeQuery,
                                    fallback: Fallback)
    -> CheckedRangeFuture<'a, Option<RangeFact>>
```

`offer` is advisory and can be skipped. Recheck the fetch result: another
reader can evict a proof between calls. The fetch replies about one CID and
profile; it does not choose a different encoding or use raw range reads as
evidence. Treat the supplied frames as untrusted, including any sizes. Verify
the tail, resolve `Whole` to `[0,extent)`, check bounds, then verify exact body
coverage and framing. With an authenticated empty selection no body is
allowed. An out-of-bounds request may be answered with just the tail so that
the wrapper, not untrusted metadata, determines `OutOfBounds`. A missing body
for in-bounds nonempty data is invalid, not a cache miss.

| Outcome | Checked wrapper behavior |
| --- | --- |
| `Absent` | `Ok(None)` from the fetch (a store report, not cryptographic nonexistence). Present empty is `Some(empty fact)`. |
| `Unavailable` | Per-key/selection miss: the source supports the format but cannot produce this proof now. No inference from raw range capability. |
| `Unsupported` | Operation, configured profile, or CID hash cannot be checked by this path. Separate from absence and per-key availability. A returned wrong profile is **invalid**, not unsupported. |
| Invalid request or limit | Reject inverted/overflowed/capped request before backend work where possible; report an exact end beyond EOF only after authenticating extent. Exceeding a local proof/work cap fails closed. |
| Invalid evidence | Retain `ProofError` for malformed, truncated, trailing, wrong digest/offset/shape, or mismatched extent. Clean early EOF inside a field is truncation; a reset/read failure retains its I/O cause. No retry as raw or downgrade on invalid evidence. |
| Backend, I/O, cancellation | Preserve source and phase. Use the CAS error facade for backend/transport and CID errors for verification. Do not translate these to `None`, normal EOF or a fallback trigger. |

`WholeObject` is an explicit fallback **only** after unsupported/unavailable.
Obtain a whole immutable object from a bounded source, verify it with
`Fact::new` (or start from an actual whole fact with matching key) and slice.
This also permits supported non-BLAKE3 hashes. Do not implement a byte cap by
checking after the default `get_fact` has already fetched the whole object:
require an incrementally capped source or an immutable resident fact with a
known size, otherwise return unsupported for that fallback. A bounded provider
hook such as `fetch_whole_for_range_unchecked(cid, max_bytes)` (and async twin)
can return `ProofReply<WholeEvidence>`, where `WholeEvidence` is immutable
bytes or a whole `Fact`. The wrapper checks the byte budget and requested
multihash even if it gets a fact. Default this hook to unsupported rather
than delegating to full `get`. Do not silently enable a buffering path for
streaming. Fallback remains off by default.

## 3. Authenticated stream and completion

Add the async driver in `crates/cas/trait/src/range/stream.rs`, reusing the
CID kernel's field stepper. Leave `get_to`, `aget_stream` and the current
whole-object check in `crates/blob/verify` alone. Add an optional
`Cas::aopen_range_proof_unchecked` returning a bounded-frame source and a
tail frame; default to unsupported. The checked driver looks like:

```text
RangeWireStream = { tail_frame: Bytes, body: Option<ByteStream<'static>> }
Cas::aopen_range_proof_unchecked<'a>(&'a self, cid: &'a Cid, query: &'a RangeQuery)
    -> CasFuture<'a, ProofReply<RangeWireStream>>
CheckedRangeExt::aopen_checked_range<'a>(&'a self, cid: &'a Cid, query: &'a RangeQuery)
    -> CheckedRangeFuture<'a, Option<AuthenticatedRange>>
AuthenticatedRange::extent(&self) -> &ExtentFact
AuthenticatedRange::next(&mut self) -> CheckedRangeFuture<'_, Option<RangeFact>>
AuthenticatedRange::finish(self) -> CheckedRangeFuture<'static, RangeCompletion>
```

- **Extent first.** Verify a small separately fetched right-edge proof
  before releasing or accumulating interior bytes. The forward stream then
  starts at a frame with the same CID and extent. For a seekable source the
  same ordering can come from a checked right-edge seek in an outboard.
  If neither method nor an existing extent fact is available, do not expose
  an opaque staging buffer or silently fall back to a provisional stream.
- **Items.** Keep a cursor at the exact requested start. A checked item has
  positive length and begins at the cursor; trim to the requested window,
  check the new cursor, and advance. Padding data outside the window does not
  advance coverage; do not emit zero-byte items. Backpressure is pull-based;
  splitting a wire field across reads cannot release it early.
- **Finish.** `next == Ok(None)` means the schedule is drained, not transport
  or CAS success. `finish` requires the caller to drain all items, exact final
  cursor, complete schedule, and confirmed enclosing-frame end with no
  leftover bytes. A strict single frame rejects trailing data; a composed
  parser returns a suffix for the outer framer to handle. Failure/cancel/drop
  cannot yield completion; an already returned item stays authentic. For a
  sink that promises a complete object or atomic range install, stage writes
  and commit only after `finish`. A sink that promises only individual verified
  offsets can consume items immediately. Completion is not a whole `Fact`.
- **Limits.** Cap extent before geometry, check arithmetic before conversion,
  cap proof bytes/items and returned bytes, and charge work at every step
  (including skipped positions, if any). Use a lazy shape/range iterator and
  bound group and source buffers; an eager vector of scheduled slots or an
  unbounded channel is not a streaming implementation. Require source-level
  bounded reads rather than promising that a wrapper caps an arbitrary
  backend's allocations. Add cancel/deadline and early-drop tests before
  making a resource claim. The target is one group plus a depth-bounded stack,
  aside from bounded framing; demonstrate that separately from Lean soundness.

## 4. Example producer

Add opt-in `crates/cas/memory/src/proofs.rs`, using
[`MemoryCas`'s immutable `Fact<Blake3>`](../crates/cas/memory/src/lib.rs) as
the snapshot. Start with on-demand `PreOrderMemOutboard` and validated
bao-tree encoding: one tail frame and a body when needed. Exercise the same
untrusted consumer path as an external provider. Keep a producer byte/work
limit (cold outboard generation may hash the entire object). Resolve in-memory
fetches from the same immutable snapshot and expose a pulling body source for
the stream stage; don't hide a whole encoded body behind a stream of chunks.

An optional byte-budgeted LRU can cache outboards by unkeyed BLAKE3 digest and
profile/geometry. Make misses observable or regenerate within policy; never
hold the store/cache lock across encoding or an await. Eviction, disabled
generation and unsupported keys do not affect raw reads. Cached outboard
bytes still go through the verifier. Defer SQLite, Fossil, KV/S3 sidecars,
new dependencies in bindings, and changing the disk/wire formats of existing
stores; none of those is needed to test the authority boundary.

## Acceptance gates

| Where | Minimum checks before enabling the stage |
| --- | --- |
| `crates/lib/cid/tests/range.rs` | Compile-fail forged facts; SHA-256 whole-fact slicing; nested absolute and empty slices; requested CID label; exact bounds, `u64::MAX`, wrong root, swapped children, opaque root, wrong starting chunk and `ROOT` vs CV. Cover zero, 1, 63/64/65, 1023/1024/1025, 2048/2049, 16383/16384/16385 and odd trees. |
| `crates/lib/cid/tests/range_wire.rs` | Use the [Lean regressions][lean-tests] and pinned upstream fixtures. For 2,049 zero bytes, an interior Bao slice at byte 1,024 can pass at claimed length 2,050 internally but cannot mint `ExtentFact` or release against the actual extent. A fabricated right edge, even with matching padding, fails. Check the true edge, empty root, separate tail, conflicting headers, outboard/data swap/short reads, framing (strict trailing vs embedded suffix), truncated header/parent/leaf, invalid canonical Iroh request, and the 17,409-byte short multi-chunk group (`byte[i] = i mod 251`, selection `16..`, frame length 1,097). Assert true final length and unchanged absolute counters and root flags. |
| `crates/cas/trait/tests/checked_range.rs` | Raw-only, proof-only, both, neither; offer/fetch race and cache miss; distinct absent/empty/unsupported/unavailable/invalid/I/O/limit results. Instrument the raw method to fail if called. Prove no fallback on corrupt evidence, unauthorized profile or backend error; explicitly permitted bounded whole-object fallback verifies before slicing, including for SHA-256. Cover `Arc<dyn Cas>`, sync/async parity and non-blocking default async behavior. |
| `crates/cas/trait/tests/checked_stream.rs` | One-byte reads, delayed or wrong tail, zero-length request, gaps/overlaps/reordering, truncated or trailing frames, late I/O, cap, cancel, backpressure and early drop. A valid prefix followed by failure may have authentic items but no completion or sink commit. Test that verified EOF doesn't imply finish, and proper interior framing without matching extent cannot start a verified stream. |
| `crates/cas/memory/tests/proofs.rs` | Compare to a whole-fact slice across boundaries. Verify cold/hot/cache eviction, miss after offer, immutable snapshot, proof producer and verifier budgets, and corruption after generation. Keep the raw range unchanged and check that pulls do not prebuild a complete frame. |

Run focused Cargo tests for each stage, feature-disabled and WASM checks where
applicable, then `./bin/glu ci`. Keep Rust/Lean differential fixtures and
profile assumptions versioned. A green fixture suite is not a refinement proof
or evidence of bounds over every input; document the remaining limits.

[lean-wire]: https://github.com/hiloopai/silmaril-lean/blob/fad3de0054acc7f55aab0aaa8eece5d684d0db17/docs/projects/blake3-wire-cas.md#L7-L74
[lean-kernel]: https://github.com/hiloopai/silmaril-lean/blob/94f0ce9cbb0ac133bc69e464b562f75c8d0a096d/docs/projects/blake3-range-kernel.md
[lean-extent]: https://github.com/hiloopai/silmaril-lean/blob/ce7dcc055af67e2498233f123ff2cc63afeed544/docs/projects/blake3-range-kernel.md
[lean-formats]: https://github.com/hiloopai/silmaril-lean/blob/cd86b0153a91a0eff616a54d64fda61ba984c323/docs/projects/bao-iroh.md
[lean-tests]: https://github.com/hiloopai/silmaril-lean/blob/fad3de0054acc7f55aab0aaa8eece5d684d0db17/tests/formats/Blake3Cas.lean#L42-L158
[blake3]: https://github.com/BLAKE3-team/BLAKE3/blob/f3149ec5bb5449af877ba20377a11008ff499fa2/src/hazmat.rs
[bao]: https://github.com/oconnor663/bao/blob/19763d70a5bda9402cc37a63e823cd7b9aa7f777/docs/spec.md#L20-L136
[bao-tree]: https://github.com/n0-computer/bao-tree/tree/67691dfbd36538730c7e7402f013259205f76fcf
[iroh]: https://github.com/n0-computer/iroh-blobs/blob/7a7c410f31c9da90854099f4a5d5fdf20da840c3/src/protocol.rs#L395-L505
