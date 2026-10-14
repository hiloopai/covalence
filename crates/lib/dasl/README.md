# covalence-lib-dasl

DASL values and their DRISL encoding.

## Terms

- [DASL](https://dasl.ing/) is a small family of specs for content-addressed
  data.
- [CBOR](https://www.rfc-editor.org/rfc/rfc8949) is a binary format like
  JSON. Each item starts with a head giving its type and a length or value.
- [DRISL](https://dasl.ing/drisl.html) is DASL's binary encoding, a strict
  subset of CBOR.
- A [DASL CID](https://dasl.ing/cid.html) names bytes by their hash and codec
  (DRISL or raw). [BDASL](https://dasl.ing/bdasl.html) adds BLAKE3 hashes.
- An `Event` is one CBOR head and its payload. Lists and maps give their length
  up front.
- A `Fact` (from `covalence-lib-cid`) is bytes already checked against their
  CID.
- The CAS is the content-addressed store (`covalence-cas-trait`), which reads
  stored bytes back as facts with `get_fact`.
- [ATProto](https://atproto.com/specs/data-model) is the AT Protocol, used by
  Bluesky.

## Values

- `Value` is the DASL data model without floats. Links are `Cid<Bdasl>` from
  `covalence-lib-cid`.
- Integers are i64. `decode` rejects DRISL integers outside that range.
- Floats give `Error::Float`, following the ATProto data model.
- Nesting is limited to `MAX_DEPTH` (126). Dropping a much deeper hand-built
  value can still overflow the stack.
- There is no size limit. Callers bound input size.

## Encoding and decoding

- `Value::encode_to` streams DRISL to any `io::Write`, such as a hasher, after
  checking the whole value. `Value::encode` collects it into a `Vec`.
- `Value::decode` reads exactly one DRISL value. It rejects:
  - non-minimal heads and indefinite lengths;
  - unsorted or duplicate map keys;
  - tags other than 42, and CIDs that are not DASL CIDs;
  - trailing bytes.
- `Value::events` lists a value's events in the order `encode` writes them.
  `Event::encode` writes one event.
- `Builder` checks events and builds the value. `decode` reads bytes as events
  into a `Builder`, so both reject the same input.
- `Value::links` lists links in the order `encode` writes them. `Links::new`
  lists the links in any sequence of events.

## CIDs and facts

- `Value::cid` is the BLAKE3 DRISL CID of the encoding.
- `Value::from_fact` decodes a checked `Fact`, such as one from the CAS
  `get_fact`:
  - DRISL decodes;
  - raw bytes are `Value::Bytes`;
  - other codecs give `Error::Codec`.
- `Value::from_cid_bytes` checks bytes against a CID first.

## JSON

The `json` feature adds `Value::to_json` and `Value::from_json`.

- Links are `{"$link": "<CID>"}`.
- Bytes are unpadded `{"$bytes": "..."}`; padded bytes are rejected.
- Any other map with a `$link` or `$bytes` key is rejected.
- `to_json` writes keys in DRISL order.
- In `from_json`, fractions, exponents and `-0` give `Error::Float`, and
  integers outside i64 give `Error::Json`.

## Specs and tests

- Follows the 2026-10-01 editions of [DRISL](https://dasl.ing/drisl.html),
  [DASL CIDs](https://dasl.ing/cid.html) and [BDASL](https://dasl.ing/bdasl.html).
- Tests run 85 vectors from the [DASL test suite](https://github.com/hyphacoop/dasl-testing)
  (see `tests/fixtures`), seven of them valid DRISL that must be rejected:
  integers outside i64 and floats.
- Tests also replay `tests/vectors`: CID parsing and printing, and DRISL and
  JSON decoding and encoding, against our Lean model.
