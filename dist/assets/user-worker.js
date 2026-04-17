// src/shared/sab-layout.ts
var SAB_SIZE = 65536;
var OFF_REQ_HEAD = 0;
var OFF_REQ_TAIL = 4;
var OFF_RES_HEAD = 8;
var OFF_RES_TAIL = 12;
var OFF_USER_WAIT_SLOT = 16;
var OFF_REQ_RING = 64;
var OFF_RES_RING = 16384;
var OFF_HEAP_SCRATCH = 32768;
var HEAP_SCRATCH_BYTES = 32768;
var REQ_SLOT_COUNT = 510;
var RES_SLOT_COUNT = 510;
var STATUS_REQUESTED = 1;

// src/shared/syscall.ts
var SLOT_SIZE = 32;
function encodeRequest(req) {
  const buf = new Uint8Array(SLOT_SIZE);
  const view = new DataView(buf.buffer);
  view.setUint16(0, req.opcode, true);
  view.setUint16(2, req.flags ?? 0, true);
  view.setUint32(4, req.requestId, true);
  if (req.args !== void 0) {
    if (req.args.length !== 16) {
      throw new Error(`syscall.encodeRequest: args must be 16 bytes, got ${req.args.length}`);
    }
    if (req.arg0 !== void 0) {
      throw new Error("syscall.encodeRequest: pass either args or arg0, not both");
    }
    buf.set(req.args, 8);
  } else if (req.arg0 !== void 0) {
    view.setUint32(8, req.arg0, true);
  }
  view.setUint32(24, req.heapPtr ?? 0, true);
  view.setUint32(28, req.heapLen ?? 0, true);
  return buf;
}
function decodeResponse(bytes) {
  if (bytes.length !== SLOT_SIZE) {
    throw new Error(`syscall.decodeResponse: expected ${SLOT_SIZE} bytes, got ${bytes.length}`);
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, SLOT_SIZE);
  return {
    requestId: view.getUint32(0, true),
    status: view.getInt32(4, true),
    value: view.getBigInt64(8, true),
    extraLen: view.getUint32(16, true)
  };
}
var OP_WASI = {
  ARGS_GET: 1,
  ARGS_SIZES_GET: 2,
  ENVIRON_GET: 3,
  ENVIRON_SIZES_GET: 4,
  FD_CLOSE: 34,
  FD_FDSTAT_GET: 36,
  FD_PRESTAT_GET: 43,
  FD_READ: 46,
  FD_WRITE: 52,
  PATH_OPEN: 68,
  PROC_EXIT: 96,
  CLOCK_TIME_GET: 17,
  RANDOM_GET: 81,
  SCHED_YIELD: 82,
  /** Unused by the WASI shim today; the tests probe it to verify
   * the dispatcher's `ENOSYS` path still fires for opcodes the
   * kernel doesn't yet handle. Swap to whichever WASI opcode is
   * still unhandled as the implementation catches up. */
  FD_SEEK: 49
};
var OP_EXT = {
  IPC_SOCKET: 4096,
  IPC_BIND: 4097,
  IPC_LISTEN: 4098,
  IPC_CONNECT: 4099,
  IPC_ACCEPT: 4100,
  PROC_SPAWN: 4352,
  PROC_SELF: 4355,
  PROC_PARENT: 4356,
  PROC_WAIT: 4353,
  DISPLAY_CONNECT: 4608,
  DISPLAY_BIND: 4609,
  CAP_CHECK: 4864,
  CAP_LIST: 4865
};
var ERRNO = {
  EBADF: 8,
  EINVAL: 28,
  ENOENT: 44,
  ENOSYS: 52,
  ENOTSUP: 58
};
var CAP = {
  DISPLAY_CLIENT: 1,
  DISPLAY_SERVER: 2,
  SHELL: 3,
  PROC_ENUMERATE: 4,
  PROC_KILL_ANY: 5,
  NET: 6,
  MOUNT: 7,
  CAP_GRANT: 8,
  DEV_BLOCK: 9,
  KEYMAP_ADMIN: 10
};
function capBit(cap) {
  return 1n << BigInt(cap);
}
function encodeSpawnManifest(manifest) {
  const path = new TextEncoder().encode(manifest.path);
  const args = new Uint8Array(16);
  const view = new DataView(args.buffer);
  view.setUint32(0, path.length, true);
  view.setBigUint64(4, manifest.caps, true);
  return { args, heap: path };
}
var CAPSET_DESKTOP_SHELL = capBit(CAP.DISPLAY_CLIENT) | capBit(CAP.SHELL) | capBit(CAP.PROC_ENUMERATE) | capBit(CAP.KEYMAP_ADMIN);
var CAPSET_ORDINARY_APP = capBit(CAP.DISPLAY_CLIENT);

// src/sab-backend.ts
var SabBackend = class {
  buffer;
  baseOffset;
  /** `Int32Array` over the SAB header so `Atomics.{load,store}` work
   * on the head/tail slots without per-call view construction. */
  header;
  pid;
  serviceHook;
  kernelWakeSlot;
  /** True iff `header.buffer` is a real `SharedArrayBuffer` (the only
   * backing on which `Atomics.notify` + `Atomics.wait` are legal).
   * Captured at construction so the per-call `dispatch` doesn't have
   * to re-check on every syscall. */
  headerIsShared;
  /** True iff `kernelWakeSlot.buffer` is a real `SharedArrayBuffer`.
   * Same rationale as `headerIsShared`; the two backings can in
   * principle differ — the kernel-wake buffer is allocated by
   * `KernelWasmHost.create` and the per-pid SAB by the main-thread
   * router, so a partial fallback is possible. */
  wakeSlotIsShared;
  constructor(options) {
    if (options.sab.byteLength < SAB_SIZE) {
      throw new Error(
        `SabBackend: sab is ${options.sab.byteLength} bytes, need at least ${SAB_SIZE}`
      );
    }
    this.buffer = options.sab.buffer;
    this.baseOffset = options.sab.byteOffset;
    this.header = new Int32Array(
      options.sab.buffer,
      options.sab.byteOffset,
      OFF_HEAP_SCRATCH / 4
    );
    this.pid = options.pid;
    this.serviceHook = options.serviceHook;
    this.kernelWakeSlot = options.kernelWakeSlot;
    this.headerIsShared = typeof SharedArrayBuffer !== "undefined" && this.buffer instanceof SharedArrayBuffer;
    this.wakeSlotIsShared = this.kernelWakeSlot !== void 0 && typeof SharedArrayBuffer !== "undefined" && this.kernelWakeSlot.buffer instanceof SharedArrayBuffer;
  }
  /**
   * Translate `request` (+ optional `heapIn`) into an SAB ring
   * round-trip. Mirrors [`KernelWasmHostBackend.dispatch`]
   * byte-for-byte: same encoded request, same heap-scratch use of
   * `request.heapPtr` as the in/out offset, same decoded response.
   *
   * Throws if the request ring is full (production v1 keeps this at
   * a defensive check — single-threaded user wasm has at most one
   * in-flight syscall), if the response ring is empty after the
   * `serviceHook` runs (the hook should have serviced before
   * returning), or if a heap payload exceeds [`HEAP_SCRATCH_BYTES`].
   */
  dispatch(request, heapIn) {
    const heapPtr = request.heapPtr ?? 0;
    if (heapIn !== void 0 && heapIn.length > 0) {
      if (heapPtr > HEAP_SCRATCH_BYTES || heapPtr + heapIn.length > HEAP_SCRATCH_BYTES) {
        throw new Error(
          `SabBackend.dispatch: heap payload ${heapPtr}+${heapIn.length} > capacity ${HEAP_SCRATCH_BYTES}`
        );
      }
      new Uint8Array(
        this.buffer,
        this.baseOffset + OFF_HEAP_SCRATCH + heapPtr,
        heapIn.length
      ).set(heapIn);
    }
    if (this.serviceHook === void 0 && this.kernelWakeSlot !== void 0) {
      Atomics.store(this.header, OFF_USER_WAIT_SLOT / 4, STATUS_REQUESTED);
    }
    const reqHead = Atomics.load(this.header, OFF_REQ_HEAD / 4);
    const reqTail = Atomics.load(this.header, OFF_REQ_TAIL / 4);
    const nextReqHead = (reqHead + 1 >>> 0) % REQ_SLOT_COUNT;
    if (nextReqHead === reqTail) {
      throw new Error(
        `SabBackend.dispatch: request ring full for pid ${this.pid}`
      );
    }
    const reqSlotIx = (reqHead >>> 0) % REQ_SLOT_COUNT;
    const reqSlotOffset = this.baseOffset + OFF_REQ_RING + reqSlotIx * SLOT_SIZE;
    const reqBytes = encodeRequest(request);
    new Uint8Array(this.buffer, reqSlotOffset, SLOT_SIZE).set(reqBytes);
    Atomics.store(this.header, OFF_REQ_HEAD / 4, nextReqHead);
    if (this.serviceHook !== void 0) {
      this.serviceHook();
    } else if (this.kernelWakeSlot !== void 0) {
      Atomics.add(this.kernelWakeSlot, 0, 1);
      if (this.wakeSlotIsShared) {
        Atomics.notify(this.kernelWakeSlot, 0);
      }
      if (this.headerIsShared) {
        Atomics.wait(this.header, OFF_USER_WAIT_SLOT / 4, STATUS_REQUESTED);
      }
    }
    const resHead = Atomics.load(this.header, OFF_RES_HEAD / 4);
    const resTail = Atomics.load(this.header, OFF_RES_TAIL / 4);
    if (resHead === resTail) {
      throw new Error(
        `SabBackend.dispatch: response ring empty for pid ${this.pid} after serviceHook; production path would have parked on Atomics.wait until READY`
      );
    }
    const resSlotIx = (resTail >>> 0) % RES_SLOT_COUNT;
    const resSlotOffset = this.baseOffset + OFF_RES_RING + resSlotIx * SLOT_SIZE;
    const resBytes = new Uint8Array(
      new Uint8Array(this.buffer, resSlotOffset, SLOT_SIZE)
    );
    const response = decodeResponse(resBytes);
    const nextResTail = (resTail + 1 >>> 0) % RES_SLOT_COUNT;
    Atomics.store(this.header, OFF_RES_TAIL / 4, nextResTail);
    let heapOut = new Uint8Array(0);
    if (response.extraLen > 0) {
      const heapOffset = this.baseOffset + OFF_HEAP_SCRATCH + heapPtr;
      heapOut = new Uint8Array(
        new Uint8Array(this.buffer, heapOffset, response.extraLen)
      );
    }
    return { response, heapOut };
  }
};

// src/user-wasm-runtime.ts
var UserProcessExited = class extends Error {
  constructor(exitCode) {
    super(`user process exited with code ${exitCode}`);
    this.exitCode = exitCode;
    this.name = "UserProcessExited";
  }
};
var UserWasmRuntime = class {
  constructor(wasmBytes, backend) {
    this.wasmBytes = wasmBytes;
    this.backend = backend;
  }
  /** Populated when `run()` begins instantiation. */
  memory;
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
  async run() {
    const imports = this.buildImports();
    const { instance } = await WebAssembly.instantiate(this.wasmBytes, imports);
    const exports = instance.exports;
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
  sizes_get(opcode, countPtr, bufSizePtr) {
    if (this.memory === void 0) return ERRNO.EINVAL;
    const { response, heapOut } = this.backend.dispatch({
      opcode,
      requestId: 0,
      heapPtr: 0,
      heapLen: 8
    });
    if (response.status !== 0) return -response.status;
    const readView = new DataView(
      heapOut.buffer,
      heapOut.byteOffset,
      heapOut.byteLength
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
  buildImports() {
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
      fd_write: (fd, iovsPtr, iovsLen, nwrittenPtr) => {
        if (this.memory === void 0) {
          return ERRNO.EINVAL;
        }
        const readView = new DataView(this.memory.buffer);
        const gathered = [];
        let total = 0;
        for (let i = 0; i < iovsLen; i += 1) {
          const iovBase = iovsPtr + i * 8;
          const bufPtr = readView.getUint32(iovBase, true);
          const bufLen = readView.getUint32(iovBase + 4, true);
          const src = new Uint8Array(this.memory.buffer, bufPtr, bufLen);
          gathered.push(new Uint8Array(src));
          total += bufLen;
        }
        const payload = new Uint8Array(total);
        {
          let offset = 0;
          for (const buf of gathered) {
            payload.set(buf, offset);
            offset += buf.length;
          }
        }
        const request = {
          opcode: OP_WASI.FD_WRITE,
          // The runtime doesn't yet track per-syscall ids. A
          // future slice with concurrent in-flight requests will
          // need a monotonic counter here.
          requestId: 0,
          arg0: fd,
          heapPtr: 0,
          heapLen: total
        };
        const { response } = this.backend.dispatch(request, payload);
        if (response.status !== 0) {
          return -response.status;
        }
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
      fd_read: (fd, iovsPtr, iovsLen, nreadPtr) => {
        if (this.memory === void 0) {
          return ERRNO.EINVAL;
        }
        let view = new DataView(this.memory.buffer);
        const iovecs = [];
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
        const { response, heapOut } = this.backend.dispatch({
          opcode: OP_WASI.FD_READ,
          requestId: 0,
          arg0: fd,
          heapPtr: 0,
          heapLen: totalCapacity
        });
        if (response.status !== 0) {
          return -response.status;
        }
        const nread = Number(response.value);
        let offset = 0;
        const writeBuf = new Uint8Array(this.memory.buffer);
        for (const iov of iovecs) {
          if (offset >= nread) break;
          const chunk = Math.min(iov.len, nread - offset);
          if (chunk === 0) continue;
          writeBuf.set(
            heapOut.subarray(offset, offset + chunk),
            iov.ptr
          );
          offset += chunk;
        }
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
      path_open: (_dirfd, _dirflags, pathPtr, pathLen, _oflags, _rightsBase, _rightsInheriting, _fdflags, fdOutPtr) => {
        if (this.memory === void 0) {
          return ERRNO.EINVAL;
        }
        const pathBytes = new Uint8Array(
          this.memory.buffer,
          pathPtr,
          pathLen
        );
        const pathCopy = new Uint8Array(pathBytes);
        const { response } = this.backend.dispatch(
          {
            opcode: OP_WASI.PATH_OPEN,
            requestId: 0,
            arg0: 0,
            // FdFlags::EMPTY — WASI fdflags not yet wired
            heapPtr: 0,
            heapLen: pathLen
          },
          pathCopy
        );
        if (response.status !== 0) {
          return -response.status;
        }
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
      proc_exit: (rval) => {
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
      args_sizes_get: (argcPtr, bufSizePtr) => {
        return this.sizes_get(OP_WASI.ARGS_SIZES_GET, argcPtr, bufSizePtr);
      },
      environ_sizes_get: (envcPtr, bufSizePtr) => {
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
      args_get: (_argvPtr, _argvBufPtr) => {
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.ARGS_GET,
          requestId: 0,
          heapPtr: 0,
          heapLen: 0
        });
        return response.status !== 0 ? -response.status : 0;
      },
      environ_get: (_envPtr, _envBufPtr) => {
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.ENVIRON_GET,
          requestId: 0,
          heapPtr: 0,
          heapLen: 0
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
      fd_fdstat_get: (fd, bufPtr) => {
        if (this.memory === void 0) return ERRNO.EINVAL;
        const { response, heapOut } = this.backend.dispatch({
          opcode: OP_WASI.FD_FDSTAT_GET,
          requestId: 0,
          arg0: fd,
          heapPtr: 0,
          heapLen: 24
        });
        if (response.status !== 0) return -response.status;
        const writeBuf = new Uint8Array(this.memory.buffer);
        writeBuf.set(heapOut.subarray(0, 24), bufPtr);
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
      clock_time_get: (clockId, _precision, timestampPtr) => {
        if (this.memory === void 0) return ERRNO.EINVAL;
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.CLOCK_TIME_GET,
          requestId: 0,
          arg0: clockId,
          heapPtr: 0,
          heapLen: 0
        });
        if (response.status !== 0) return -response.status;
        const view = new DataView(this.memory.buffer);
        view.setBigInt64(timestampPtr, response.value, true);
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
      fd_prestat_get: (fd, _bufPtr) => {
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.FD_PRESTAT_GET,
          requestId: 0,
          arg0: fd,
          heapPtr: 0,
          heapLen: 0
        });
        return -response.status;
      }
    };
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
      proc_spawn: (pathPtr, pathLen, caps) => {
        if (this.memory === void 0) {
          return -ERRNO.EINVAL;
        }
        const pathBytes = new Uint8Array(
          this.memory.buffer,
          pathPtr,
          pathLen
        );
        const path = new TextDecoder().decode(pathBytes);
        const manifest = encodeSpawnManifest({ path, caps });
        const { response } = this.backend.dispatch(
          {
            opcode: OP_EXT.PROC_SPAWN,
            requestId: 0,
            args: manifest.args,
            heapPtr: 0,
            heapLen: manifest.heap.length
          },
          manifest.heap
        );
        if (response.status !== 0) {
          return response.status;
        }
        return Number(response.value);
      },
      // `ipc_socket(ty: i32) -> i32`
      //
      // Create an unbound socket. Returns the new fd (positive)
      // or negative errno. `ty` is 0 = Stream, 1 = Dgram; any
      // other value returns -EINVAL.
      ipc_socket: (ty) => {
        const { response } = this.backend.dispatch({
          opcode: OP_EXT.IPC_SOCKET,
          requestId: 0,
          arg0: ty,
          heapPtr: 0,
          heapLen: 0
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
      ipc_bind: (fd, pathPtr, pathLen) => {
        if (this.memory === void 0) return -ERRNO.EINVAL;
        const pathBytes = new Uint8Array(
          this.memory.buffer,
          pathPtr,
          pathLen
        );
        const pathCopy = new Uint8Array(pathBytes);
        const { response } = this.backend.dispatch(
          {
            opcode: OP_EXT.IPC_BIND,
            requestId: 0,
            arg0: fd,
            heapPtr: 0,
            heapLen: pathLen
          },
          pathCopy
        );
        return response.status;
      },
      // `ipc_listen(fd: i32, backlog: i32) -> i32`
      //
      // Transition a bound socket to listening. Returns 0 on
      // success, negative errno on failure (EINVAL for bad
      // state, EBADF for bad fd).
      ipc_listen: (fd, backlog) => {
        const args = new Uint8Array(16);
        const view = new DataView(args.buffer);
        view.setUint32(0, fd, true);
        view.setUint32(4, backlog, true);
        const { response } = this.backend.dispatch({
          opcode: OP_EXT.IPC_LISTEN,
          requestId: 0,
          args,
          heapPtr: 0,
          heapLen: 0
        });
        return response.status;
      },
      // `ipc_connect(fd: i32, path_ptr: i32, path_len: i32) -> i32`
      //
      // Connect an unbound socket to the listener at `path`.
      // Returns 0 on success, negative errno on failure
      // (ECONNREFUSED for unbound path, EBADF for bad fd,
      // EINVAL for bad state).
      ipc_connect: (fd, pathPtr, pathLen) => {
        if (this.memory === void 0) return -ERRNO.EINVAL;
        const pathBytes = new Uint8Array(
          this.memory.buffer,
          pathPtr,
          pathLen
        );
        const pathCopy = new Uint8Array(pathBytes);
        const { response } = this.backend.dispatch(
          {
            opcode: OP_EXT.IPC_CONNECT,
            requestId: 0,
            arg0: fd,
            heapPtr: 0,
            heapLen: pathLen
          },
          pathCopy
        );
        return response.status;
      },
      // `ipc_accept(listener_fd: i32) -> i32`
      //
      // Accept one pending connection from the listener.
      // Returns the new server-side fd (positive) or negative
      // errno (EAGAIN if no client pending, EBADF for bad fd,
      // EINVAL if the fd isn't a listening socket).
      ipc_accept: (listenerFd) => {
        const { response } = this.backend.dispatch({
          opcode: OP_EXT.IPC_ACCEPT,
          requestId: 0,
          arg0: listenerFd,
          heapPtr: 0,
          heapLen: 0
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
      display_bind: () => {
        const { response } = this.backend.dispatch({
          opcode: OP_EXT.DISPLAY_BIND,
          requestId: 0,
          heapPtr: 0,
          heapLen: 0
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
      display_connect: () => {
        const { response } = this.backend.dispatch({
          opcode: OP_EXT.DISPLAY_CONNECT,
          requestId: 0,
          heapPtr: 0,
          heapLen: 0
        });
        if (response.status !== 0) return response.status;
        return Number(response.value);
      }
    };
    return {
      wasi_snapshot_preview1: shim,
      pmos_ext: pmosExtShim
    };
  }
};

// src/user-worker-entry.ts
function installUserWorkerEntry(messaging, options = {}) {
  let resolveExited;
  const whenExited = new Promise((resolve) => {
    resolveExited = resolve;
  });
  let bootSeen = false;
  messaging.onmessage = (ev) => {
    if (bootSeen) {
      return;
    }
    const msg = ev.data;
    if (msg.kind !== "boot") {
      messaging.postMessage({
        kind: "exited",
        pid: -1,
        code: -1,
        trap: `user-worker: ${msg.kind} received before boot`
      });
      resolveExited();
      return;
    }
    bootSeen = true;
    void runOnce(messaging, msg, options).finally(() => resolveExited());
  };
  return { whenExited };
}
async function runOnce(messaging, boot, options) {
  const sabView = new Uint8Array(boot.sab);
  const kernelWakeSlot = boot.kernelWakeSlot !== void 0 ? new Int32Array(boot.kernelWakeSlot, 0, 8) : void 0;
  const backend = new SabBackend({
    sab: sabView,
    pid: boot.pid,
    ...options.serviceHook ? { serviceHook: options.serviceHook } : {},
    ...kernelWakeSlot !== void 0 ? { kernelWakeSlot } : {}
  });
  const runtime = new UserWasmRuntime(boot.wasmBytes, backend);
  try {
    const code = await runtime.run();
    messaging.postMessage({ kind: "exited", pid: boot.pid, code });
  } catch (err) {
    const trap = err instanceof Error ? err.message : String(err);
    messaging.postMessage({ kind: "exited", pid: boot.pid, code: -1, trap });
  }
}
if (typeof DedicatedWorkerGlobalScope !== "undefined" && typeof self !== "undefined" && self instanceof DedicatedWorkerGlobalScope) {
  installUserWorkerEntry(self);
}
export {
  installUserWorkerEntry
};
