// Unit tests for kernel-worker-entry.ts — the glue between a
// dedicated Worker's messaging interface and the kernel-worker
// scaffold.
//
// Uses a `FakeWorkerMessaging` object instead of a real Worker
// so the boot sequence is driven deterministically.

import { describe, expect, it } from "vitest";
import { installWorkerEntry } from "../../src/kernel-worker-entry";
import type { WorkerMessaging } from "../../src/kernel-worker-entry";
import type { KernelToMain, MainToKernel } from "../../src/shared/worker-proto";

interface FakeMessaging extends WorkerMessaging {
  readonly posted: KernelToMain[];
  /** Push a main-thread-originated message to the Worker. */
  send(msg: MainToKernel): void;
}

function makeMessaging(): FakeMessaging {
  const posted: KernelToMain[] = [];
  const fake: FakeMessaging = {
    onmessage: null,
    posted,
    postMessage(msg: KernelToMain): void {
      posted.push(msg);
    },
    send(msg: MainToKernel): void {
      fake.onmessage?.({ data: msg });
    },
  };
  return fake;
}

describe("installWorkerEntry", () => {
  it("installs an onmessage handler before any messages arrive", () => {
    const msg = makeMessaging();
    installWorkerEntry(msg);
    expect(msg.onmessage).not.toBeNull();
  });

  it("pre-boot non-boot messages trigger a panic reply", () => {
    const msg = makeMessaging();
    installWorkerEntry(msg);
    msg.send({ kind: "shutdown" });
    expect(msg.posted).toHaveLength(1);
    const p = msg.posted[0];
    expect(p?.kind).toBe("panic");
    if (p && p.kind === "panic") {
      expect(p.message).toMatch(/before boot/);
    }
  });

  it("boot message spins up the scaffold and emits 'ready'", () => {
    const msg = makeMessaging();
    const entry = installWorkerEntry(msg);
    expect(entry.scaffold).toBeUndefined();
    msg.send({
      kind: "boot",
      config: { enableConsole: true, enableInput: false, enableFramebuffer: false },
    });
    expect(entry.scaffold).toBeDefined();
    expect(entry.scaffold?.driverCount).toBe(1);
    expect(msg.posted).toEqual([{ kind: "ready" }]);
  });

  it("a second boot after the first posts a panic but keeps the scaffold alive", () => {
    const msg = makeMessaging();
    const entry = installWorkerEntry(msg);
    msg.send({
      kind: "boot",
      config: { enableConsole: true, enableInput: false, enableFramebuffer: false },
    });
    const firstScaffold = entry.scaffold;
    msg.send({
      kind: "boot",
      config: { enableConsole: true, enableInput: true, enableFramebuffer: false },
    });
    // The scaffold itself posts the "already booted" panic.
    const panic = msg.posted.find((m) => m.kind === "panic");
    expect(panic).toBeDefined();
    // The original scaffold is still in place.
    expect(entry.scaffold).toBe(firstScaffold);
  });

  it("post-boot console:input round-trips through the faux shell back out as console:write", () => {
    // This is the TS equivalent of the Rust-side
    // principle_viii_headless_shell_gate, driven entirely
    // through the Worker messaging interface. A
    // `console:input` message of "echo hello\n" arrives at
    // the Worker; the mock kernel's faux-shell policy
    // rewrites it to "hello\n"; the scaffold's console
    // driver posts it back out as a `console:write`.
    const msg = makeMessaging();
    installWorkerEntry(msg);
    msg.send({
      kind: "boot",
      config: { enableConsole: true, enableInput: false, enableFramebuffer: false },
    });
    msg.send({
      kind: "console:input",
      bytes: new TextEncoder().encode("echo hello\n"),
    });
    const writes = msg.posted.filter((m) => m.kind === "console:write");
    expect(writes).toHaveLength(1);
    const w = writes[0];
    if (w && w.kind === "console:write") {
      expect(new TextDecoder().decode(w.bytes)).toBe("hello\n");
    }
  });

  it("post-boot shutdown message clears registered drivers", () => {
    const msg = makeMessaging();
    const entry = installWorkerEntry(msg);
    msg.send({
      kind: "boot",
      config: { enableConsole: true, enableInput: true, enableFramebuffer: false },
    });
    expect(entry.scaffold?.driverCount).toBe(2);
    msg.send({ kind: "shutdown" });
    expect(entry.scaffold?.driverCount).toBe(0);
  });

  it("unknown command produces '?\\n' through the faux shell", () => {
    const msg = makeMessaging();
    installWorkerEntry(msg);
    msg.send({
      kind: "boot",
      config: { enableConsole: true, enableInput: false, enableFramebuffer: false },
    });
    msg.send({
      kind: "console:input",
      bytes: new TextEncoder().encode("nope\n"),
    });
    const writes = msg.posted.filter((m) => m.kind === "console:write");
    expect(writes).toHaveLength(1);
    const w = writes[0];
    if (w && w.kind === "console:write") {
      expect(new TextDecoder().decode(w.bytes)).toBe("?\n");
    }
  });

  it("boot with enableFramebuffer=true emits an fb:set-mode + fb:blit splash on first input", () => {
    const msg = makeMessaging();
    installWorkerEntry(msg);
    msg.send({
      kind: "boot",
      config: { enableConsole: true, enableInput: false, enableFramebuffer: true },
    });
    // Before the first input the splash has not been emitted.
    expect(msg.posted.some((m) => m.kind === "fb:set-mode")).toBe(false);
    expect(msg.posted.some((m) => m.kind === "fb:blit")).toBe(false);

    msg.send({
      kind: "console:input",
      bytes: new TextEncoder().encode("echo hello\n"),
    });

    // Splash arrived on the main thread.
    const setMode = msg.posted.find((m) => m.kind === "fb:set-mode");
    const blit = msg.posted.find((m) => m.kind === "fb:blit");
    expect(setMode).toBeDefined();
    expect(blit).toBeDefined();
    if (setMode && setMode.kind === "fb:set-mode") {
      expect(setMode.width).toBeGreaterThan(0);
      expect(setMode.height).toBeGreaterThan(0);
    }
    if (blit && blit.kind === "fb:blit") {
      expect(blit.rgba.byteLength).toBe(blit.width * blit.height * 4);
    }

    // The echo response is still delivered alongside the splash.
    const write = msg.posted.find((m) => m.kind === "console:write");
    expect(write).toBeDefined();
    if (write && write.kind === "console:write") {
      expect(new TextDecoder().decode(write.bytes)).toBe("hello\n");
    }
  });

  it("boot with enableFramebuffer=false does NOT emit a splash on first input", () => {
    const msg = makeMessaging();
    installWorkerEntry(msg);
    msg.send({
      kind: "boot",
      config: { enableConsole: true, enableInput: false, enableFramebuffer: false },
    });
    msg.send({
      kind: "console:input",
      bytes: new TextEncoder().encode("echo hi\n"),
    });
    expect(msg.posted.some((m) => m.kind === "fb:set-mode")).toBe(false);
    expect(msg.posted.some((m) => m.kind === "fb:blit")).toBe(false);
  });
});
