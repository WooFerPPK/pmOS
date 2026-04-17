// Tests for `SabBackend` — the per-pid `KernelBackend` implementation
// that translates a [`SyscallRequest`] into an SAB ring round-trip.
//
// The contract this slice (T231 / M1.2) pins down is byte-equivalence
// with [`KernelWasmHostBackend`], the in-process backend the user-wasm
// runtime uses today. Each test runs the same [`SyscallRequest`]
// through both backends against fresh kernel instances and asserts the
// returned [`DispatchResult`] is identical (response slot bytes +
// heap-out bytes), and that any kernel-side observable effect (e.g.
// `onConsoleWrite`) fires identically.
//
// In production (T233+), `SabBackend.dispatch` will park on
// `Atomics.wait(user_wait_slot, REQUESTED)` after publishing the
// request; the kernel Worker's poll loop wakes, calls
// [`KernelWasmHost.serviceSab`], and notifies the user wait slot. For
// T231 the user side stays single-threaded inside vitest, so a
// `serviceHook` callback is the synchronous stand-in: it runs
// `KernelWasmHost.serviceSab(pid, sab)` between the request push and
// the response pop. The wake slots themselves are intentionally NOT
// touched by either `SabBackend` or `serviceSab` — that wiring is
// T233's concern.

import fs from "node:fs";
import path from "node:path";
import { beforeAll, describe, expect, it } from "vitest";

import {
  KernelWasmHost,
  type DispatchResult,
} from "../../src/kernel-wasm-host";
import { SabBackend } from "../../src/sab-backend";
import { SAB_SIZE } from "../../src/shared/sab-layout";
import {
  CAPSET_ALL,
  DEV,
  ERRNO,
  OP_WASI,
  type SyscallRequest,
} from "../../src/shared/syscall";
import { KernelWasmHostBackend } from "../../src/user-wasm-runtime";

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

interface TestFixture {
  host: KernelWasmHost;
  consoleWrites: Uint8Array[];
}

async function freshHost(): Promise<TestFixture> {
  const consoleWrites: Uint8Array[] = [];
  const host = await KernelWasmHost.create(wasmBytes, {
    onConsoleWrite: (bytes) => {
      consoleWrites.push(bytes);
    },
    nowNs: () => 0n,
  });
  return { host, consoleWrites };
}

/** Plain-`ArrayBuffer` stand-in for a per-pid `SharedArrayBuffer`.
 * Vitest runs under node without cross-origin-isolation, so SABs are
 * unavailable; the layout math + atomics paths that `SabBackend` and
 * `serviceSab` walk are byte-identical against either backing. */
function freshSab(): Uint8Array {
  return new Uint8Array(new ArrayBuffer(SAB_SIZE));
}

/** Wrap (host, pid, sab) into a `SabBackend` whose `serviceHook`
 * synchronously runs the kernel servicing primitive — the T231
 * stand-in for the production `Atomics.wait` round trip. */
function makeSabBackend(
  host: KernelWasmHost,
  pid: number,
  sab: Uint8Array,
): SabBackend {
  return new SabBackend({
    sab,
    pid,
    serviceHook: () => {
      host.serviceSab(pid, sab);
    },
  });
}

/** Assert two `DispatchResult`s are byte-identical: same response
 * slot fields + same heap-out bytes. Using a helper keeps each test
 * focused on the request setup + observable side-effects instead of
 * repeating the four field checks five times. */
function expectByteEquivalent(
  actual: DispatchResult,
  reference: DispatchResult,
): void {
  expect(actual.response).toEqual(reference.response);
  expect(Array.from(actual.heapOut)).toEqual(Array.from(reference.heapOut));
}

// ---- FD_WRITE → /dev/console ---------------------------------------

describe("SabBackend: FD_WRITE → /dev/console", () => {
  it("matches KernelWasmHostBackend byte-for-byte and fires onConsoleWrite", async () => {
    const message = new TextEncoder().encode("hi\n");
    const request: SyscallRequest = {
      opcode: OP_WASI.FD_WRITE,
      requestId: 1,
      arg0: 1,
      heapPtr: 0,
      heapLen: message.length,
    };

    const ref = await freshHost();
    const refPid = ref.host.registerProcess(CAPSET_ALL);
    ref.host.installConsoleFd(refPid, 1);
    ref.host.markRunning(refPid);
    const refResult = new KernelWasmHostBackend(ref.host, refPid).dispatch(
      request,
      message,
    );

    const sub = await freshHost();
    const subPid = sub.host.registerProcess(CAPSET_ALL);
    sub.host.installConsoleFd(subPid, 1);
    sub.host.markRunning(subPid);
    const sab = freshSab();
    const subResult = makeSabBackend(sub.host, subPid, sab).dispatch(
      request,
      message,
    );

    expectByteEquivalent(subResult, refResult);

    expect(ref.consoleWrites).toHaveLength(1);
    expect(sub.consoleWrites).toHaveLength(1);
    expect(Array.from(sub.consoleWrites[0]!)).toEqual(
      Array.from(ref.consoleWrites[0]!),
    );
    expect(Array.from(sub.consoleWrites[0]!)).toEqual(Array.from(message));
  });
});

// ---- FD_READ after pre-injected console input ----------------------

describe("SabBackend: FD_READ with pre-seeded console input", () => {
  it("returns the injected bytes byte-equivalently to KernelWasmHostBackend", async () => {
    const seed = new TextEncoder().encode("abc");
    const request: SyscallRequest = {
      opcode: OP_WASI.FD_READ,
      requestId: 2,
      arg0: 0,
      heapPtr: 0,
      heapLen: 16,
    };

    const ref = await freshHost();
    ref.host.injectInput(DEV.CONSOLE, seed);
    const refPid = ref.host.registerProcess(CAPSET_ALL);
    ref.host.installConsoleFd(refPid, 0);
    ref.host.markRunning(refPid);
    const refResult = new KernelWasmHostBackend(ref.host, refPid).dispatch(
      request,
    );

    const sub = await freshHost();
    sub.host.injectInput(DEV.CONSOLE, seed);
    const subPid = sub.host.registerProcess(CAPSET_ALL);
    sub.host.installConsoleFd(subPid, 0);
    sub.host.markRunning(subPid);
    const sab = freshSab();
    const subResult = makeSabBackend(sub.host, subPid, sab).dispatch(request);

    expectByteEquivalent(subResult, refResult);
    expect(Array.from(subResult.heapOut)).toEqual(Array.from(seed));
  });
});

// ---- PATH_OPEN ENOENT ----------------------------------------------

describe("SabBackend: PATH_OPEN on a missing path", () => {
  it("propagates -ENOENT byte-equivalently to KernelWasmHostBackend", async () => {
    const missing = new TextEncoder().encode("/does/not/exist");
    const request: SyscallRequest = {
      opcode: OP_WASI.PATH_OPEN,
      requestId: 3,
      arg0: 0,
      heapPtr: 0,
      heapLen: missing.length,
    };

    const ref = await freshHost();
    const refPid = ref.host.registerProcess(CAPSET_ALL);
    ref.host.markRunning(refPid);
    const refResult = new KernelWasmHostBackend(ref.host, refPid).dispatch(
      request,
      missing,
    );

    const sub = await freshHost();
    const subPid = sub.host.registerProcess(CAPSET_ALL);
    sub.host.markRunning(subPid);
    const sab = freshSab();
    const subResult = makeSabBackend(sub.host, subPid, sab).dispatch(
      request,
      missing,
    );

    expectByteEquivalent(subResult, refResult);
    expect(subResult.response.status).toBe(-ERRNO.ENOENT);
  });
});

// ---- PROC_EXIT ------------------------------------------------------

describe("SabBackend: PROC_EXIT", () => {
  it("services PROC_EXIT(0) byte-equivalently to KernelWasmHostBackend", async () => {
    const request: SyscallRequest = {
      opcode: OP_WASI.PROC_EXIT,
      requestId: 4,
      arg0: 0,
    };

    const ref = await freshHost();
    const refPid = ref.host.registerProcess(CAPSET_ALL);
    ref.host.markRunning(refPid);
    const refResult = new KernelWasmHostBackend(ref.host, refPid).dispatch(
      request,
    );

    const sub = await freshHost();
    const subPid = sub.host.registerProcess(CAPSET_ALL);
    sub.host.markRunning(subPid);
    const sab = freshSab();
    const subResult = makeSabBackend(sub.host, subPid, sab).dispatch(request);

    expectByteEquivalent(subResult, refResult);
    expect(subResult.response.status).toBe(0);
    expect(subResult.response.value).toBe(0n);
    expect(subResult.response.extraLen).toBe(0);
  });
});

// ---- heap-overflow rejection ---------------------------------------

describe("SabBackend: heap overflow", () => {
  it("rejects payloads larger than the SAB heap-scratch capacity, like KernelWasmHostBackend rejects payloads above its own scratch capacity", async () => {
    // 40 KiB > 32 KiB SAB heap-scratch capacity AND > 4 KiB kernel
    // heap-scratch capacity, so both backends must throw.
    const huge = new Uint8Array(40_000);
    huge.fill(0x41);
    const request: SyscallRequest = {
      opcode: OP_WASI.FD_WRITE,
      requestId: 5,
      arg0: 1,
      heapPtr: 0,
      heapLen: huge.length,
    };

    const ref = await freshHost();
    const refPid = ref.host.registerProcess(CAPSET_ALL);
    ref.host.installConsoleFd(refPid, 1);
    ref.host.markRunning(refPid);
    expect(() =>
      new KernelWasmHostBackend(ref.host, refPid).dispatch(request, huge),
    ).toThrow(/capacity/);

    const sub = await freshHost();
    const subPid = sub.host.registerProcess(CAPSET_ALL);
    sub.host.installConsoleFd(subPid, 1);
    sub.host.markRunning(subPid);
    const sab = freshSab();
    expect(() =>
      makeSabBackend(sub.host, subPid, sab).dispatch(request, huge),
    ).toThrow(/capacity/);
  });
});
