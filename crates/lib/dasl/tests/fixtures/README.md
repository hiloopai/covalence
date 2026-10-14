- These JSON files are unmodified DASL test vectors from
  https://github.com/hyphacoop/dasl-testing/tree/755242366875b25a537a461dcb2d9c4660a62e44/fixtures/cbor
  (retrieved 2026-09-23). The MIT license in this folder applies to them.
- The test uses vectors tagged `basic`, `dag-cbor` or `dasl-cid`.
- Vectors tagged only `dCBOR`, `CBOR-Core`, `CDE` or `rfc8949` are for other
  CBOR profiles and are not DASL rules.
- `invalid_out` vectors describe values that `Value` cannot hold, so they are
  skipped.
