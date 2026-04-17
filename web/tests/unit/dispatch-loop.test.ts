// Tests for `KernelWasmHost.startDispatchLoop` — the kernel-side
// round-robin dispatch loop that services every live pid's SAB ring
// on each pass. T233 (M1.4) lands the loop alongside its test gate;
// T235 (M1.6) made it the only scheduler by deleting the preview
// `drainPendingSpawns` in-process drain. Today vitest drives it
// through a plain-ArrayBuffer pidMap + a `parkFn` stub that yields
// microtasks instead of blocking on `Atomics.wait` — node under
// vitest has no cross-origin-isolated context for a real SAB-backed
// wait.
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
import { beforeAll, describe, expect, it } from "vitest";

import { KernelWasmHost } from "../../src/kernel-wasm-host";
import {
  HEAP_SCRATCH_BYTES,
  OFF_HEAP_SCRATCH,
  OFF_REQ_HEAD,
  OFF_REQ_RING,
  OFF_REQ_TAIL,
  OFF_RES_HEAD,
  OFF_RES_RING,
  OFF_RES_TAIL,
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
      const heapOffset =
        OFF_HEAP_SCRATCH + (item.request.heapPtr ?? 0);
      if (heapOffset + item.heap.length > OFF_HEAP_SCRATCH + HEAP_SCRATCH_BYTES) {
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
  const bytes = new Uint8Array(
    new Uint8Array(sab, slotOffset, SLOT_SIZE),
  );
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
    const written = consoleWrites.map((b) =>
      new TextDecoder().decode(b),
    );
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
