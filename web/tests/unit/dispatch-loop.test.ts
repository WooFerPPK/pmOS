// Tests for `KernelWasmHost.startDispatchLoop` — the kernel-side
// round-robin dispatch loop that services every live pid's SAB ring
// on each pass. T233 (M1.4) lands the loop alongside its test gate;
// T235 (M1.6) made it the only scheduler by deleting the preview
// `drainPendingSpawns` in-process drain. Today vitest drives it
// through a plain-ArrayBuffer pidMap + a `parkFn` stub that yields
// microtasks instead of blocking on `Atomics.wait`. A focused wake-race
// regression uses the host's shared wake slot and a deterministic
// `Atomics.waitAsync` stub to cover the production parker contract.
//
// What this file pins down:
//
//   * Round-robin order: in one pass, every live pid gets at least
//     one ring pop before any pid gets a second.
//   * 8-request-per-pid budget: within one pass, no pid gets more
//     than `budget` requests serviced before the next pid is
//     visited. Proven by seeding pid A with 16 requests and pid B
//     with 1, halting after exactly one pass, and asserting A
//     serviced 8 while B serviced 1.
//   * Termination: when `halted()` returns true the loop resolves
//     promptly; when every live ring is empty and `halted()` still
//     returns false, the loop parks on `parkFn` rather than
//     busy-looping.
//
// The loop is the last plumbing piece before the first real user
// Worker; T234 wires `bootstrap.ts`'s production boot onto the real
// Worker spawn + dispatch-loop path and strengthens
// `real-kernel.spec.ts` accordingly.

import fs from "node:fs";
import path from "node:path";
import { beforeAll, describe, expect, it, vi } from "vitest";

import { KernelWasmHost, pollTimeoutMs } from "../../src/kernel-wasm-host";
import { resolveCargoTargetDirectory } from "../helpers/cargo-target";
import {
  HEAP_SCRATCH_BYTES,
  OFF_HEAP_SCRATCH,
  OFF_REQ_HEAD,
  OFF_REQ_RING,
  OFF_REQ_TAIL,
  OFF_RES_HEAD,
  OFF_RES_RING,
  OFF_USER_WAIT_SLOT,
  SAB_SIZE,
  STATUS_READY,
  STATUS_REQUESTED,
} from "../../src/shared/sab-layout";
import {
  CAPSET_ALL,
  decodeResponse,
  encodeRequest,
  OP_WASI,
  SLOT_SIZE,
  type SyscallRequest,
} from "../../src/shared/syscall";

// ---- shared fixture -------------------------------------------------

let wasmBytes: ArrayBuffer;

beforeAll(() => {
  const cargoTargetDirectory = resolveCargoTargetDirectory(
    path.resolve(__dirname, "../../.."),
    process.env.CARGO_TARGET_DIR,
  );
  const wasmPath = path.join(
    cargoTargetDirectory,
    "wasm32-unknown-unknown/release/kernel.wasm",
  );
  if (!fs.existsSync(wasmPath)) {
    throw new Error(
      `kernel.wasm not found at ${wasmPath}. Run \`just build\` first.`,
    );
  }
  const raw = fs.readFileSync(wasmPath);
  wasmBytes = raw.buffer.slice(
    raw.byteOffset,
    raw.byteOffset + raw.byteLength,
  ) as ArrayBuffer;
});

interface FreshHostResult {
  host: KernelWasmHost;
  consoleWrites: Uint8Array[];
}

async function freshHost(): Promise<FreshHostResult> {
  const consoleWrites: Uint8Array[] = [];
  const host = await KernelWasmHost.create(wasmBytes, {
    onConsoleWrite: (bytes) => {
      consoleWrites.push(bytes);
    },
    nowNs: () => 0n,
  });
  return { host, consoleWrites };
}

interface TrackingMessageChannelStats {
  constructions: number;
  postMessages: number;
  readonly closedPorts: string[];
}

function installTrackingMessageChannel(): TrackingMessageChannelStats {
  const RealMessageChannel = globalThis.MessageChannel;
  const stats: TrackingMessageChannelStats = {
    constructions: 0,
    postMessages: 0,
    closedPorts: [],
  };
  class TrackingMessageChannel {
    readonly port1: MessagePort;
    readonly port2: MessagePort;

    constructor() {
      stats.constructions += 1;
      const channel = new RealMessageChannel();
      this.port1 = channel.port1;
      this.port2 = channel.port2;
      const postMessage = this.port2.postMessage.bind(this.port2);
      Object.defineProperty(this.port2, "postMessage", {
        configurable: true,
        value: (
          message: unknown,
          options?: StructuredSerializeOptions,
        ): void => {
          stats.postMessages += 1;
          postMessage(message, options);
        },
      });
      for (const [name, port] of [
        ["port1", this.port1],
        ["port2", this.port2],
      ] as const) {
        const close = port.close.bind(port);
        Object.defineProperty(port, "close", {
          configurable: true,
          value: (): void => {
            stats.closedPorts.push(name);
            close();
          },
        });
      }
    }
  }
  vi.stubGlobal("MessageChannel", TrackingMessageChannel);
  return stats;
}

/** Empty 64 KiB SAB stand-in backed by a plain ArrayBuffer. */
function freshSab(): ArrayBuffer {
  return new ArrayBuffer(SAB_SIZE);
}

/**
 * Seed one or more request slots into an otherwise-empty SAB. Advances
 * REQ_HEAD to `requests.length` so the dispatch loop sees every
 * seeded request in the same pass (until the budget kicks in).
 */
function seedRequests(
  sab: ArrayBuffer,
  requests: Array<{ request: SyscallRequest; heap?: Uint8Array }>,
): void {
  for (let i = 0; i < requests.length; i++) {
    const item = requests[i]!;
    const slotOffset = OFF_REQ_RING + i * SLOT_SIZE;
    const reqBytes = encodeRequest(item.request);
    new Uint8Array(sab, slotOffset, SLOT_SIZE).set(reqBytes);
    if (item.heap !== undefined && item.heap.length > 0) {
      const heapOffset = OFF_HEAP_SCRATCH + (item.request.heapPtr ?? 0);
      if (
        heapOffset + item.heap.length >
        OFF_HEAP_SCRATCH + HEAP_SCRATCH_BYTES
      ) {
        throw new Error("seedRequests: heap overflow");
      }
      new Uint8Array(sab, heapOffset, item.heap.length).set(item.heap);
    }
  }
  const header = new Int32Array(sab, 0, OFF_HEAP_SCRATCH / 4);
  Atomics.store(header, OFF_REQ_HEAD / 4, requests.length);
}

/** Read the response at slot `i` of the SAB's response ring. */
function readResponseSlot(sab: ArrayBuffer, i: number) {
  const slotOffset = OFF_RES_RING + i * SLOT_SIZE;
  const bytes = new Uint8Array(new Uint8Array(sab, slotOffset, SLOT_SIZE));
  return decodeResponse(bytes);
}

function responseRingHead(sab: ArrayBuffer): number {
  const header = new Int32Array(sab, 0, OFF_HEAP_SCRATCH / 4);
  return Atomics.load(header, OFF_RES_HEAD / 4);
}

function requestRingTail(sab: ArrayBuffer): number {
  const header = new Int32Array(sab, 0, OFF_HEAP_SCRATCH / 4);
  return Atomics.load(header, OFF_REQ_TAIL / 4);
}

// ---- round-robin ----------------------------------------------------

describe("startDispatchLoop: round-robin", () => {
  it("services every live pid's ring until all requests drain", async () => {
    const { host, consoleWrites } = await freshHost();
    const pidA = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pidA, 1);
    host.markRunning(pidA);
    const pidB = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pidB, 1);
    host.markRunning(pidB);

    const sabA = freshSab();
    const sabB = freshSab();
    const msgA = new TextEncoder().encode("a\n");
    const msgB = new TextEncoder().encode("b\n");
    seedRequests(sabA, [
      {
        request: {
          opcode: OP_WASI.FD_WRITE,
          requestId: 1,
          arg0: 1,
          heapPtr: 0,
          heapLen: msgA.length,
        },
        heap: msgA,
      },
    ]);
    seedRequests(sabB, [
      {
        request: {
          opcode: OP_WASI.FD_WRITE,
          requestId: 2,
          arg0: 1,
          heapPtr: 0,
          heapLen: msgB.length,
        },
        heap: msgB,
      },
    ]);

    const pidMap = new Map<number, ArrayBufferLike>([
      [pidA, sabA],
      [pidB, sabB],
    ]);

    // Halt after the first pass: once no ring has pending work, the
    // loop will park; we instead flip the halt flag so it returns.
    let parks = 0;
    const parkFn = async (): Promise<void> => {
      parks += 1;
    };
    await host.startDispatchLoop({
      pidSource: () => pidMap,
      halted: () => parks > 0,
      parkFn,
    });

    // Both rings drained.
    expect(requestRingTail(sabA)).toBe(1);
    expect(requestRingTail(sabB)).toBe(1);
    expect(responseRingHead(sabA)).toBe(1);
    expect(responseRingHead(sabB)).toBe(1);

    // Responses carry the original requestIds.
    const rA = readResponseSlot(sabA, 0);
    expect(rA.requestId).toBe(1);
    expect(rA.status).toBe(0);
    const rB = readResponseSlot(sabB, 0);
    expect(rB.requestId).toBe(2);
    expect(rB.status).toBe(0);

    // Console driver saw both messages.
    const written = consoleWrites.map((b) => new TextDecoder().decode(b));
    expect(written.sort()).toEqual(["a\n", "b\n"]);
  });
});

// ---- per-pid budget --------------------------------------------------

describe("startDispatchLoop: 8-request-per-pid budget", () => {
  it("services at most 8 requests from one pid before moving to the next", async () => {
    const { host } = await freshHost();
    const pidA = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pidA, 1);
    host.markRunning(pidA);
    const pidB = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pidB, 1);
    host.markRunning(pidB);

    const sabA = freshSab();
    const sabB = freshSab();

    // Pid A: 16 FD_WRITE requests queued.
    const aRequests: Array<{ request: SyscallRequest; heap?: Uint8Array }> = [];
    for (let i = 0; i < 16; i++) {
      const line = new TextEncoder().encode(`a${i}\n`);
      // Stage each line at a distinct heap offset inside the SAB's
      // heap-scratch region so successive serviceSab calls don't
      // clobber one another's in-flight payloads. (The kernel's
      // own scratch region gets reused across dispatches, but the
      // SAB heap-scratch is where the user's bytes live between
      // push and pop.)
      const heapPtr = i * 16;
      aRequests.push({
        request: {
          opcode: OP_WASI.FD_WRITE,
          requestId: 100 + i,
          arg0: 1,
          heapPtr,
          heapLen: line.length,
        },
        heap: line,
      });
    }
    seedRequests(sabA, aRequests);

    // Pid B: 1 FD_WRITE request.
    const bLine = new TextEncoder().encode("b\n");
    seedRequests(sabB, [
      {
        request: {
          opcode: OP_WASI.FD_WRITE,
          requestId: 200,
          arg0: 1,
          heapPtr: 0,
          heapLen: bLine.length,
        },
        heap: bLine,
      },
    ]);

    const pidMap = new Map<number, ArrayBufferLike>([
      [pidA, sabA],
      [pidB, sabB],
    ]);

    // Halt after exactly one pass.
    let passes = 0;
    const parkFn = async (): Promise<void> => {
      // Parks are reached only when nothing was serviced this pass —
      // impossible in pass 1 here because both rings have work.
      passes = -1;
    };
    const halted = (): boolean => {
      if (passes >= 1) return true;
      passes += 1;
      return false;
    };
    await host.startDispatchLoop({
      pidSource: () => pidMap,
      halted,
      parkFn,
    });

    // After 1 pass with budget=8: A has 8 of 16 serviced, B has 1 of 1.
    expect(requestRingTail(sabA)).toBe(8);
    expect(responseRingHead(sabA)).toBe(8);
    expect(requestRingTail(sabB)).toBe(1);
    expect(responseRingHead(sabB)).toBe(1);
    // parkFn was NOT reached — both pids serviced work this pass.
    expect(passes).toBe(1);
  });
});

// ---- termination ----------------------------------------------------

describe("pollTimeoutMs", () => {
  it("preserves finite sub-millisecond deadlines and the no-timer sentinel", () => {
    expect(pollTimeoutMs(0n)).toBe(0);
    expect(pollTimeoutMs(250_000n)).toBe(0.25);
    expect(pollTimeoutMs(1_500_000n)).toBe(1.5);
    expect(pollTimeoutMs(0xffff_ffff_ffff_ffffn)).toBeUndefined();
  });

  it("normalizes the real wasm no-poll sentinel before timeout conversion", async () => {
    const { host } = await freshHost();
    const timeoutNs = host.nextPollTimeoutNs();

    expect(timeoutNs).toBe(0xffff_ffff_ffff_ffffn);
    expect(pollTimeoutMs(timeoutNs)).toBeUndefined();
  });
});

describe("startDispatchLoop: termination", () => {
  it("returns promptly when halted() is true from the start", async () => {
    const { host } = await freshHost();
    const pidMap = new Map<number, ArrayBufferLike>();
    let parkCalls = 0;
    await host.startDispatchLoop({
      pidSource: () => pidMap,
      halted: () => true,
      parkFn: async () => {
        parkCalls += 1;
      },
    });
    expect(parkCalls).toBe(0);
  });

  it("parks when every ring is empty and halt has not fired", async () => {
    const { host } = await freshHost();
    const pidA = host.registerProcess(CAPSET_ALL);
    host.markRunning(pidA);
    const sabA = freshSab(); // empty rings
    const pidMap = new Map<number, ArrayBufferLike>([[pidA, sabA]]);

    let parkCalls = 0;
    const halted = (): boolean => {
      // Halt right after the loop parks for the first time, so the
      // test itself terminates cleanly.
      return parkCalls > 0;
    };
    await host.startDispatchLoop({
      pidSource: () => pidMap,
      halted,
      parkFn: async () => {
        parkCalls += 1;
      },
    });
    expect(parkCalls).toBe(1);
  });

  it("parks against the wake epoch from before the scan", async () => {
    const { host } = await freshHost();
    const wakeSlot = host.wakeSlot;
    Atomics.store(wakeSlot, 0, 41);
    let haltChecks = 0;
    const observed: Array<{ expected: number; current: number }> = [];

    await host.startDispatchLoop({
      pidSource: () => new Map(),
      halted: () => {
        haltChecks += 1;
        if (haltChecks === 2) {
          // Reproduce the race: a producer publishes its wake after the
          // empty scan but immediately before the loop enters its parker.
          Atomics.add(wakeSlot, 0, 1);
        }
        return haltChecks >= 3;
      },
      parkFn: async (expected) => {
        observed.push({ expected, current: Atomics.load(wakeSlot, 0) });
      },
    });

    expect(observed).toEqual([{ expected: 41, current: 42 }]);
  });

  it("double-checks parked polls and never sleeps with a queued wake", async () => {
    const { host } = await freshHost();
    let serviceCalls = 0;
    let parkCalls = 0;
    vi.spyOn(host, "servicePollWaiters").mockImplementation(() => {
      serviceCalls += 1;
      return 1;
    });

    await host.startDispatchLoop({
      pidSource: () => new Map(),
      halted: () => serviceCalls > 0,
      parkFn: async () => {
        parkCalls += 1;
      },
    });

    expect(serviceCalls).toBe(1);
    expect(parkCalls).toBe(0);
  });

  it("repasses when a later pid parks after queuing an earlier pid's expired clock wake", async () => {
    let nowNs = 0n;
    const host = await KernelWasmHost.create(wasmBytes, {
      nowNs: () => nowNs,
    });
    const earlier = host.registerProcess(CAPSET_ALL);
    host.markRunning(earlier);
    const later = host.registerProcess(CAPSET_ALL);
    host.installSignalChannelFd(later, 3);
    host.markRunning(later);
    const earlierSab = freshSab();
    const laterSab = freshSab();

    const pollArgs = new Uint8Array(16);
    new DataView(pollArgs.buffer).setUint32(0, 1, true);
    new DataView(pollArgs.buffer).setUint32(4, 1, true);
    const clockSub = new Uint8Array(48);
    const clockView = new DataView(clockSub.buffer);
    clockView.setBigUint64(0, 1n, true);
    clockView.setUint8(8, 0); // CLOCK
    clockView.setUint32(16, 1, true); // CLOCKID_MONOTONIC
    clockView.setBigUint64(24, 10n, true);
    clockView.setUint16(40, 1, true); // ABSTIME
    seedRequests(earlierSab, [
      {
        request: {
          opcode: OP_WASI.POLL_ONEOFF,
          requestId: 700,
          args: pollArgs,
          heapPtr: 0,
          heapLen: clockSub.length,
        },
        heap: clockSub,
      },
    ]);
    expect(host.serviceSab(earlier, new Uint8Array(earlierSab))).toBe(0);
    expect(responseRingHead(earlierSab)).toBe(0);

    nowNs = 10n;
    expect(host.nextPollTimeoutNs()).toBe(0n);
    const fdSub = new Uint8Array(48);
    const fdView = new DataView(fdSub.buffer);
    fdView.setBigUint64(0, 2n, true);
    fdView.setUint8(8, 1); // FD_READ
    fdView.setUint32(16, 3, true);
    seedRequests(laterSab, [
      {
        request: {
          opcode: OP_WASI.POLL_ONEOFF,
          requestId: 701,
          args: pollArgs,
          heapPtr: 0,
          heapLen: fdSub.length,
        },
        heap: fdSub,
      },
    ]);

    let parkCalls = 0;
    await host.startDispatchLoop({
      pidSource: () =>
        new Map<number, ArrayBufferLike>([
          [earlier, earlierSab],
          [later, laterSab],
        ]),
      halted: () => responseRingHead(earlierSab) > 0,
      parkFn: async () => {
        parkCalls += 1;
      },
    });

    expect(requestRingTail(laterSab)).toBe(1);
    expect(responseRingHead(earlierSab)).toBe(1);
    expect(responseRingHead(laterSab)).toBe(0);
    expect(readResponseSlot(earlierSab, 0).requestId).toBe(700);
    expect(parkCalls).toBe(0);
  });

  it("services a due clock while another pid continuously issues read-only syscalls", async () => {
    let nowNs = 0n;
    const host = await KernelWasmHost.create(wasmBytes, {
      nowNs: () => nowNs,
    });
    const clockPid = host.registerProcess(CAPSET_ALL);
    host.markRunning(clockPid);
    const busyPid = host.registerProcess(CAPSET_ALL);
    host.markRunning(busyPid);
    const clockSab = freshSab();
    const busySab = freshSab();

    const pollArgs = new Uint8Array(16);
    new DataView(pollArgs.buffer).setUint32(0, 1, true);
    new DataView(pollArgs.buffer).setUint32(4, 1, true);
    const clockSub = new Uint8Array(48);
    const clockView = new DataView(clockSub.buffer);
    clockView.setBigUint64(0, 0xcafen, true);
    clockView.setUint8(8, 0); // CLOCK
    clockView.setUint32(16, 1, true); // CLOCKID_MONOTONIC
    clockView.setBigUint64(24, 10n, true);
    clockView.setUint16(40, 1, true); // ABSTIME
    seedRequests(clockSab, [
      {
        request: {
          opcode: OP_WASI.POLL_ONEOFF,
          requestId: 710,
          args: pollArgs,
          heapPtr: 0,
          heapLen: clockSub.length,
        },
        heap: clockSub,
      },
    ]);
    expect(host.serviceSab(clockPid, new Uint8Array(clockSab))).toBe(0);

    seedRequests(
      busySab,
      Array.from({ length: 4 }, (_, index) => ({
        request: {
          opcode: OP_WASI.CLOCK_TIME_GET,
          requestId: 720 + index,
          arg0: 1, // CLOCKID_MONOTONIC
        },
      })),
    );
    nowNs = 10n;

    let parkCalls = 0;
    await host.startDispatchLoop({
      pidSource: () =>
        new Map<number, ArrayBufferLike>([
          [clockPid, clockSab],
          [busyPid, busySab],
        ]),
      halted: () => responseRingHead(clockSab) > 0,
      budget: 1,
      parkFn: async () => {
        parkCalls += 1;
      },
    });

    expect(responseRingHead(clockSab)).toBe(1);
    expect(readResponseSlot(clockSab, 0).requestId).toBe(710);
    expect(requestRingTail(busySab)).toBeGreaterThanOrEqual(2);
    expect(parkCalls).toBe(0);
  });

  it("task-yields after bounded immediately-resolved idle parks", async () => {
    const { host } = await freshHost();
    let parkCalls = 0;
    let taskYields = 0;

    await host.startDispatchLoop({
      pidSource: () => new Map(),
      halted: () => taskYields > 0,
      // Models waitAsync's synchronous `not-equal` result while producers
      // keep changing the wake epoch. Awaiting this resolved promise alone
      // yields only to microtasks, not to the Worker's message task queue.
      parkFn: async () => {
        parkCalls += 1;
      },
      passesBeforeTaskYield: 3,
      taskYieldFn: async () => {
        taskYields += 1;
      },
    });

    expect(parkCalls).toBe(3);
    expect(taskYields).toBe(1);
  });

  it("default waitAsync compares against the pre-scan wake epoch", async () => {
    const { host } = await freshHost();
    const wakeSlot = host.wakeSlot;
    Atomics.store(wakeSlot, 0, 91);
    type WaitResult =
      | { readonly async: false; readonly value: "not-equal" | "timed-out" }
      | {
          readonly async: true;
          readonly value: Promise<"ok" | "timed-out">;
        };
    type WaitAsync = (
      view: Int32Array,
      index: number,
      value: number,
      timeout?: number,
    ) => WaitResult;
    const atomics = Atomics as unknown as { waitAsync: WaitAsync };
    const original = atomics.waitAsync;
    const waits: Array<{
      expected: number;
      current: number;
      timeout: number | undefined;
    }> = [];
    atomics.waitAsync = (view, index, expected, timeout) => {
      waits.push({
        expected,
        current: Atomics.load(view, index),
        timeout,
      });
      return { async: false, value: "not-equal" };
    };
    let haltChecks = 0;

    try {
      await host.startDispatchLoop({
        pidSource: () => new Map(),
        halted: () => {
          haltChecks += 1;
          if (haltChecks === 2) {
            Atomics.add(wakeSlot, 0, 1);
          }
          return haltChecks >= 3;
        },
      });
    } finally {
      atomics.waitAsync = original;
    }

    expect(waits).toEqual([{ expected: 91, current: 92, timeout: undefined }]);
  });

  it("passes the nearest clock deadline to waitAsync without a polling cap", async () => {
    const { host } = await freshHost();
    vi.spyOn(host, "servicePollWaiters").mockReturnValue(0);
    vi.spyOn(host, "nextPollTimeoutNs").mockReturnValue(1_500_000n);
    type WaitResult = {
      readonly async: false;
      readonly value: "not-equal" | "timed-out";
    };
    type WaitAsync = (
      view: Int32Array,
      index: number,
      value: number,
      timeout?: number,
    ) => WaitResult;
    const atomics = Atomics as unknown as { waitAsync: WaitAsync };
    const original = atomics.waitAsync;
    const timeouts: Array<number | undefined> = [];
    atomics.waitAsync = (_view, _index, _expected, timeout) => {
      timeouts.push(timeout);
      return { async: false, value: "timed-out" };
    };

    try {
      await host.startDispatchLoop({
        pidSource: () => new Map(),
        halted: () => timeouts.length > 0,
      });
    } finally {
      atomics.waitAsync = original;
    }

    expect(timeouts).toEqual([1.5]);
  });

  it("fails explicitly when waitAsync disappears instead of timer-spinning", async () => {
    const { host } = await freshHost();
    vi.spyOn(host, "servicePollWaiters").mockReturnValue(0);
    vi.spyOn(host, "nextPollTimeoutNs").mockReturnValue(0xffff_ffff_ffff_ffffn);
    const atomics = Atomics as unknown as { waitAsync: unknown };
    const original = atomics.waitAsync;
    atomics.waitAsync = undefined;
    try {
      await expect(
        host.startDispatchLoop({
          pidSource: () => new Map(),
          halted: () => false,
        }),
      ).rejects.toThrow("requires SharedArrayBuffer and Atomics.waitAsync");
    } finally {
      atomics.waitAsync = original;
    }
  });

  it("terminates after every pid is removed from pidSource between passes", async () => {
    const { host, consoleWrites } = await freshHost();
    const pidA = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pidA, 1);
    host.markRunning(pidA);

    const sabA = freshSab();
    const line = new TextEncoder().encode("once\n");
    seedRequests(sabA, [
      {
        request: {
          opcode: OP_WASI.FD_WRITE,
          requestId: 7,
          arg0: 1,
          heapPtr: 0,
          heapLen: line.length,
        },
        heap: line,
      },
    ]);

    const pidMap = new Map<number, ArrayBufferLike>([[pidA, sabA]]);
    // Simulate the kernel-worker-entry's proc:exited handler: after
    // the first pass drains pid A's one request, the next time the
    // pidSource is read (via halted's side-effect or between passes),
    // the pidMap is empty.
    let hasSpawned = false;
    const halted = (): boolean => {
      if (!hasSpawned) {
        hasSpawned = pidMap.size > 0;
        return false;
      }
      // Simulate the `proc:exited` arrival: remove pid A and return
      // true once the map is empty.
      if (pidMap.has(pidA)) {
        pidMap.delete(pidA);
        return false;
      }
      return pidMap.size === 0;
    };

    let parkCalls = 0;
    await host.startDispatchLoop({
      pidSource: () => pidMap,
      halted,
      parkFn: async () => {
        parkCalls += 1;
      },
    });

    // The single FD_WRITE was serviced before the pid was reaped.
    expect(consoleWrites).toHaveLength(1);
    expect(new TextDecoder().decode(consoleWrites[0]!)).toBe("once\n");
    // No parks happened — halt fired before the first would-be park.
    expect(parkCalls).toBe(0);
  });

  it("yields to worker messages under continuously busy SAB traffic", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const sab = freshSab();
    const requests: Array<{ request: SyscallRequest; heap?: Uint8Array }> = [];
    for (let index = 0; index < 8; index += 1) {
      const heap = new TextEncoder().encode(`busy-${index}\n`);
      requests.push({
        request: {
          opcode: OP_WASI.FD_WRITE,
          requestId: 900 + index,
          arg0: 1,
          heapPtr: index * 16,
          heapLen: heap.length,
        },
        heap,
      });
    }
    seedRequests(sab, requests);

    let yielded = false;
    let parkCalls = 0;
    await host.startDispatchLoop({
      pidSource: () => new Map([[pid, sab]]),
      halted: () => yielded,
      budget: 1,
      passesBeforeTaskYield: 2,
      taskYieldFn: async () => {
        // This callback stands in for the Worker's next message task. A loop
        // that only awaits on idle can never reach it while the ring remains
        // non-empty.
        yielded = true;
      },
      parkFn: async () => {
        parkCalls += 1;
      },
    });

    expect(yielded).toBe(true);
    expect(parkCalls).toBe(0);
    expect(requestRingTail(sab)).toBe(2);
    expect(responseRingHead(sab)).toBe(2);
  });

  it("uses a real task for the default yield instead of only a microtask", async () => {
    const { host } = await freshHost();
    const queuedTask = new MessageChannel();
    let taskObserved = false;
    queuedTask.port1.onmessage = () => {
      taskObserved = true;
    };
    queuedTask.port1.start();
    queuedTask.port2.postMessage(undefined);
    let haltChecksWithoutTask = 0;

    try {
      await host.startDispatchLoop({
        pidSource: () => new Map(),
        halted: () => taskObserved || (haltChecksWithoutTask += 1) >= 12,
        parkFn: async () => {},
        passesBeforeTaskYield: 1,
      });
      expect(taskObserved).toBe(true);
      expect(haltChecksWithoutTask).toBeLessThan(12);
    } finally {
      queuedTask.port1.close();
      queuedTask.port2.close();
    }
  });

  it("admits a queued task within two passes of continuously busy SAB work", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);
    const sab = freshSab();
    seedRequests(
      sab,
      Array.from({ length: 8 }, (_, index) => ({
        request: {
          opcode: OP_WASI.CLOCK_TIME_GET,
          requestId: 950 + index,
          arg0: 1,
        },
      })),
    );

    const queuedTask = new MessageChannel();
    let servicedWhenTaskRan: number | undefined;
    queuedTask.port1.onmessage = () => {
      servicedWhenTaskRan = requestRingTail(sab);
    };
    queuedTask.port1.start();
    queuedTask.port2.postMessage(undefined);

    try {
      await host.startDispatchLoop({
        pidSource: () => new Map([[pid, sab]]),
        halted: () =>
          servicedWhenTaskRan !== undefined || requestRingTail(sab) >= 6,
        budget: 1,
        passesBeforeTaskYield: 2,
        parkFn: async () => {
          throw new Error("busy dispatch loop unexpectedly parked");
        },
      });
      expect(servicedWhenTaskRan).toBe(2);
      expect(responseRingHead(sab)).toBe(2);
    } finally {
      queuedTask.port1.close();
      queuedTask.port2.close();
    }
  });

  it("reuses one default channel across task yields and closes both ports", async () => {
    const { host } = await freshHost();
    const stats = installTrackingMessageChannel();
    try {
      await host.startDispatchLoop({
        pidSource: () => new Map(),
        halted: () => stats.postMessages >= 3,
        parkFn: async () => {},
        passesBeforeTaskYield: 1,
      });
      expect(stats.constructions).toBe(1);
      expect(stats.postMessages).toBe(3);
      expect(stats.closedPorts).toEqual(["port1", "port2"]);
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("does not self-yield while genuinely parked", async () => {
    const { host } = await freshHost();
    const stats = installTrackingMessageChannel();
    let halt = false;
    let resumePark!: () => void;
    let reportParked!: () => void;
    const parked = new Promise<void>((resolve) => {
      reportParked = resolve;
    });
    const parkGate = new Promise<void>((resolve) => {
      resumePark = resolve;
    });
    let loopSettled = false;

    try {
      const loop = host
        .startDispatchLoop({
          pidSource: () => new Map(),
          halted: () => halt,
          parkFn: async () => {
            reportParked();
            await parkGate;
          },
        })
        .finally(() => {
          loopSettled = true;
        });

      await parked;
      await Promise.resolve();
      expect(loopSettled).toBe(false);
      expect(stats.constructions).toBe(1);
      expect(stats.postMessages).toBe(0);

      halt = true;
      resumePark();
      await loop;
      expect(stats.postMessages).toBe(0);
      expect(stats.closedPorts).toEqual(["port1", "port2"]);
    } finally {
      resumePark();
      vi.unstubAllGlobals();
    }
  });

  it("closes both default task-yield ports when dispatch throws", async () => {
    const { host } = await freshHost();
    const stats = installTrackingMessageChannel();
    try {
      await expect(
        host.startDispatchLoop({
          pidSource: () => {
            throw new Error("synthetic dispatch failure");
          },
          halted: () => false,
          parkFn: async () => {},
        }),
      ).rejects.toThrow("synthetic dispatch failure");
      expect(stats.constructions).toBe(1);
      expect(stats.postMessages).toBe(0);
      expect(stats.closedPorts).toEqual(["port1", "port2"]);
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("requires MessageChannel only when the default task yield is selected", async () => {
    const { host } = await freshHost();
    vi.stubGlobal("MessageChannel", undefined);
    try {
      await expect(
        host.startDispatchLoop({
          pidSource: () => new Map(),
          halted: () => true,
          parkFn: async () => {},
        }),
      ).rejects.toThrow("requires MessageChannel");

      let injectedYields = 0;
      await host.startDispatchLoop({
        pidSource: () => new Map(),
        halted: () => injectedYields > 0,
        parkFn: async () => {},
        passesBeforeTaskYield: 1,
        taskYieldFn: async () => {
          injectedYields += 1;
        },
      });
      expect(injectedYields).toBe(1);
    } finally {
      vi.unstubAllGlobals();
    }
  });
});

// ---- T234 production wake protocol (kernel side) -------------------

describe("startDispatchLoop: production wake protocol", () => {
  it("writes user_wait_slot = STATUS_READY after each successful serviceSab so the user's Atomics.wait returns", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const sab = freshSab();
    const header = new Int32Array(sab, 0, OFF_HEAP_SCRATCH / 4);
    // Pre-stage `STATUS_REQUESTED` the way `SabBackend` does at the top
    // of every dispatch — so the assertion below proves the loop wrote
    // a NEW value rather than just observing the initial zero.
    Atomics.store(header, OFF_USER_WAIT_SLOT / 4, STATUS_REQUESTED);

    const line = new TextEncoder().encode("hi\n");
    seedRequests(sab, [
      {
        request: {
          opcode: OP_WASI.FD_WRITE,
          requestId: 1,
          arg0: 1,
          heapPtr: 0,
          heapLen: line.length,
        },
        heap: line,
      },
    ]);

    const pidMap = new Map<number, ArrayBufferLike>([[pid, sab]]);
    let parks = 0;
    await host.startDispatchLoop({
      pidSource: () => pidMap,
      halted: () => parks > 0,
      parkFn: async (): Promise<void> => {
        parks += 1;
      },
    });

    // The serviceSab landed a response — request ring drained, response
    // ring head advanced.
    expect(requestRingTail(sab)).toBe(1);
    expect(responseRingHead(sab)).toBe(1);
    // T234: user_wait_slot transitioned from STATUS_REQUESTED (the
    // pre-stage above) to STATUS_READY. A waiter on
    // Atomics.wait(header, OFF_USER_WAIT_SLOT/4, STATUS_REQUESTED) sees
    // the new value and returns "not-equal" immediately, OR was woken
    // by the notify the loop also issues (no-op on a plain ArrayBuffer
    // here; real behavior covered by Playwright in real-kernel.spec.ts).
    expect(Atomics.load(header, OFF_USER_WAIT_SLOT / 4)).toBe(STATUS_READY);
  });

  it("writes user_wait_slot = STATUS_READY for every serviced request in a multi-budget pass", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const sab = freshSab();
    const header = new Int32Array(sab, 0, OFF_HEAP_SCRATCH / 4);
    // Three FD_WRITEs in the ring at once — the loop services them
    // back-to-back inside the same per-pid budget block. The wake-slot
    // store must happen on every iteration, not just the last one,
    // otherwise an early-iteration user that's parked between syscalls
    // would never wake.
    const requests: Array<{ request: SyscallRequest; heap?: Uint8Array }> = [];
    for (let i = 0; i < 3; i++) {
      const heap = new TextEncoder().encode(`r${i}\n`);
      requests.push({
        request: {
          opcode: OP_WASI.FD_WRITE,
          requestId: 10 + i,
          arg0: 1,
          heapPtr: i * 16,
          heapLen: heap.length,
        },
        heap,
      });
    }
    seedRequests(sab, requests);

    const pidMap = new Map<number, ArrayBufferLike>([[pid, sab]]);
    let parks = 0;
    await host.startDispatchLoop({
      pidSource: () => pidMap,
      halted: () => parks > 0,
      parkFn: async (): Promise<void> => {
        parks += 1;
      },
    });

    // All three requests serviced.
    expect(requestRingTail(sab)).toBe(3);
    expect(responseRingHead(sab)).toBe(3);
    // user_wait_slot landed on STATUS_READY (the final write of the
    // three iterations; equality after the last write is what the
    // protocol commits to).
    expect(Atomics.load(header, OFF_USER_WAIT_SLOT / 4)).toBe(STATUS_READY);
  });
});

describe("startDispatchLoop: browser-substrate work", () => {
  it("wakes a dispatcher parked with no pending user-worker syscall", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);
    const sab = freshSab(); // no request: the dispatcher must park
    const pidMap = new Map<number, ArrayBufferLike>([[pid, sab]]);
    let reportPark!: (observedWake: number) => void;
    const parked = new Promise<number>((resolve) => {
      reportPark = resolve;
    });
    let reported = false;
    let externalWorkObserved = false;

    const loop = host.startDispatchLoop({
      pidSource: () => pidMap,
      halted: () => externalWorkObserved,
      parkFn: async (observedWake): Promise<void> => {
        if (!reported) {
          reported = true;
          reportPark(observedWake);
        }
        await new Promise<void>((resolve, reject) => {
          let attempts = 0;
          const poll = (): void => {
            if (Atomics.load(host.wakeSlot, 0) !== observedWake) {
              externalWorkObserved = true;
              resolve();
              return;
            }
            attempts += 1;
            if (attempts >= 100) {
              reject(
                new Error("host file drop did not wake the dispatch loop"),
              );
              return;
            }
            setTimeout(poll, 0);
          };
          poll();
        });
      },
    });

    const observedWake = await parked;
    expect(responseRingHead(sab)).toBe(0);
    expect(Atomics.load(host.wakeSlot, 0)).toBe(observedWake);
    expect(
      host.hostFileDropped(
        0x4567,
        "dropped.txt",
        "text/plain",
        new TextEncoder().encode("from host"),
      ),
    ).toBe(true);

    await loop;
    expect(Atomics.load(host.wakeSlot, 0)).toBe(observedWake + 1);
    expect(externalWorkObserved).toBe(true);
    expect(requestRingTail(sab)).toBe(0);
    expect(responseRingHead(sab)).toBe(0);
  });
});
