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
import {
  MockKernel,
  SPLASH_HEIGHT,
  SPLASH_WIDTH,
  fauxShellTransform,
  generateSplashPixels,
} from "../../src/mock-kernel";
import type { KernelWorker } from "../../src/kernel-worker";
import { DriverErrorCode } from "../../src/drivers/types";
import type { DriverResult } from "../../src/drivers/types";
import {
  CONSOLE_DRIVER_ID,
  DEV_CONSOLE_NODE,
  OP_WRITE_LINE,
} from "../../src/drivers/console";
import {
  FB_DRIVER_ID,
  OP_BLIT as FB_OP_BLIT,
  OP_SET_MODE as FB_OP_SET_MODE,
} from "../../src/drivers/fb";
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

// ---- splash emission -----------------------------------------------

/**
 * Build a scaffold whose `callDriver` always succeeds EXCEPT
 * the framebuffer path, which returns a caller-specified
 * value. Used by the splash tests that want to simulate
 * "fb driver missing".
 */
function makeScaffoldWithFbResult(fbResult: DriverResult): CapturingScaffold {
  const calls: Array<{ driverId: number; op: number; payload: Uint8Array }> = [];
  return {
    calls,
    handleMainMessage(): void {
      /* unused */
    },
    callDriver(driverId: number, op: number, payload: Uint8Array): DriverResult {
      const copy = new Uint8Array(payload.byteLength);
      copy.set(payload);
      calls.push({ driverId, op, payload: copy });
      if (driverId === FB_DRIVER_ID) {
        return fbResult;
      }
      return { ok: true, value: payload.byteLength };
    },
    get driverCount(): number {
      return 2;
    },
  };
}

describe("MockKernel splash emission", () => {
  it("is opt-in: default constructor does not emit on input", () => {
    const mock = new MockKernel({ policy: { kind: "echo" } });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("hi\n"));
    const fbCalls = scaffold.calls.filter((c) => c.driverId === FB_DRIVER_ID);
    expect(fbCalls).toHaveLength(0);
  });

  it("emitSplashOnFirstInput=true emits SET_MODE + BLIT on the first injected input", () => {
    const mock = new MockKernel({
      policy: { kind: "echo" },
      emitSplashOnFirstInput: true,
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);

    // Before input: no scaffold calls.
    expect(scaffold.calls).toHaveLength(0);

    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("hi\n"));

    const fbCalls = scaffold.calls.filter((c) => c.driverId === FB_DRIVER_ID);
    expect(fbCalls).toHaveLength(2);
    expect(fbCalls[0]?.op).toBe(FB_OP_SET_MODE);
    expect(fbCalls[1]?.op).toBe(FB_OP_BLIT);

    // The SET_MODE payload is a 2x u32 LE geometry blob.
    const setModePayload = fbCalls[0]?.payload;
    if (setModePayload) {
      const view = new DataView(
        setModePayload.buffer,
        setModePayload.byteOffset,
        setModePayload.byteLength,
      );
      expect(view.getUint32(0, true)).toBe(SPLASH_WIDTH);
      expect(view.getUint32(4, true)).toBe(SPLASH_HEIGHT);
    }

    // The BLIT payload is header + width*height*4 pixel bytes.
    const blitPayload = fbCalls[1]?.payload;
    if (blitPayload) {
      expect(blitPayload.byteLength).toBe(8 + SPLASH_WIDTH * SPLASH_HEIGHT * 4);
    }
  });

  it("emits the splash only once even across many inputs", () => {
    const mock = new MockKernel({
      policy: { kind: "echo" },
      emitSplashOnFirstInput: true,
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    mock.injectInput(
      DEV_CONSOLE_NODE,
      new TextEncoder().encode("one\ntwo\nthree\nfour\n"),
    );
    const fbCalls = scaffold.calls.filter((c) => c.driverId === FB_DRIVER_ID);
    expect(fbCalls).toHaveLength(2);
  });

  it("skips the splash when the scaffold reports NotReady for the fb driver", () => {
    const scaffold = makeScaffoldWithFbResult({
      ok: false,
      error: DriverErrorCode.NotReady,
    });
    const mock = new MockKernel({
      policy: { kind: "echo" },
      emitSplashOnFirstInput: true,
    });
    mock.bindScaffold(scaffold);
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("go\n"));

    const fbCalls = scaffold.calls.filter((c) => c.driverId === FB_DRIVER_ID);
    // SET_MODE was ATTEMPTED (one call recorded on the fake
    // scaffold), then the mock bailed out and never posted BLIT.
    expect(fbCalls).toHaveLength(1);
    expect(fbCalls[0]?.op).toBe(FB_OP_SET_MODE);
  });

  it("still delivers the echo response after emitting the splash", () => {
    const mock = new MockKernel({
      policy: { kind: "faux-shell" },
      emitSplashOnFirstInput: true,
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    mock.injectInput(
      DEV_CONSOLE_NODE,
      new TextEncoder().encode("echo hello\n"),
    );
    const consoleCalls = scaffold.calls.filter(
      (c) => c.driverId === CONSOLE_DRIVER_ID,
    );
    expect(consoleCalls).toHaveLength(1);
    expect(consoleCalls[0]?.op).toBe(OP_WRITE_LINE);
    expect(new TextDecoder().decode(consoleCalls[0]?.payload)).toBe("hello\n");
  });

  it("skips the splash when no scaffold is bound", () => {
    const mock = new MockKernel({
      policy: { kind: "echo" },
      emitSplashOnFirstInput: true,
    });
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("stray\n"));
    // No scaffold => nothing to verify, but the mock must
    // not throw.
  });

  it("generateSplashPixels produces width*height*4 bytes", () => {
    const rgba = generateSplashPixels(8, 4);
    expect(rgba.byteLength).toBe(8 * 4 * 4);
    // The top-left pixel should differ from the center pixel
    // (center is brighter under the radial gradient).
    const cornerG = rgba[1] ?? 0;
    const centerIdx = (2 * 8 + 4) * 4;
    const centerG = rgba[centerIdx + 1] ?? 0;
    expect(centerG).toBeGreaterThan(cornerG);
    // Every alpha byte is 255.
    for (let i = 3; i < rgba.byteLength; i += 4) {
      expect(rgba[i]).toBe(255);
    }
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

  it("'help' prints the full command list", async () => {
    const { FAUX_SHELL_HELP } = await import("../../src/mock-kernel");
    const out = fauxShellTransform(new TextEncoder().encode("help\n"));
    const text = new TextDecoder().decode(out);
    expect(text.endsWith("\n")).toBe(true);
    // Every line from FAUX_SHELL_HELP appears in the output.
    for (const line of FAUX_SHELL_HELP) {
      expect(text).toContain(line);
    }
  });

  it("'date' prints a fixed date string", () => {
    const out = fauxShellTransform(new TextEncoder().encode("date\n"));
    expect(new TextDecoder().decode(out)).toBe("2026-04-14\n");
  });

  it("'whoami' prints 'pmos'", () => {
    const out = fauxShellTransform(new TextEncoder().encode("whoami\n"));
    expect(new TextDecoder().decode(out)).toBe("pmos\n");
  });

  it("'uname' prints the system banner", () => {
    const out = fauxShellTransform(new TextEncoder().encode("uname\n"));
    expect(new TextDecoder().decode(out)).toBe("PMos 0.1.0-demo\n");
  });

  it("commands are case-sensitive: 'HELP' is unknown", () => {
    const out = fauxShellTransform(new TextEncoder().encode("HELP\n"));
    expect(new TextDecoder().decode(out)).toBe("?\n");
  });
});

describe("MockKernel panic command", () => {
  it("'panic <message>' calls the configured panicEmit sink", () => {
    const panics: string[] = [];
    const mock = new MockKernel({
      policy: { kind: "faux-shell" },
      panicEmit: (m) => panics.push(m),
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);

    mock.injectInput(
      DEV_CONSOLE_NODE,
      new TextEncoder().encode("panic kernel exploded\n"),
    );

    expect(panics).toEqual(["kernel: kernel exploded"]);
    // Panic short-circuits — no console:write goes out.
    const consoleCalls = scaffold.calls.filter(
      (c) => c.driverId === CONSOLE_DRIVER_ID,
    );
    expect(consoleCalls).toHaveLength(0);
  });

  it("'panic' with no message emits a default notice", () => {
    const panics: string[] = [];
    const mock = new MockKernel({
      policy: { kind: "faux-shell" },
      panicEmit: (m) => panics.push(m),
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("panic\n"));
    expect(panics).toHaveLength(1);
    expect(panics[0]).toContain("no message");
  });

  it("'panic' command in echo policy mode also fires the sink", () => {
    // The panic interceptor lives at the line-flush
    // layer, not inside the policy switch, so it works
    // regardless of which policy is active.
    const panics: string[] = [];
    const mock = new MockKernel({
      policy: { kind: "echo" },
      panicEmit: (m) => panics.push(m),
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("panic boom\n"));
    expect(panics).toEqual(["kernel: boom"]);
  });

  it("without a panicEmit sink, 'panic' is silently swallowed", () => {
    const mock = new MockKernel({
      policy: { kind: "faux-shell" },
      // No panicEmit configured.
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("panic test\n"));
    // No console:write either — panic short-circuits.
    expect(scaffold.calls.filter((c) => c.driverId === CONSOLE_DRIVER_ID)).toHaveLength(0);
  });

  it("a normal command after a panic still works", () => {
    const panics: string[] = [];
    const mock = new MockKernel({
      policy: { kind: "faux-shell" },
      panicEmit: (m) => panics.push(m),
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);

    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("panic first\n"));
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("echo hello\n"));

    expect(panics).toEqual(["kernel: first"]);
    const writes = scaffold.calls.filter((c) => c.driverId === CONSOLE_DRIVER_ID);
    expect(writes).toHaveLength(1);
    expect(new TextDecoder().decode(writes[0]?.payload)).toBe("hello\n");
  });

  it("FAUX_SHELL_HELP advertises the panic command", async () => {
    const { FAUX_SHELL_HELP } = await import("../../src/mock-kernel");
    const text = FAUX_SHELL_HELP.join("\n");
    expect(text).toContain("panic");
  });
});
