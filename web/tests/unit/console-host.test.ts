// Unit tests for the main-thread `ConsoleHost` wrapper.
//
// Drives the host against a `FakeWorker` that captures every
// `postMessage` and lets the test inject kernel-to-main
// messages as if they came from a real Worker.

import { describe, expect, it } from "vitest";
import { ConsoleHost } from "../../src/console-host";
import type { WorkerLike } from "../../src/console-host";
import type { KernelToMain, MainToKernel } from "../../src/shared/worker-proto";

interface FakeWorker extends WorkerLike {
  readonly posted: MainToKernel[];
  readonly terminated: boolean;
  /** Inject a message as if the Worker had sent it. */
  emit(msg: KernelToMain): void;
}

function makeFakeWorker(): FakeWorker {
  const posted: MainToKernel[] = [];
  let handler: ((ev: { data: KernelToMain }) => void) | null = null;
  const state: { terminated: boolean } = { terminated: false };
  return {
    posted,
    get terminated(): boolean {
      return state.terminated;
    },
    postMessage(msg: MainToKernel): void {
      posted.push(msg);
    },
    addEventListener(
      _type: "message",
      h: (ev: { data: KernelToMain }) => void,
    ): void {
      handler = h;
    },
    terminate(): void {
      state.terminated = true;
    },
    emit(msg: KernelToMain): void {
      handler?.({ data: msg });
    },
  };
}

describe("ConsoleHost", () => {
  it("posts a boot message on construction with the provided config", () => {
    const w = makeFakeWorker();
    new ConsoleHost({
      worker: w,
      bootConfig: { enableConsole: true, enableInput: false, enableFramebuffer: false },
    });
    expect(w.posted).toHaveLength(1);
    expect(w.posted[0]).toEqual({
      kind: "boot",
      config: { enableConsole: true, enableInput: false, enableFramebuffer: false },
    });
  });

  it("is not ready until a 'ready' message arrives", () => {
    const w = makeFakeWorker();
    const host = new ConsoleHost({
      worker: w,
      bootConfig: { enableConsole: true, enableInput: false, enableFramebuffer: false },
    });
    expect(host.ready).toBe(false);
    w.emit({ kind: "ready" });
    expect(host.ready).toBe(true);
  });

  it("fires lifecycle handlers on ready", () => {
    const w = makeFakeWorker();
    const host = new ConsoleHost({
      worker: w,
      bootConfig: { enableConsole: true, enableInput: false, enableFramebuffer: false },
    });
    const events: string[] = [];
    host.onLifecycle((e) => events.push(e.kind));
    w.emit({ kind: "ready" });
    expect(events).toEqual(["ready"]);
  });

  it("fans out console:write bytes to every output handler in registration order", () => {
    const w = makeFakeWorker();
    const host = new ConsoleHost({
      worker: w,
      bootConfig: { enableConsole: true, enableInput: false, enableFramebuffer: false },
    });
    const calls: string[] = [];
    host.onOutput((b) => calls.push(`A:${new TextDecoder().decode(b)}`));
    host.onOutput((b) => calls.push(`B:${new TextDecoder().decode(b)}`));
    w.emit({ kind: "ready" });
    w.emit({ kind: "console:write", bytes: new TextEncoder().encode("hi\n") });
    expect(calls).toEqual(["A:hi\n", "B:hi\n"]);
  });

  it("queues input sent before ready and flushes it on ready", () => {
    const w = makeFakeWorker();
    const host = new ConsoleHost({
      worker: w,
      bootConfig: { enableConsole: true, enableInput: false, enableFramebuffer: false },
    });
    host.sendLine("early\n");
    // Only the boot message is posted — no console:input yet.
    expect(w.posted).toHaveLength(1);

    w.emit({ kind: "ready" });
    // Queued input flushed after ready.
    expect(w.posted).toHaveLength(2);
    const second = w.posted[1];
    expect(second?.kind).toBe("console:input");
    if (second && second.kind === "console:input") {
      expect(new TextDecoder().decode(second.bytes)).toBe("early\n");
    }
  });

  it("sends input immediately after ready without queuing", () => {
    const w = makeFakeWorker();
    const host = new ConsoleHost({
      worker: w,
      bootConfig: { enableConsole: true, enableInput: false, enableFramebuffer: false },
    });
    w.emit({ kind: "ready" });
    host.sendLine("late\n");
    expect(w.posted).toHaveLength(2);
    const second = w.posted[1];
    if (second && second.kind === "console:input") {
      expect(new TextDecoder().decode(second.bytes)).toBe("late\n");
    }
  });

  it("copies queued input defensively so callers can reuse their buffer", () => {
    const w = makeFakeWorker();
    const host = new ConsoleHost({
      worker: w,
      bootConfig: { enableConsole: true, enableInput: false, enableFramebuffer: false },
    });
    const buf = new Uint8Array([0x41, 0x42, 0x0a]);
    host.sendInput(buf);
    buf[0] = 0xff;
    w.emit({ kind: "ready" });
    const posted = w.posted[1];
    if (posted && posted.kind === "console:input") {
      // Mutating the caller's buffer did NOT affect the queued copy.
      expect(posted.bytes[0]).toBe(0x41);
    }
  });

  it("fires lifecycle panic handlers and resets ready on panic", () => {
    const w = makeFakeWorker();
    const host = new ConsoleHost({
      worker: w,
      bootConfig: { enableConsole: true, enableInput: false, enableFramebuffer: false },
    });
    w.emit({ kind: "ready" });
    expect(host.ready).toBe(true);

    const events: string[] = [];
    host.onLifecycle((e) => {
      if (e.kind === "panic") {
        events.push(`panic:${e.message}`);
      }
    });
    w.emit({ kind: "panic", message: "kernel unreachable" });
    expect(host.ready).toBe(false);
    expect(events).toEqual(["panic:kernel unreachable"]);
  });

  it("shutdown posts a shutdown message and terminates the worker", () => {
    const w = makeFakeWorker();
    const host = new ConsoleHost({
      worker: w,
      bootConfig: { enableConsole: true, enableInput: false, enableFramebuffer: false },
    });
    w.emit({ kind: "ready" });
    host.shutdown();
    const last = w.posted[w.posted.length - 1];
    expect(last?.kind).toBe("shutdown");
    expect(w.terminated).toBe(true);
    expect(host.ready).toBe(false);
  });

  it("sendInput after shutdown is a silent no-op", () => {
    const w = makeFakeWorker();
    const host = new ConsoleHost({
      worker: w,
      bootConfig: { enableConsole: true, enableInput: false, enableFramebuffer: false },
    });
    w.emit({ kind: "ready" });
    host.shutdown();
    const postedBefore = w.posted.length;
    host.sendLine("after-shutdown\n");
    expect(w.posted.length).toBe(postedBefore);
  });

  it("double shutdown is idempotent (no extra messages posted)", () => {
    const w = makeFakeWorker();
    const host = new ConsoleHost({
      worker: w,
      bootConfig: { enableConsole: true, enableInput: false, enableFramebuffer: false },
    });
    w.emit({ kind: "ready" });
    host.shutdown();
    const postedAfterFirst = w.posted.length;
    host.shutdown();
    expect(w.posted.length).toBe(postedAfterFirst);
  });

  it("full round-trip: send a line, kernel echoes it, main-thread handler observes the response", () => {
    // This is the main-thread-side analogue of the Rust-side
    // principle_viii_headless_shell_gate — with a fake
    // Worker standing in for the real one.
    const w = makeFakeWorker();
    const host = new ConsoleHost({
      worker: w,
      bootConfig: { enableConsole: true, enableInput: false, enableFramebuffer: false },
    });
    const received: string[] = [];
    host.onOutput((b) => received.push(new TextDecoder().decode(b)));
    w.emit({ kind: "ready" });

    host.sendLine("echo hello\n");
    // Simulate the kernel worker's response.
    w.emit({
      kind: "console:write",
      bytes: new TextEncoder().encode("hello\n"),
    });
    expect(received).toEqual(["hello\n"]);
  });
});
