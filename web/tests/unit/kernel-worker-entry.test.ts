// Unit tests for kernel-worker-entry.ts — the glue between a
// dedicated Worker's messaging interface and the kernel-worker
// scaffold.
//
// Uses a `FakeWorkerMessaging` object instead of a real Worker
// so the boot sequence is driven deterministically.

import fs from "node:fs";
import path from "node:path";
import { beforeAll, describe, expect, it } from "vitest";
import { installWorkerEntry } from "../../src/kernel-worker-entry";
import type { WorkerMessaging } from "../../src/kernel-worker-entry";
import type { KernelToMain, MainToKernel } from "../../src/shared/worker-proto";
import {
  MouseButton,
  MouseButtonState,
  packMouseButton,
  packMouseMotion,
} from "../../src/shared/input-proto";
import { CAPSET_ALL, OP_WASI } from "../../src/shared/syscall";

let helloStdWasmBytes: ArrayBuffer;

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

  it("'panic <message>' typed at the faux shell flows through as a panic event", () => {
    const msg = makeMessaging();
    installWorkerEntry(msg);
    msg.send({
      kind: "boot",
      config: { enableConsole: true, enableInput: false, enableFramebuffer: false },
    });
    msg.send({
      kind: "console:input",
      bytes: new TextEncoder().encode("panic kernel exploded\n"),
    });

    const panic = msg.posted.find((m) => m.kind === "panic");
    expect(panic).toBeDefined();
    if (panic && panic.kind === "panic") {
      expect(panic.message).toContain("kernel exploded");
    }
    // No console:write was produced — panic short-circuits.
    expect(msg.posted.some((m) => m.kind === "console:write")).toBe(false);
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

  it("input:mouse messages flow through the scaffold into the mock kernel's pointer state", () => {
    // End-to-end main-thread → worker → scaffold → input
    // driver → mock kernel. Send a motion event, then a
    // button event, then drain the mock's pointer state
    // via `entry.scaffold` is not sufficient here — the
    // scaffold exposes a Kernel interface, not internal
    // MockKernel state. For this test we check that the
    // scaffold accepted the messages without panicking
    // AND that no errors were posted back.
    const msg = makeMessaging();
    installWorkerEntry(msg);
    msg.send({
      kind: "boot",
      config: {
        enableConsole: true,
        enableInput: true,
        enableFramebuffer: false,
      },
    });
    // Pre-count any posted messages from boot (ready, etc.)
    // so later assertions only see what input events produce.
    const panicsBefore = msg.posted.filter((m) => m.kind === "panic").length;

    msg.send({ kind: "input:mouse", bytes: packMouseMotion(100, 50) });
    msg.send({
      kind: "input:mouse",
      bytes: packMouseButton(
        100,
        50,
        MouseButton.Left,
        MouseButtonState.Pressed,
      ),
    });

    // No panic was posted — the scaffold routed the
    // messages through the input driver cleanly.
    const panicsAfter = msg.posted.filter((m) => m.kind === "panic").length;
    expect(panicsAfter).toBe(panicsBefore);
  });

  it("input:mouse messages do not post fb blits when the framebuffer is off", () => {
    const msg = makeMessaging();
    installWorkerEntry(msg);
    msg.send({
      kind: "boot",
      config: {
        enableConsole: true,
        enableInput: true,
        enableFramebuffer: false,
      },
    });
    msg.send({ kind: "input:mouse", bytes: packMouseMotion(10, 10) });
    expect(msg.posted.some((m) => m.kind === "fb:blit")).toBe(false);
  });

  it("live-terminal mode blits once on every input:mouse motion event", () => {
    const msg = makeMessaging();
    installWorkerEntry(msg);
    msg.send({
      kind: "boot",
      config: {
        enableConsole: true,
        enableInput: true,
        enableFramebuffer: true,
        liveTerminal: true,
        terminalBanner: ["ready"],
      },
    });
    // Baseline: SET_MODE + initial blit from bindScaffold.
    const baseBlits = msg.posted.filter((m) => m.kind === "fb:blit").length;
    msg.send({ kind: "input:mouse", bytes: packMouseMotion(1, 1) });
    const after = msg.posted.filter((m) => m.kind === "fb:blit").length;
    expect(after - baseBlits).toBe(1);
  });
});

// ---- useRealKernel: KernelWasmHost as the production kernel -----------
//
// When the boot config carries `useRealKernel: true`, the entry
// constructs a `KernelWasmHost` from the real `kernel.wasm` cdylib
// instead of a `MockKernel`. Production fetches the wasm at Worker
// scope; tests inject the bytes via the optional second argument to
// `installWorkerEntry` so the boot path is exercised without a real
// fetch.

let kernelWasmBytes: ArrayBuffer;

beforeAll(() => {
  const wasmPath = path.resolve(
    __dirname,
    "../../../target/wasm32-unknown-unknown/release/kernel.wasm",
  );
  if (!fs.existsSync(wasmPath)) {
    throw new Error(
      `kernel.wasm not found at ${wasmPath}. Run \`just build\` first.`,
    );
  }
  const raw = fs.readFileSync(wasmPath);
  kernelWasmBytes = raw.buffer.slice(
    raw.byteOffset,
    raw.byteOffset + raw.byteLength,
  ) as ArrayBuffer;

  const helloPath = path.resolve(
    __dirname,
    "../../../target/wasm32-wasip1/release/hello-std.wasm",
  );
  if (!fs.existsSync(helloPath)) {
    throw new Error(
      `hello-std.wasm not found at ${helloPath}. Run \`just build\` first.`,
    );
  }
  const helloRaw = fs.readFileSync(helloPath);
  helloStdWasmBytes = helloRaw.buffer.slice(
    helloRaw.byteOffset,
    helloRaw.byteOffset + helloRaw.byteLength,
  ) as ArrayBuffer;
});

describe("installWorkerEntry with useRealKernel", () => {
  it("constructs a KernelWasmHost asynchronously and posts ready once initialised", async () => {
    const msg = makeMessaging();
    const entry = installWorkerEntry(msg, { kernelWasmBytes });
    msg.send({
      kind: "boot",
      config: {
        enableConsole: true,
        enableInput: false,
        enableFramebuffer: false,
        useRealKernel: true,
      },
    });
    // KernelWasmHost.create is async, so the scaffold is not yet
    // bound the moment `boot` returns.
    expect(entry.scaffold).toBeUndefined();
    await entry.whenReady;
    expect(entry.scaffold).toBeDefined();
    expect(entry.scaffold?.driverCount).toBe(1);
    const readyCount = msg.posted.filter((m) => m.kind === "ready").length;
    expect(readyCount).toBe(1);
  });

  it("routes a real-kernel FD_WRITE to /dev/console as a console:write postMessage", async () => {
    const msg = makeMessaging();
    const entry = installWorkerEntry(msg, { kernelWasmBytes });
    msg.send({
      kind: "boot",
      config: {
        enableConsole: true,
        enableInput: false,
        enableFramebuffer: false,
        useRealKernel: true,
      },
    });
    await entry.whenReady;
    const host = entry.realKernel;
    expect(host).toBeDefined();
    if (!host) return;

    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const message = new TextEncoder().encode("hello from slice 2\n");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.FD_WRITE,
        requestId: 1,
        arg0: 1,
        heapPtr: 0,
        heapLen: message.length,
      },
      message,
    );
    expect(response!.status).toBe(0);

    const writes = msg.posted.filter((m) => m.kind === "console:write");
    expect(writes).toHaveLength(1);
    if (writes[0]?.kind === "console:write") {
      expect(new TextDecoder().decode(writes[0].bytes)).toBe(
        "hello from slice 2\n",
      );
    }
  });

  it("falls back to the injected fetcher to load /assets/kernel.wasm when kernelWasmBytes is absent", async () => {
    const msg = makeMessaging();
    const fetched: string[] = [];
    const entry = installWorkerEntry(msg, {
      fetcher: async (url: string): Promise<ArrayBuffer> => {
        fetched.push(url);
        if (url === "/assets/kernel.wasm") return kernelWasmBytes;
        throw new Error(`unexpected fetch: ${url}`);
      },
    });
    msg.send({
      kind: "boot",
      config: {
        enableConsole: true,
        enableInput: false,
        enableFramebuffer: false,
        useRealKernel: true,
      },
    });
    // No bootBinary was configured, so no PROC_SPAWN + proc:spawn
    // fires. `runBootBinary` is skipped entirely; the entry's
    // `whenReady` resolves after `bootKernelWorker` posts `ready`.
    await entry.whenReady;
    expect(fetched).toEqual(["/assets/kernel.wasm"]);
    expect(entry.realKernel).toBeDefined();
    expect(msg.posted.filter((m) => m.kind === "ready")).toHaveLength(1);
  });

  it("populates the binary registry from /manifest.json when binaryRegistry is absent", async () => {
    const msg = makeMessaging();
    const manifest = {
      version: 0,
      assets: [
        "_headers",
        "assets/bin/hello-std.wasm",
        // Decoy assets the entry must IGNORE (kernel.wasm sits at
        // assets/, not assets/bin/; bootstrap.js is JS not wasm).
        "assets/bootstrap.js",
        "assets/kernel.wasm",
        "index.html",
      ],
    };
    const fetched: string[] = [];
    const entry = installWorkerEntry(msg, {
      fetcher: async (url: string): Promise<ArrayBuffer> => {
        fetched.push(url);
        if (url === "/assets/kernel.wasm") return kernelWasmBytes;
        if (url === "/manifest.json") {
          return new TextEncoder().encode(JSON.stringify(manifest)).buffer as ArrayBuffer;
        }
        if (url === "/assets/bin/hello-std.wasm") return helloStdWasmBytes;
        throw new Error(`unexpected fetch: ${url}`);
      },
    });
    msg.send({
      kind: "boot",
      config: {
        enableConsole: true,
        enableInput: false,
        enableFramebuffer: false,
        useRealKernel: true,
        bootBinary: "/bin/hello-std",
      },
    });

    // After T235 the kernel-worker-entry unconditionally routes
    // PROC_SPAWN through `proc:spawn` + awaits the main-thread
    // spawn router's `proc:sab` + `proc:exited` handshake. No real
    // user Worker runs in vitest, so we simulate main by pre-seeding
    // the SAB with a FD_WRITE of the expected "hello from std\n"
    // payload and posting proc:sab+proc:exited ourselves. The
    // kernel's dispatch loop services the ring the way it would for
    // a real user Worker, producing the same `console:write` the
    // pre-T235 in-process drain used to produce.
    await waitFor(() => msg.posted.some((m) => m.kind === "proc:spawn"));
    const spawn = msg.posted.find((m) => m.kind === "proc:spawn");
    if (!spawn || spawn.kind !== "proc:spawn") throw new Error("unreachable");
    expect(spawn.path).toBe("/bin/hello-std");
    const sab = new ArrayBuffer(SAB_SIZE);
    seedFdWriteOnce(sab, "hello from std\n", 1);
    msg.send({ kind: "proc:sab", pid: spawn.pid, sab });
    await waitFor(() =>
      msg.posted.some(
        (m) =>
          m.kind === "console:write" &&
          new TextDecoder().decode(m.bytes) === "hello from std\n",
      ),
    );
    msg.send({ kind: "proc:exited", pid: spawn.pid, code: 0 });
    await entry.whenReady;

    // Manifest was fetched; only the bin/*.wasm asset was followed
    // up with a binary fetch. bootstrap.js + decoys must not have
    // been pulled.
    expect(fetched).toContain("/manifest.json");
    expect(fetched).toContain("/assets/bin/hello-std.wasm");
    expect(fetched).not.toContain("/assets/bootstrap.js");

    // The seeded FD_WRITE emerged on the `console:write` channel
    // (byte-for-byte what a real hello-std run would produce).
    const writes = msg.posted
      .filter((m) => m.kind === "console:write")
      .flatMap((m) =>
        m.kind === "console:write" ? [new TextDecoder().decode(m.bytes)] : [],
      );
    expect(writes.join("")).toBe("hello from std\n");
  });

  it("auto-spawns the configured boot binary and routes its console output through the channel", async () => {
    const msg = makeMessaging();
    const registry = new Map<string, BufferSource>([
      ["/bin/hello-std", helloStdWasmBytes],
    ]);
    const entry = installWorkerEntry(msg, {
      kernelWasmBytes,
      binaryRegistry: registry,
    });
    msg.send({
      kind: "boot",
      config: {
        enableConsole: true,
        enableInput: false,
        enableFramebuffer: false,
        useRealKernel: true,
        bootBinary: "/bin/hello-std",
      },
    });

    // Same main-thread spawn-router simulation as the manifest test
    // above — the boot binary's PROC_SPAWN lands as `proc:spawn`,
    // the test hands the kernel a pre-seeded SAB with hello-std's
    // expected fd_write payload, and the dispatch loop services it.
    await waitFor(() => msg.posted.some((m) => m.kind === "proc:spawn"));
    const spawn = msg.posted.find((m) => m.kind === "proc:spawn");
    if (!spawn || spawn.kind !== "proc:spawn") throw new Error("unreachable");
    expect(spawn.path).toBe("/bin/hello-std");
    const sab = new ArrayBuffer(SAB_SIZE);
    seedFdWriteOnce(sab, "hello from std\n", 1);
    msg.send({ kind: "proc:sab", pid: spawn.pid, sab });
    await waitFor(() =>
      msg.posted.some(
        (m) =>
          m.kind === "console:write" &&
          new TextDecoder().decode(m.bytes) === "hello from std\n",
      ),
    );
    msg.send({ kind: "proc:exited", pid: spawn.pid, code: 0 });
    await entry.whenReady;

    // The boot binary's stdout flushed through the kernel and out
    // as a `console:write`. hello-std's payload is exactly
    // `"hello from std\n"`.
    const writes = msg.posted
      .filter((m) => m.kind === "console:write")
      .flatMap((m) =>
        m.kind === "console:write" ? [new TextDecoder().decode(m.bytes)] : [],
      );
    expect(writes.join("")).toBe("hello from std\n");

    // No panic was posted along the way.
    expect(msg.posted.some((m) => m.kind === "panic")).toBe(false);
  });

  it("falls back to MockKernel when useRealKernel is false (regression)", async () => {
    const msg = makeMessaging();
    const entry = installWorkerEntry(msg, { kernelWasmBytes });
    msg.send({
      kind: "boot",
      config: {
        enableConsole: true,
        enableInput: false,
        enableFramebuffer: false,
        useRealKernel: false,
      },
    });
    // MockKernel construction is synchronous: scaffold is ready
    // immediately, ready already posted.
    expect(entry.scaffold).toBeDefined();
    expect(msg.posted).toEqual([{ kind: "ready" }]);
    // whenReady still resolves cleanly.
    await entry.whenReady;
    expect(entry.scaffold?.driverCount).toBe(1);
  });

  it("forwards a sync:request to realKernel.syncAll() once the host is bound", async () => {
    const msg = makeMessaging();
    const entry = installWorkerEntry(msg, { kernelWasmBytes });
    msg.send({
      kind: "boot",
      config: {
        enableConsole: false,
        enableInput: false,
        enableFramebuffer: false,
        useRealKernel: true,
      },
    });
    await entry.whenReady;
    const host = entry.realKernel;
    expect(host).toBeDefined();
    if (!host) return;
    let calls = 0;
    const original = host.syncAll.bind(host);
    host.syncAll = (): boolean => {
      calls += 1;
      return original();
    };
    msg.send({ kind: "sync:request" });
    expect(calls).toBe(1);
  });

  it("ignores a sync:request before the real kernel finishes booting (no panic)", () => {
    const msg = makeMessaging();
    installWorkerEntry(msg, { kernelWasmBytes });
    // Don't send `boot` — sync:request should be dropped silently
    // because there is no kernel to flush yet.
    msg.send({ kind: "sync:request" });
    const panics = msg.posted.filter((m) => m.kind === "panic");
    expect(panics).toEqual([]);
  });
});

// ---- T233: kernel-Worker dispatch loop choreography -----------------
//
// These tests exercise the full spawn-via-proc:spawn path that lands
// in T233 (M1.4): the entry's `runBootBinary` collapses to a single
// in-process `PROC_SPAWN init + startDispatchLoop`, the default
// `onSpawnProcess` posts `proc:spawn` on the messaging channel
// instead of queuing an in-process spawn, and the entry's onmessage
// handler routes main-thread `proc:sab` / `proc:exited` messages
// into the dispatch loop's pidMap.
//
// FakeMessaging stands in for the kernel Worker's real
// `DedicatedWorkerGlobalScope`. Each test pre-seeds a SAB with the
// user's first syscall (since there's no real user Worker running
// against the SAB here), sends `proc:sab` to hand the SAB to the
// kernel, waits for the dispatch loop to service the pre-seeded
// request, and then sends `proc:exited` to drain the pid from the
// loop.

import {
  OFF_HEAP_SCRATCH,
  OFF_REQ_HEAD,
  OFF_REQ_RING,
  OFF_RES_HEAD,
  OFF_RES_RING,
  SAB_SIZE,
  SLOT_SIZE as SAB_SLOT_SIZE,
} from "../../src/shared/sab-layout";
import { decodeResponse, encodeRequest } from "../../src/shared/syscall";

/**
 * Seed one FD_WRITE-to-/dev/console request into the SAB's request
 * ring and advance REQ_HEAD to 1 so the dispatch loop sees it.
 * Returns the line of bytes that the kernel will end up writing out
 * as a `console:write`.
 */
function seedFdWriteOnce(sab: ArrayBuffer, text: string, requestId: number): Uint8Array {
  const line = new TextEncoder().encode(text);
  const reqBytes = encodeRequest({
    opcode: OP_WASI.FD_WRITE,
    requestId,
    arg0: 1,
    heapPtr: 0,
    heapLen: line.length,
  });
  new Uint8Array(sab, OFF_REQ_RING, SAB_SLOT_SIZE).set(reqBytes);
  new Uint8Array(sab, OFF_HEAP_SCRATCH, line.length).set(line);
  const header = new Int32Array(sab, 0, OFF_HEAP_SCRATCH / 4);
  Atomics.store(header, OFF_REQ_HEAD / 4, 1);
  return line;
}

/** Poll `predicate` at microtask + setTimeout cadence until it returns
 * truthy, then resolve. Used by the spawn-choreography tests to wait
 * for async-posted messages without blocking on a real `Atomics.wait`.
 * Throws after `timeoutMs` so a stuck test surfaces a useful error. */
async function waitFor(
  predicate: () => boolean,
  timeoutMs: number = 2000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() > deadline) {
      throw new Error(`waitFor: predicate did not become truthy within ${timeoutMs}ms`);
    }
    await new Promise<void>((resolve) => setTimeout(resolve, 1));
  }
}

describe("installWorkerEntry with useRealKernel + proc:spawn routing", () => {
  it("PROC_SPAWN init posts proc:spawn to the messaging channel and the dispatch loop services proc:sab-supplied SAB rings", async () => {
    const msg = makeMessaging();
    const registry = new Map<string, BufferSource>([
      ["/bin/hello-std", helloStdWasmBytes],
    ]);
    const entry = installWorkerEntry(msg, {
      kernelWasmBytes,
      binaryRegistry: registry,
    });
    msg.send({
      kind: "boot",
      config: {
        enableConsole: true,
        enableInput: false,
        enableFramebuffer: false,
        useRealKernel: true,
        bootBinary: "/bin/hello-std",
      },
    });

    // Step 1: wait for proc:spawn to arrive on the messaging channel.
    // The boot binary was spawned via in-process PROC_SPAWN dispatch;
    // the default onSpawnProcess posted proc:spawn to the channel.
    await waitFor(() => msg.posted.some((m) => m.kind === "proc:spawn"));
    const spawn = msg.posted.find((m) => m.kind === "proc:spawn");
    expect(spawn).toBeDefined();
    if (!spawn || spawn.kind !== "proc:spawn") throw new Error("unreachable");
    expect(spawn.path).toBe("/bin/hello-std");
    expect(spawn.pid).toBeGreaterThan(0);

    // Step 2: simulate main allocating a SAB and pre-seeding it with
    // a FD_WRITE request. Real main would post `boot {sab, ...}` to a
    // user Worker and the user wasm would drive the SAB; we skip the
    // user side and hand the kernel a SAB that already has work.
    const sab = new ArrayBuffer(SAB_SIZE);
    seedFdWriteOnce(sab, "hello from fake user\n", 42);

    msg.send({ kind: "proc:sab", pid: spawn.pid, sab });

    // Step 3: wait for the FD_WRITE to land — the kernel's dispatch
    // loop services the pid's ring and the kernel posts a console
    // line out through the existing `console:write` channel.
    await waitFor(() =>
      msg.posted.some((m) => {
        return (
          m.kind === "console:write" &&
          new TextDecoder().decode(m.bytes) === "hello from fake user\n"
        );
      }),
    );

    // The response slot is now populated in the SAB.
    const resBytes = new Uint8Array(
      new Uint8Array(sab, OFF_RES_RING, SAB_SLOT_SIZE),
    );
    const resp = decodeResponse(resBytes);
    expect(resp.requestId).toBe(42);
    expect(resp.status).toBe(0);
    const header = new Int32Array(sab, 0, OFF_HEAP_SCRATCH / 4);
    expect(Atomics.load(header, OFF_RES_HEAD / 4)).toBe(1);

    // Step 4: simulate the user Worker exiting. The kernel's
    // proc:exited handler drops the pid from the dispatch loop's
    // pidMap, and since it was the last pid the loop halts.
    msg.send({ kind: "proc:exited", pid: spawn.pid, code: 0 });

    await entry.whenReady;
    // No panic posted along the way.
    expect(msg.posted.some((m) => m.kind === "panic")).toBe(false);
  });

  it("proc:exited for an unknown pid is ignored; the loop keeps running until a real pid exits", async () => {
    const msg = makeMessaging();
    const registry = new Map<string, BufferSource>([
      ["/bin/hello-std", helloStdWasmBytes],
    ]);
    const entry = installWorkerEntry(msg, {
      kernelWasmBytes,
      binaryRegistry: registry,
    });
    msg.send({
      kind: "boot",
      config: {
        enableConsole: true,
        enableInput: false,
        enableFramebuffer: false,
        useRealKernel: true,
        bootBinary: "/bin/hello-std",
      },
    });

    await waitFor(() => msg.posted.some((m) => m.kind === "proc:spawn"));
    const spawn = msg.posted.find((m) => m.kind === "proc:spawn");
    if (!spawn || spawn.kind !== "proc:spawn") throw new Error("unreachable");

    // Spurious proc:exited for a pid the kernel never spawned: must
    // NOT terminate the loop (otherwise whenReady resolves before the
    // real child exits; the system would look alive-but-dead).
    msg.send({ kind: "proc:exited", pid: 9999, code: -1 });

    // Loop still waiting. Hand it a SAB, let it service a no-op
    // (empty ring), and then send the real exit.
    const sab = new ArrayBuffer(SAB_SIZE);
    msg.send({ kind: "proc:sab", pid: spawn.pid, sab });
    // Tiny pause so the dispatch loop gets a tick (parkFn).
    await new Promise<void>((resolve) => setTimeout(resolve, 20));
    msg.send({ kind: "proc:exited", pid: spawn.pid, code: 0 });

    await entry.whenReady;
    expect(msg.posted.some((m) => m.kind === "panic")).toBe(false);
  });
});
