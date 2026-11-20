// Shared by the tests. COVALENCE_JS names the built package's directory,
// `target/glu/js` by default.
import { readFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

const pkg = resolve(process.env.COVALENCE_JS ?? "target/glu/js");
export const js = await import(pathToFileURL(join(pkg, "covalence_js.js")));
js.initSync({ module: await readFile(join(pkg, "covalence_js_bg.wasm")) });

export const HELLO = "bafkr4ihkr4ld3m4gqkjf4reryxsy2s5tkbxprqkow6fin2iiyvreuzzab4";
export const HELLO_KEY = "cas/blake3/5khrmpntq2bjexseshc6ldklwnig56gbj23yvbxjbdcwestheahq/bytes";
export const CHUNK = 64 * 1024;
export const text = (s) => new TextEncoder().encode(s);

/** Multicodec codes for building CIDs. */
export const RAW = 0x55;
export const DRISL = 0x71;
export const SHA2_256 = 0x12;
export const SHA2_512 = 0x13;
export const BLAKE3 = 0x1e;

/** The text of the CIDv1 with `codec`, `hash` and `digest`; codes below 128. */
export const cidOf = (codec, hash, digest) =>
  js.formatCid(Uint8Array.of(0x01, codec, hash, digest.length, ...digest));

/** The digest in the binary CID `text`, whose codes are below 128. */
export const digestOf = (cid) => js.parseCid(cid).subarray(4);

/** A promise and the function that resolves it. */
export function deferred() {
  let resolve;
  const promise = new Promise((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

/** Let every other ready task run before this one goes on. */
export const yieldNow = () => new Promise((resolve) => setImmediate(resolve));

/**
 * A stream of `chunks` that queues none ahead of its reader. An `Error` in
 * `chunks` errors the stream there. It counts the chunks it has handed over
 * in `enqueued`, keeps the reason it was cancelled for in `cancelled`, and
 * resolves `whenCancelled` with that reason.
 */
export function source(chunks) {
  const queue = [...chunks];
  const cancelled = deferred();
  const stream = new ReadableStream(
    {
      pull(controller) {
        if (queue.length === 0) return controller.close();
        const next = queue.shift();
        if (next instanceof Error) return controller.error(next);
        controller.enqueue(next);
        stream.enqueued += 1;
      },
      cancel(reason) {
        stream.cancelled = reason ?? "cancelled";
        cancelled.resolve(stream.cancelled);
      },
    },
    { highWaterMark: 0 },
  );
  stream.enqueued = 0;
  stream.cancelled = undefined;
  stream.whenCancelled = cancelled.promise;
  return stream;
}

/** `bytes` in 64 KiB chunks. */
export const chunked = (bytes) => {
  const chunks = [];
  for (let i = 0; i < bytes.length; i += CHUNK) chunks.push(bytes.slice(i, i + CHUNK));
  return chunks;
};

export async function readAll(body) {
  const chunks = [];
  for await (const chunk of body) chunks.push(chunk);
  return chunks;
}

export const concat = (chunks) => {
  const out = new Uint8Array(chunks.reduce((n, c) => n + c.length, 0));
  let at = 0;
  for (const chunk of chunks) {
    out.set(chunk, at);
    at += chunk.length;
  }
  return out;
};

/**
 * Host storage in memory. `putNewFrom` reads to the end and checks the
 * length; with `eager`, it stores as soon as it has `length` bytes instead, as
 * an upload of a known length may.
 */
export class MemoryHost {
  constructor({ eager = false } = {}) {
    this.map = new Map();
    this.eager = eager;
    this.commits = 0;
  }
  async get(key) {
    return this.map.get(key) ?? null;
  }
  async size(key) {
    return this.map.get(key)?.length ?? null;
  }
  async putNew(key, value) {
    if (this.map.has(key)) return false;
    this.map.set(key, value.slice());
    this.commits += 1;
    return true;
  }
  async putNewFrom(key, body, length) {
    if (this.map.has(key)) return false;
    const reader = body.getReader();
    const chunks = [];
    let received = 0;
    while (!(this.eager && received === length)) {
      const { done, value } = await reader.read();
      if (done) break;
      chunks.push(value);
      received += value.length;
      if (received > length) throw new Error("body is longer than its length");
    }
    if (received !== length) throw new Error("body is shorter than its length");
    return this.putNew(key, concat(chunks));
  }
  async getStream(key) {
    const value = this.map.get(key);
    return value === undefined ? null : { size: value.length, body: source(chunked(value)) };
  }
}

/** Host storage that fails the test if the binding calls it. */
export const untouched = new Proxy(
  {},
  {
    get(_, method) {
      return () => {
        throw new Error(`the host's ${String(method)} was called`);
      };
    },
  },
);

/** Whether `error` is the binding's error with `code`, and `cause` if given. */
export const coded = (code, cause) => (error) =>
  error instanceof Error &&
  error.code === code &&
  (cause === undefined || error.cause === cause);
