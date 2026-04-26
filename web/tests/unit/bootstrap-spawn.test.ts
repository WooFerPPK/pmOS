// Unit tests for the main-thread spawn router exported from
// `bootstrap.ts`. T232 lands the routing primitive side-by-side with
// the existing in-process drain; T234 will flip the production
// `runRealKernelMode()` boot path to actually wire the router. Until
// then this test file is the only consumer.
//
// FakeWorker harness: both the kernel-side channel and the user-
// Worker-side channel are mocked via plain objects that capture every
// posted message and expose injection hooks for incoming events. A
// real `new Worker(...)` is never called.

import { describe, expect, it } from "vitest";

import {
  createSpawnRouter,
  type UserWorkerLike,
} from "../../src/bootstrap";
import { SAB_SIZE } from "../../src/shared/sab-layout";
import type {
  KernelToMain,
  MainToKernel,
  MainToUser,
  UserToMain,
} from "../../src/shared/worker-proto";

interface FakeKernel {
  readonly posted: MainToKernel[];
  postMessage(msg: MainToKernel): void;
}

function makeFakeKernel(): FakeKernel {
  const posted: MainToKernel[] = [];
  return {
    posted,
    postMessage(msg: MainToKernel): void {
      posted.push(msg);
    },
  };
}

class FakeUserWorker implements UserWorkerLike {
  readonly posted: MainToUser[] = [];
  terminated = false;
  private readonly listeners: {
    message: Array<(ev: { data: UserToMain }) => void>;
    error: Array<(ev: { message?: string }) => void>;
  } = { message: [], error: [] };

  postMessage(msg: MainToUser): void {
    this.posted.push(msg);
  }

  addEventListener(
    type: "message" | "error",
    listener: ((ev: { data: UserToMain }) => void) | ((ev: { message?: string }) => void),
  ): void {
    if (type === "message") {
      this.listeners.message.push(listener as (ev: { data: UserToMain }) => void);
    } else {
      this.listeners.error.push(listener as (ev: { message?: string }) => void);
    }
  }

  removeEventListener(
    type: "message" | "error",
    listener: ((ev: { data: UserToMain }) => void) | ((ev: { message?: string }) => void),
  ): void {
    if (type === "message") {
      const idx = this.listeners.message.indexOf(
        listener as (ev: { data: UserToMain }) => void,
      );
      if (idx !== -1) this.listeners.message.splice(idx, 1);
    } else {
      const idx = this.listeners.error.indexOf(
        listener as (ev: { message?: string }) => void,
      );
      if (idx !== -1) this.listeners.error.splice(idx, 1);
    }
  }

  terminate(): void {
    this.terminated = true;
  }

  // Test-only: simulate the user Worker posting a message back to main.
  injectMessage(data: UserToMain): void {
    for (const l of this.listeners.message) l({ data });
  }

  // Test-only: simulate a Worker `error` event.
  injectError(message: string): void {
    for (const l of this.listeners.error) l({ message });
  }
}

interface Harness {
  readonly kernel: FakeKernel;
  readonly workers: FakeUserWorker[];
  readonly sabs: ArrayBuffer[];
  readonly router: ReturnType<typeof createSpawnRouter>;
}

function makeHarness(): Harness {
  const kernel = makeFakeKernel();
  const workers: FakeUserWorker[] = [];
  const sabs: ArrayBuffer[] = [];
  const router = createSpawnRouter({
    kernelWorker: kernel,
    workerFactory: () => {
      const w = new FakeUserWorker();
      workers.push(w);
      return w;
    },
    allocSab: () => {
      const sab = new ArrayBuffer(SAB_SIZE);
      sabs.push(sab);
      return sab;
    },
  });
  return { kernel, workers, sabs, router };
}

const sampleSpawn = (pid: number): KernelToMain => ({
  kind: "proc:spawn",
  pid,
  path: "/bin/example",
  wasmBytes: new ArrayBuffer(8),
});

describe("createSpawnRouter", () => {
  it("on proc:spawn allocates a SAB, creates a user Worker, posts boot to it, and posts proc:sab to the kernel with the same SAB", () => {
    const h = makeHarness();
    const spawn = sampleSpawn(7);
    h.router.handleKernelMessage(spawn);

    // SAB allocated, full SAB_SIZE bytes.
    expect(h.sabs).toHaveLength(1);
    expect(h.sabs[0]?.byteLength).toBe(SAB_SIZE);

    // User Worker created and tracked.
    expect(h.workers).toHaveLength(1);
    const w = h.workers[0]!;
    expect(h.router.liveWorkers.size).toBe(1);
    expect(h.router.liveWorkers.get(7)?.worker).toBe(w);
    expect(h.router.liveWorkers.get(7)?.sab).toBe(h.sabs[0]);

    // Boot message posted to the user Worker with the right shape.
    expect(w.posted).toHaveLength(1);
    const boot = w.posted[0];
    expect(boot?.kind).toBe("boot");
    if (boot?.kind === "boot") {
      expect(boot.pid).toBe(7);
      expect(boot.sab).toBe(h.sabs[0]);
      expect(boot.wasmBytes).toBe(spawn.wasmBytes);
    }

    // proc:sab posted to the kernel with the same SAB.
    expect(h.kernel.posted).toHaveLength(1);
    const psab = h.kernel.posted[0];
    expect(psab?.kind).toBe("proc:sab");
    if (psab?.kind === "proc:sab") {
      expect(psab.pid).toBe(7);
      expect(psab.sab).toBe(h.sabs[0]);
    }
  });

  it("on user-worker exited message, posts proc:exited to the kernel, terminates the worker, and removes the entry from liveWorkers", () => {
    const h = makeHarness();
    h.router.handleKernelMessage(sampleSpawn(11));
    const w = h.workers[0]!;
    // Drain the spawn-time messages so the assertion below isolates
    // the exit-time traffic.
    h.kernel.posted.length = 0;

    w.injectMessage({ kind: "exited", pid: 11, code: 7 });

    expect(w.terminated).toBe(true);
    expect(h.router.liveWorkers.has(11)).toBe(false);
    expect(h.kernel.posted).toEqual([
      { kind: "proc:exited", pid: 11, code: 7 },
    ]);
  });

  it("on user-worker memory message, forwards proc:memory to the kernel while the worker remains live", () => {
    const h = makeHarness();
    h.router.handleKernelMessage(sampleSpawn(12));
    const w = h.workers[0]!;
    h.kernel.posted.length = 0;

    w.injectMessage({ kind: "memory", pid: 12, bytes: 131_072 });

    expect(w.terminated).toBe(false);
    expect(h.router.liveWorkers.has(12)).toBe(true);
    expect(h.kernel.posted).toEqual([
      { kind: "proc:memory", pid: 12, bytes: 131_072 },
    ]);
  });

  it("on exited(memoryBytes), records memory before proc:exited", () => {
    const h = makeHarness();
    h.router.handleKernelMessage(sampleSpawn(14));
    const w = h.workers[0]!;
    h.kernel.posted.length = 0;

    w.injectMessage({ kind: "exited", pid: 14, code: 0, memoryBytes: 262_144 });

    expect(w.terminated).toBe(true);
    expect(h.router.liveWorkers.has(14)).toBe(false);
    expect(h.kernel.posted).toEqual([
      { kind: "proc:memory", pid: 14, bytes: 262_144 },
      { kind: "proc:exited", pid: 14, code: 0 },
    ]);
  });

  it("on user-worker error event, posts proc:exited(code=-1, trap) to the kernel and terminates the worker", () => {
    const h = makeHarness();
    h.router.handleKernelMessage(sampleSpawn(13));
    const w = h.workers[0]!;
    h.kernel.posted.length = 0;

    w.injectError("boom from worker");

    expect(w.terminated).toBe(true);
    expect(h.router.liveWorkers.has(13)).toBe(false);
    expect(h.kernel.posted).toHaveLength(1);
    const m = h.kernel.posted[0];
    expect(m?.kind).toBe("proc:exited");
    if (m?.kind === "proc:exited") {
      expect(m.pid).toBe(13);
      expect(m.code).toBe(-1);
      expect(m.trap).toBe("boom from worker");
    }
  });

  it("when getKernelWakeSlot returns a buffer, includes it in the boot message posted to the user Worker (T234)", () => {
    // Production wires `getKernelWakeSlot: () => kernelWakeSlot` so
    // the router can stamp each new boot message with the kernel's
    // wake-slot buffer (allocated by `KernelWasmHost.create`,
    // forwarded to main via the `kernel:wake-slot` message). The
    // user-worker entry constructs an `Int32Array` view and hands it
    // to `SabBackend` for the production wake protocol.
    const kernel = makeFakeKernel();
    const workers: FakeUserWorker[] = [];
    const sabs: ArrayBuffer[] = [];
    const wakeSlotBuf = new ArrayBuffer(32);
    const router = createSpawnRouter({
      kernelWorker: kernel,
      workerFactory: () => {
        const w = new FakeUserWorker();
        workers.push(w);
        return w;
      },
      allocSab: () => {
        const sab = new ArrayBuffer(SAB_SIZE);
        sabs.push(sab);
        return sab;
      },
      getKernelWakeSlot: () => wakeSlotBuf,
    });
    router.handleKernelMessage(sampleSpawn(21));

    expect(workers).toHaveLength(1);
    const w = workers[0]!;
    expect(w.posted).toHaveLength(1);
    const boot = w.posted[0];
    expect(boot?.kind).toBe("boot");
    if (boot?.kind === "boot") {
      expect(boot.pid).toBe(21);
      expect(boot.sab).toBe(sabs[0]);
      expect(boot.kernelWakeSlot).toBe(wakeSlotBuf);
    }
  });

  it("when getKernelWakeSlot returns null (wake slot hasn't arrived yet), boot message omits kernelWakeSlot", () => {
    // Defensive: production posts `kernel:wake-slot` BEFORE the first
    // `proc:spawn` so a null reading shouldn't happen, but the router
    // tolerates it by omitting the field. SabBackend's serviceHook
    // path stays viable for tests that drive the entry without a
    // production wake slot.
    const kernel = makeFakeKernel();
    const workers: FakeUserWorker[] = [];
    const router = createSpawnRouter({
      kernelWorker: kernel,
      workerFactory: () => {
        const w = new FakeUserWorker();
        workers.push(w);
        return w;
      },
      allocSab: () => new ArrayBuffer(SAB_SIZE),
      getKernelWakeSlot: () => null,
    });
    router.handleKernelMessage(sampleSpawn(22));

    const w = workers[0]!;
    const boot = w.posted[0];
    expect(boot?.kind).toBe("boot");
    if (boot?.kind === "boot") {
      expect(boot.kernelWakeSlot).toBeUndefined();
    }
  });
});

// ---- T137: pagehide-driven persistence sync -------------------------

describe("installPagehideSync", () => {
  interface FakeTarget {
    readonly listeners: Array<() => void>;
    addEventListener(type: "pagehide", listener: () => void): void;
  }
  function makeFakeTarget(): FakeTarget {
    const listeners: Array<() => void> = [];
    return {
      listeners,
      addEventListener(_type, listener) {
        listeners.push(listener);
      },
    };
  }

  it("registers a pagehide listener that posts sync:request to the kernel Worker", async () => {
    const { installPagehideSync } = await import("../../src/bootstrap");
    const kernel = makeFakeKernel();
    const target = makeFakeTarget();
    const listener = installPagehideSync(kernel, target);
    expect(target.listeners).toEqual([listener]);
    expect(kernel.posted).toEqual([]);
    listener();
    expect(kernel.posted).toEqual([{ kind: "sync:request" }]);
  });

  it("each subsequent pagehide fires another sync:request (e.g., bfcache restore + re-stash)", async () => {
    const { installPagehideSync } = await import("../../src/bootstrap");
    const kernel = makeFakeKernel();
    const target = makeFakeTarget();
    const listener = installPagehideSync(kernel, target);
    listener();
    listener();
    listener();
    expect(kernel.posted).toEqual([
      { kind: "sync:request" },
      { kind: "sync:request" },
      { kind: "sync:request" },
    ]);
  });
});

describe("installPeriodicSync", () => {
  function makeFakeScheduler(): {
    setIntervalCalls: Array<{ handler: () => void; ms: number; handle: number }>;
    clearIntervalCalls: number[];
    nextHandle: number;
    fire(handle: number): void;
    setInterval(handler: () => void, ms: number): number;
    clearInterval(handle: number): void;
  } {
    const setIntervalCalls: Array<{ handler: () => void; ms: number; handle: number }> = [];
    const clearIntervalCalls: number[] = [];
    let nextHandle = 1;
    return {
      setIntervalCalls,
      clearIntervalCalls,
      get nextHandle() {
        return nextHandle;
      },
      setInterval(handler, ms) {
        const handle = nextHandle++;
        setIntervalCalls.push({ handler, ms, handle });
        return handle;
      },
      clearInterval(handle) {
        clearIntervalCalls.push(handle);
      },
      fire(handle) {
        const entry = setIntervalCalls.find((c) => c.handle === handle);
        if (entry) entry.handler();
      },
    };
  }

  it("schedules a setInterval at the requested cadence and posts sync:request on each tick", async () => {
    const { installPeriodicSync } = await import("../../src/bootstrap");
    const kernel = makeFakeKernel();
    const sched = makeFakeScheduler();
    installPeriodicSync(kernel, 60_000, sched);
    expect(sched.setIntervalCalls).toHaveLength(1);
    expect(sched.setIntervalCalls[0]!.ms).toBe(60_000);
    expect(kernel.posted).toEqual([]);
    sched.fire(sched.setIntervalCalls[0]!.handle);
    sched.fire(sched.setIntervalCalls[0]!.handle);
    expect(kernel.posted).toEqual([
      { kind: "sync:request" },
      { kind: "sync:request" },
    ]);
  });

  it("dispose() calls clearInterval with the registered handle", async () => {
    const { installPeriodicSync } = await import("../../src/bootstrap");
    const kernel = makeFakeKernel();
    const sched = makeFakeScheduler();
    const dispose = installPeriodicSync(kernel, 30_000, sched);
    const handle = sched.setIntervalCalls[0]!.handle;
    expect(sched.clearIntervalCalls).toEqual([]);
    dispose();
    expect(sched.clearIntervalCalls).toEqual([handle]);
  });

  it("rejects non-positive or non-finite intervalMs", async () => {
    const { installPeriodicSync } = await import("../../src/bootstrap");
    const kernel = makeFakeKernel();
    const sched = makeFakeScheduler();
    expect(() => installPeriodicSync(kernel, 0, sched)).toThrow(/positive finite/);
    expect(() => installPeriodicSync(kernel, -1, sched)).toThrow(/positive finite/);
    expect(() => installPeriodicSync(kernel, Number.NaN, sched)).toThrow(/positive finite/);
    expect(() => installPeriodicSync(kernel, Number.POSITIVE_INFINITY, sched)).toThrow(/positive finite/);
    expect(sched.setIntervalCalls).toEqual([]);
  });
});
