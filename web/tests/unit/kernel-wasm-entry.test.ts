// Integration tests for the kernel.wasm extern "C" entry
// points — the seam between the browser's WebAssembly engine
// and the Rust syscall dispatcher.
//
// These tests load the release-mode kernel.wasm built by
// `just build` (or `cargo build --release --target
// wasm32-unknown-unknown -p kernel --no-default-features`),
// instantiate it with stubs for the four host imports the
// kernel's WasmPlatform pulls in (`pmos_host_*`), then drive
// the exports the way `kernel-worker.ts` will in production:
// init, register process, mark running, write a Request into
// the kernel's linear memory at the advertised scratch region,
// call `kernel_dispatch`, and read the Response back.
//
// The test depends on `target/wasm32-unknown-unknown/release/
// kernel.wasm` existing. On a clean checkout, run `just build`
// (or the `cargo build` command above) once first. The
// `beforeAll` hook fails loudly with a reproducible "run
// `just build` first" hint if the file is missing.
//
// Coverage matches what `crates/kernel/tests/syscall.rs`
// already proves for the native dispatcher: the opcode handlers
// for `PROC_SELF`, `PROC_PARENT`, `CAP_CHECK`, `CAP_LIST`,
// `FD_WRITE`, `FD_CLOSE`, `PATH_OPEN`, plus an unknown-opcode
// ENOSYS probe. FD_WRITE in particular exercises the host-
// import path end-to-end because console writes route through
// `pmos_host_driver_call` — the stub captures bytes so the
// test can assert the correct payload landed.

import fs from "node:fs";
import path from "node:path";
import { beforeAll, describe, expect, it } from "vitest";

// ---- opcode + cap constants (mirror abi/src/wasi.rs + ext.rs + cap.rs) -

const OP_FD_WRITE = 0x0034;
const OP_FD_CLOSE = 0x0022;
const OP_PATH_OPEN = 0x0044;
const OP_PROC_SELF = 0x1103;
const OP_PROC_PARENT = 0x1104;
const OP_CAP_CHECK = 0x1300;
const OP_CAP_LIST = 0x1301;

const CAP_DISPLAY_CLIENT = 1;
const CAP_DISPLAY_SERVER = 2;
// CapSet::ALL = u64::MAX — every bit set. The kernel stores caps as
// u64 and transports them as i64; u64::MAX re-interpreted as i64 is -1.
const CAPSET_ALL = 0xffff_ffff_ffff_ffffn;
const CAPSET_DESKTOP_SHELL =
  (1n << BigInt(CAP_DISPLAY_CLIENT)) |
  (1n << 3n) | // Shell
  (1n << 4n) | // ProcEnumerate
  (1n << 10n); // KeymapAdmin

// DevId::Console — matches platform/mod.rs::DevId::Console = 5.
const DEV_CONSOLE = 5;

// errno — matches abi/src/errno.rs.
const EBADF = 8;
const ENOENT = 44;
const ENOSYS = 52;

// ---- kernel.wasm export surface ---------------------------------------

interface KernelExports {
  memory: WebAssembly.Memory;
  kernel_init: () => number;
  kernel_register_process: (caps: bigint) => number;
  kernel_install_console_fd: (pid: number, fd: number) => number;
  kernel_mark_running: (pid: number) => number;
  kernel_dispatch: (pid: number) => number;
  kernel_req_ptr: () => number;
  kernel_resp_ptr: () => number;
  kernel_heap_ptr: () => number;
  kernel_heap_len: () => number;
}

interface HostState {
  /** Bytes the kernel routed through `driver_call(Console, ...)`. */
  consoleWrites: Uint8Array[];
  /** Count of `pmos_host_panic` invocations. Any value > 0 is a test failure. */
  panics: number;
}

async function loadKernel(): Promise<{ kernel: KernelExports; host: HostState }> {
  const wasmPath = path.resolve(
    __dirname,
    "../../../target/wasm32-unknown-unknown/release/kernel.wasm",
  );
  if (!fs.existsSync(wasmPath)) {
    throw new Error(
      `kernel.wasm not found at ${wasmPath}. Run \`just build\` first.`,
    );
  }
  const bytes = fs.readFileSync(wasmPath);

  const host: HostState = { consoleWrites: [], panics: 0 };
  let memory: WebAssembly.Memory | undefined;

  const imports = {
    env: {
      pmos_host_halt: (_ptr: number, _len: number): never => {
        throw new Error("pmos_host_halt called from test");
      },
      pmos_host_now_ns: (): bigint => {
        // Deterministic monotonic clock for the direct-export
        // test surface: returns 0 on every call. `CLOCK_TIME_GET`
        // tests that need strict monotonicity live in the
        // higher-level `kernel-wasm-host.test.ts` where the
        // default `performance.now()`-based clock is in play.
        return 0n;
      },
      pmos_host_now_realtime_ns: (): bigint => {
        // Deterministic wall clock, same rationale as `now_ns`.
        // `CLOCK_TIME_GET(REALTIME)` behaviour is covered in
        // `kernel-wasm-host.test.ts` + the `hello-clock`
        // acceptance test in `user-wasm-runtime.test.ts`.
        return 0n;
      },
      pmos_host_random_bytes: (_ptr: number, _len: number): void => {
        // No-op — none of the tested opcodes need random bytes.
      },
      pmos_host_driver_call: (
        dev: number,
        _op: number,
        argsPtr: number,
        argsLen: number,
        _resultPtr: number,
      ): number => {
        if (dev === DEV_CONSOLE && memory !== undefined) {
          // Copy immediately; the WASM memory buffer can be
          // detached by a subsequent `memory.grow`, and a view
          // into a detached buffer throws.
          const view = new Uint8Array(memory.buffer, argsPtr, argsLen);
          host.consoleWrites.push(new Uint8Array(view));
        }
        return 0;
      },
      pmos_host_panic: (_ptr: number, _len: number): void => {
        host.panics += 1;
      },
      pmos_host_spawn_process: (
        _pid: number,
        _pathPtr: number,
        _pathLen: number,
      ): number => {
        // No-op stub: this test file exercises the *direct export
        // surface* and does not issue PROC_SPAWN. The stub exists
        // only so instantiation succeeds after the import was added
        // to `platform/wasm.rs`. A richer spawn test lives in
        // `kernel-wasm-host.test.ts`.
        return 0;
      },
    },
  };

  const { instance } = await WebAssembly.instantiate(bytes, imports);
  const exports = instance.exports as unknown as KernelExports;
  memory = exports.memory;
  return { kernel: exports, host };
}

// ---- request / response memory helpers --------------------------------

/**
 * Write a [`Request`] into the kernel's linear memory at
 * `kernel_req_ptr()`. Caller provides the opcode, a u32 arg at
 * offset 0 of the inline args window (most handlers need only
 * that), request_id, and the heap pointer + length fields.
 *
 * Always re-reads `memory.buffer` because the bump allocator
 * may have grown the memory between calls, detaching the
 * previous buffer reference.
 */
function writeRequest(
  kernel: KernelExports,
  opts: {
    opcode: number;
    requestId: number;
    arg0: number;
    heapPtr: number;
    heapLen: number;
  },
): void {
  const reqPtr = kernel.kernel_req_ptr();
  const view = new DataView(kernel.memory.buffer);
  view.setUint16(reqPtr + 0, opts.opcode, true);
  view.setUint16(reqPtr + 2, 0, true); // flags
  view.setUint32(reqPtr + 4, opts.requestId, true);
  // Zero the 16-byte inline args region, then write arg0.
  for (let i = 0; i < 16; i += 1) {
    view.setUint8(reqPtr + 8 + i, 0);
  }
  view.setUint32(reqPtr + 8, opts.arg0, true);
  view.setUint32(reqPtr + 24, opts.heapPtr, true);
  view.setUint32(reqPtr + 28, opts.heapLen, true);
}

/**
 * Read the [`Response`] the kernel wrote into `kernel_resp_ptr()`.
 * Returns the four semantically-interesting fields; the 12-byte
 * padding is ignored because no handler uses it yet.
 */
function readResponse(kernel: KernelExports): {
  requestId: number;
  status: number;
  value: bigint;
  extraLen: number;
} {
  const respPtr = kernel.kernel_resp_ptr();
  const view = new DataView(kernel.memory.buffer);
  return {
    requestId: view.getUint32(respPtr + 0, true),
    status: view.getInt32(respPtr + 4, true),
    value: view.getBigInt64(respPtr + 8, true),
    extraLen: view.getUint32(respPtr + 16, true),
  };
}

/**
 * Write a byte slice into the kernel's heap scratch region at
 * the given offset (relative to `kernel_heap_ptr()`).
 */
function writeHeap(kernel: KernelExports, offset: number, bytes: Uint8Array): void {
  const heapPtr = kernel.kernel_heap_ptr();
  const dest = new Uint8Array(kernel.memory.buffer, heapPtr + offset, bytes.length);
  dest.set(bytes);
}

// ---- tests ------------------------------------------------------------

describe("kernel.wasm extern C entry points", () => {
  let kernel: KernelExports;
  let host: HostState;

  beforeAll(async () => {
    ({ kernel, host } = await loadKernel());
    expect(kernel.kernel_init()).toBe(0);
    // Idempotent — a second call is a no-op.
    expect(kernel.kernel_init()).toBe(0);
  });

  // ---- module surface sanity checks -------------------------------

  it("exports point at nonzero addresses within linear memory", () => {
    const reqPtr = kernel.kernel_req_ptr();
    const respPtr = kernel.kernel_resp_ptr();
    const heapPtr = kernel.kernel_heap_ptr();
    const heapLen = kernel.kernel_heap_len();
    const memSize = kernel.memory.buffer.byteLength;

    expect(reqPtr).toBeGreaterThan(0);
    expect(respPtr).toBeGreaterThan(0);
    expect(heapPtr).toBeGreaterThan(0);
    expect(heapLen).toBe(4096);
    // Every pointer region fits inside the current linear memory.
    expect(reqPtr + 32).toBeLessThanOrEqual(memSize);
    expect(respPtr + 32).toBeLessThanOrEqual(memSize);
    expect(heapPtr + heapLen).toBeLessThanOrEqual(memSize);
    // Scratch regions are disjoint: the response doesn't
    // overlap the request, and neither overlaps the heap.
    expect(reqPtr).not.toBe(respPtr);
    expect(heapPtr).not.toBe(reqPtr);
    expect(heapPtr).not.toBe(respPtr);
  });

  it("no panics recorded during init", () => {
    expect(host.panics).toBe(0);
  });

  // ---- process lifecycle ------------------------------------------

  it("register_process returns fresh monotonic pids", () => {
    const a = kernel.kernel_register_process(CAPSET_ALL);
    const b = kernel.kernel_register_process(CAPSET_ALL);
    expect(a).toBeGreaterThan(0);
    expect(b).toBeGreaterThan(a);
  });

  it("mark_running transitions a registered process", () => {
    const pid = kernel.kernel_register_process(CAPSET_ALL);
    expect(kernel.kernel_mark_running(pid)).toBe(0);
  });

  // ---- PROC_SELF + PROC_PARENT ------------------------------------

  it("PROC_SELF returns the caller pid", () => {
    const pid = kernel.kernel_register_process(CAPSET_ALL);
    expect(kernel.kernel_mark_running(pid)).toBe(0);

    writeRequest(kernel, {
      opcode: OP_PROC_SELF,
      requestId: 100,
      arg0: 0,
      heapPtr: 0,
      heapLen: 0,
    });
    expect(kernel.kernel_dispatch(pid)).toBe(0);

    const resp = readResponse(kernel);
    expect(resp.requestId).toBe(100);
    expect(resp.status).toBe(0);
    expect(resp.value).toBe(BigInt(pid));
  });

  it("PROC_PARENT returns the ppid (0 for register_process)", () => {
    const pid = kernel.kernel_register_process(CAPSET_ALL);
    expect(kernel.kernel_mark_running(pid)).toBe(0);

    writeRequest(kernel, {
      opcode: OP_PROC_PARENT,
      requestId: 101,
      arg0: 0,
      heapPtr: 0,
      heapLen: 0,
    });
    expect(kernel.kernel_dispatch(pid)).toBe(0);

    const resp = readResponse(kernel);
    expect(resp.requestId).toBe(101);
    expect(resp.status).toBe(0);
    // kernel_register_process hardcodes ppid = 0.
    expect(resp.value).toBe(0n);
  });

  // ---- CAP_CHECK + CAP_LIST ---------------------------------------

  it("CAP_CHECK returns 1 for a held cap", () => {
    const pid = kernel.kernel_register_process(CAPSET_ALL);
    expect(kernel.kernel_mark_running(pid)).toBe(0);

    writeRequest(kernel, {
      opcode: OP_CAP_CHECK,
      requestId: 200,
      arg0: CAP_DISPLAY_CLIENT,
      heapPtr: 0,
      heapLen: 0,
    });
    expect(kernel.kernel_dispatch(pid)).toBe(0);

    const resp = readResponse(kernel);
    expect(resp.status).toBe(0);
    expect(resp.value).toBe(1n);
  });

  it("CAP_CHECK returns 0 for a cap not held by the caller", () => {
    // A process with desktop-shell caps does NOT hold DisplayServer.
    const pid = kernel.kernel_register_process(CAPSET_DESKTOP_SHELL);
    expect(kernel.kernel_mark_running(pid)).toBe(0);

    writeRequest(kernel, {
      opcode: OP_CAP_CHECK,
      requestId: 201,
      arg0: CAP_DISPLAY_SERVER,
      heapPtr: 0,
      heapLen: 0,
    });
    expect(kernel.kernel_dispatch(pid)).toBe(0);

    const resp = readResponse(kernel);
    expect(resp.status).toBe(0);
    expect(resp.value).toBe(0n);
  });

  it("CAP_LIST returns the full u64 cap bitset (-1 for CapSet::ALL)", () => {
    const pid = kernel.kernel_register_process(CAPSET_ALL);
    expect(kernel.kernel_mark_running(pid)).toBe(0);

    writeRequest(kernel, {
      opcode: OP_CAP_LIST,
      requestId: 202,
      arg0: 0,
      heapPtr: 0,
      heapLen: 0,
    });
    expect(kernel.kernel_dispatch(pid)).toBe(0);

    const resp = readResponse(kernel);
    expect(resp.status).toBe(0);
    // Response.value is i64. u64::MAX reinterpreted as i64 is -1.
    expect(resp.value).toBe(-1n);
  });

  // ---- FD_WRITE via /dev/console (exercises pmos_host_driver_call) -

  it("FD_WRITE to /dev/console routes captured bytes through driver_call", () => {
    const pid = kernel.kernel_register_process(CAPSET_ALL);
    expect(kernel.kernel_mark_running(pid)).toBe(0);
    // Install fd 1 as /dev/console.
    expect(kernel.kernel_install_console_fd(pid, 1)).toBe(0);

    // Write "hello\n" — the newline is what makes console_write
    // flush the line through `platform::current().driver_call`,
    // which the host stub captures.
    const message = new TextEncoder().encode("hello\n");
    writeHeap(kernel, 0, message);

    host.consoleWrites.length = 0;
    writeRequest(kernel, {
      opcode: OP_FD_WRITE,
      requestId: 300,
      arg0: 1, // fd
      heapPtr: 0,
      heapLen: message.length,
    });
    expect(kernel.kernel_dispatch(pid)).toBe(0);

    const resp = readResponse(kernel);
    expect(resp.requestId).toBe(300);
    expect(resp.status).toBe(0);
    expect(resp.value).toBe(BigInt(message.length));

    // The host stub received exactly one driver_call with the
    // full line. console_write only flushes complete lines.
    expect(host.consoleWrites).toHaveLength(1);
    expect(Array.from(host.consoleWrites[0]!)).toEqual(Array.from(message));
  });

  it("FD_WRITE with bad fd returns -EBADF in the response", () => {
    const pid = kernel.kernel_register_process(CAPSET_ALL);
    expect(kernel.kernel_mark_running(pid)).toBe(0);

    writeRequest(kernel, {
      opcode: OP_FD_WRITE,
      requestId: 301,
      arg0: 99, // no fd installed
      heapPtr: 0,
      heapLen: 0,
    });
    expect(kernel.kernel_dispatch(pid)).toBe(0);

    const resp = readResponse(kernel);
    expect(resp.status).toBe(-EBADF);
  });

  // ---- PATH_OPEN --------------------------------------------------

  it("PATH_OPEN on /dev/console returns a fresh fd", () => {
    const pid = kernel.kernel_register_process(CAPSET_ALL);
    expect(kernel.kernel_mark_running(pid)).toBe(0);

    const path = new TextEncoder().encode("/dev/console");
    writeHeap(kernel, 0, path);

    writeRequest(kernel, {
      opcode: OP_PATH_OPEN,
      requestId: 400,
      arg0: 0, // FdFlags::EMPTY
      heapPtr: 0,
      heapLen: path.length,
    });
    expect(kernel.kernel_dispatch(pid)).toBe(0);

    const resp = readResponse(kernel);
    expect(resp.requestId).toBe(400);
    expect(resp.status).toBe(0);
    // Fresh process, no fds yet, so fd 0 is returned.
    expect(resp.value).toBe(0n);
  });

  it("PATH_OPEN on a nonexistent path returns -ENOENT", () => {
    const pid = kernel.kernel_register_process(CAPSET_ALL);
    expect(kernel.kernel_mark_running(pid)).toBe(0);

    const path = new TextEncoder().encode("/nope/definitely-not-here");
    writeHeap(kernel, 0, path);

    writeRequest(kernel, {
      opcode: OP_PATH_OPEN,
      requestId: 401,
      arg0: 0,
      heapPtr: 0,
      heapLen: path.length,
    });
    expect(kernel.kernel_dispatch(pid)).toBe(0);

    const resp = readResponse(kernel);
    expect(resp.status).toBe(-ENOENT);
  });

  // ---- FD_CLOSE ---------------------------------------------------

  it("FD_CLOSE releases an installed fd", () => {
    const pid = kernel.kernel_register_process(CAPSET_ALL);
    expect(kernel.kernel_mark_running(pid)).toBe(0);
    expect(kernel.kernel_install_console_fd(pid, 2)).toBe(0);

    writeRequest(kernel, {
      opcode: OP_FD_CLOSE,
      requestId: 500,
      arg0: 2,
      heapPtr: 0,
      heapLen: 0,
    });
    expect(kernel.kernel_dispatch(pid)).toBe(0);

    const resp = readResponse(kernel);
    expect(resp.status).toBe(0);

    // A second close on the same fd returns EBADF.
    writeRequest(kernel, {
      opcode: OP_FD_CLOSE,
      requestId: 501,
      arg0: 2,
      heapPtr: 0,
      heapLen: 0,
    });
    expect(kernel.kernel_dispatch(pid)).toBe(0);
    const again = readResponse(kernel);
    expect(again.status).toBe(-EBADF);
  });

  // ---- unknown opcode ---------------------------------------------

  it("unknown opcode returns -ENOSYS", () => {
    const pid = kernel.kernel_register_process(CAPSET_ALL);
    expect(kernel.kernel_mark_running(pid)).toBe(0);

    writeRequest(kernel, {
      opcode: 0x4242, // not in either range
      requestId: 600,
      arg0: 0,
      heapPtr: 0,
      heapLen: 0,
    });
    expect(kernel.kernel_dispatch(pid)).toBe(0);

    const resp = readResponse(kernel);
    expect(resp.requestId).toBe(600);
    expect(resp.status).toBe(-ENOSYS);
  });

  it("known WASI opcode with no handler returns -ENOSYS (PATH_SYMLINK)", () => {
    const pid = kernel.kernel_register_process(CAPSET_ALL);
    expect(kernel.kernel_mark_running(pid)).toBe(0);

    writeRequest(kernel, {
      opcode: 0x0048, // PATH_SYMLINK — in WASI range but not implemented
      requestId: 601,
      arg0: 0,
      heapPtr: 0,
      heapLen: 0,
    });
    expect(kernel.kernel_dispatch(pid)).toBe(0);
    expect(readResponse(kernel).status).toBe(-ENOSYS);
  });

  // ---- request_id echo + no panics --------------------------------

  it("never triggers a host panic during normal dispatch traffic", () => {
    expect(host.panics).toBe(0);
  });
});
