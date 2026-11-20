# @hiloopai/covalence-js

JavaScript bindings for Covalence content-addressed stores, compiled to
WebAssembly with [wasm-bindgen](https://wasm-bindgen.github.io/wasm-bindgen/).
They run where WebAssembly and web streams do, such as Node.js and Cloudflare
Workers. The bindings exist only on `wasm32`; on other targets the crate is
empty.

## Terms

- A content-addressed store (CAS) keeps blocks of bytes under keys computed
  from the bytes.
- A [CID](https://dasl.ing/cid.html) (content identifier) names a hash
  function, the hash's digest of the bytes, and a codec. `Cas.put` returns
  raw [BLAKE3](https://github.com/BLAKE3-team/BLAKE3-specs) CIDs, such as
  `bafkr4ihkr4ld3m4gqkjf4reryxsy2s5tkbxprqkow6fin2iiyvreuzzab4` for `hello`.

## Pieces

- `Cas`: the Rust `KvCas` over storage the host provides as a `HostStorage`
  object (`get`, `size`, `putNew`, `putNewFrom`, `getStream`). Block keys,
  CIDs and checks are the Rust library's: `hello` lies under
  `blake3/5khrmpntq2bjexseshc6ldklwnig56gbj23yvbxjbdcwestheahq/bytes`, after
  any prefix.
- `Cas.putFrom` and `Cas.getStream` stream blocks as `ReadableStream`s. Each
  side is read only as the other reads, and the last chunk is held back until
  the bytes match the CID, so a mismatched upload stores nothing and a corrupt
  block errors in place of its last chunk.
- `Cas.putFrom` reads and checks every byte, even if the block already
  exists, so success shows the caller had the bytes, not only the CID.
- `Cas.putFrom` takes the block's CID before its bytes, because the key is
  derived from it. A caller that does not know the CID has the client
  declare it, which `putFrom` checks, or stages the bytes itself while a
  `Hasher` hashes them, then calls `putFrom` with the CID it gives. The
  binding never stages a block to compute its CID.
- `Hasher`: the raw BLAKE3 CID of bytes fed chunk by chunk, holding only
  the hash state.
- `Verifier`: checks a stream against a CID and length, chunk by chunk.
- `parseCid`, `formatCid` and `blake3Cid`: CIDs between text and bytes, and
  the CID of some bytes.

Objects of the `Cas`, `Hasher` and `Verifier` classes hold WebAssembly
memory; call `free()` when done with one.

## Limits

- Lengths and sizes are whole numbers from 0 to 2^53 - 1, the largest that
  JavaScript numbers hold exactly. Other numbers are invalid.
- `parseCid` and `formatCid` take CIDs with digests of up to 64 bytes, such
  as SHA-512. `Cas` methods reject a digest over 32 bytes as invalid.
- `Cas` stores raw BLAKE3 CIDs. Reads find a block by a BLAKE3 CID of any
  codec with a 32-byte digest, and nothing for another hash. `putFrom` takes
  a BLAKE3 CID of any codec with a 32-byte digest, as the Python binding's
  `put_from` does, and returns the raw BLAKE3 CID.
- `Verifier` takes CIDs of any codec whose hash is BLAKE3 or SHA-256, with a
  32-byte digest.
- The binding holds about one chunk of a stream at a time, so memory does
  not grow with the length of a block. `put`, `get` and `blake3Cid` hold the
  whole value.

## Errors

The binding throws, or rejects with, an `Error` whose `code` and `name` say
what it is about. Match on `code`; messages may change.

| `code` | `name` | When |
| --- | --- | --- |
| `"invalid"` | `InvalidError` | An argument is malformed: CID text, a length, a prefix, a locked body |
| `"integrity"` | `IntegrityError` | Bytes do not match their CID |
| `"unsupported"` | `UnsupportedError` | The CID's hash or digest length is not supported |
| `"store"` | `StoreError` | The host storage threw, or gave an answer of the wrong type or size |
| `"io"` | `IoError` | A body errored, had too few or too many bytes, or a chunk that is not a `Uint8Array` |

Where the host or a body threw, the error's `cause` is the value it threw.
A `getStream` body that fails its check errors with `"integrity"` (another
digest) or `"io"` (another length), and one whose host stream errors with
`"store"`.

## Example

A host storage over a Cloudflare R2 bucket, which creates objects only if
absent:

```js
import init, { Cas } from "@hiloopai/covalence-js";
import wasm from "@hiloopai/covalence-js/covalence_js_bg.wasm";

const r2 = (bucket) => ({
  async get(key) {
    const object = await bucket.get(key);
    return object && new Uint8Array(await object.arrayBuffer());
  },
  async size(key) {
    return (await bucket.head(key))?.size ?? null;
  },
  async putNew(key, value) {
    const only = { onlyIf: { etagDoesNotMatch: "*" } };
    return (await bucket.put(key, value, only)) !== null;
  },
  async putNewFrom(key, body, length) {
    const sized = body.pipeThrough(new FixedLengthStream(length));
    const only = { onlyIf: { etagDoesNotMatch: "*" } };
    return (await bucket.put(key, sized, only)) !== null;
  },
  async getStream(key) {
    const object = await bucket.get(key);
    return object && { size: object.size, body: object.body };
  },
});

await init({ module_or_path: wasm });
const cas = new Cas(r2(env.BUCKET), "cas");
try {
  const cid = await cas.putFrom(expectedCid, request.body, contentLength);
  const found = await cas.getStream(cid);
} finally {
  cas.free();
}
```

## Build

The package is not on npm yet. Each [GitHub release] attaches it as
`hiloopai-covalence-js-<version>.tgz`, which `npm install` accepts as a
file or URL. Build it from a checkout with `glu build`, which writes it to
`target/glu/js`, or by hand:

```sh
cargo build --locked --release -p covalence-js --target wasm32-unknown-unknown
wasm-bindgen --target web --out-dir target/js \
  target/wasm32-unknown-unknown/release/covalence_js.wasm
cp crates/js/package.json target/js/
```

The `wasm-bindgen` command-line tool must be the version of the
`wasm-bindgen` crate in `Cargo.lock` (`glu` installs that version under
`target/glu/tools`). The generated JavaScript and WebAssembly are build
output under `target/`, not committed.

[GitHub release]: https://github.com/hiloopai/covalence/releases
