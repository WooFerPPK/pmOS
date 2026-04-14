// Unit tests for the kernel-worker scaffold.
//
// Drives `bootKernelWorker` against a MockKernel that just
// captures `injectInput` calls, and a captured `postToMain`
// that records every outbound message. Covers:
//
//   * boot-time registration of the console driver
//   * `callDriver` routing for registered and unknown devIds
//   * main-message dispatch (console:input, shutdown, re-boot)
//   * the `ready` announce message
//
// Together with `console-driver.test.ts`, this is the TS-side
// equivalent of the kernel's sys.rs tests for the T077 gate:
// end-to-end bytes flow from a main-thread input message,
// through the scaffold, into the console driver, into the
// (mock) kernel's input ring — and from a (synthetic) kernel
// `callDriver` invocation, through the console driver, out to
// the captured main-thread message queue.

import { describe, expect, it } from "vitest";
import { bootKernelWorker } from "../../src/kernel-worker";
import type { Kernel } from "../../src/kernel-worker";
import { DEV_CONSOLE, OP_WRITE_LINE } from "../../src/drivers/console";
import { DriverErrorCode } from "../../src/drivers/types";
import type { KernelToMain } from "../../src/shared/worker-proto";

interface MockKernel extends Kernel {
  readonly injected: Array<{ devnum: number; bytes: Uint8Array }>;
}

function makeMockKernel(): MockKernel {
  const injected: Array<{ devnum: number; bytes: Uint8Array }> = [];
  return {
    injected,
    injectInput(devnum: number, bytes: Uint8Array): void {
      injected.push({ devnum, bytes });
    },
  };
}

interface MainCapture {
  readonly messages: KernelToMain[];
  readonly postToMain: (msg: KernelToMain) => void;
}

function captureMain(): MainCapture {
  const messages: KernelToMain[] = [];
  return {
    messages,
    postToMain(msg: KernelToMain): void {
      messages.push(msg);
    },
  };
}

describe("bootKernelWorker", () => {
  it("posts a 'ready' message to main on boot", () => {
    const kernel = makeMockKernel();
    const main = captureMain();
    bootKernelWorker({
      kernel,
      config: { enableConsole: true },
      postToMain: main.postToMain,
    });
    expect(main.messages).toEqual([{ kind: "ready" }]);
  });

  it("registers the console driver when enableConsole is true", () => {
    const kernel = makeMockKernel();
    const main = captureMain();
    const kw = bootKernelWorker({
      kernel,
      config: { enableConsole: true },
      postToMain: main.postToMain,
    });
    expect(kw.driverCount).toBe(1);
  });

  it("does not register the console driver when enableConsole is false", () => {
    const kernel = makeMockKernel();
    const main = captureMain();
    const kw = bootKernelWorker({
      kernel,
      config: { enableConsole: false },
      postToMain: main.postToMain,
    });
    expect(kw.driverCount).toBe(0);
  });

  it("callDriver routes OP_WRITE_LINE to the console driver", () => {
    const kernel = makeMockKernel();
    const main = captureMain();
    const kw = bootKernelWorker({
      kernel,
      config: { enableConsole: true },
      postToMain: main.postToMain,
    });
    const result = kw.callDriver(
      DEV_CONSOLE,
      OP_WRITE_LINE,
      new TextEncoder().encode("hello\n"),
    );
    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.value).toBe(6);
    }
    // Main received 'ready' first, then the driver's 'console:write'.
    expect(main.messages).toHaveLength(2);
    const second = main.messages[1];
    expect(second).toMatchObject({ kind: "console:write" });
    if (second && second.kind === "console:write") {
      expect(new TextDecoder().decode(second.bytes)).toBe("hello\n");
    }
  });

  it("callDriver on an unknown devId returns NotReady", () => {
    const kernel = makeMockKernel();
    const main = captureMain();
    const kw = bootKernelWorker({
      kernel,
      config: { enableConsole: false },
      postToMain: main.postToMain,
    });
    const result = kw.callDriver(99, 0, new Uint8Array(0));
    expect(result).toEqual({ ok: false, error: DriverErrorCode.NotReady });
  });

  it("callDriver with enableConsole:false returns NotReady for DEV_CONSOLE", () => {
    const kernel = makeMockKernel();
    const main = captureMain();
    const kw = bootKernelWorker({
      kernel,
      config: { enableConsole: false },
      postToMain: main.postToMain,
    });
    const result = kw.callDriver(DEV_CONSOLE, OP_WRITE_LINE, new Uint8Array(0));
    expect(result).toEqual({ ok: false, error: DriverErrorCode.NotReady });
  });

  it("handleMainMessage({console:input}) reaches the kernel via the console driver", () => {
    const kernel = makeMockKernel();
    const main = captureMain();
    const kw = bootKernelWorker({
      kernel,
      config: { enableConsole: true },
      postToMain: main.postToMain,
    });
    const bytes = new TextEncoder().encode("ls\n");
    kw.handleMainMessage({ kind: "console:input", bytes });

    expect(kernel.injected).toHaveLength(1);
    expect(kernel.injected[0]?.devnum).toBe(DEV_CONSOLE);
    expect(new TextDecoder().decode(kernel.injected[0]?.bytes)).toBe("ls\n");
  });

  it("handleMainMessage({console:input}) is a silent no-op when the driver isn't registered", () => {
    const kernel = makeMockKernel();
    const main = captureMain();
    const kw = bootKernelWorker({
      kernel,
      config: { enableConsole: false },
      postToMain: main.postToMain,
    });
    kw.handleMainMessage({
      kind: "console:input",
      bytes: new TextEncoder().encode("ignored\n"),
    });
    expect(kernel.injected).toHaveLength(0);
  });

  it("handleMainMessage({boot}) while already booted posts a panic but does not throw", () => {
    const kernel = makeMockKernel();
    const main = captureMain();
    const kw = bootKernelWorker({
      kernel,
      config: { enableConsole: true },
      postToMain: main.postToMain,
    });
    kw.handleMainMessage({ kind: "boot", config: { enableConsole: true } });
    const panic = main.messages.find((m) => m.kind === "panic");
    expect(panic).toBeDefined();
    if (panic && panic.kind === "panic") {
      expect(panic.message).toMatch(/already booted/);
    }
  });

  it("handleMainMessage({shutdown}) clears registered drivers", () => {
    const kernel = makeMockKernel();
    const main = captureMain();
    const kw = bootKernelWorker({
      kernel,
      config: { enableConsole: true },
      postToMain: main.postToMain,
    });
    expect(kw.driverCount).toBe(1);
    kw.handleMainMessage({ kind: "shutdown" });
    expect(kw.driverCount).toBe(0);
    // callDriver on the now-unregistered driver reports NotReady.
    const result = kw.callDriver(DEV_CONSOLE, OP_WRITE_LINE, new Uint8Array(0));
    expect(result.ok).toBe(false);
  });

  it("round-trip: inject input, callDriver writes output, both flow through the scaffold", () => {
    // This is the TS equivalent of the kernel's
    // `principle_viii_headless_shell_gate`: bytes from the
    // main thread reach the kernel, and bytes from the kernel
    // reach the main thread, via the same scaffold + driver.
    const kernel = makeMockKernel();
    const main = captureMain();
    const kw = bootKernelWorker({
      kernel,
      config: { enableConsole: true },
      postToMain: main.postToMain,
    });

    // Main thread types "echo hello\n".
    kw.handleMainMessage({
      kind: "console:input",
      bytes: new TextEncoder().encode("echo hello\n"),
    });
    expect(kernel.injected).toHaveLength(1);
    expect(new TextDecoder().decode(kernel.injected[0]?.bytes)).toBe("echo hello\n");

    // The (synthetic) kernel has written "hello\n" back to
    // DEV_CONSOLE via its output pipeline.
    const result = kw.callDriver(
      DEV_CONSOLE,
      OP_WRITE_LINE,
      new TextEncoder().encode("hello\n"),
    );
    expect(result.ok).toBe(true);

    // Main thread received: ready, console:write.
    const writes = main.messages.filter((m) => m.kind === "console:write");
    expect(writes).toHaveLength(1);
    const w = writes[0];
    if (w && w.kind === "console:write") {
      expect(new TextDecoder().decode(w.bytes)).toBe("hello\n");
    }
  });
});
