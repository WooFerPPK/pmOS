// Unit tests for the canonical driver "common" module
// (`src/drivers/common.ts`).
//
// Four concerns, one file:
//
//   1. The `Driver` interface contract is implementable by a toy
//      class and exercises the init → call lifecycle that real
//      drivers follow.
//   2. The re-exported `DevId` numeric values match the Rust
//      `kernel::platform::DevId` #[repr(u8)] values byte-for-byte.
//      This is the cross-language parity check.
//   3. `createMockRing` supports the push/pop FIFO a test harness
//      needs to simulate kernel↔driver traffic without a real SAB.
//   4. The `ToDriver`/`FromDriver` envelopes are structurally
//      correct — a compile-time type assertion plus a runtime
//      shape round-trip.
//
// The real per-driver tests (console-driver, fb-driver,
// input-driver) cover message shape and edge cases for their
// specific opcodes; this file is deliberately about the SHARED
// module only.

import { describe, expect, it } from "vitest";
import {
  type Driver,
  type DriverHost,
  type DriverResult,
  DriverErrorCode,
  DriverId,
  DevId,
  type ToDriver,
  type FromDriver,
  createMockRing,
  createMockRingPair,
} from "../../src/drivers/common";

// ---------------------------------------------------------------------
// 1. Driver interface contract
// ---------------------------------------------------------------------

/** The simplest possible driver: initialises, echoes payload back, counts calls. */
class ToyDriver implements Driver {
  readonly driverId = DriverId.Console;
  readonly name = "toy";

  public initCount = 0;
  public callCount = 0;
  public lastHost: DriverHost | undefined;
  public hostMessages: unknown[] = [];

  init(host: DriverHost): void {
    this.initCount += 1;
    this.lastHost = host;
  }

  call(op: number, payload: Uint8Array): DriverResult {
    this.callCount += 1;
    if (!this.lastHost) {
      return { ok: false, error: DriverErrorCode.NotReady };
    }
    // Echo the op code as the return value; real drivers vary.
    return { ok: true, value: op, output: payload };
  }

  onHostMessage(msg: unknown): void {
    this.hostMessages.push(msg);
  }
}

function makeStubHost(): DriverHost {
  return {
    postToMain(): void {},
    pushInputToKernel(): void {},
  };
}

describe("common: Driver interface contract", () => {
  it("a toy class that implements Driver satisfies init → call → onHostMessage lifecycle", () => {
    const d = new ToyDriver();
    expect(d.initCount).toBe(0);
    expect(d.callCount).toBe(0);

    // Calls before init must report NotReady — the driver has
    // no host yet.
    const early = d.call(0x01, new Uint8Array(0));
    expect(early).toEqual({ ok: false, error: DriverErrorCode.NotReady });

    d.init(makeStubHost());
    expect(d.initCount).toBe(1);
    expect(d.lastHost).toBeDefined();

    const payload = new Uint8Array([1, 2, 3]);
    const result = d.call(0x42, payload);
    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.value).toBe(0x42);
      expect(result.output).toBe(payload);
    }
    // Total = 1 early NotReady call + 1 post-init call.
    expect(d.callCount).toBe(2);

    d.onHostMessage?.({ kind: "custom", foo: 1 });
    expect(d.hostMessages).toHaveLength(1);
  });

  it("exposes DriverErrorCode variants used by driver returns", () => {
    // Cover every named variant so a later rename/delete surfaces
    // here rather than silently at a runtime site.
    expect(DriverErrorCode.NotReady).toBe(1);
    expect(DriverErrorCode.Transport).toBe(2);
    expect(DriverErrorCode.Errno).toBe(3);
  });
});

// ---------------------------------------------------------------------
// 2. DevId enum matches kernel
// ---------------------------------------------------------------------

describe("common: DevId enum matches kernel::platform::DevId", () => {
  it("every DevId variant has the same u8 value as the Rust enum", () => {
    // Source of truth: crates/kernel/src/platform/mod.rs
    // `pub enum DevId { Framebuffer=0, InputKbd=1, InputMouse=2,
    //                   Block=3, Net=4, Console=5 }`
    expect(DevId.Framebuffer).toBe(0);
    expect(DevId.InputKbd).toBe(1);
    expect(DevId.InputMouse).toBe(2);
    expect(DevId.Block).toBe(3);
    expect(DevId.Net).toBe(4);
    expect(DevId.Console).toBe(5);
  });

  it("`DevId` re-export is the same object as `DriverId`", () => {
    // If a future refactor splits the two aliases, we want the
    // drift to show up here rather than at a call site.
    expect(DevId).toBe(DriverId);
  });

  it("DevId values are dense and unique — matches `#[repr(u8)]` layout", () => {
    const values = Object.values(DevId);
    const uniq = new Set(values);
    expect(uniq.size).toBe(values.length);
    // A dense, zero-based u8 enum on the Rust side: max + 1 == count.
    const max = Math.max(...values);
    expect(max + 1).toBe(values.length);
  });
});

// ---------------------------------------------------------------------
// 3. createMockRing round-trip
// ---------------------------------------------------------------------

describe("common: createMockRing", () => {
  it("empty ring reports length 0 and pops undefined", () => {
    const ring = createMockRing<number>();
    expect(ring.length).toBe(0);
    expect(ring.pop()).toBeUndefined();
    expect(ring.peek()).toBeUndefined();
  });

  it("push/pop behaves as FIFO", () => {
    const ring = createMockRing<string>();
    ring.push("a");
    ring.push("b");
    ring.push("c");
    expect(ring.length).toBe(3);
    expect(ring.peek()).toBe("a");
    expect(ring.pop()).toBe("a");
    expect(ring.pop()).toBe("b");
    expect(ring.length).toBe(1);
    expect(ring.pop()).toBe("c");
    expect(ring.pop()).toBeUndefined();
  });

  it("clear() empties the ring", () => {
    const ring = createMockRing<number>();
    ring.push(1);
    ring.push(2);
    ring.clear();
    expect(ring.length).toBe(0);
    expect(ring.pop()).toBeUndefined();
  });

  it("createMockRingPair round-trips a ToDriver → FromDriver exchange", () => {
    // A mini simulation: fake kernel enqueues a ToDriver request;
    // fake driver dequeues it, acts on it, enqueues a FromDriver
    // response; fake kernel dequeues the response.
    const OP_WRITE = 0x01;
    const pair = createMockRingPair<typeof OP_WRITE, Uint8Array, typeof OP_WRITE, Uint8Array>();

    const request: ToDriver<typeof OP_WRITE, Uint8Array> = {
      opcode: OP_WRITE,
      payload: new Uint8Array([0x48, 0x69]),
      requestId: 7,
    };
    pair.toDriver.push(request);
    expect(pair.toDriver.length).toBe(1);

    const got = pair.toDriver.pop();
    expect(got).toBeDefined();
    expect(got?.opcode).toBe(OP_WRITE);
    expect(got?.requestId).toBe(7);
    expect(Array.from(got?.payload ?? [])).toEqual([0x48, 0x69]);

    const response: FromDriver<typeof OP_WRITE, Uint8Array> = {
      opcode: OP_WRITE,
      payload: new Uint8Array([0x00]),
      requestId: 7,
    };
    pair.fromDriver.push(response);

    const replay = pair.fromDriver.pop();
    expect(replay?.opcode).toBe(OP_WRITE);
    expect(replay?.requestId).toBe(7);
    expect(replay?.payload.byteLength).toBe(1);
  });
});

// ---------------------------------------------------------------------
// 4. ToDriver/FromDriver envelope shape
// ---------------------------------------------------------------------

describe("common: ToDriver/FromDriver envelopes", () => {
  it("accept a concrete opcode literal and refined payload type", () => {
    // Runtime shape check; the real story is the compile-time
    // signature.
    const OP_BLIT = 0x02 as const;
    interface Blit {
      readonly width: number;
      readonly height: number;
    }
    const req: ToDriver<typeof OP_BLIT, Blit> = {
      opcode: OP_BLIT,
      payload: { width: 640, height: 480 },
    };
    expect(req.opcode).toBe(0x02);
    expect(req.payload.width).toBe(640);
    expect(req.requestId).toBeUndefined();

    const res: FromDriver<typeof OP_BLIT, number> = {
      opcode: OP_BLIT,
      payload: 0,
      requestId: 42,
    };
    expect(res.opcode).toBe(0x02);
    expect(res.payload).toBe(0);
    expect(res.requestId).toBe(42);
  });

  it("generic parameters flow to nested fields (structural assertion)", () => {
    // A type-level assertion: if the generic parameters did not
    // flow through, these assignments would fail to type-check
    // and `tsc --noEmit` would break the build.
    type OpId = 0x10;
    type Req = ToDriver<OpId, { id: number }>;
    type Res = FromDriver<OpId, { ok: boolean }>;
    const req: Req = { opcode: 0x10, payload: { id: 1 } };
    const res: Res = { opcode: 0x10, payload: { ok: true } };
    expect(req.payload.id).toBe(1);
    expect(res.payload.ok).toBe(true);
  });

  it("defaults: ToDriver<> with no generics falls back to Uint8Array payloads", () => {
    // This is the common path for simple byte-ferrying drivers.
    const env: ToDriver = {
      opcode: 0xff,
      payload: new Uint8Array([0xab, 0xcd]),
    };
    expect(env.payload).toBeInstanceOf(Uint8Array);
    expect(env.payload.byteLength).toBe(2);
  });
});
