import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { test } from "node:test";

import { CHUNK, HELLO, digestOf, js, text } from "./support.mjs";

const { DigestAccumulator, blake3Cid } = js;
const sha256 = (bytes) => new Uint8Array(createHash("sha256").update(bytes).digest());

test("a larger chunk and offset views produce both digests and the exact length", () => {
  const payload = Uint8Array.from({ length: CHUNK * 2 + 17 }, (_, index) => index % 251);
  const digest = new DigestAccumulator("both", payload.length);
  try {
    digest.update(payload.subarray(0, 1023));
    digest.update(new Uint8Array());
    digest.update(payload.subarray(1023));
    const result = digest.finish();
    const cid = blake3Cid(payload);
    assert.deepEqual(result, {
      len: payload.length, sha256: sha256(payload), blake3: digestOf(cid), rawBlake3: cid,
    });
    assert.throws(() => digest.finish(), { code: "invalid" });
    assert.throws(() => digest.update(new Uint8Array()), { code: "invalid" });
  } finally {
    digest.free();
  }
});

test("only the requested algorithms are computed, including for empty input", () => {
  for (const [algorithms, payload, maxBytes, wantSha256, wantCid] of [
    ["sha256", text("hello"), undefined, true, null],
    ["blake3", text("hello"), undefined, false, HELLO],
    [undefined, text(""), 0, true, blake3Cid(text(""))],
  ]) {
    const digest = new DigestAccumulator(algorithms, maxBytes);
    try {
      digest.update(payload);
      assert.deepEqual(digest.finish(), {
        len: payload.length,
        sha256: wantSha256 ? sha256(payload) : null,
        blake3: wantCid === null ? null : digestOf(wantCid),
        rawBlake3: wantCid,
      });
    } finally {
      digest.free();
    }
  }
});

test("the first over-limit chunk closes the accumulator before copying it", () => {
  class TooLong extends Uint8Array {
    static get [Symbol.species]() {
      assert.fail("an over-limit chunk must not be sliced");
    }
  }
  for (const size of [2, 65 * 1024]) {
    const digest = new DigestAccumulator("both", 3);
    try {
      digest.update(text("ab"));
      assert.throws(() => digest.update(new TooLong(size)), (error) => {
        assert.equal(error.code, "io");
        assert.match(error.message, /limit of 3 bytes/);
        return true;
      });
      assert.throws(() => digest.update(text("")), { code: "invalid" });
      assert.throws(() => digest.finish(), { code: "invalid" });
    } finally {
      digest.free();
    }
  }

  class BadSpecies extends Uint8Array {
    static get [Symbol.species]() {
      throw new Error("species boom");
    }
  }
  const speciesDigest = new DigestAccumulator("blake3");
  try {
    assert.throws(() => speciesDigest.update(new BadSpecies(65 * 1024)), (error) => {
      assert.equal(error.code, "invalid");
      assert.equal(error.cause?.message, "species boom");
      return true;
    });
    assert.throws(() => speciesDigest.finish(), { code: "invalid" });
  } finally {
    speciesDigest.free();
  }
});

test("invalid algorithm names, lengths and non-binary chunks fail", () => {
  for (const algorithms of ["", "sha1", ["both"]]) {
    assert.throws(() => new DigestAccumulator(algorithms), { code: "invalid" });
  }
  for (const maxBytes of [-1, 0.5, NaN, Infinity, "3", 2 ** 53]) {
    assert.throws(() => new DigestAccumulator("both", maxBytes), { code: "invalid" });
  }
  const digest = new DigestAccumulator("both", Number.MAX_SAFE_INTEGER);
  try {
    assert.throws(() => digest.update("hello"), { code: "invalid" });
    assert.throws(() => digest.update(new Uint16Array(2)), { code: "invalid" });
    digest.update(Buffer.from("hello"));
    assert.equal(digest.finish().rawBlake3, HELLO);
  } finally {
    digest.free();
  }
});

test("a limit error leaves stream cancellation and reader errors to the caller", async () => {
  const cause = new Error("body failed");
  let pulls = 0;
  let cancelled = false;
  const body = new ReadableStream({
    pull(controller) {
      if (++pulls === 1) controller.enqueue(text("too long"));
      else controller.error(cause);
    },
    cancel() { cancelled = true; },
  }, { highWaterMark: 0 });
  const reader = body.getReader();
  const digest = new DigestAccumulator("both", 0);
  try {
    const { value } = await reader.read();
    assert.throws(() => digest.update(value), { code: "io" });
    assert.equal(pulls, 1);
    assert.equal(cancelled, false);
    await assert.rejects(reader.read(), (error) => error === cause);
  } finally {
    reader.releaseLock();
    digest.free();
  }
});
