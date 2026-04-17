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
});
