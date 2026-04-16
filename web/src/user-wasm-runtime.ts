// `UserWasmRuntime` — loads a `wasm32-wasip1` user binary,
// satisfies its WASI preview 1 imports, and runs `_start` against
// a [`KernelBackend`] that funnels each translated WASI call into
// a PMos syscall.
//
// This is the first production path where a real user wasm binary
// runs against a real kernel. No mocks on either side: the user
// binary speaks `wasi_snapshot_preview1.*`, the runtime's shim
// translates those calls into PMos opcodes, and the backend
// dispatches them through [`KernelWasmHost`] (or, in tests, any
// other object that implements `KernelBackend`).
//
// ## Scope today (first real-user-wasm slice)
//
// The WASI shim implements exactly the two functions the
// `crates/hello-wasi-min` test fixture needs: `fd_write` and
// `proc_exit`. Every other `wasi_snapshot_preview1` function is
// absent. A user wasm that imports anything else will fail to
// instantiate with a clear "missing import" error — which is the
// right signal for the next "add the N WASI shims we need"
// slice.
//
// The runtime is single-threaded and **synchronous**. It is
// designed for the in-process case where the user wasm and the
// kernel live in the same JS thread (vitest harness, future
// bootstrap-driven preview loop). The cross-thread production
// path — where the user wasm runs in its own dedicated Web
// Worker and communicates with the kernel Worker over a SAB with
// `Atomics.wait` — is a separate slice that will own the real
// scheduling + wake model. By sharing the `KernelBackend`
// interface, both paths dispatch through the same PMos opcode
// format.
//
// ## Contract with the backend
//
// The shim calls `backend.dispatch(request, heapIn?)` exactly
// once per WASI function call. The backend is responsible for
// delivering the request to a kernel and returning the response
// synchronously. No queuing, no batching, no buffering — one
// WASI call = one PMos syscall. Keeping that contract tight
// means future multi-syscall WASI functions (e.g. `fd_readdir`,
// which in a real OS might span several reads) can be layered on
// without changing the backend interface.

import type { KernelWasmHost, DispatchResult } from "./kernel-wasm-host";
import {
  ERRNO,
  OP_WASI,
  type SyscallRequest,
} from "./shared/syscall";

// ---- Backend interface -----------------------------------------------

/**
 * Synchronous syscall dispatcher the WASI shim calls into. Any
 * object that can accept a [`SyscallRequest`] + optional heap
 * input and return a [`DispatchResult`] is a valid backend.
 *
 * The pid is NOT part of this interface because backends are
 * bound to a specific pid at construction — the runtime that
 * calls `dispatch` doesn't thread pids through every call, which
 * lets the backend be swapped wholesale between runs without
 * leaking process identity into the shim layer.
 */
export interface KernelBackend {
  dispatch(request: SyscallRequest, heapIn?: Uint8Array): DispatchResult;
}

/**
 * [`KernelBackend`] adapter that forwards every `dispatch` call
 * into a [`KernelWasmHost`] for a specific pid. The pid is frozen
 * at construction so every WASI shim call on the resulting
 * runtime lands on the same kernel process.
 *
 * This lives here instead of in `kernel-wasm-host.ts` so the
 * runtime module is self-contained: a caller that wants to run a
 * user binary against a kernel host needs exactly one import
 * (`UserWasmRuntime` + `KernelWasmHostBackend`) from this file.
 */
export class KernelWasmHostBackend implements KernelBackend {
  constructor(
    private readonly host: KernelWasmHost,
    private readonly pid: number,
  ) {}

  dispatch(request: SyscallRequest, heapIn?: Uint8Array): DispatchResult {
    return this.host.dispatch(this.pid, request, heapIn);
  }
}

// ---- Exit sentinel ---------------------------------------------------

/**
 * Sentinel error thrown by the WASI shim's `proc_exit` to unwind
 * out of the user wasm's `_start` call. The runtime catches it,
 * extracts the exit code, and returns normally from `run()`.
 *
 * Using a thrown sentinel instead of a `longjmp`-style flag is
 * the only way to unwind out of `wasm._start()` in JS without
 * running arbitrary code after the exit. Any wasm frame on the
 * JS stack is torn down by the throw, which matches the WASI
 * `proc_exit: never returns` contract.
 */
export class UserProcessExited extends Error {
  constructor(public readonly exitCode: number) {
    super(`user process exited with code ${exitCode}`);
    this.name = "UserProcessExited";
  }
}

// ---- Runtime ---------------------------------------------------------

/** Shape of the subset of user-wasm exports the runtime touches. */
interface UserExports {
  readonly memory: WebAssembly.Memory;
  readonly _start: () => void;
}

/**
 * Handles one wasm32-wasip1 binary + one kernel backend + one
 * run of `_start`. Single-use: construct, call `run()`, read the
 * exit code.
 */
export class UserWasmRuntime {
  /** Populated when `run()` begins instantiation. */
  private memory: WebAssembly.Memory | undefined;

  constructor(
    private readonly wasmBytes: BufferSource,
    private readonly backend: KernelBackend,
  ) {}

  /**
   * Instantiate the user wasm, satisfy its WASI imports with the
   * shim, call `_start`, and return the exit code.
   *
   * Returns `0` if `_start` runs to completion via
   * `proc_exit(0)` (the normal path). Returns whatever code the
   * user passed to `proc_exit` otherwise. If `_start` returns
   * without ever calling `proc_exit` — which the hello-wasi-min
   * fixture never does — returns `0` as well (matches WASI's
   * "main returning is equivalent to proc_exit(0)" convention).
   *
   * Any error that is NOT a [`UserProcessExited`] sentinel
   * bubbles up to the caller.
   */
  async run(): Promise<number> {
    const imports = this.buildImports();
    const { instance } = await WebAssembly.instantiate(this.wasmBytes, imports);
    const exports = instance.exports as unknown as UserExports;
    if (typeof exports._start !== "function") {
      throw new Error("UserWasmRuntime: user wasm has no `_start` export");
    }
    if (!(exports.memory instanceof WebAssembly.Memory)) {
      throw new Error("UserWasmRuntime: user wasm does not export `memory`");
    }
    this.memory = exports.memory;

    try {
      exports._start();
      return 0;
    } catch (err) {
      if (err instanceof UserProcessExited) {
        return err.exitCode;
      }
      throw err;
    }
  }

  /**
   * Build the `wasi_snapshot_preview1` import namespace the user
   * wasm expects. Each function closes over `this` so it can
   * reach the user's memory (for iovec reads + nwritten writes)
   * and the backend (for syscall dispatch).
   */
  private buildImports(): WebAssembly.Imports {
    const shim = {
      // WASI `fd_write`.
      //
      // Signature (lowered):
      //   (fd: i32, iovs_ptr: i32, iovs_len: i32, nwritten_ptr: i32) -> errno: i32
      //
      // Iovec layout at `iovs_ptr + i*8`:
      //   [0..4]  = buf (i32 pointer into user memory)
      //   [4..8]  = buf_len (u32)
      //
      // We concatenate every iov's bytes into one flat Uint8Array
      // and submit as a single PMos `FD_WRITE` syscall. The kernel
      // can't see the gather list because the PMos opcode API
      // carries a single contiguous heap payload per request; the
      // shim absorbs the scatter-gather complexity on this side.
      //
      // Errno mapping: PMos `Response.status` is the negated
      // errno (-EBADF, -EINVAL, ...). WASI expects the positive
      // errno. We negate back.
      fd_write: (
        fd: number,
        iovsPtr: number,
        iovsLen: number,
        nwrittenPtr: number,
      ): number => {
        if (this.memory === undefined) {
          return ERRNO.EINVAL;
        }

        // Read the scatter-gather list out of user memory and
        // compute the total byte count. Each iov copy is into a
        // freshly-allocated Uint8Array so the concatenation below
        // doesn't reference the user's memory (which could in
        // theory be grown mid-call).
        const readView = new DataView(this.memory.buffer);
        const gathered: Uint8Array[] = [];
        let total = 0;
        for (let i = 0; i < iovsLen; i += 1) {
          const iovBase = iovsPtr + i * 8;
          const bufPtr = readView.getUint32(iovBase, true);
          const bufLen = readView.getUint32(iovBase + 4, true);
          const src = new Uint8Array(this.memory.buffer, bufPtr, bufLen);
          gathered.push(new Uint8Array(src));
          total += bufLen;
        }

        // Flatten for the syscall payload.
        const payload = new Uint8Array(total);
        {
          let offset = 0;
          for (const buf of gathered) {
            payload.set(buf, offset);
            offset += buf.length;
          }
        }

        // Dispatch the PMos FD_WRITE syscall.
        const request: SyscallRequest = {
          opcode: OP_WASI.FD_WRITE,
          // The runtime doesn't yet track per-syscall ids. A
          // future slice with concurrent in-flight requests will
          // need a monotonic counter here.
          requestId: 0,
          arg0: fd,
          heapPtr: 0,
          heapLen: total,
        };
        const { response } = this.backend.dispatch(request, payload);

        if (response.status !== 0) {
          // Negated errno coming back from the kernel; WASI wants
          // the positive form.
          return -response.status;
        }

        // Write the actual byte count into user memory at
        // `nwritten_ptr`. Re-read `memory.buffer` because the
        // backend's dispatch could in principle have caused the
        // kernel's allocator to grow its own memory — which
        // doesn't affect the USER wasm's memory here, but the
        // discipline of "never hold a view across a kernel call"
        // is worth keeping consistent.
        const writeView = new DataView(this.memory.buffer);
        writeView.setUint32(nwrittenPtr, Number(response.value), true);
        return 0;
      },

      // WASI `proc_exit`.
      //
      // Signature: (rval: i32) -> never.
      //
      // Throws a `UserProcessExited` sentinel that the runtime's
      // `run()` method catches. The wasm instance's _start frame
      // is torn down as the throw unwinds through it — exactly
      // the semantics WASI specifies.
      proc_exit: (rval: number): never => {
        throw new UserProcessExited(rval);
      },
    } as const;

    return {
      wasi_snapshot_preview1: shim,
    };
  }
}
