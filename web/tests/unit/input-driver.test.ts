// Unit tests for the TS input driver.
//
// Covers the shape that differs from the console driver:
//
//   * No output path: every `call(op, payload)` is a caller
//     bug, reported as `Transport`.
//   * Two devnums share one driver-class id. `onHostMessage`
//     demultiplexes kbd-vs-mouse and routes into the matching
//     kernel input ring.

import { describe, expect, it } from "vitest";
import {
  DEV_INPUT_KBD_NODE,
  DEV_INPUT_MOUSE_NODE,
  INPUT_DRIVER_ID,
  InputDriver,
} from "../../src/drivers/input";
import { DriverErrorCode } from "../../src/drivers/types";
import type { DriverHost } from "../../src/drivers/types";
import { Devnum, DriverId } from "../../src/shared/platform-constants";

interface CapturingHost extends DriverHost {
  readonly posted: unknown[];
  readonly pushed: Array<{ devnum: number; bytes: Uint8Array }>;
}

function makeHost(): CapturingHost {
  const posted: unknown[] = [];
  const pushed: Array<{ devnum: number; bytes: Uint8Array }> = [];
  return {
    posted,
    pushed,
    postToMain(msg: unknown): void {
      posted.push(msg);
    },
    pushInputToKernel(devnum: number, bytes: Uint8Array): void {
      pushed.push({ devnum, bytes });
    },
  };
}

describe("InputDriver", () => {
  it("registers under the InputKbd driver-class id", () => {
    const d = new InputDriver();
    expect(d.driverId).toBe(INPUT_DRIVER_ID);
    expect(d.driverId).toBe(DriverId.InputKbd);
    expect(d.name).toBe("input");
  });

  it("DEV_INPUT_*_NODE constants match the Devnum namespace", () => {
    expect(DEV_INPUT_KBD_NODE).toBe(Devnum.InputKbd);
    expect(DEV_INPUT_MOUSE_NODE).toBe(Devnum.InputMouse);
    expect(DEV_INPUT_KBD_NODE).not.toBe(DEV_INPUT_MOUSE_NODE);
  });

  it("call() returns Transport for every opcode (input is read-only)", () => {
    const host = makeHost();
    const d = new InputDriver();
    d.init(host);
    for (const op of [0, 1, 0x10, 0xff]) {
      const result = d.call(op, new Uint8Array(0));
      expect(result).toEqual({ ok: false, error: DriverErrorCode.Transport });
    }
    // No messages posted for any of those calls.
    expect(host.posted).toHaveLength(0);
  });

  it("onHostMessage(input:kbd) pushes bytes into the kbd devnum ring", () => {
    const host = makeHost();
    const d = new InputDriver();
    d.init(host);

    const bytes = new Uint8Array([0x01, 0x02, 0x03]);
    d.onHostMessage?.({ kind: "input:kbd", bytes });

    expect(host.pushed).toHaveLength(1);
    expect(host.pushed[0]?.devnum).toBe(DEV_INPUT_KBD_NODE);
    expect(Array.from(host.pushed[0]?.bytes ?? [])).toEqual([0x01, 0x02, 0x03]);
  });

  it("onHostMessage(input:mouse) pushes bytes into the mouse devnum ring", () => {
    const host = makeHost();
    const d = new InputDriver();
    d.init(host);

    const bytes = new Uint8Array([0x10, 0x20, 0x30, 0x40]);
    d.onHostMessage?.({ kind: "input:mouse", bytes });

    expect(host.pushed).toHaveLength(1);
    expect(host.pushed[0]?.devnum).toBe(DEV_INPUT_MOUSE_NODE);
    expect(Array.from(host.pushed[0]?.bytes ?? [])).toEqual([0x10, 0x20, 0x30, 0x40]);
  });

  it("kbd and mouse messages demultiplex correctly when interleaved", () => {
    const host = makeHost();
    const d = new InputDriver();
    d.init(host);

    d.onHostMessage?.({ kind: "input:kbd", bytes: new Uint8Array([0xaa]) });
    d.onHostMessage?.({ kind: "input:mouse", bytes: new Uint8Array([0xbb]) });
    d.onHostMessage?.({ kind: "input:kbd", bytes: new Uint8Array([0xcc]) });

    expect(host.pushed).toHaveLength(3);
    expect(host.pushed[0]?.devnum).toBe(DEV_INPUT_KBD_NODE);
    expect(host.pushed[1]?.devnum).toBe(DEV_INPUT_MOUSE_NODE);
    expect(host.pushed[2]?.devnum).toBe(DEV_INPUT_KBD_NODE);
    expect(host.pushed[0]?.bytes[0]).toBe(0xaa);
    expect(host.pushed[1]?.bytes[0]).toBe(0xbb);
    expect(host.pushed[2]?.bytes[0]).toBe(0xcc);
  });

  it("onHostMessage copies bytes defensively so the main thread can reuse its buffer", () => {
    const host = makeHost();
    const d = new InputDriver();
    d.init(host);

    const bytes = new Uint8Array([0x11, 0x22]);
    d.onHostMessage?.({ kind: "input:kbd", bytes });
    bytes[0] = 0xff;

    expect(host.pushed[0]?.bytes[0]).toBe(0x11);
    expect(host.pushed[0]?.bytes[1]).toBe(0x22);
  });

  it("onHostMessage ignores unrelated kinds and malformed payloads", () => {
    const host = makeHost();
    const d = new InputDriver();
    d.init(host);
    d.onHostMessage?.({ kind: "console:write", bytes: new Uint8Array(1) });
    d.onHostMessage?.({ kind: "input:kbd", bytes: "not-a-buffer" });
    d.onHostMessage?.({ kind: "input:mouse", bytes: [1, 2, 3] });
    d.onHostMessage?.(null);
    d.onHostMessage?.(undefined);
    d.onHostMessage?.("string");
    expect(host.pushed).toHaveLength(0);
  });

  it("onHostMessage before init() is a silent no-op", () => {
    const d = new InputDriver();
    // Should not throw.
    d.onHostMessage?.({ kind: "input:kbd", bytes: new Uint8Array(1) });
  });
});
