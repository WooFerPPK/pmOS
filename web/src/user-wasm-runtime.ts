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
  encodeSpawnManifest,
  ERRNO,
  OP_EXT,
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
   * Shared helper for `args_sizes_get` + `environ_sizes_get`.
   * Both opcodes return (count, buf_size) as two little-endian
   * u32s in the kernel's heap-out window; this method dispatches,
   * reads the pair, and writes them into user memory at the two
   * out-pointers the WASI signature carries.
   */
  private sizes_get(opcode: number, countPtr: number, bufSizePtr: number): number {
    if (this.memory === undefined) return ERRNO.EINVAL;
    const { response, heapOut } = this.backend.dispatch({
      opcode,
      requestId: 0,
      heapPtr: 0,
      heapLen: 8,
    });
    if (response.status !== 0) return -response.status;
    const readView = new DataView(
      heapOut.buffer,
      heapOut.byteOffset,
      heapOut.byteLength,
    );
    const count = readView.getUint32(0, true);
    const bufSize = readView.getUint32(4, true);
    const writeView = new DataView(this.memory.buffer);
    writeView.setUint32(countPtr, count, true);
    writeView.setUint32(bufSizePtr, bufSize, true);
    return 0;
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

      // WASI `fd_read`.
      //
      // Signature (lowered):
      //   (fd: i32, iovs_ptr: i32, iovs_len: i32, nread_ptr: i32) -> errno: i32
      //
      // Iovec layout at `iovs_ptr + i*8`:
      //   [0..4]  = buf (i32 pointer into user memory; writable
      //                  destination for this iovec's slice)
      //   [4..8]  = buf_len (u32, capacity)
      //
      // Strategy: sum every iovec's `buf_len` into a single total
      // capacity, dispatch ONE `FD_READ` syscall with that
      // capacity, and then distribute the returned bytes across
      // the iovecs in order (filling each up to its own capacity
      // before moving to the next). This mirrors the `fd_write`
      // path that gather-concatenates user buffers into one
      // payload — the PMos opcode API carries a single contiguous
      // heap window per request, so scatter-gather lives entirely
      // on this side.
      //
      // Zero-length reads (iovs_len == 0 or every buf_len == 0)
      // short-circuit to `nread = 0` without calling the kernel,
      // matching the WASI contract that a zero-capacity read is
      // a trivial success.
      fd_read: (
        fd: number,
        iovsPtr: number,
        iovsLen: number,
        nreadPtr: number,
      ): number => {
        if (this.memory === undefined) {
          return ERRNO.EINVAL;
        }

        // Read the iovec list and compute total capacity. Each
        // iovec is captured as (ptr, len) in a JS array so the
        // distribution loop below can walk it without re-reading
        // user memory (which could detach if the dispatch causes
        // an unexpected grow).
        let view = new DataView(this.memory.buffer);
        const iovecs: Array<{ ptr: number; len: number }> = [];
        let totalCapacity = 0;
        for (let i = 0; i < iovsLen; i += 1) {
          const iovBase = iovsPtr + i * 8;
          const bufPtr = view.getUint32(iovBase, true);
          const bufLen = view.getUint32(iovBase + 4, true);
          iovecs.push({ ptr: bufPtr, len: bufLen });
          totalCapacity += bufLen;
        }

        if (totalCapacity === 0) {
          view.setUint32(nreadPtr, 0, true);
          return 0;
        }

        // Dispatch a single FD_READ with the combined capacity.
        const { response, heapOut } = this.backend.dispatch({
          opcode: OP_WASI.FD_READ,
          requestId: 0,
          arg0: fd,
          heapPtr: 0,
          heapLen: totalCapacity,
        });

        if (response.status !== 0) {
          return -response.status;
        }

        const nread = Number(response.value);
        // Distribute `heapOut` across the iovecs in order.
        // `heapOut.length` equals `response.extraLen` which equals
        // `nread`, but we loop with `nread` to stay explicit about
        // the byte count contract.
        let offset = 0;
        const writeBuf = new Uint8Array(this.memory.buffer);
        for (const iov of iovecs) {
          if (offset >= nread) break;
          const chunk = Math.min(iov.len, nread - offset);
          if (chunk === 0) continue;
          writeBuf.set(
            heapOut.subarray(offset, offset + chunk),
            iov.ptr,
          );
          offset += chunk;
        }

        // `view` may be stale if the set() above triggered a
        // grow. Re-fetch before writing nread.
        view = new DataView(this.memory.buffer);
        view.setUint32(nreadPtr, nread, true);
        return 0;
      },

      // WASI `path_open`.
      //
      // Signature (lowered):
      //   path_open(
      //     dirfd:     i32,  // directory fd (ignored — we don't
      //                      //   do preopens; every path is
      //                      //   absolute)
      //     dirflags:  i32,  // symlink-follow flags (ignored
      //                      //   in v1)
      //     path_ptr:  i32,
      //     path_len:  i32,
      //     oflags:    i32,  // CREAT / TRUNC / DIRECTORY / EXCL
      //                      //   — not wired on the PMos kernel
      //                      //   side (`Kernel::path_open` takes
      //                      //   only `FdFlags`), ignored for now
      //     rights_base:       i64, // ignored (v1 rights model)
      //     rights_inheriting: i64, // ignored
      //     fdflags:   i32,  // APPEND / NONBLOCK / ... WASI bits
      //                      //   don't line up with PMos FdFlags
      //                      //   bits (different positions);
      //                      //   v1 userland passes 0 and the
      //                      //   shim passes 0 through
      //     fd_out_ptr: i32, // i32 out-pointer for the new fd
      //   ) -> errno: i32
      //
      // The PMos `PATH_OPEN` opcode carries only `flags` (u32)
      // and a UTF-8 `path` on the heap, so the shim ignores
      // every argument WASI has that the kernel doesn't yet
      // care about. When a future slice wires the other bits
      // (preopens for `/home/user`, oflags for O_CREAT, the
      // rights model for sandboxed apps), each becomes a new
      // decode step here — no wire-format break because the
      // kernel's `FD_READ` / `FD_WRITE` semantics are
      // unchanged.
      path_open: (
        _dirfd: number,
        _dirflags: number,
        pathPtr: number,
        pathLen: number,
        _oflags: number,
        _rightsBase: bigint,
        _rightsInheriting: bigint,
        _fdflags: number,
        fdOutPtr: number,
      ): number => {
        if (this.memory === undefined) {
          return ERRNO.EINVAL;
        }
        const pathBytes = new Uint8Array(
          this.memory.buffer,
          pathPtr,
          pathLen,
        );
        const pathCopy = new Uint8Array(pathBytes);

        const { response } = this.backend.dispatch(
          {
            opcode: OP_WASI.PATH_OPEN,
            requestId: 0,
            arg0: 0, // FdFlags::EMPTY — WASI fdflags not yet wired
            heapPtr: 0,
            heapLen: pathLen,
          },
          pathCopy,
        );

        if (response.status !== 0) {
          return -response.status;
        }

        // Write the new fd to the out-pointer. Re-fetch the
        // memory view in case dispatch triggered a grow.
        const view = new DataView(this.memory.buffer);
        view.setUint32(fdOutPtr, Number(response.value), true);
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

      // WASI `args_sizes_get` / `environ_sizes_get`.
      //
      // Signature: (argc_or_envc_ptr: i32, buf_size_ptr: i32) -> errno.
      //
      // Dispatches the PMos opcode with an 8-byte heap-out window,
      // reads the two u32s the kernel wrote, and stores them at the
      // user-memory out-pointers. v1 always reports `(0, 0)`; a
      // future slice that attaches real argv/envp to `SpawnArgs`
      // changes only the kernel handler — this shim stays intact.
      args_sizes_get: (
        argcPtr: number,
        bufSizePtr: number,
      ): number => {
        return this.sizes_get(OP_WASI.ARGS_SIZES_GET, argcPtr, bufSizePtr);
      },

      environ_sizes_get: (
        envcPtr: number,
        bufSizePtr: number,
      ): number => {
        return this.sizes_get(OP_WASI.ENVIRON_SIZES_GET, envcPtr, bufSizePtr);
      },

      // WASI `args_get` / `environ_get`.
      //
      // Signature: (argv_ptr: i32, argv_buf_ptr: i32) -> errno.
      //
      // v1 returns an empty list, so there's nothing for the shim
      // to write into user memory — the kernel handler is a no-op
      // success. The out-pointers are accepted and ignored; when
      // argc transitions to non-zero, the handler + this shim gain
      // a pointer-table build step in lockstep.
      args_get: (_argvPtr: number, _argvBufPtr: number): number => {
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.ARGS_GET,
          requestId: 0,
          heapPtr: 0,
          heapLen: 0,
        });
        return response.status !== 0 ? -response.status : 0;
      },

      environ_get: (_envPtr: number, _envBufPtr: number): number => {
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.ENVIRON_GET,
          requestId: 0,
          heapPtr: 0,
          heapLen: 0,
        });
        return response.status !== 0 ? -response.status : 0;
      },

      // WASI `fd_fdstat_get`.
      //
      // Signature: (fd: i32, buf_ptr: i32) -> errno.
      //
      // Dispatches FD_FDSTAT_GET with a 24-byte heap-out window,
      // then copies the 24 bytes of fdstat_t from the heap scratch
      // into user memory at `buf_ptr`. The bytes come out
      // already-laid-out by the kernel handler: filetype (byte 0),
      // _pad (1), fs_flags (2..4), _pad (4..8), fs_rights_base
      // (8..16), fs_rights_inheriting (16..24).
      fd_fdstat_get: (fd: number, bufPtr: number): number => {
        if (this.memory === undefined) return ERRNO.EINVAL;
        const { response, heapOut } = this.backend.dispatch({
          opcode: OP_WASI.FD_FDSTAT_GET,
          requestId: 0,
          arg0: fd,
          heapPtr: 0,
          heapLen: 24,
        });
        if (response.status !== 0) return -response.status;
        const writeBuf = new Uint8Array(this.memory.buffer);
        writeBuf.set(heapOut.subarray(0, 24), bufPtr);
        return 0;
      },

      // WASI `fd_prestat_get`.
      //
      // Signature: (fd: i32, buf_ptr: i32) -> errno.
      //
      // The kernel always returns -EBADF (no preopens in v1). The
      // shim doesn't touch user memory — `buf_ptr` would only be
      // written on success. WASI's preopen-discovery loop sees
      // EBADF at the first probe and terminates.
      fd_prestat_get: (fd: number, _bufPtr: number): number => {
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.FD_PRESTAT_GET,
          requestId: 0,
          arg0: fd,
          heapPtr: 0,
          heapLen: 0,
        });
        return -response.status;
      },
    } as const;

    // PMos extension namespace — syscalls WASI doesn't cover.
    // A future slice will replace these inline imports with a
    // `pmos-rt` Rust crate every userland binary links against,
    // but for the first few test binaries we declare the
    // imports directly.
    const pmosExtShim = {
      // `proc_spawn(path_ptr, path_len, caps: u64) -> i32`
      //
      // Reads the binary path from user memory, packs a
      // `PROC_SPAWN` manifest (path + caps), dispatches the
      // opcode through the backend. Returns:
      //
      //   * Positive pid on success (new child process).
      //   * Negative errno on failure — already negated on the
      //     Rust side (`Response.status` carries `-errno`), so
      //     we just pass it through.
      //
      // The child doesn't actually *run* here: the kernel's
      // PROC_SPAWN handler invokes the host's `onSpawnProcess`
      // callback, which typically posts `proc:spawn` to the
      // main-thread spawn router. The router spins up a user
      // Worker that instantiates the child wasm against a fresh
      // per-pid SAB; the parent gets a pid back immediately and
      // keeps running. A parent that calls `proc_spawn` mid-run
      // is unblocked the moment the callback returns — the
      // child runs concurrently in its own Worker.
      proc_spawn: (
        pathPtr: number,
        pathLen: number,
        caps: bigint,
      ): number => {
        if (this.memory === undefined) {
          return -ERRNO.EINVAL;
        }
        const pathBytes = new Uint8Array(
          this.memory.buffer,
          pathPtr,
          pathLen,
        );
        const path = new TextDecoder().decode(pathBytes);
        const manifest = encodeSpawnManifest({ path, caps });

        const { response } = this.backend.dispatch(
          {
            opcode: OP_EXT.PROC_SPAWN,
            requestId: 0,
            args: manifest.args,
            heapPtr: 0,
            heapLen: manifest.heap.length,
          },
          manifest.heap,
        );

        if (response.status !== 0) {
          // Rust side already negated the errno, so `status`
          // is already the negative form our ABI promises.
          return response.status;
        }
        return Number(response.value);
      },

      // `ipc_socket(ty: i32) -> i32`
      //
      // Create an unbound socket. Returns the new fd (positive)
      // or negative errno. `ty` is 0 = Stream, 1 = Dgram; any
      // other value returns -EINVAL.
      ipc_socket: (ty: number): number => {
        const { response } = this.backend.dispatch({
          opcode: OP_EXT.IPC_SOCKET,
          requestId: 0,
          arg0: ty,
          heapPtr: 0,
          heapLen: 0,
        });
        if (response.status !== 0) return response.status;
        return Number(response.value);
      },

      // `ipc_bind(fd: i32, path_ptr: i32, path_len: i32) -> i32`
      //
      // Bind an unbound socket fd to the kernel-visible path
      // the caller has written into user memory at (path_ptr,
      // path_len). Returns 0 on success, negative errno on
      // failure (EADDRINUSE, EBADF, EINVAL for bad fd/path/state).
      ipc_bind: (
        fd: number,
        pathPtr: number,
        pathLen: number,
      ): number => {
        if (this.memory === undefined) return -ERRNO.EINVAL;
        const pathBytes = new Uint8Array(
          this.memory.buffer,
          pathPtr,
          pathLen,
        );
        // Copy out of user memory immediately — the Rust side
        // will read the path from its own heap scratch, not
        // from user wasm memory.
        const pathCopy = new Uint8Array(pathBytes);
        const { response } = this.backend.dispatch(
          {
            opcode: OP_EXT.IPC_BIND,
            requestId: 0,
            arg0: fd,
            heapPtr: 0,
            heapLen: pathLen,
          },
          pathCopy,
        );
        return response.status;
      },

      // `ipc_listen(fd: i32, backlog: i32) -> i32`
      //
      // Transition a bound socket to listening. Returns 0 on
      // success, negative errno on failure (EINVAL for bad
      // state, EBADF for bad fd).
      ipc_listen: (fd: number, backlog: number): number => {
        // Pack fd + backlog as two u32s into the 16-byte args
        // window. `args` rather than `arg0` because there are
        // two scalar arguments.
        const args = new Uint8Array(16);
        const view = new DataView(args.buffer);
        view.setUint32(0, fd, true);
        view.setUint32(4, backlog, true);
        const { response } = this.backend.dispatch({
          opcode: OP_EXT.IPC_LISTEN,
          requestId: 0,
          args,
          heapPtr: 0,
          heapLen: 0,
        });
        return response.status;
      },

      // `ipc_connect(fd: i32, path_ptr: i32, path_len: i32) -> i32`
      //
      // Connect an unbound socket to the listener at `path`.
      // Returns 0 on success, negative errno on failure
      // (ECONNREFUSED for unbound path, EBADF for bad fd,
      // EINVAL for bad state).
      ipc_connect: (
        fd: number,
        pathPtr: number,
        pathLen: number,
      ): number => {
        if (this.memory === undefined) return -ERRNO.EINVAL;
        const pathBytes = new Uint8Array(
          this.memory.buffer,
          pathPtr,
          pathLen,
        );
        const pathCopy = new Uint8Array(pathBytes);
        const { response } = this.backend.dispatch(
          {
            opcode: OP_EXT.IPC_CONNECT,
            requestId: 0,
            arg0: fd,
            heapPtr: 0,
            heapLen: pathLen,
          },
          pathCopy,
        );
        return response.status;
      },

      // `ipc_accept(listener_fd: i32) -> i32`
      //
      // Accept one pending connection from the listener.
      // Returns the new server-side fd (positive) or negative
      // errno (EAGAIN if no client pending, EBADF for bad fd,
      // EINVAL if the fd isn't a listening socket).
      ipc_accept: (listenerFd: number): number => {
        const { response } = this.backend.dispatch({
          opcode: OP_EXT.IPC_ACCEPT,
          requestId: 0,
          arg0: listenerFd,
          heapPtr: 0,
          heapLen: 0,
        });
        if (response.status !== 0) return response.status;
        return Number(response.value);
      },

      // `display_bind() -> i32`
      //
      // Bind the kernel-wide `/run/display` listening socket
      // with the kernel's `Cap::DisplayServer` check. Returns
      // the listener fd (positive) or negative errno —
      // typically `-ENOTCAPABLE` if the caller doesn't hold
      // `DisplayServer`, or `-EADDRINUSE` if another server
      // is already bound.
      display_bind: (): number => {
        const { response } = this.backend.dispatch({
          opcode: OP_EXT.DISPLAY_BIND,
          requestId: 0,
          heapPtr: 0,
          heapLen: 0,
        });
        if (response.status !== 0) return response.status;
        return Number(response.value);
      },

      // `display_connect() -> i32`
      //
      // Connect to the `/run/display` listener with the
      // kernel's `Cap::DisplayClient` check. Returns the
      // connected client-side fd (positive) or negative errno
      // — typically `-ENOTCAPABLE` if the caller lacks
      // `DisplayClient`, or `-ECONNREFUSED` if no display
      // server is bound.
      display_connect: (): number => {
        const { response } = this.backend.dispatch({
          opcode: OP_EXT.DISPLAY_CONNECT,
          requestId: 0,
          heapPtr: 0,
          heapLen: 0,
        });
        if (response.status !== 0) return response.status;
        return Number(response.value);
      },
    } as const;

    return {
      wasi_snapshot_preview1: shim,
      pmos_ext: pmosExtShim,
    };
  }
}
