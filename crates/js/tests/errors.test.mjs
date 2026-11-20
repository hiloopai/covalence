// Argument checks, host answers the binding refuses, and the verifier and
// CID helpers. Run with `node --test` after `glu build`.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { test } from "node:test";

import {
  BLAKE3,
  HELLO,
  HELLO_KEY,
  MemoryHost,
  RAW,
  SHA2_256,
  SHA2_512,
  cidOf,
  coded,
  concat,
  js,
  readAll,
  source,
  text,
  untouched,
} from "./support.mjs";

const { Cas, Verifier, blake3Cid, formatCid, parseCid } = js;

const SHA512 = cidOf(RAW, SHA2_512, new Uint8Array(64).fill(7));
const SHORT_BLAKE3 = cidOf(RAW, BLAKE3, new Uint8Array(20).fill(7));
const BAD_LENGTHS = [-1, 1.5, Number.NaN, Infinity, 2 ** 53];

test("errors name their kind in `code` and `name`", async () => {
  const error = await new Cas(untouched).get("not a CID").catch((error) => error);
  assert.ok(error instanceof Error);
  assert.equal(error.code, "invalid");
  assert.equal(error.name, "InvalidError");
});

test("malformed arguments fail before the host is called", async () => {
  const cas = new Cas(untouched, "cas");
  for (const cid of ["not a CID", HELLO.toUpperCase(), SHA512]) {
    await assert.rejects(cas.get(cid), coded("invalid"));
    await assert.rejects(cas.size(cid), coded("invalid"));
    await assert.rejects(cas.getStream(cid), coded("invalid"));
    await assert.rejects(cas.putFrom(cid, source([]), 0), coded("invalid"));
  }
  for (const length of BAD_LENGTHS) {
    await assert.rejects(cas.putFrom(HELLO, source([]), length), coded("invalid"));
  }
  const locked = source([text("hello")]);
  locked.getReader();
  await assert.rejects(cas.putFrom(HELLO, locked, 5), coded("invalid"));
  // putFrom takes BLAKE3 CIDs with a 32-byte digest only.
  for (const cid of [cidOf(RAW, SHA2_256, new Uint8Array(32)), SHORT_BLAKE3]) {
    await assert.rejects(cas.putFrom(cid, source([]), 0), coded("unsupported"));
  }
  for (const prefix of ["", "/p", "a//b", ".", "..", "a/../b"]) {
    assert.throws(() => new Cas(untouched, prefix), coded("invalid"));
  }
});

/** A memory host whose `method` answers with `answer` instead. */
function answering(method, answer) {
  const host = new MemoryHost();
  host.map.set(HELLO_KEY, text("hello"));
  host[method] = answer;
  return new Cas(host, "cas");
}

test("host answers of the wrong type or size are store errors", async () => {
  const wrong = [
    ["get", "hello", (cas) => cas.get(HELLO)],
    ["get", [104, 101], (cas) => cas.get(HELLO)],
    ...[-1, 1.5, Number.NaN, 2 ** 53, "5"].map((size) => ["size", size, (cas) => cas.size(HELLO)]),
    ["putNew", "yes", (cas) => cas.put(text("other"))],
    ["putNew", undefined, (cas) => cas.put(text("other"))],
    ["putNewFrom", 1, (cas) => cas.putFrom(blake3Cid(text("x")), source([text("x")]), 1)],
    ["getStream", 5, (cas) => cas.getStream(HELLO)],
    ["getStream", { body: source([]) }, (cas) => cas.getStream(HELLO)],
    ["getStream", { size: -1, body: source([]) }, (cas) => cas.getStream(HELLO)],
    ["getStream", { size: 5, body: "hello" }, (cas) => cas.getStream(HELLO)],
  ];
  for (const [method, answer, call] of wrong) {
    const cas = answering(method, async () => answer);
    await assert.rejects(call(cas), coded("store"), `${method} answering ${String(answer)}`);
  }
  const locked = source([text("hello")]);
  locked.getReader();
  const cas = answering("getStream", async () => ({ size: 5, body: locked }));
  await assert.rejects(cas.getStream(HELLO), coded("store"));
});

test("what the host throws, or rejects with, is the cause of a store error", async () => {
  const calls = [
    ["get", (cas) => cas.get(HELLO)],
    ["size", (cas) => cas.size(HELLO)],
    ["putNew", (cas) => cas.put(text("other"))],
    ["putNewFrom", (cas) => cas.putFrom(blake3Cid(text("x")), source([text("x")]), 1)],
    ["getStream", (cas) => cas.getStream(HELLO)],
  ];
  for (const [method, call] of calls) {
    for (const thrown of [new Error(`${method} failed`), `${method} failed`, new TypeError()]) {
      const rejecting = answering(method, async () => {
        throw thrown;
      });
      await assert.rejects(call(rejecting), coded("store", thrown), `${method} rejecting`);
      const throwing = answering(method, () => {
        throw thrown;
      });
      await assert.rejects(call(throwing), coded("store", thrown), `${method} throwing`);
    }
  }
  // An error with no message is reported by its name.
  const nameless = answering("get", async () => {
    throw new TypeError();
  });
  const error = await nameless.get(HELLO).catch((error) => error);
  assert.match(error.message, /TypeError/);
});

test("host methods may answer without a promise", async () => {
  const memory = new MemoryHost();
  const host = {
    get: (key) => memory.map.get(key) ?? null,
    size: (key) => memory.map.get(key)?.length ?? null,
    putNew: (key, value) => {
      memory.map.set(key, value.slice());
      return true;
    },
    putNewFrom: (key, body, length) => memory.putNewFrom(key, body, length),
    getStream: (key) => ({ size: 5, body: source([memory.map.get(key)]) }),
  };
  const cas = new Cas(host, "cas");
  assert.equal(await cas.put(text("hello")), HELLO);
  assert.deepEqual(await cas.get(HELLO), text("hello"));
  assert.equal(await cas.size(HELLO), 5);
  assert.deepEqual(concat(await readAll((await cas.getStream(HELLO)).body)), text("hello"));
});

test("a get stream whose host body errors fails with the body's error as cause", async () => {
  const failed = new Error("disk failed");
  const cas = answering("getStream", async () => ({ size: 5, body: source([failed]) }));
  await assert.rejects(readAll((await cas.getStream(HELLO)).body), coded("store", failed));
});

test("the verifier checks chunk by chunk, against BLAKE3 or SHA-256", () => {
  const verifier = new Verifier(HELLO, 5);
  verifier.update(text("hel"));
  assert.throws(() => verifier.finish(), coded("io"));
  verifier.update(text("lo"));
  assert.equal(verifier.received, 5);
  assert.equal(verifier.remaining, 0);
  verifier.finish();
  // An update past the length hashes none of it.
  assert.throws(() => verifier.update(text("!")), coded("io"));
  assert.equal(verifier.received, 5);
  assert.equal(verifier.remaining, 0);
  verifier.finish();

  const bad = new Verifier(HELLO, 5);
  bad.update(text("hellO"));
  assert.throws(() => bad.finish(), coded("integrity"));

  const sha256 = createHash("sha256").update("hello").digest();
  const sha = new Verifier(cidOf(0x71, SHA2_256, sha256), 5);
  sha.update(text("hello"));
  sha.finish();
});

test("the verifier refuses hashes and digests it cannot check, and bad lengths", () => {
  for (const cid of [SHA512, SHORT_BLAKE3]) {
    assert.throws(() => new Verifier(cid, 5), coded("unsupported"));
  }
  const short = (() => {
    try {
      new Verifier(SHORT_BLAKE3, 5);
    } catch (error) {
      return error;
    }
  })();
  assert.match(short.message, /20-byte/);
  assert.throws(() => new Verifier("not a CID", 5), coded("invalid"));
  for (const length of BAD_LENGTHS) {
    assert.throws(() => new Verifier(HELLO, length), coded("invalid"));
  }
});

test("CIDs parse and format as the Rust library does, with digests up to 64 bytes", () => {
  assert.equal(blake3Cid(text("hello")), HELLO);
  const bytes = parseCid(HELLO);
  assert.equal(bytes.length, 36);
  assert.equal(formatCid(bytes), HELLO);
  const sha512 = parseCid(SHA512);
  assert.equal(sha512.length, 68);
  assert.equal(formatCid(sha512), SHA512);
  assert.throws(() => parseCid(HELLO.toUpperCase()), coded("invalid"));
  assert.throws(() => formatCid(bytes.subarray(1)), coded("invalid"));
  assert.throws(() => formatCid(Uint8Array.of(...bytes, 0)), coded("invalid"));
});
