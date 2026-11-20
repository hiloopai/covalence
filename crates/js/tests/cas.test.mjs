// Run with `node --test` after `glu build`.
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  BLAKE3,
  CHUNK,
  DRISL,
  HELLO,
  HELLO_KEY,
  MemoryHost,
  RAW,
  SHA2_256,
  chunked,
  cidOf,
  coded,
  concat,
  deferred,
  digestOf,
  js,
  readAll,
  source,
  text,
  yieldNow,
} from "./support.mjs";

const { Cas, Hasher, blake3Cid } = js;

test("blocks land under the golden key and read back checked", async () => {
  const host = new MemoryHost();
  const cas = new Cas(host, "cas");
  assert.equal(await cas.put(text("hello")), HELLO);
  assert.deepEqual([...host.map.keys()], [HELLO_KEY]);
  assert.deepEqual(await cas.get(HELLO), text("hello"));
  assert.equal(await cas.size(HELLO), 5);
  const missing = blake3Cid(text("missing"));
  assert.equal(await cas.get(missing), null);
  assert.equal(await cas.size(missing), null);
  assert.equal(await cas.getStream(missing), null);

  host.map.set(HELLO_KEY, text("jello"));
  await assert.rejects(cas.get(HELLO), coded("integrity"));
  // Sizes are the host's, unchecked.
  assert.equal(await cas.size(HELLO), 5);
});

test("a BLAKE3 CID of any codec finds the block; another hash finds nothing", async () => {
  const host = new MemoryHost();
  const cas = new Cas(host, "cas");
  await cas.put(text("hello"));
  const drisl = cidOf(DRISL, BLAKE3, digestOf(HELLO));
  assert.deepEqual(await cas.get(drisl), text("hello"));
  assert.equal(await cas.size(drisl), 5);
  assert.deepEqual(concat(await readAll((await cas.getStream(drisl)).body)), text("hello"));

  const sha256 = cidOf(RAW, SHA2_256, digestOf(HELLO));
  assert.equal(await cas.get(sha256), null);
  assert.equal(await cas.size(sha256), null);
  assert.equal(await cas.getStream(sha256), null);
});

// The CIDs and keys the Python binding gives for the same bytes; its tests
// and the Rust stream tests assert the same vectors.
const GOLDEN = [
  [text("hello"), HELLO, "5khrmpntq2bjexseshc6ldklwnig56gbj23yvbxjbdcwestheahq"],
  [
    new Uint8Array(),
    "bafkr4ifpcne3t5pzugtkaqcn5i3nzskjtpfslsnnyejlpte2spfoihzsmi",
    "v4jutopv7gq2nicajxvdnxgjjgn4wjojvxarfn6mtkj4vza7gjra",
  ],
  [
    Uint8Array.from({ length: 1280 }, (_, i) => i % 256),
    "bafkr4idvq35ccjdoryl3hbfy7z4jjdmrncaoltskqdidb5cvasksu2paka",
    "owdpuijen2hbpm4exd7hrfensfuibzoojkanamhukucjkktj4bia",
  ],
];

test("blocks have the CIDs and keys the Python binding gives", async () => {
  for (const [bytes, cid, digest] of GOLDEN) {
    assert.equal(blake3Cid(bytes), cid);
    for (const prefix of [undefined, "cas"]) {
      const key = `${prefix ? `${prefix}/` : ""}blake3/${digest}/bytes`;
      for (const streamed of [false, true]) {
        const host = new MemoryHost();
        const cas = new Cas(host, prefix);
        const parts = [];
        for (let i = 0; i < bytes.length; i += 7) parts.push(bytes.slice(i, i + 7));
        const stored = streamed
          ? await cas.putFrom(cid, source(parts), bytes.length)
          : await cas.put(bytes);
        assert.equal(stored, cid);
        assert.deepEqual([...host.map.keys()], [key]);
        assert.deepEqual(host.map.get(key), bytes);
        assert.deepEqual(concat(await readAll((await cas.getStream(cid)).body)), bytes);
      }
    }
  }
});

test("a hasher gives the golden CIDs chunk by chunk, for staging then putFrom", async () => {
  for (const [bytes, cid] of GOLDEN) {
    for (const size of [1, 7, 4096]) {
      const hasher = new Hasher();
      // A host stages the bytes as they arrive and hashes them on the way.
      const staged = [];
      for (let i = 0; i < bytes.length; i += size) {
        const chunk = bytes.slice(i, i + size);
        hasher.update(chunk);
        staged.push(chunk);
      }
      assert.equal(hasher.received, bytes.length);
      assert.equal(hasher.finish(), cid);
      const host = new MemoryHost();
      assert.equal(await new Cas(host).putFrom(hasher.finish(), source(staged), bytes.length), cid);
      assert.equal(host.commits, 1);
      hasher.free();
    }
  }
  const hasher = new Hasher();
  hasher.update(text("hel"));
  assert.equal(hasher.finish(), blake3Cid(text("hel")));
  hasher.update(text("lo"));
  assert.equal(hasher.finish(), HELLO);
});

test("streamed puts are checked, stream back, and take BLAKE3 CIDs of any codec", async () => {
  const host = new MemoryHost();
  const cas = new Cas(host, "cas");
  const body = source([text("he"), text("llo")]);
  assert.equal(await cas.putFrom(HELLO, body, 5), HELLO);
  assert.deepEqual(host.map.get(HELLO_KEY), text("hello"));
  const { size, body: read } = await cas.getStream(HELLO);
  assert.equal(size, 5);
  assert.deepEqual(concat(await readAll(read)), text("hello"));

  const empty = blake3Cid(new Uint8Array());
  assert.equal(await cas.putFrom(empty, source([]), 0), empty);
  assert.equal((await cas.getStream(empty)).size, 0);

  // As the Python binding's put_from: any codec, and the raw CID back.
  const two = blake3Cid(text("two"));
  const drisl = cidOf(DRISL, BLAKE3, digestOf(two));
  assert.equal(await cas.putFrom(drisl, source([text("two")]), 3), two);
});

test("streamed puts never replace a block, and check the body even if it exists", async () => {
  const host = new MemoryHost();
  const cas = new Cas(host, "cas");
  host.map.set(HELLO_KEY, text("wrong"));
  // The host finds the block without reading the body; the binding still
  // reads and checks all of it, so knowing the CID alone is not enough.
  const body = source([text("hel"), text("lo")]);
  assert.equal(await cas.putFrom(HELLO, body, 5), HELLO);
  assert.equal(body.enqueued, 2);
  const aborted = new Error("source aborted");
  const bad = [
    [[text("hellO")], coded("integrity")],
    [[text("hel")], coded("io")],
    [[text("hello"), text("!")], coded("io")],
    [[text("he"), aborted], coded("io", aborted)],
  ];
  for (const [chunks, error] of bad) {
    await assert.rejects(cas.putFrom(HELLO, source(chunks), 5), error);
  }
  assert.equal(await cas.put(text("hello")), HELLO);
  assert.deepEqual(host.map.get(HELLO_KEY), text("wrong"));
  assert.equal(host.commits, 0);
});

for (const eager of [false, true]) {
  const which = eager ? ", even in a host that stores at the length" : "";
  test(`bad bodies store nothing${which}`, async () => {
    const host = new MemoryHost({ eager });
    const cas = new Cas(host, "cas");
    const aborted = new Error("source aborted");
    const bad = [
      [[text("hellO")], coded("integrity")],
      [[text("hel"), text("lO")], coded("integrity")],
      [[text("hel")], coded("io")],
      [[text("hello"), text("!")], coded("io")],
      [[text("hello!")], coded("io")],
      [[text("he"), aborted], coded("io", aborted)],
      [[text("he"), "not bytes"], coded("io")],
    ];
    for (const [chunks, error] of bad) {
      await assert.rejects(cas.putFrom(HELLO, source(chunks), 5), error);
    }
    // An empty block's body is read to its end before the host sees it.
    const empty = blake3Cid(new Uint8Array());
    for (const chunks of [[text("x")], [aborted]]) {
      await assert.rejects(cas.putFrom(empty, source(chunks), 0), coded("io"));
    }
    await assert.rejects(cas.putFrom(HELLO, source([]), 0), coded("integrity"));
    assert.equal(host.map.size, 0);
  });
}

test("a corrupt or misreported block errors in place of its last chunk", async () => {
  const data = new Uint8Array(200_000).map((_, i) => i % 251);
  const cid = blake3Cid(data);
  const host = new MemoryHost();
  const cas = new Cas(host, "cas");
  assert.equal(await cas.putFrom(cid, source(chunked(data)), data.length), cid);
  const key = [...host.map.keys()][0];

  const intact = await readAll((await cas.getStream(cid)).body);
  assert.deepEqual(concat(intact), data);

  const corrupt = data.slice();
  corrupt[0] ^= 1;
  host.map.set(key, corrupt);
  const reader = (await cas.getStream(cid)).body.getReader();
  let received = 0;
  for (let i = 0; i < 3; i++) received += (await reader.read()).value.length;
  assert.equal(received, 3 * CHUNK);
  await assert.rejects(reader.read(), coded("integrity"));

  // A host whose stream is shorter or longer than the size it reports.
  host.map.set(HELLO_KEY, text("hello"));
  for (const chunks of [[text("hell")], [text("hello"), text("!")]]) {
    host.getStream = async () => ({ size: 5, body: source(chunks) });
    await assert.rejects(readAll((await cas.getStream(HELLO)).body), coded("io"));
  }
  const failed = new Error("host stream failed");
  host.getStream = async () => ({ size: 5, body: source([text("he"), failed]) });
  await assert.rejects(readAll((await cas.getStream(HELLO)).body), coded("store", failed));
});

test("a slow host pulls the body: the binding holds at most one chunk", async () => {
  const data = new Uint8Array(40 * CHUNK).map((_, i) => i % 249);
  const cid = blake3Cid(data);
  const body = source(chunked(data));
  const host = new MemoryHost();
  let ahead = 0;
  host.putNewFrom = async (key, stream) => {
    const reader = stream.getReader();
    const chunks = [];
    for (let next; !(next = await reader.read()).done; ) {
      chunks.push(next.value);
      ahead = Math.max(ahead, body.enqueued - chunks.length);
      await yieldNow();
    }
    return host.putNew(key, concat(chunks));
  };
  const cas = new Cas(host);
  assert.equal(await cas.putFrom(cid, body, data.length), cid);
  assert.equal(body.enqueued, 40);
  assert.ok(ahead <= 1, `the binding read ${ahead} chunks ahead of the host`);

  let served;
  host.getStream = async (key) => {
    const value = host.map.get(key);
    served = source(chunked(value));
    return { size: value.length, body: served };
  };
  const reader = (await cas.getStream(cid)).body.getReader();
  for (let read = 1; read <= 10; read++) {
    await reader.read();
    await yieldNow();
    assert.ok(served.enqueued <= read + 1, `${served.enqueued} chunks served for ${read} read`);
  }
  await reader.cancel("reader gave up");
  await served.whenCancelled;
});

test("a host or body that gives up partway stores nothing", async () => {
  const data = new Uint8Array(10 * CHUNK).map((_, i) => i % 247);
  const cid = blake3Cid(data);
  // The host may cancel its reader or just throw; either way the caller's
  // body is cancelled and unlocked, and the error's cause is the host's.
  for (const cancels of [true, false]) {
    const host = new MemoryHost();
    const gaveUp = new Error("host gave up");
    host.putNewFrom = async (key, stream) => {
      const reader = stream.getReader();
      await reader.read();
      await reader.read();
      if (cancels) await reader.cancel("host gave up");
      throw gaveUp;
    };
    const body = source(chunked(data));
    await assert.rejects(new Cas(host).putFrom(cid, body, data.length), coded("store", gaveUp));
    assert.ok(body.enqueued <= 3, `${body.enqueued} chunks read`);
    await body.whenCancelled;
    assert.equal(body.locked, false);
    assert.equal(host.map.size, 0);
  }

  for (const eager of [false, true]) {
    const host = new MemoryHost({ eager });
    const chunks = chunked(data);
    const aborted = new Error("body aborted");
    chunks.splice(5, 5, aborted);
    const put = new Cas(host).putFrom(cid, source(chunks), data.length);
    await assert.rejects(put, coded("io", aborted));
    assert.equal(host.map.size, 0);
  }
});

test("a retry after a failure stores once, and racing writers each succeed", async () => {
  const host = new MemoryHost();
  const cas = new Cas(host, "cas");
  await assert.rejects(cas.putFrom(HELLO, source([text("hellO")]), 5), coded("integrity"));
  await assert.rejects(cas.putFrom(HELLO, source([text("hel")]), 5), coded("io"));
  assert.equal(await cas.putFrom(HELLO, source([text("hel"), text("lo")]), 5), HELLO);
  assert.equal(host.commits, 1);
  assert.deepEqual([...host.map.keys()], [HELLO_KEY]);

  // Each body gives its first part, then waits until every writer has
  // started before giving the rest.
  const started = deferred();
  const slow = (last = "lo") =>
    new ReadableStream({
      async start(controller) {
        controller.enqueue(text("hel"));
        await started.promise;
        controller.enqueue(text(last));
        controller.close();
      },
    });
  const racing = new MemoryHost();
  const raced = new Cas(racing, "cas");
  const writes = [
    raced.putFrom(HELLO, slow(), 5),
    raced.putFrom(HELLO, slow("lO"), 5),
    raced.putFrom(HELLO, slow(), 5),
    raced.put(text("hello")),
  ];
  started.resolve();
  const results = await Promise.allSettled(writes);
  assert.deepEqual(
    results.map((result) => result.value ?? result.reason.code),
    [HELLO, "integrity", HELLO, HELLO],
  );
  assert.equal(racing.commits, 1);
  assert.deepEqual(racing.map.get(HELLO_KEY), text("hello"));
});
