// Unit tests for the pre-WASM MockKernel.
//
// Covers the line-buffering logic, both echo and faux-shell
// policies, and the `fauxShellTransform` helper in isolation.
// The `MockKernel.bindScaffold` integration is deliberately
// minimal — we stub `callDriver` with a capturing fake rather
// than wiring up a full `bootKernelWorker`, because the
// scaffold's routing is already covered by
// `kernel-worker.test.ts`.

import { describe, expect, it } from "vitest";
import { MockKernel, fauxShellTransform } from "../../src/mock-kernel";
import type { KernelWorker } from "../../src/kernel-worker";
import type { DriverResult } from "../../src/drivers/types";
import {
  CONSOLE_DRIVER_ID,
  DEV_CONSOLE_NODE,
  OP_WRITE_LINE,
} from "../../src/drivers/console";
import { Devnum } from "../../src/shared/platform-constants";

interface CapturingScaffold extends KernelWorker {
  readonly calls: Array<{ driverId: number; op: number; payload: Uint8Array }>;
}

function makeScaffold(): CapturingScaffold {
  const calls: Array<{ driverId: number; op: number; payload: Uint8Array }> = [];
  return {
    calls,
    handleMainMessage(): void {
      // Unused in these tests.
    },
    callDriver(driverId: number, op: number, payload: Uint8Array): DriverResult {
      const copy = new Uint8Array(payload.byteLength);
      copy.set(payload);
      calls.push({ driverId, op, payload: copy });
      return { ok: true, value: payload.byteLength };
    },
    get driverCount(): number {
      return 1;
    },
  };
}

describe("MockKernel echo policy", () => {
  it("buffers input until newline, then echoes the whole line", () => {
    const mock = new MockKernel({ policy: { kind: "echo" } });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);

    // Partial line — no output yet.
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("hel"));
    expect(scaffold.calls).toHaveLength(0);

    // Completing newline — flush.
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("lo\n"));
    expect(scaffold.calls).toHaveLength(1);
    expect(scaffold.calls[0]?.driverId).toBe(CONSOLE_DRIVER_ID);
    expect(scaffold.calls[0]?.op).toBe(OP_WRITE_LINE);
    expect(new TextDecoder().decode(scaffold.calls[0]?.payload)).toBe("hello\n");
  });

  it("flushes multiple lines in one injected chunk", () => {
    const mock = new MockKernel({ policy: { kind: "echo" } });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);

    mock.injectInput(
      DEV_CONSOLE_NODE,
      new TextEncoder().encode("one\ntwo\nthree\n"),
    );
    expect(scaffold.calls).toHaveLength(3);
    expect(new TextDecoder().decode(scaffold.calls[0]?.payload)).toBe("one\n");
    expect(new TextDecoder().decode(scaffold.calls[1]?.payload)).toBe("two\n");
    expect(new TextDecoder().decode(scaffold.calls[2]?.payload)).toBe("three\n");
  });

  it("drops non-console devnums in v1", () => {
    const mock = new MockKernel({ policy: { kind: "echo" } });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);

    mock.injectInput(Devnum.InputKbd, new TextEncoder().encode("ignored\n"));
    mock.injectInput(Devnum.InputMouse, new TextEncoder().encode("ignored\n"));
    expect(scaffold.calls).toHaveLength(0);
  });

  it("before bindScaffold, lines are buffered but no output is emitted", () => {
    const mock = new MockKernel({ policy: { kind: "echo" } });
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("stray\n"));
    // No scaffold bound yet → the line is lost. Should not throw.
  });
});

describe("MockKernel faux-shell policy", () => {
  it("translates 'echo X' into 'X\\n'", () => {
    const mock = new MockKernel({ policy: { kind: "faux-shell" } });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);

    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("echo hello\n"));
    expect(scaffold.calls).toHaveLength(1);
    expect(new TextDecoder().decode(scaffold.calls[0]?.payload)).toBe("hello\n");
  });

  it("unknown commands emit '?\\n'", () => {
    const mock = new MockKernel({ policy: { kind: "faux-shell" } });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);

    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("ls\n"));
    expect(scaffold.calls).toHaveLength(1);
    expect(new TextDecoder().decode(scaffold.calls[0]?.payload)).toBe("?\n");
  });

  it("empty lines produce no output", () => {
    const mock = new MockKernel({ policy: { kind: "faux-shell" } });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);

    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("\n"));
    expect(scaffold.calls).toHaveLength(0);
  });
});

describe("fauxShellTransform", () => {
  it("extracts the echo argument and appends a newline", () => {
    const out = fauxShellTransform(new TextEncoder().encode("echo hi\n"));
    expect(new TextDecoder().decode(out)).toBe("hi\n");
  });

  it("handles a line without a trailing newline", () => {
    const out = fauxShellTransform(new TextEncoder().encode("echo hi"));
    expect(new TextDecoder().decode(out)).toBe("hi\n");
  });

  it("returns '?\\n' for an unknown command", () => {
    const out = fauxShellTransform(new TextEncoder().encode("wat\n"));
    expect(new TextDecoder().decode(out)).toBe("?\n");
  });

  it("returns an empty buffer for a bare newline", () => {
    const out = fauxShellTransform(new TextEncoder().encode("\n"));
    expect(out.byteLength).toBe(0);
  });

  it("echoes an empty argument as just a newline", () => {
    const out = fauxShellTransform(new TextEncoder().encode("echo \n"));
    expect(new TextDecoder().decode(out)).toBe("\n");
  });
});
