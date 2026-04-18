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
  POLL_EVENT_SIZE,
  POLL_SUBSCRIPTION_SIZE,
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

      // WASI `fd_seek`.
      //
      // Signature (lowered):
      //   (fd: i32, offset: i64, whence: i32, new_offset_ptr: i32) -> errno: i32
      //
      // `whence` selects the reference point: SET=0 (absolute), CUR=1
      // (relative to current — `fd_seek(fd, 0, CUR, *)` is the
      // `fd_tell` idiom), END=2 (relative to file size). `offset` is
      // signed: a SeekCur with a negative offset rewinds; SeekSet with
      // a negative offset is rejected (-EINVAL); SeekEnd with a
      // negative offset is the typical "seek N bytes from end" case.
      //
      // Dispatches FD_SEEK packing `(fd, whence, offset)` into the
      // inline args window — fd at args[0..4] (u32), whence at
      // args[4..8] (u32; only the low byte is meaningful), offset at
      // args[8..16] (i64 bit pattern as u64). The kernel returns the
      // new absolute offset in `response.value`; the shim writes it
      // as a u64 LE at `new_offset_ptr`. Same shape as
      // `clock_time_get`: one u64 result, no heap round-trip.
      fd_seek: (
        fd: number,
        offset: bigint,
        whence: number,
        newOffsetPtr: number,
      ): number => {
        if (this.memory === undefined) return ERRNO.EINVAL;
        const args = new Uint8Array(16);
        const argsView = new DataView(args.buffer);
        argsView.setUint32(0, fd, true);
        argsView.setUint32(4, whence, true);
        argsView.setBigInt64(8, offset, true);
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.FD_SEEK,
          requestId: 0,
          args,
          heapPtr: 0,
          heapLen: 0,
        });
        if (response.status !== 0) return -response.status;
        // setBigInt64 with the kernel's i64 bit pattern writes 8 bytes
        // identical to setBigUint64 with the u64 absolute offset — the
        // wire is bit-exact through the i64↔u64 reinterpretation.
        const view = new DataView(this.memory.buffer);
        view.setBigInt64(newOffsetPtr, response.value, true);
        return 0;
      },

      // WASI `fd_tell`.
      //
      // Signature (lowered):
      //   (fd: i32, offset_ptr: i32) -> errno: i32
      //
      // The read-only sibling of `fd_seek`: report the fd's current
      // absolute position without mutating it. Functionally a
      // `fd_seek(fd, 0, Cur, *)` at the WASI-surface level — which
      // is exactly how libc's `ftell()` lowers when `fd_tell` is
      // absent — but fd_tell is its own opcode (0x0033) because it's
      // strictly cheaper on the kernel side (no whence to decode, no
      // signed arithmetic, no file-size lookup).
      //
      // Dispatches FD_TELL packing `fd` as the u32 at args[0..4] via
      // the `arg0` path. On success writes the i64 offset as u64 LE
      // at `offset_ptr`; the wire is bit-exact through the i64↔u64
      // reinterpretation, same trick `fd_seek` + `clock_time_get`
      // use for their u64 results.
      fd_tell: (fd: number, offsetPtr: number): number => {
        if (this.memory === undefined) return ERRNO.EINVAL;
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.FD_TELL,
          requestId: 0,
          arg0: fd,
          heapPtr: 0,
          heapLen: 0,
        });
        if (response.status !== 0) return -response.status;
        const view = new DataView(this.memory.buffer);
        view.setBigInt64(offsetPtr, response.value, true);
        return 0;
      },

      // WASI fd-state opcodes (`fd_advise` / `fd_allocate` / `fd_sync`
      // / `fd_datasync`). All four take only an fd (at arg0), all four
      // share the same EBADF + non-Vnode-EINVAL guards, and all four
      // collapse to trivial semantics in v1's tmpfs-backed VFS:
      //
      //   fd_advise / fd_sync / fd_datasync  →  no-op success on Vnode
      //   fd_allocate                        →  ENOTSUP on Vnode
      //
      // The shim ignores every WASI argument the kernel doesn't
      // decode: fd_advise's (offset, len, advice) and fd_allocate's
      // (offset, len) all go unused in v1. The kernel's no-op /
      // ENOTSUP response is independent of those values; passing
      // them on the wire would cost bytes without changing behaviour.

      // WASI `fd_advise`.
      //
      // Signature (lowered):
      //   (fd: i32, offset: i64, len: i64, advice: i32) -> errno: i32
      //
      // The advice byte (NORMAL / SEQUENTIAL / RANDOM / WILLNEED /
      // DONTNEED / NOREUSE) is a hint WASI permits the implementation
      // to ignore. v1 has no page cache to advise, so the kernel
      // returns success without looking at any of the arguments.
      fd_advise: (
        _fd: number,
        _offset: bigint,
        _len: bigint,
        _advice: number,
      ): number => {
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.FD_ADVISE,
          requestId: 0,
          arg0: _fd,
          heapPtr: 0,
          heapLen: 0,
        });
        return response.status !== 0 ? -response.status : 0;
      },

      // WASI `fd_allocate`.
      //
      // Signature (lowered):
      //   (fd: i32, offset: i64, len: i64) -> errno: i32
      //
      // Requests that the filesystem reserve space on disk. v1 tmpfs
      // has no preallocation primitive, so the kernel returns
      // ENOTSUP; a success response would lie about reserved space.
      fd_allocate: (
        _fd: number,
        _offset: bigint,
        _len: bigint,
      ): number => {
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.FD_ALLOCATE,
          requestId: 0,
          arg0: _fd,
          heapPtr: 0,
          heapLen: 0,
        });
        return response.status !== 0 ? -response.status : 0;
      },

      // WASI `fd_sync`.
      //
      // Signature (lowered): (fd: i32) -> errno: i32
      //
      // Flushes all modified data + metadata to durable storage.
      // v1 writes are synchronous into the vfs state (tmpfs +
      // devfs + procfs are in-memory; opfs is backed by the OPFS
      // block driver which flushes on every write), so there's
      // nothing to flush — no-op success.
      fd_sync: (fd: number): number => {
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.FD_SYNC,
          requestId: 0,
          arg0: fd,
          heapPtr: 0,
          heapLen: 0,
        });
        return response.status !== 0 ? -response.status : 0;
      },

      // WASI `fd_datasync`.
      //
      // Signature (lowered): (fd: i32) -> errno: i32
      //
      // Same as fd_sync but only requires the data to reach
      // storage, not the metadata. Same no-op-success semantics
      // in v1 for the same reason.
      fd_datasync: (fd: number): number => {
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.FD_DATASYNC,
          requestId: 0,
          arg0: fd,
          heapPtr: 0,
          heapLen: 0,
        });
        return response.status !== 0 ? -response.status : 0;
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
      // First dispatches the PMos `PROC_EXIT` opcode so the kernel
      // can record the exit status on the process table, release
      // the fd-table-owned resources (IPC bindings, pipe refs),
      // and remove the pid from the scheduler. Without this
      // round-trip the kernel would keep treating the exited pid
      // as Running — its `/run/*` bindings would live forever and
      // a follow-up `ipc_connect` on the same path would succeed
      // against an orphan listener instead of cleanly returning
      // `ConnectionRefused`. The kernel's response is ignored; a
      // process that is exiting no longer has a way to observe it,
      // and `PROC_EXIT` is contractually infallible from userland.
      //
      // Then throws the `UserProcessExited` sentinel that the
      // runtime's `run()` method catches. The wasm instance's
      // `_start` frame is torn down as the throw unwinds through
      // it — exactly the semantics WASI specifies.
      proc_exit: (rval: number): never => {
        this.backend.dispatch({
          opcode: OP_WASI.PROC_EXIT,
          requestId: 0,
          arg0: rval >>> 0,
        });
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

      // WASI `fd_filestat_get`.
      //
      // Signature: (fd: i32, buf_ptr: i32) -> errno.
      //
      // Dispatches FD_FILESTAT_GET with a 64-byte heap-out window,
      // then copies the 64 bytes of filestat_t from the heap scratch
      // into user memory at `buf_ptr`. The bytes come out already-
      // laid-out by the kernel handler: dev (0..8), ino (8..16),
      // filetype (16) + 7 bytes alignment padding (17..24), nlink
      // (24..32), size (32..40), atim (40..48), mtim (48..56), ctim
      // (56..64) — all little-endian u64s except filetype's single
      // byte. Unblocks `std::fs::File::metadata` + `std::fs::metadata`
      // for std binaries.
      fd_filestat_get: (fd: number, bufPtr: number): number => {
        if (this.memory === undefined) return ERRNO.EINVAL;
        const { response, heapOut } = this.backend.dispatch({
          opcode: OP_WASI.FD_FILESTAT_GET,
          requestId: 0,
          arg0: fd,
          heapPtr: 0,
          heapLen: 64,
        });
        if (response.status !== 0) return -response.status;
        const writeBuf = new Uint8Array(this.memory.buffer);
        writeBuf.set(heapOut.subarray(0, 64), bufPtr);
        return 0;
      },

      // WASI `path_filestat_get`.
      //
      // Signature (lowered):
      //   path_filestat_get(
      //     dirfd:     i32, // directory fd — ignored (v1 has no
      //                     //   preopens; every path is absolute)
      //     flags:     i32, // lookup flags — v1 accepts any value
      //                     //   (LOOKUP_SYMLINK_FOLLOW=0x1 is a
      //                     //   no-op since the VFS doesn't follow
      //                     //   symlinks either way)
      //     path_ptr:  i32,
      //     path_len:  i32,
      //     buf_ptr:   i32, // out-pointer for the 64-byte filestat_t
      //   ) -> errno: i32
      //
      // Reads the path bytes from user memory and stages them into
      // the kernel's heap-scratch region (input). The kernel reads
      // the path, resolves it, and writes a 64-byte filestat_t back
      // into the same heap region (input + output share the heap
      // ptr since `Request` only carries one heap window). The shim
      // then copies `heapOut` to user memory at `buf_ptr`. Mirrors
      // the path-input shape of `path_open` + the output-copy shape
      // of `fd_filestat_get`.
      //
      // Unblocks `std::fs::metadata(path)` for std binaries; the
      // fd-based sibling (`fd_filestat_get`) already covers
      // `File::metadata()`, but a lot of std code goes through the
      // path variant directly.
      path_filestat_get: (
        _dirfd: number,
        _flags: number,
        pathPtr: number,
        pathLen: number,
        bufPtr: number,
      ): number => {
        if (this.memory === undefined) return ERRNO.EINVAL;
        const pathBytes = new Uint8Array(
          this.memory.buffer,
          pathPtr,
          pathLen,
        );
        const pathCopy = new Uint8Array(pathBytes);

        const { response, heapOut } = this.backend.dispatch(
          {
            opcode: OP_WASI.PATH_FILESTAT_GET,
            requestId: 0,
            arg0: 0, // dir_fd — ignored
            heapPtr: 0,
            heapLen: pathLen,
          },
          pathCopy,
        );
        if (response.status !== 0) return -response.status;
        // Re-fetch memory view — dispatch may have grown it.
        const writeBuf = new Uint8Array(this.memory.buffer);
        writeBuf.set(heapOut.subarray(0, 64), bufPtr);
        return 0;
      },

      // WASI `path_filestat_set_times`.
      //
      // Signature (lowered):
      //   (dirfd: i32, flags: i32, path_ptr: i32, path_len: i32,
      //    atim: i64, mtim: i64, fstflags: i32) -> errno: i32
      //
      // Write-side sibling of `path_filestat_get`: sets a vnode's
      // atim / mtim per the `fstflags` bitfield (SET_ATIM=0x1,
      // SET_ATIM_NOW=0x2, SET_MTIM=0x4, SET_MTIM_NOW=0x8). Zero
      // fstflags is a legal no-op success — callers use it as a
      // permission / existence probe. Invalid pairs (SET_ATIM +
      // SET_ATIM_NOW, SET_MTIM + SET_MTIM_NOW) return EINVAL.
      //
      // Wire layout: dir_fd + lookup_flags + fstflags go in the
      // inline args window (u32 each at offsets 0 / 4 / 8); atim +
      // mtim + path share the heap (two u64 LE at [0..16] then the
      // UTF-8 path bytes at [16..]). heap_len = 16 + path_len. The
      // shim packs a single Uint8Array combining those three into
      // the heap buffer — cheaper than a second dispatch round-trip.
      path_filestat_set_times: (
        _dirfd: number,
        _flags: number,
        pathPtr: number,
        pathLen: number,
        atim: bigint,
        mtim: bigint,
        fstflags: number,
      ): number => {
        if (this.memory === undefined) return ERRNO.EINVAL;
        const pathBytes = new Uint8Array(
          this.memory.buffer,
          pathPtr,
          pathLen,
        );
        const heap = new Uint8Array(16 + pathLen);
        const heapView = new DataView(heap.buffer);
        heapView.setBigUint64(0, atim, true);
        heapView.setBigUint64(8, mtim, true);
        heap.set(pathBytes, 16);

        const args = new Uint8Array(16);
        const argsView = new DataView(args.buffer);
        argsView.setUint32(0, 0, true); // dir_fd — ignored
        argsView.setUint32(4, 0, true); // lookup_flags — ignored
        argsView.setUint32(8, fstflags, true);

        const { response } = this.backend.dispatch(
          {
            opcode: OP_WASI.PATH_FILESTAT_SET_TIMES,
            requestId: 0,
            args,
            heapPtr: 0,
            heapLen: heap.length,
          },
          heap,
        );
        return response.status !== 0 ? -response.status : 0;
      },

      // WASI `fd_readdir`.
      //
      // Signature (lowered):
      //   (fd: i32, buf_ptr: i32, buf_len: i32, cookie: i64,
      //    bufused_ptr: i32) -> errno: i32
      //
      // Directory listing. Wire layout at the kernel: fd at
      // args[0..4], cookie (u64 LE) at args[4..12], heap is the
      // output buffer. Kernel writes 24-byte dirent_t headers +
      // inline name bytes. Shim copies the kernel's heapOut back
      // into user memory at buf_ptr and writes the byte-count to
      // bufused_ptr as u32. A buffer that fills mid-entry receives
      // a truncated entry; the caller spots it via
      // bytes_written == buf_len and re-issues with the last d_next
      // cookie they decoded successfully.
      fd_readdir: (
        fd: number,
        bufPtr: number,
        bufLen: number,
        cookie: bigint,
        bufusedPtr: number,
      ): number => {
        if (this.memory === undefined) return ERRNO.EINVAL;
        const args = new Uint8Array(16);
        const argsView = new DataView(args.buffer);
        argsView.setUint32(0, fd, true);
        argsView.setBigUint64(4, cookie, true);
        // Kernel writes directly into its own heap scratch; the shim
        // sizes the heap buffer at bufLen (or 0 if bufLen is 0) so
        // the dispatch layer's "heap_len = caller capacity" contract
        // matches.
        const heap = new Uint8Array(bufLen);
        const { response, heapOut } = this.backend.dispatch(
          {
            opcode: OP_WASI.FD_READDIR,
            requestId: 0,
            args,
            heapPtr: 0,
            heapLen: heap.length,
          },
          heap,
        );
        if (response.status !== 0) return -response.status;
        const written = Number(response.value);
        // Copy written bytes out to user memory at bufPtr. Re-fetch
        // the memory view in case the dispatch path grew it.
        const memBytes = new Uint8Array(this.memory.buffer);
        memBytes.set(heapOut.subarray(0, written), bufPtr);
        const memView = new DataView(this.memory.buffer);
        memView.setUint32(bufusedPtr, written, true);
        return 0;
      },

      // WASI `path_unlink_file`.
      //
      // Signature (lowered):
      //   (dirfd: i32, path_ptr: i32, path_len: i32) -> errno: i32
      //
      // Strictly for regular files — unlinking a directory returns
      // EISDIR (WASI callers should use path_remove_directory).
      // Threads through Vfs::unlink on the owning mount.
      path_unlink_file: (
        _dirfd: number,
        pathPtr: number,
        pathLen: number,
      ): number => {
        if (this.memory === undefined) return ERRNO.EINVAL;
        const pathBytes = new Uint8Array(this.memory.buffer, pathPtr, pathLen);
        const heap = new Uint8Array(pathLen);
        heap.set(pathBytes, 0);
        const { response } = this.backend.dispatch(
          {
            opcode: OP_WASI.PATH_UNLINK_FILE,
            requestId: 0,
            arg0: 0, // dir_fd ignored in v1
            heapPtr: 0,
            heapLen: heap.length,
          },
          heap,
        );
        return response.status !== 0 ? -response.status : 0;
      },

      // WASI `path_rename`.
      //
      // Signature (lowered):
      //   (old_fd: i32, old_path_ptr: i32, old_path_len: i32,
      //    new_fd: i32, new_path_ptr: i32, new_path_len: i32) -> errno: i32
      //
      // The only WASI opcode that ferries two heap strings. Wire:
      // old_len at args[8..12] marks the split point in a single
      // heap buffer containing `(old_path, new_path)` concatenated.
      // The kernel reads old_len from the inline args and splits
      // the heap at that offset; no null-separator scan needed.
      path_rename: (
        _oldFd: number,
        oldPathPtr: number,
        oldPathLen: number,
        _newFd: number,
        newPathPtr: number,
        newPathLen: number,
      ): number => {
        if (this.memory === undefined) return ERRNO.EINVAL;
        const oldBytes = new Uint8Array(
          this.memory.buffer,
          oldPathPtr,
          oldPathLen,
        );
        const newBytes = new Uint8Array(
          this.memory.buffer,
          newPathPtr,
          newPathLen,
        );
        const heap = new Uint8Array(oldPathLen + newPathLen);
        heap.set(oldBytes, 0);
        heap.set(newBytes, oldPathLen);

        const args = new Uint8Array(16);
        const argsView = new DataView(args.buffer);
        argsView.setUint32(0, 0, true); // from_dir_fd — ignored
        argsView.setUint32(4, 0, true); // to_dir_fd — ignored
        argsView.setUint32(8, oldPathLen, true);

        const { response } = this.backend.dispatch(
          {
            opcode: OP_WASI.PATH_RENAME,
            requestId: 0,
            args,
            heapPtr: 0,
            heapLen: heap.length,
          },
          heap,
        );
        return response.status !== 0 ? -response.status : 0;
      },

      // WASI `path_create_directory`.
      //
      // Signature (lowered):
      //   (dirfd: i32, path_ptr: i32, path_len: i32) -> errno: i32
      //
      // mkdir opcode. Wire layout matches path_unlink_file. The
      // kernel hard-codes mode 0o755 — WASI's mkdir signature has
      // no mode argument. Threads through Vfs::mkdir on the owning
      // mount; devfs + procfs return EROFS.
      path_create_directory: (
        _dirfd: number,
        pathPtr: number,
        pathLen: number,
      ): number => {
        if (this.memory === undefined) return ERRNO.EINVAL;
        const pathBytes = new Uint8Array(this.memory.buffer, pathPtr, pathLen);
        const heap = new Uint8Array(pathLen);
        heap.set(pathBytes, 0);
        const { response } = this.backend.dispatch(
          {
            opcode: OP_WASI.PATH_CREATE_DIRECTORY,
            requestId: 0,
            arg0: 0, // dir_fd ignored in v1
            heapPtr: 0,
            heapLen: heap.length,
          },
          heap,
        );
        return response.status !== 0 ? -response.status : 0;
      },

      // WASI `path_remove_directory`.
      //
      // Signature (lowered):
      //   (dirfd: i32, path_ptr: i32, path_len: i32) -> errno: i32
      //
      // rmdir opcode. Wire layout matches path_unlink_file. Strictly
      // for directories — rmdir on a regular file returns ENOTDIR
      // (tmpfs.rmdir returns NotADirectory for non-dir targets), and
      // rmdir on a non-empty directory returns ENOTEMPTY. Callers
      // must unlink file children first, then rmdir the container.
      path_remove_directory: (
        _dirfd: number,
        pathPtr: number,
        pathLen: number,
      ): number => {
        if (this.memory === undefined) return ERRNO.EINVAL;
        const pathBytes = new Uint8Array(this.memory.buffer, pathPtr, pathLen);
        const heap = new Uint8Array(pathLen);
        heap.set(pathBytes, 0);
        const { response } = this.backend.dispatch(
          {
            opcode: OP_WASI.PATH_REMOVE_DIRECTORY,
            requestId: 0,
            arg0: 0, // dir_fd ignored in v1
            heapPtr: 0,
            heapLen: heap.length,
          },
          heap,
        );
        return response.status !== 0 ? -response.status : 0;
      },

      // WASI `fd_renumber`.
      //
      // Signature (lowered):
      //   (from: i32, to: i32) -> errno: i32
      //
      // WASI's dup2-spelling: atomically move the FdEntry at `from`
      // to `to`, closing whatever was at `to` first. If `from == to`
      // on an open fd, it's a no-op success; if on a closed fd,
      // EBADF. If `from` is not open, EBADF with `to` untouched.
      // The kernel's `fd_renumber` releases any prior `to` object
      // (pipe / socket ref) via `release_object` — same path
      // `fd_close` uses.
      //
      // Dispatches FD_RENUMBER packing `(from, to)` as two u32s in
      // the inline args window. No heap round-trip.
      fd_renumber: (from: number, to: number): number => {
        const args = new Uint8Array(16);
        const argsView = new DataView(args.buffer);
        argsView.setUint32(0, from, true);
        argsView.setUint32(4, to, true);
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.FD_RENUMBER,
          requestId: 0,
          args,
          heapPtr: 0,
          heapLen: 0,
        });
        return response.status !== 0 ? -response.status : 0;
      },

      // WASI `fd_filestat_set_times`.
      //
      // Signature (lowered):
      //   (fd: i32, atim: i64, mtim: i64, fstflags: i32) -> errno: i32
      //
      // Fd-based sibling of `path_filestat_set_times`: same fstflags
      // + Options shape; the only difference is that the target vnode
      // is reached via an fd instead of a path. Wire layout: fd at
      // args[0..4], fstflags at args[4..8]; atim + mtim share the heap
      // (two u64 LE at [0..16], heap_len = 16). Guards mirror the path
      // variant (exclusive pair validation → EINVAL, short heap →
      // EINVAL) plus the fd guards (EBADF on unopened fd; EINVAL on
      // non-Vnode FdObject). Zero fstflags is a legal no-op success
      // for a valid Vnode fd — the caller holds an open fd so the
      // path-resolve + rights checks already ran at path_open time.
      fd_filestat_set_times: (
        fd: number,
        atim: bigint,
        mtim: bigint,
        fstflags: number,
      ): number => {
        const heap = new Uint8Array(16);
        const heapView = new DataView(heap.buffer);
        heapView.setBigUint64(0, atim, true);
        heapView.setBigUint64(8, mtim, true);

        const args = new Uint8Array(16);
        const argsView = new DataView(args.buffer);
        argsView.setUint32(0, fd, true);
        argsView.setUint32(4, fstflags, true);

        const { response } = this.backend.dispatch(
          {
            opcode: OP_WASI.FD_FILESTAT_SET_TIMES,
            requestId: 0,
            args,
            heapPtr: 0,
            heapLen: heap.length,
          },
          heap,
        );
        return response.status !== 0 ? -response.status : 0;
      },

      // WASI `poll_oneoff`.
      //
      // Signature (lowered):
      //   (in: i32, out: i32, nsubscriptions: i32, nevents: i32) -> errno: i32
      //
      // Multi-subscription readiness check. `in` points at an array
      // of `nsubscriptions` subscription_t records (48 bytes each)
      // and `out` points at an array of up to `nsubscriptions`
      // event_t records (32 bytes each); the kernel fills in
      // events for every subscription that is ready right now and
      // writes the actual event count through `nevents`.
      //
      // v1 kernel is non-blocking: CLOCK fires only if the target
      // time is already past; FD_READ / FD_WRITE fire only if the
      // op would make progress without waiting. A caller that
      // expects to block walks a spin loop at the WASI-call layer.
      //
      // Wire layout: args pack (n_subs, n_events_cap) as two u32s
      // at offsets 0 and 4. The heap is a single buffer with subs
      // at [0..n_subs*48] and events at [n_subs*48..n_subs*48 +
      // n_events_cap*32]; n_events_cap is always == n_subs here
      // because WASI's signature sizes the output buffer that way.
      // The shim reads subs out of user memory into the heap buffer
      // before dispatch, then copies emitted events back into user
      // memory after dispatch.
      poll_oneoff: (
        inPtr: number,
        outPtr: number,
        nsubscriptions: number,
        neventsPtr: number,
      ): number => {
        if (this.memory === undefined) return ERRNO.EINVAL;
        if (nsubscriptions === 0) return ERRNO.EINVAL;
        const subsBytes = nsubscriptions * POLL_SUBSCRIPTION_SIZE;
        // Size the heap for the bigger of (subs, events). The kernel
        // reads subs out of the heap, then overwrites the same region
        // with event records in place — see `handle_poll_oneoff` in
        // `crates/kernel/src/syscall/wasi.rs`. Subs is always bigger
        // here (48 > 32 and n_events_cap == n_subs), so sizing to
        // subsBytes is sufficient.
        const heap = new Uint8Array(subsBytes);
        // Copy subscriptions out of the wasm linear memory into the
        // heap buffer the backend will ferry to the kernel.
        heap.set(
          new Uint8Array(this.memory.buffer, inPtr, subsBytes),
          0,
        );

        const args = new Uint8Array(16);
        const argsView = new DataView(args.buffer);
        argsView.setUint32(0, nsubscriptions, true);
        argsView.setUint32(4, nsubscriptions, true);

        const { response, heapOut } = this.backend.dispatch(
          {
            opcode: OP_WASI.POLL_ONEOFF,
            requestId: 0,
            args,
            heapPtr: 0,
            heapLen: heap.length,
          },
          heap,
        );
        if (response.status !== 0) return -response.status;
        const nEvents = Number(response.value);
        // Copy the emitted events out of `heapOut` (which the backend
        // produces by reading `extra_len` bytes from the kernel's
        // scratch region) into user memory at `outPtr`.
        const memBytes = new Uint8Array(this.memory.buffer);
        memBytes.set(heapOut.subarray(0, nEvents * POLL_EVENT_SIZE), outPtr);
        const memView = new DataView(this.memory.buffer);
        memView.setUint32(neventsPtr, nEvents, true);
        return 0;
      },

      // WASI `clock_time_get`.
      //
      // Signature (lowered):
      //   (clock_id: i32, precision: i64, timestamp_ptr: i32) -> errno: i32
      //
      // `clock_id` selects the clock source (0 = REALTIME,
      // 1 = MONOTONIC, 2/3 = cputime — ENOTSUP in v1). `precision`
      // is the caller's advisory precision hint in ns; the PMos
      // handler ignores it (the Platform clock is nanosecond-
      // resolution already). `timestamp_ptr` is where to write
      // the resulting i64 nanoseconds value in user memory.
      //
      // Dispatches a `CLOCK_TIME_GET` opcode packing `clock_id`
      // as the u32 at args[0..4]. On success, writes the i64
      // value (as little-endian bytes) to `timestamp_ptr` and
      // returns 0. On failure, returns the positive errno (WASI
      // convention); the Rust-side `Response.status` is already
      // the negated errno, so the shim negates once more.
      clock_time_get: (
        clockId: number,
        _precision: bigint,
        timestampPtr: number,
      ): number => {
        if (this.memory === undefined) return ERRNO.EINVAL;
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.CLOCK_TIME_GET,
          requestId: 0,
          arg0: clockId,
          heapPtr: 0,
          heapLen: 0,
        });
        if (response.status !== 0) return -response.status;
        // The kernel returns the i64 nanoseconds value in
        // `response.value`. Write the bit pattern out verbatim —
        // a u64 ns value that exceeds `Number.MAX_SAFE_INTEGER`
        // (2^53) still survives as bigint → little-endian bytes
        // without the precision loss that `setUint32` + Number
        // arithmetic would introduce.
        const view = new DataView(this.memory.buffer);
        view.setBigInt64(timestampPtr, response.value, true);
        return 0;
      },

      // WASI `clock_res_get`.
      //
      // Signature (lowered):
      //   (clock_id: i32, resolution_ptr: i32) -> errno: i32
      //
      // The precision-query sibling to `clock_time_get`. Given a
      // clock id, report the finest tick the clock can resolve.
      // PMos's `Platform::now_*` are nanosecond-granular, so the
      // kernel handler returns 1 ns for both MONOTONIC + REALTIME
      // and -ENOTSUP for the two cputime clocks — same split the
      // time handler uses.
      //
      // Dispatches `CLOCK_RES_GET` packing `clock_id` as the u32
      // at args[0..4]. On success, writes the i64 resolution value
      // to `resolution_ptr` (little-endian) and returns 0. On
      // failure, returns the positive errno.
      clock_res_get: (clockId: number, resolutionPtr: number): number => {
        if (this.memory === undefined) return ERRNO.EINVAL;
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.CLOCK_RES_GET,
          requestId: 0,
          arg0: clockId,
          heapPtr: 0,
          heapLen: 0,
        });
        if (response.status !== 0) return -response.status;
        // The resolution value fits in a small integer today
        // (1 ns), but the wire format is an i64 per the WASI
        // contract + the kernel's `Response.value` i64 field —
        // keep the BigInt path for bit-exactness, same as the
        // time shim.
        const view = new DataView(this.memory.buffer);
        view.setBigInt64(resolutionPtr, response.value, true);
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
