// T087: Net driver tests against stub `fetch` and stub `WebSocket`.
//
// jsdom doesn't expose either — we drive the driver through its
// dependency-injected `Fetcher` + `WebSocketFactory` test seams so
// the test harness controls the network behaviour deterministically.

import { describe, expect, it } from "vitest";

import {
  EAGAIN,
  EBADF,
  ECONNRESET,
  EINVAL,
  NetDriver,
  OP_FETCH_BEGIN,
  OP_FETCH_POLL,
  OP_WS_CLOSE,
  OP_WS_OPEN,
  OP_WS_RECV,
  OP_WS_SEND,
  type Fetcher,
  type WebSocketFactory,
  type WebSocketLike,
} from "../../src/drivers/net";
import { DriverErrorCode } from "../../src/drivers/types";

// ---- payload encoders ------------------------------------------------

function encodeFetchBegin(
  method: string,
  url: string,
  headers: ReadonlyArray<readonly [string, string]> = [],
  body: Uint8Array = new Uint8Array(0),
): Uint8Array {
  const enc = new TextEncoder();
  const methodBytes = enc.encode(method);
  const urlBytes = enc.encode(url);
  const headerBytes = headers.map(([n, v]) => ({
    nameBytes: enc.encode(n),
    valueBytes: enc.encode(v),
  }));
  let total = 1 + methodBytes.length + 2 + urlBytes.length + 2;
  for (const { nameBytes, valueBytes } of headerBytes) {
    total += 2 + nameBytes.length + 2 + valueBytes.length;
  }
  total += 4 + body.length;
  const buf = new Uint8Array(total);
  const view = new DataView(buf.buffer);
  let cursor = 0;
  buf[cursor] = methodBytes.length;
  cursor += 1;
  buf.set(methodBytes, cursor);
  cursor += methodBytes.length;
  view.setUint16(cursor, urlBytes.length, true);
  cursor += 2;
  buf.set(urlBytes, cursor);
  cursor += urlBytes.length;
  view.setUint16(cursor, headerBytes.length, true);
  cursor += 2;
  for (const { nameBytes, valueBytes } of headerBytes) {
    view.setUint16(cursor, nameBytes.length, true);
    cursor += 2;
    buf.set(nameBytes, cursor);
    cursor += nameBytes.length;
    view.setUint16(cursor, valueBytes.length, true);
    cursor += 2;
    buf.set(valueBytes, cursor);
    cursor += valueBytes.length;
  }
  view.setUint32(cursor, body.length, true);
  cursor += 4;
  buf.set(body, cursor);
  return buf;
}

function encodeFetchPoll(handle: number, capacity: number): Uint8Array {
  const buf = new Uint8Array(4 + capacity);
  new DataView(buf.buffer).setUint32(0, handle, true);
  return buf;
}

function encodeWsOpen(url: string): Uint8Array {
  const urlBytes = new TextEncoder().encode(url);
  const buf = new Uint8Array(2 + urlBytes.length);
  new DataView(buf.buffer).setUint16(0, urlBytes.length, true);
  buf.set(urlBytes, 2);
  return buf;
}

function encodeWsHandle(handle: number, dataCapacity = 0): Uint8Array {
  const buf = new Uint8Array(4 + dataCapacity);
  new DataView(buf.buffer).setUint32(0, handle, true);
  return buf;
}

function encodeWsSend(handle: number, data: Uint8Array): Uint8Array {
  const buf = new Uint8Array(4 + data.length);
  new DataView(buf.buffer).setUint32(0, handle, true);
  buf.set(data, 4);
  return buf;
}

// ---- fake fetch ------------------------------------------------------

interface FakeResponse {
  status: number;
  headers: Record<string, string>;
  body: Uint8Array;
}

function makeFakeFetcher(): {
  fetcher: Fetcher;
  /** Resolve the most recent in-flight fetch with `resp`. */
  resolveNext(resp: FakeResponse): Promise<void>;
  /** Reject the most recent in-flight fetch. */
  rejectNext(): Promise<void>;
  /** Inflight requests in dispatch order. */
  readonly calls: Array<{ url: string; init?: { method?: string; headers?: Record<string, string>; body?: Uint8Array } }>;
} {
  const pending: Array<{ resolve: (r: FakeResponse) => void; reject: (e: Error) => void }> = [];
  const calls: Array<{ url: string; init?: { method?: string; headers?: Record<string, string>; body?: Uint8Array } }> = [];
  const fetcher: Fetcher = (url, init) => {
    const entry: { url: string; init?: typeof init } = { url };
    if (init !== undefined) entry.init = init;
    calls.push(entry);
    return new Promise((resolve, reject) => {
      pending.push({
        resolve: (r) =>
          resolve({
            status: r.status,
            headers: r.headers,
            arrayBuffer: () =>
              Promise.resolve(
                r.body.buffer.slice(
                  r.body.byteOffset,
                  r.body.byteOffset + r.body.byteLength,
                ) as ArrayBuffer,
              ),
          }),
        reject: (e) => reject(e),
      });
    });
  };
  async function flush(): Promise<void> {
    // Yield to the microtask queue so the in-flight Promise's
    // .then chain runs before the test inspects driver state.
    await Promise.resolve();
    await Promise.resolve();
  }
  return {
    fetcher,
    calls,
    async resolveNext(resp) {
      const next = pending.shift();
      if (!next) throw new Error("no pending fetch");
      next.resolve(resp);
      await flush();
    },
    async rejectNext() {
      const next = pending.shift();
      if (!next) throw new Error("no pending fetch");
      next.reject(new Error("simulated network error"));
      await flush();
    },
  };
}

// ---- fake websocket --------------------------------------------------

class FakeWebSocket implements WebSocketLike {
  onopen: ((this: WebSocketLike, ev: Event) => void) | null = null;
  onmessage: ((this: WebSocketLike, ev: { data: ArrayBuffer | string }) => void) | null = null;
  onerror: ((this: WebSocketLike, ev: Event) => void) | null = null;
  onclose: ((this: WebSocketLike, ev: { code: number; reason: string }) => void) | null = null;
  readonly sent: Uint8Array[] = [];
  closed = false;
  throwOnSend = false;
  send(data: Uint8Array | string): void {
    if (this.throwOnSend) {
      const e = new Error("send refused");
      (e as Error & { name: string }).name = "QuotaExceededError";
      throw e;
    }
    if (typeof data === "string") {
      this.sent.push(new TextEncoder().encode(data));
    } else {
      this.sent.push(new Uint8Array(data));
    }
  }
  close(): void {
    this.closed = true;
  }
  triggerOpen(): void {
    this.onopen?.call(this, new Event("open"));
  }
  triggerMessage(data: Uint8Array | string): void {
    if (typeof data === "string") {
      this.onmessage?.call(this, { data });
    } else {
      const ab = data.buffer.slice(
        data.byteOffset,
        data.byteOffset + data.byteLength,
      ) as ArrayBuffer;
      this.onmessage?.call(this, { data: ab });
    }
  }
  triggerError(): void {
    this.onerror?.call(this, new Event("error"));
  }
  triggerClose(): void {
    this.onclose?.call(this, { code: 1000, reason: "" });
  }
}

function makeFakeWsFactory(): {
  factory: WebSocketFactory;
  readonly opened: FakeWebSocket[];
} {
  const opened: FakeWebSocket[] = [];
  const factory: WebSocketFactory = (_url) => {
    const ws = new FakeWebSocket();
    opened.push(ws);
    return ws;
  };
  return { factory, opened };
}

// ---- fetch tests -----------------------------------------------------

describe("NetDriver — fetch", () => {
  it("FETCH_BEGIN issues a fetch and returns a handle", () => {
    const { fetcher, calls } = makeFakeFetcher();
    const driver = new NetDriver(fetcher);
    const r = driver.call(OP_FETCH_BEGIN, encodeFetchBegin("GET", "https://example.com/data"));
    expect(r).toEqual({ ok: true, value: 1 });
    expect(calls).toHaveLength(1);
    expect(calls[0]!.url).toBe("https://example.com/data");
    expect(calls[0]!.init?.method).toBe("GET");
  });

  it("FETCH_POLL on an unresolved fetch returns EAGAIN", () => {
    const { fetcher } = makeFakeFetcher();
    const driver = new NetDriver(fetcher);
    const begin = driver.call(
      OP_FETCH_BEGIN,
      encodeFetchBegin("GET", "https://example.com/"),
    );
    expect(begin.ok).toBe(true);
    if (!begin.ok) return;
    const poll = driver.call(OP_FETCH_POLL, encodeFetchPoll(begin.value, 1024));
    expect(poll).toEqual({
      ok: false,
      error: DriverErrorCode.Errno,
      errno: EAGAIN,
    });
  });

  it("FETCH_POLL after resolution returns the response bytes", async () => {
    const { fetcher, resolveNext } = makeFakeFetcher();
    const driver = new NetDriver(fetcher);
    const begin = driver.call(
      OP_FETCH_BEGIN,
      encodeFetchBegin("GET", "https://example.com/"),
    );
    expect(begin.ok).toBe(true);
    if (!begin.ok) return;
    const handle = begin.value;

    await resolveNext({
      status: 200,
      headers: { "content-type": "text/plain" },
      body: new TextEncoder().encode("hello"),
    });

    const pollPayload = encodeFetchPoll(handle, 1024);
    const poll = driver.call(OP_FETCH_POLL, pollPayload);
    expect(poll.ok).toBe(true);
    if (!poll.ok) return;
    // Decode the response from payload[4..4+value].
    const view = new DataView(pollPayload.buffer, pollPayload.byteOffset);
    expect(view.getUint16(4, true)).toBe(200); // status
    expect(view.getUint16(6, true)).toBe(1); // header count
    let cursor = 8;
    const nameLen = view.getUint16(cursor, true);
    cursor += 2;
    const name = new TextDecoder().decode(
      pollPayload.subarray(cursor, cursor + nameLen),
    );
    cursor += nameLen;
    const valueLen = view.getUint16(cursor, true);
    cursor += 2;
    const value = new TextDecoder().decode(
      pollPayload.subarray(cursor, cursor + valueLen),
    );
    cursor += valueLen;
    expect(name).toBe("content-type");
    expect(value).toBe("text/plain");
    const bodyLen = view.getUint32(cursor, true);
    cursor += 4;
    const body = new TextDecoder().decode(
      pollPayload.subarray(cursor, cursor + bodyLen),
    );
    expect(body).toBe("hello");
  });

  it("FETCH_POLL after rejection returns ECONNRESET and recycles the handle", async () => {
    const { fetcher, rejectNext } = makeFakeFetcher();
    const driver = new NetDriver(fetcher);
    const begin = driver.call(
      OP_FETCH_BEGIN,
      encodeFetchBegin("GET", "https://example.com/"),
    );
    if (!begin.ok) return;
    await rejectNext();
    const poll = driver.call(OP_FETCH_POLL, encodeFetchPoll(begin.value, 1024));
    expect(poll).toEqual({
      ok: false,
      error: DriverErrorCode.Errno,
      errno: ECONNRESET,
    });
    // Second poll on the same handle now sees EBADF — entry was deleted.
    const poll2 = driver.call(
      OP_FETCH_POLL,
      encodeFetchPoll(begin.value, 1024),
    );
    expect(poll2).toEqual({
      ok: false,
      error: DriverErrorCode.Errno,
      errno: EBADF,
    });
  });

  it("FETCH_POLL on an unknown handle returns EBADF", () => {
    const { fetcher } = makeFakeFetcher();
    const driver = new NetDriver(fetcher);
    const poll = driver.call(OP_FETCH_POLL, encodeFetchPoll(999, 1024));
    expect(poll).toEqual({
      ok: false,
      error: DriverErrorCode.Errno,
      errno: EBADF,
    });
  });

  it("FETCH_POLL with a too-small heap window returns EINVAL and keeps the entry", async () => {
    const { fetcher, resolveNext } = makeFakeFetcher();
    const driver = new NetDriver(fetcher);
    const begin = driver.call(
      OP_FETCH_BEGIN,
      encodeFetchBegin("GET", "https://example.com/"),
    );
    if (!begin.ok) return;
    await resolveNext({
      status: 200,
      headers: {},
      body: new Uint8Array(2048),
    });
    // Poll with a 32-byte window — far too small for a 2 KiB body.
    const poll = driver.call(OP_FETCH_POLL, encodeFetchPoll(begin.value, 32));
    expect(poll.ok).toBe(false);
    if (poll.ok) return;
    expect(poll.error).toBe(DriverErrorCode.Errno);
    expect(poll.errno).toBe(EINVAL);
    // The entry survives — the kernel can retry with a bigger window.
    const poll2 = driver.call(
      OP_FETCH_POLL,
      encodeFetchPoll(begin.value, 4096),
    );
    expect(poll2.ok).toBe(true);
  });

  it("FETCH_BEGIN with a malformed (truncated) payload returns EINVAL", () => {
    const { fetcher } = makeFakeFetcher();
    const driver = new NetDriver(fetcher);
    const r = driver.call(OP_FETCH_BEGIN, new Uint8Array([10, 0, 0])); // method_len=10 but no bytes
    expect(r).toEqual({
      ok: false,
      error: DriverErrorCode.Errno,
      errno: EINVAL,
    });
  });
});

// ---- websocket tests -------------------------------------------------

describe("NetDriver — websocket", () => {
  it("WS_OPEN constructs a WebSocket and returns a handle", () => {
    const { factory, opened } = makeFakeWsFactory();
    const driver = new NetDriver(undefined, factory);
    const r = driver.call(OP_WS_OPEN, encodeWsOpen("wss://echo.example.com/"));
    expect(r).toEqual({ ok: true, value: 1 });
    expect(opened).toHaveLength(1);
  });

  it("WS_RECV with no queued frames returns 0 bytes (poll-friendly)", () => {
    const { factory } = makeFakeWsFactory();
    const driver = new NetDriver(undefined, factory);
    const open = driver.call(OP_WS_OPEN, encodeWsOpen("wss://x/"));
    if (!open.ok) return;
    const recv = driver.call(OP_WS_RECV, encodeWsHandle(open.value, 64));
    expect(recv).toEqual({ ok: true, value: 0 });
  });

  it("WS_RECV after a frame arrives returns the bytes", () => {
    const { factory, opened } = makeFakeWsFactory();
    const driver = new NetDriver(undefined, factory);
    const open = driver.call(OP_WS_OPEN, encodeWsOpen("wss://x/"));
    if (!open.ok) return;
    opened[0]!.triggerOpen();
    opened[0]!.triggerMessage(new Uint8Array([1, 2, 3, 4]));

    const buf = encodeWsHandle(open.value, 64);
    const recv = driver.call(OP_WS_RECV, buf);
    expect(recv).toEqual({ ok: true, value: 4 });
    expect(buf.subarray(4, 8)).toEqual(new Uint8Array([1, 2, 3, 4]));
  });

  it("WS_RECV with too-small buffer returns EINVAL and re-queues the frame", () => {
    const { factory, opened } = makeFakeWsFactory();
    const driver = new NetDriver(undefined, factory);
    const open = driver.call(OP_WS_OPEN, encodeWsOpen("wss://x/"));
    if (!open.ok) return;
    opened[0]!.triggerMessage(new Uint8Array(100));
    const tooSmall = encodeWsHandle(open.value, 16);
    const recv1 = driver.call(OP_WS_RECV, tooSmall);
    expect(recv1).toEqual({
      ok: false,
      error: DriverErrorCode.Errno,
      errno: EINVAL,
    });
    // Frame still queued: a bigger window drains it.
    const big = encodeWsHandle(open.value, 256);
    const recv2 = driver.call(OP_WS_RECV, big);
    expect(recv2).toEqual({ ok: true, value: 100 });
  });

  it("WS_SEND forwards bytes to the underlying socket", () => {
    const { factory, opened } = makeFakeWsFactory();
    const driver = new NetDriver(undefined, factory);
    const open = driver.call(OP_WS_OPEN, encodeWsOpen("wss://x/"));
    if (!open.ok) return;
    const r = driver.call(
      OP_WS_SEND,
      encodeWsSend(open.value, new Uint8Array([10, 20, 30])),
    );
    expect(r).toEqual({ ok: true, value: 3 });
    expect(opened[0]!.sent).toHaveLength(1);
    expect(opened[0]!.sent[0]).toEqual(new Uint8Array([10, 20, 30]));
  });

  it("WS_SEND on a closed socket returns ECONNRESET", () => {
    const { factory, opened } = makeFakeWsFactory();
    const driver = new NetDriver(undefined, factory);
    const open = driver.call(OP_WS_OPEN, encodeWsOpen("wss://x/"));
    if (!open.ok) return;
    opened[0]!.triggerClose();
    const r = driver.call(
      OP_WS_SEND,
      encodeWsSend(open.value, new Uint8Array([1, 2, 3])),
    );
    expect(r).toEqual({
      ok: false,
      error: DriverErrorCode.Errno,
      errno: ECONNRESET,
    });
  });

  it("WS_RECV on a closed socket with no queued frames returns ECONNRESET", () => {
    const { factory, opened } = makeFakeWsFactory();
    const driver = new NetDriver(undefined, factory);
    const open = driver.call(OP_WS_OPEN, encodeWsOpen("wss://x/"));
    if (!open.ok) return;
    opened[0]!.triggerClose();
    const recv = driver.call(OP_WS_RECV, encodeWsHandle(open.value, 64));
    expect(recv).toEqual({
      ok: false,
      error: DriverErrorCode.Errno,
      errno: ECONNRESET,
    });
  });

  it("WS_CLOSE calls socket.close() and recycles the handle", () => {
    const { factory, opened } = makeFakeWsFactory();
    const driver = new NetDriver(undefined, factory);
    const open = driver.call(OP_WS_OPEN, encodeWsOpen("wss://x/"));
    if (!open.ok) return;
    const close = driver.call(OP_WS_CLOSE, encodeWsHandle(open.value));
    expect(close).toEqual({ ok: true, value: 0 });
    expect(opened[0]!.closed).toBe(true);
    // Subsequent operations on the handle return EBADF.
    const r = driver.call(
      OP_WS_SEND,
      encodeWsSend(open.value, new Uint8Array([1])),
    );
    expect(r).toEqual({
      ok: false,
      error: DriverErrorCode.Errno,
      errno: EBADF,
    });
  });

  it("WS_OPEN with malformed payload returns EINVAL", () => {
    const { factory } = makeFakeWsFactory();
    const driver = new NetDriver(undefined, factory);
    const r = driver.call(OP_WS_OPEN, new Uint8Array([0xff, 0xff, 0x01])); // url_len=65535 but no bytes
    expect(r).toEqual({
      ok: false,
      error: DriverErrorCode.Errno,
      errno: EINVAL,
    });
  });

  it("WS_SEND that throws QuotaExceededError surfaces ENOSPC", () => {
    const { factory, opened } = makeFakeWsFactory();
    const driver = new NetDriver(undefined, factory);
    const open = driver.call(OP_WS_OPEN, encodeWsOpen("wss://x/"));
    if (!open.ok) return;
    opened[0]!.throwOnSend = true;
    const r = driver.call(
      OP_WS_SEND,
      encodeWsSend(open.value, new Uint8Array([1, 2, 3])),
    );
    expect(r.ok).toBe(false);
    if (r.ok) return;
    expect(r.error).toBe(DriverErrorCode.Errno);
    // ENOSPC = 51
    expect(r.errno).toBe(51);
  });

  it("an unknown opcode returns Transport", () => {
    const { factory } = makeFakeWsFactory();
    const driver = new NetDriver(undefined, factory);
    const r = driver.call(0xff, new Uint8Array(0));
    expect(r).toEqual({ ok: false, error: DriverErrorCode.Transport });
  });
});
