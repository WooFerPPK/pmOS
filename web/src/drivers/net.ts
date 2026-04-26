// Net driver — the TS half of the kernel's network access channel.
//
// The kernel issues `Platform::driver_call(DevId::Net, op, payload)`
// for every network operation; this module is the other side of that
// channel. v1 surfaces two transport families:
//
//   * HTTP fetch — `OP_FETCH_BEGIN` (start a request, returns a
//     handle), `OP_FETCH_POLL` (read the response bytes once
//     ready). Modelled as a request → eventual response pair so
//     the kernel doesn't need to block on async work; the driver
//     side parks the in-flight `Promise` and the kernel polls.
//
//   * WebSocket — `OP_WS_OPEN`, `OP_WS_SEND`, `OP_WS_RECV`,
//     `OP_WS_CLOSE`. Bidirectional message stream; the driver
//     buffers received frames and `OP_WS_RECV` drains them in FIFO
//     order. `OP_WS_SEND` is fire-and-forget; the kernel learns
//     about delivery failure via the next `OP_WS_RECV` returning
//     `EPIPE`.
//
// Both transports use a numeric handle (u32) that's local to the
// driver. The kernel passes the handle in subsequent calls; the
// driver stores per-handle state in two maps. Handles are recycled
// on `OP_WS_CLOSE` / on a fetch's response delivery + a follow-up
// `OP_FETCH_POLL`.
//
// Wire format. Each `payload` is a flat byte stream:
//
//   OP_FETCH_BEGIN: [method_len: u8][method_bytes][url_len: u16 LE]
//                   [url_bytes][header_count: u16 LE]
//                   [(header_name_len: u16 LE, header_name,
//                     header_value_len: u16 LE, header_value)...]
//                   [body_len: u32 LE][body_bytes]
//   OP_FETCH_POLL:  [handle: u32 LE]
//                   on response, payload[4..4+max] is filled with
//                   [status: u16 LE][header_count: u16 LE]
//                   [(name_len: u16, name, value_len: u16, value)...]
//                   [body_len: u32 LE][body_bytes]
//
//   OP_WS_OPEN:     [url_len: u16 LE][url_bytes]
//   OP_WS_SEND:     [handle: u32 LE][data_bytes]
//   OP_WS_RECV:     [handle: u32 LE]
//                   on data, payload[4..4+n] is filled with frame
//                   bytes; result `value` = bytes written (0 if no
//                   frame is queued; the kernel polls).
//   OP_WS_CLOSE:    [handle: u32 LE]
//
// Errno mapping:
//
//   * Bad URL → EINVAL.
//   * Unknown handle → EBADF.
//   * Fetch hasn't completed yet → EAGAIN (kernel polls; the
//     fetch resolves on the JS event loop).
//   * Network error / WS closed by peer → ECONNRESET.
//   * QuotaExceededError on send → ENOSPC.

import type { Driver, DriverHost, DriverResult } from "./types";
import { DriverErrorCode } from "./types";
import { DriverId } from "../shared/platform-constants";

/** Driver-class identifier for the net driver. */
export const NET_DRIVER_ID = DriverId.Net;

export const OP_FETCH_BEGIN = 0x01;
export const OP_FETCH_POLL = 0x02;
export const OP_WS_OPEN = 0x03;
export const OP_WS_SEND = 0x04;
export const OP_WS_RECV = 0x05;
export const OP_WS_CLOSE = 0x06;

/** Errno constants — mirrored from `abi::errno`. */
export const EBADF = 8;
export const EAGAIN = 9;
export const EINVAL = 22;
export const ENOSPC = 51;
export const ECONNRESET = 73;
export const ENOTREADY = 76; // ENOTCAPABLE per abi::errno; net.ts uses it for "no driver"

/**
 * Subset of the global `fetch` API used by the driver. Tests pass a
 * stub that returns a deterministic Promise so the response-decoding
 * paths can be exercised without hitting the network.
 */
export type Fetcher = (url: string, init?: {
  method?: string;
  headers?: Record<string, string>;
  body?: Uint8Array;
}) => Promise<{
  status: number;
  headers: Record<string, string>;
  arrayBuffer(): Promise<ArrayBuffer>;
}>;

/**
 * Subset of the `WebSocket` API used by the driver. Tests pass a
 * stub that lets the test harness deliver frames synchronously.
 */
export interface WebSocketLike {
  send(data: Uint8Array | string): void;
  close(): void;
  onopen: ((this: WebSocketLike, ev: Event) => void) | null;
  onmessage: ((this: WebSocketLike, ev: { data: ArrayBuffer | string }) => void) | null;
  onerror: ((this: WebSocketLike, ev: Event) => void) | null;
  onclose: ((this: WebSocketLike, ev: { code: number; reason: string }) => void) | null;
}

export type WebSocketFactory = (url: string) => WebSocketLike;

interface FetchEntry {
  done: boolean;
  status?: number;
  headers?: Record<string, string>;
  body?: Uint8Array;
  error?: number;
}

interface WsEntry {
  socket: WebSocketLike;
  open: boolean;
  closed: boolean;
  /** FIFO of received frames. Each frame is the full message bytes. */
  recvQueue: Uint8Array[];
}

export class NetDriver implements Driver {
  readonly driverId = NET_DRIVER_ID;
  readonly name = "net";

  private fetches = new Map<number, FetchEntry>();
  private sockets = new Map<number, WsEntry>();
  private nextFetchHandle = 1;
  private nextSocketHandle = 1;

  constructor(
    private readonly fetcher: Fetcher = defaultFetcher,
    private readonly wsFactory: WebSocketFactory = defaultWsFactory,
  ) {}

  init(_host: DriverHost): void {
    // No host messages — net is purely request/response through
    // driver_call.
  }

  call(op: number, payload: Uint8Array): DriverResult {
    switch (op) {
      case OP_FETCH_BEGIN:
        return this.fetchBegin(payload);
      case OP_FETCH_POLL:
        return this.fetchPoll(payload);
      case OP_WS_OPEN:
        return this.wsOpen(payload);
      case OP_WS_SEND:
        return this.wsSend(payload);
      case OP_WS_RECV:
        return this.wsRecv(payload);
      case OP_WS_CLOSE:
        return this.wsClose(payload);
      default:
        return { ok: false, error: DriverErrorCode.Transport };
    }
  }

  // ---- fetch ---------------------------------------------------------

  private fetchBegin(payload: Uint8Array): DriverResult {
    let cursor = 0;
    if (payload.byteLength < 1) {
      return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
    }
    const methodLen = payload[cursor]!;
    cursor += 1;
    if (cursor + methodLen + 2 > payload.byteLength) {
      return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
    }
    const method = new TextDecoder().decode(
      payload.subarray(cursor, cursor + methodLen),
    );
    cursor += methodLen;
    const view = new DataView(payload.buffer, payload.byteOffset);
    const urlLen = view.getUint16(cursor, true);
    cursor += 2;
    if (cursor + urlLen + 2 > payload.byteLength) {
      return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
    }
    const url = new TextDecoder().decode(
      payload.subarray(cursor, cursor + urlLen),
    );
    cursor += urlLen;
    const headerCount = view.getUint16(cursor, true);
    cursor += 2;
    const headers: Record<string, string> = {};
    for (let i = 0; i < headerCount; i += 1) {
      if (cursor + 2 > payload.byteLength) {
        return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
      }
      const nameLen = view.getUint16(cursor, true);
      cursor += 2;
      if (cursor + nameLen + 2 > payload.byteLength) {
        return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
      }
      const name = new TextDecoder().decode(
        payload.subarray(cursor, cursor + nameLen),
      );
      cursor += nameLen;
      const valueLen = view.getUint16(cursor, true);
      cursor += 2;
      if (cursor + valueLen > payload.byteLength) {
        return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
      }
      const value = new TextDecoder().decode(
        payload.subarray(cursor, cursor + valueLen),
      );
      cursor += valueLen;
      headers[name] = value;
    }
    if (cursor + 4 > payload.byteLength) {
      return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
    }
    const bodyLen = view.getUint32(cursor, true);
    cursor += 4;
    if (cursor + bodyLen > payload.byteLength) {
      return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
    }
    const body = bodyLen > 0 ? new Uint8Array(payload.subarray(cursor, cursor + bodyLen)) : undefined;

    const handle = this.nextFetchHandle;
    this.nextFetchHandle += 1;
    const entry: FetchEntry = { done: false };
    this.fetches.set(handle, entry);

    const init: { method?: string; headers?: Record<string, string>; body?: Uint8Array } = {};
    if (method.length > 0) init.method = method;
    if (headerCount > 0) init.headers = headers;
    if (body !== undefined) init.body = body;
    void this.fetcher(url, init).then(
      async (resp) => {
        try {
          const buf = await resp.arrayBuffer();
          entry.status = resp.status;
          entry.headers = resp.headers;
          entry.body = new Uint8Array(buf);
          entry.done = true;
        } catch {
          entry.error = ECONNRESET;
          entry.done = true;
        }
      },
      () => {
        entry.error = ECONNRESET;
        entry.done = true;
      },
    );
    return { ok: true, value: handle };
  }

  private fetchPoll(payload: Uint8Array): DriverResult {
    if (payload.byteLength < 4) {
      return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
    }
    const view = new DataView(payload.buffer, payload.byteOffset);
    const handle = view.getUint32(0, true);
    const entry = this.fetches.get(handle);
    if (!entry) {
      return { ok: false, error: DriverErrorCode.Errno, errno: EBADF };
    }
    if (!entry.done) {
      return { ok: false, error: DriverErrorCode.Errno, errno: EAGAIN };
    }
    if (entry.error !== undefined) {
      this.fetches.delete(handle);
      return { ok: false, error: DriverErrorCode.Errno, errno: entry.error };
    }
    // Encode response into payload[4..]: status (u16) | headers
    // (count u16, repeated [name_len u16, name, value_len u16,
    // value]) | body_len u32 | body bytes.
    const status = entry.status ?? 0;
    const headers = entry.headers ?? {};
    const body = entry.body ?? new Uint8Array(0);
    const headerEntries = Object.entries(headers);
    let needed = 4; // status u16 + count u16
    const headerBytes: Array<{ nameBytes: Uint8Array; valueBytes: Uint8Array }> = [];
    for (const [name, value] of headerEntries) {
      const nameBytes = new TextEncoder().encode(name);
      const valueBytes = new TextEncoder().encode(value);
      headerBytes.push({ nameBytes, valueBytes });
      needed += 2 + nameBytes.length + 2 + valueBytes.length;
    }
    needed += 4 + body.length;
    if (4 + needed > payload.byteLength) {
      // Caller's heap window is too small. Keep the entry around
      // so the caller can retry with a larger window.
      return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
    }
    let cursor = 4;
    view.setUint16(cursor, status, true);
    cursor += 2;
    view.setUint16(cursor, headerEntries.length, true);
    cursor += 2;
    for (let i = 0; i < headerEntries.length; i += 1) {
      const { nameBytes, valueBytes } = headerBytes[i]!;
      view.setUint16(cursor, nameBytes.length, true);
      cursor += 2;
      payload.set(nameBytes, cursor);
      cursor += nameBytes.length;
      view.setUint16(cursor, valueBytes.length, true);
      cursor += 2;
      payload.set(valueBytes, cursor);
      cursor += valueBytes.length;
    }
    view.setUint32(cursor, body.length, true);
    cursor += 4;
    payload.set(body, cursor);
    cursor += body.length;
    this.fetches.delete(handle);
    return { ok: true, value: cursor - 4 };
  }

  // ---- websocket -----------------------------------------------------

  private wsOpen(payload: Uint8Array): DriverResult {
    if (payload.byteLength < 2) {
      return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
    }
    const view = new DataView(payload.buffer, payload.byteOffset);
    const urlLen = view.getUint16(0, true);
    if (2 + urlLen > payload.byteLength) {
      return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
    }
    const url = new TextDecoder().decode(payload.subarray(2, 2 + urlLen));
    const handle = this.nextSocketHandle;
    this.nextSocketHandle += 1;
    let socket: WebSocketLike;
    try {
      socket = this.wsFactory(url);
    } catch {
      return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
    }
    const entry: WsEntry = { socket, open: false, closed: false, recvQueue: [] };
    this.sockets.set(handle, entry);
    socket.onopen = () => {
      entry.open = true;
    };
    socket.onmessage = (ev) => {
      let bytes: Uint8Array;
      if (typeof ev.data === "string") {
        bytes = new TextEncoder().encode(ev.data);
      } else {
        bytes = new Uint8Array(ev.data);
      }
      entry.recvQueue.push(bytes);
    };
    socket.onerror = () => {
      entry.closed = true;
    };
    socket.onclose = () => {
      entry.closed = true;
    };
    return { ok: true, value: handle };
  }

  private wsSend(payload: Uint8Array): DriverResult {
    if (payload.byteLength < 4) {
      return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
    }
    const view = new DataView(payload.buffer, payload.byteOffset);
    const handle = view.getUint32(0, true);
    const entry = this.sockets.get(handle);
    if (!entry) {
      return { ok: false, error: DriverErrorCode.Errno, errno: EBADF };
    }
    if (entry.closed) {
      return { ok: false, error: DriverErrorCode.Errno, errno: ECONNRESET };
    }
    const data = new Uint8Array(payload.subarray(4));
    try {
      entry.socket.send(data);
      return { ok: true, value: data.length };
    } catch (e: unknown) {
      const errno = isQuotaExceeded(e) ? ENOSPC : ECONNRESET;
      return { ok: false, error: DriverErrorCode.Errno, errno };
    }
  }

  private wsRecv(payload: Uint8Array): DriverResult {
    if (payload.byteLength < 4) {
      return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
    }
    const view = new DataView(payload.buffer, payload.byteOffset);
    const handle = view.getUint32(0, true);
    const entry = this.sockets.get(handle);
    if (!entry) {
      return { ok: false, error: DriverErrorCode.Errno, errno: EBADF };
    }
    if (entry.recvQueue.length === 0) {
      if (entry.closed) {
        return { ok: false, error: DriverErrorCode.Errno, errno: ECONNRESET };
      }
      return { ok: true, value: 0 };
    }
    const frame = entry.recvQueue.shift()!;
    const cap = payload.byteLength - 4;
    if (frame.length > cap) {
      // Caller's window is too small. Re-queue at the head and
      // signal EINVAL so the kernel-side caller widens its window.
      entry.recvQueue.unshift(frame);
      return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
    }
    payload.set(frame, 4);
    return { ok: true, value: frame.length };
  }

  private wsClose(payload: Uint8Array): DriverResult {
    if (payload.byteLength < 4) {
      return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
    }
    const view = new DataView(payload.buffer, payload.byteOffset);
    const handle = view.getUint32(0, true);
    const entry = this.sockets.get(handle);
    if (!entry) {
      return { ok: false, error: DriverErrorCode.Errno, errno: EBADF };
    }
    try {
      entry.socket.close();
    } catch {
      // ignore — the close is best-effort.
    }
    this.sockets.delete(handle);
    return { ok: true, value: 0 };
  }
}

function defaultFetcher(url: string, init?: {
  method?: string;
  headers?: Record<string, string>;
  body?: Uint8Array;
}): ReturnType<Fetcher> {
  const reqInit: RequestInit = {};
  if (init?.method !== undefined) reqInit.method = init.method;
  if (init?.headers !== undefined) reqInit.headers = init.headers;
  if (init?.body !== undefined) reqInit.body = init.body;
  return globalThis.fetch(url, reqInit).then(async (r) => {
    const headers: Record<string, string> = {};
    r.headers.forEach((v, k) => {
      headers[k] = v;
    });
    return {
      status: r.status,
      headers,
      arrayBuffer: () => r.arrayBuffer(),
    };
  });
}

function defaultWsFactory(url: string): WebSocketLike {
  const ws = new WebSocket(url);
  ws.binaryType = "arraybuffer";
  return ws as unknown as WebSocketLike;
}

function isQuotaExceeded(e: unknown): boolean {
  if (typeof e !== "object" || e === null) return false;
  const cand = e as { name?: unknown };
  return cand.name === "QuotaExceededError";
}
