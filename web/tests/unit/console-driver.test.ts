// Unit tests for the TS console driver.
//
// Covers every branch of `ConsoleDriver.call` and
// `ConsoleDriver.onHostMessage` in isolation, against an
// in-memory stand-in for `DriverHost`. The kernel-worker
// scaffold's own routing is covered in
// `kernel-worker.test.ts`.

import { describe, expect, it } from "vitest";
import {
  ConsoleDriver,
  DEV_CONSOLE,
  OP_WRITE_LINE,
} from "../../src/drivers/console";
import { DriverErrorCode } from "../../src/drivers/types";
import type { DriverHost } from "../../src/drivers/types";

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

describe("ConsoleDriver", () => {
  it("identifies itself as the /dev/console driver", () => {
    const d = new ConsoleDriver();
    expect(d.devId).toBe(DEV_CONSOLE);
    expect(d.name).toBe("console");
  });

  it("call() before init() returns NotReady", () => {
    const d = new ConsoleDriver();
    const result = d.call(OP_WRITE_LINE, new Uint8Array(4));
    expect(result).toEqual({ ok: false, error: DriverErrorCode.NotReady });
  });

  it("call(OP_WRITE_LINE, bytes) posts a console:write message to main", () => {
    const host = makeHost();
    const d = new ConsoleDriver();
    d.init(host);
    const payload = new TextEncoder().encode("hello\n");

    const result = d.call(OP_WRITE_LINE, payload);
    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.value).toBe(payload.byteLength);
    }

    expect(host.posted).toHaveLength(1);
    const msg = host.posted[0] as { kind: string; bytes: Uint8Array };
    expect(msg.kind).toBe("console:write");
    expect(new TextDecoder().decode(msg.bytes)).toBe("hello\n");
  });

  it("call() copies the payload so the kernel can reuse its outbound buffer", () => {
    const host = makeHost();
    const d = new ConsoleDriver();
    d.init(host);
    const buf = new Uint8Array([1, 2, 3, 4]);

    d.call(OP_WRITE_LINE, buf);
    // The kernel would typically reuse its scratch buffer
    // immediately after driver_call returns. The driver must
    // have taken a defensive copy so our stored message is
    // unaffected.
    buf[0] = 99;
    const msg = host.posted[0] as { bytes: Uint8Array };
    expect(msg.bytes[0]).toBe(1);
    expect(Array.from(msg.bytes)).toEqual([1, 2, 3, 4]);
  });

  it("call() with an unknown opcode returns Transport", () => {
    const host = makeHost();
    const d = new ConsoleDriver();
    d.init(host);
    const result = d.call(0xff, new Uint8Array(0));
    expect(result).toEqual({ ok: false, error: DriverErrorCode.Transport });
    expect(host.posted).toHaveLength(0);
  });

  it("call(OP_WRITE_LINE) on a zero-length payload still posts an empty chunk", () => {
    // A real kernel will not call driver_call with a zero-length
    // buffer in v1, but the driver is resilient to it — some
    // future caller may legitimately flush an empty line.
    const host = makeHost();
    const d = new ConsoleDriver();
    d.init(host);
    const result = d.call(OP_WRITE_LINE, new Uint8Array(0));
    expect(result.ok).toBe(true);
    expect(host.posted).toHaveLength(1);
    const msg = host.posted[0] as { bytes: Uint8Array };
    expect(msg.bytes.byteLength).toBe(0);
  });

  it("onHostMessage(console:input) pushes bytes into the kernel for DEV_CONSOLE", () => {
    const host = makeHost();
    const d = new ConsoleDriver();
    d.init(host);

    const bytes = new TextEncoder().encode("echo hi\n");
    d.onHostMessage?.({ kind: "console:input", bytes });

    expect(host.pushed).toHaveLength(1);
    expect(host.pushed[0]?.devnum).toBe(DEV_CONSOLE);
    expect(new TextDecoder().decode(host.pushed[0]?.bytes)).toBe("echo hi\n");
  });

  it("onHostMessage copies the input bytes so the main thread can reuse its buffer", () => {
    const host = makeHost();
    const d = new ConsoleDriver();
    d.init(host);

    const bytes = new Uint8Array([0x61, 0x62]);
    d.onHostMessage?.({ kind: "console:input", bytes });
    bytes[0] = 0xff;

    const pushed = host.pushed[0]?.bytes;
    expect(pushed?.[0]).toBe(0x61);
    expect(pushed?.[1]).toBe(0x62);
  });

  it("onHostMessage ignores unrelated message kinds", () => {
    const host = makeHost();
    const d = new ConsoleDriver();
    d.init(host);
    d.onHostMessage?.({ kind: "unrelated", bytes: new Uint8Array(1) });
    d.onHostMessage?.(null);
    d.onHostMessage?.("not an object");
    d.onHostMessage?.(undefined);
    expect(host.pushed).toHaveLength(0);
  });

  it("onHostMessage rejects console:input with a non-Uint8Array bytes field", () => {
    const host = makeHost();
    const d = new ConsoleDriver();
    d.init(host);
    // Intentional shape violation — the TS type guard must
    // refuse to dispatch a malformed payload rather than
    // throwing.
    d.onHostMessage?.({ kind: "console:input", bytes: "hello" });
    d.onHostMessage?.({ kind: "console:input", bytes: [1, 2, 3] });
    expect(host.pushed).toHaveLength(0);
  });

  it("onHostMessage before init() is a silent no-op, not a throw", () => {
    const d = new ConsoleDriver();
    // Should not throw.
    d.onHostMessage?.({ kind: "console:input", bytes: new Uint8Array(1) });
  });
});
