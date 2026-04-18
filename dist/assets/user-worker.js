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
  FD_FILESTAT_GET: 39,
  FD_PRESTAT_GET: 43,
  FD_READ: 46,
  FD_WRITE: 52,
  PATH_FILESTAT_GET: 65,
  /** Wire-format identity for `path_filestat_set_times`. The shim
   * packs `dir_fd`, `lookup_flags`, and `fstflags` into the inline
   * args window (dir_fd + lookup_flags ignored in v1; fstflags is
   * the SET_ATIM / SET_ATIM_NOW / SET_MTIM / SET_MTIM_NOW bitfield)
   * and puts `atim | mtim | path` in the heap (two u64 LE
   * timestamps followed by the UTF-8 path bytes). The kernel
   * returns 0 on success or -errno on error; no heap round-trip. */
  PATH_FILESTAT_SET_TIMES: 66,
  /** Wire-format identity for `fd_filestat_set_times`. Fd-based
   * sibling of `path_filestat_set_times`: the shim packs `fd` at
   * args[0..4] and `fstflags` at args[4..8]; atim + mtim share the
   * heap (two u64 LE at [0..16], heap_len = 16). Guards mirror the
   * path variant for the flag-pair + heap-length checks, then add
   * the fd guards: EBADF on an unopened fd, EINVAL on any non-Vnode
   * FdObject (char devices / sockets / pipes / signal channels
   * carry no time metadata). Filesystem rejections (EROFS from
   * devfs / procfs) pass through unchanged. */
  FD_FILESTAT_SET_TIMES: 41,
  /** Wire-format identity for `fd_renumber`. WASI's dup2-spelling:
   * atomically move the FdEntry at `from` to `to`, closing whatever
   * was at `to` first. Args pack (from, to) as two u32s at offsets
   * 0 / 4; no heap. `from == to` on an open fd is a no-op success;
   * `from == to` on a closed fd is EBADF (mirrors POSIX's
   * dup2(bad, bad)); `from` not open is EBADF with `to` untouched;
   * prior `to`'s object is released via the kernel's
   * release_object path so pipe / socket refs are not leaked. */
  FD_RENUMBER: 48,
  /** Wire-format identity for `path_rename`. Two heap strings (old
   * + new path) packed into a single heap window with an in-band
   * split point: the shim writes old_len at args[8..12] and lays
   * the heap out as `(old_path, new_path)` concatenated; the kernel
   * splits at that offset. from_dir_fd + to_dir_fd at args[0..4] +
   * [4..8] are ignored in v1 (no preopens). Cross-mount rename is
   * rejected with ENOTSUP (use create+write+unlink instead); within
   * a mount, tmpfs replaces any existing destination per POSIX
   * rename semantics. */
  PATH_RENAME: 71,
  /** Wire-format identity for `path_unlink_file`. Strictly for
   * regular files — unlinking a directory returns EISDIR (use
   * path_remove_directory). dir_fd at args[0..4] is ignored in v1;
   * heap holds the UTF-8 path bytes. Threads through Vfs::unlink
   * (→ Filesystem::unlink on the owning mount). */
  PATH_UNLINK_FILE: 73,
  /** Wire-format identity for `path_create_directory`. mkdir
   * opcode; wire layout matches path_unlink_file. dir_fd at
   * args[0..4] is ignored in v1; heap holds the UTF-8 path bytes.
   * The kernel hard-codes mode 0o755 on the Vfs::mkdir call —
   * WASI's mkdir signature has no mode argument. Branches:
   * AlreadyExists → EEXIST, missing parent → ENOENT, devfs/procfs
   * → EROFS, invalid UTF-8 → EINVAL. */
  PATH_CREATE_DIRECTORY: 64,
  /** Wire-format identity for `path_remove_directory`. rmdir
   * opcode; wire layout matches path_unlink_file. dir_fd at
   * args[0..4] is ignored in v1; heap holds the UTF-8 path bytes.
   * Threads through Vfs::rmdir (→ Filesystem::rmdir on the owning
   * mount). Branches: non-empty directory → ENOTEMPTY, regular
   * file target → ENOTDIR (tmpfs.rmdir returns NotADirectory for a
   * non-dir target — callers must use path_unlink_file for regular
   * files), missing → ENOENT, devfs/procfs → EROFS, invalid UTF-8
   * → EINVAL. */
  PATH_REMOVE_DIRECTORY: 70,
  PATH_OPEN: 68,
  PROC_EXIT: 96,
  CLOCK_RES_GET: 16,
  CLOCK_TIME_GET: 17,
  RANDOM_GET: 81,
  SCHED_YIELD: 82,
  /** Wire-format identity for `fd_seek`. The shim packs
   * `(fd, whence, offset)` into the inline args window; the kernel
   * returns the new absolute offset in `response.value`. */
  FD_SEEK: 49,
  /** Wire-format identity for `fd_tell`. The read-only sibling of
   * `fd_seek`: the shim packs `fd` as a single u32 at `arg0`; the
   * kernel returns the current absolute offset in `response.value`
   * without mutating it. Functionally a `fd_seek(fd, 0, Cur, *)` at
   * the WASI surface level. */
  FD_TELL: 51,
  /** Wire-format identity for the four fd-state opcodes. All four
   * take an fd as a u32 at `arg0` and collapse to trivial semantics
   * in v1's tmpfs-backed VFS: advise/sync/datasync = no-op success
   * on a Vnode; allocate = ENOTSUP on a Vnode; EBADF on an unopened
   * fd and EINVAL on every non-Vnode FdObject. */
  FD_ADVISE: 32,
  FD_ALLOCATE: 33,
  FD_SYNC: 50,
  FD_DATASYNC: 35,
  /** Wire-format identity for `fd_fdstat_set_flags`. WASI's
   * equivalent of POSIX fcntl(F_SETFL): overwrites the fd's
   * file-status flags (NONBLOCK / APPEND / DSYNC / RSYNC / SYNC).
   * Wire: fd at args[0..4], new_fdflags (WASI encoding — see
   * FDFLAGS below) at args[4..8]. v1 recognises only NONBLOCK +
   * APPEND meaningfully; DSYNC/RSYNC/SYNC are accepted + ignored
   * (tmpfs writes are already synchronous). CLOEXEC is preserved
   * across the call (F_SETFD owns that bit, not F_SETFL), so a
   * CLOEXEC-marked fd that receives fd_fdstat_set_flags(NONBLOCK)
   * ends up CLOEXEC + NONBLOCK. EBADF on an unopened fd; no
   * FdObject-variant rejection (WASI permits the call on any fd
   * type). */
  FD_FDSTAT_SET_FLAGS: 37,
  /** Wire-format identity for `fd_filestat_set_size`. WASI's
   * equivalent of POSIX ftruncate: truncate or zero-extend a
   * seekable fd to an exact byte count. Wire: fd at args[0..4],
   * new_size u64 LE at args[4..12]. Vnode-only — char device /
   * socket / pipe / signal-channel / display-connection fds are
   * rejected with EINVAL (same non-Vnode guard as fd_seek /
   * fd_tell / fd_filestat_set_times). Directory target passes
   * through to tmpfs.truncate → IsADirectory → EISDIR;
   * read-only filesystems (procfs) return EROFS. Shrinking
   * discards tail bytes, extending past EOF zero-fills. */
  FD_FILESTAT_SET_SIZE: 40,
  /** Wire-format identity for `fd_pread` / `fd_pwrite`. Positional
   * I/O variants of fd_read / fd_write: take an explicit offset
   * from inline args and do NOT mutate FdEntry.offset. Wire
   * (both shapes identical except for heap direction): fd at
   * args[0..4], offset u64 LE at args[4..12]; heap = destination
   * buffer (pread) or source bytes (pwrite). Vnode-only — non-
   * Vnode FdObject variants reject with EINVAL (same guard shape
   * as fd_seek / fd_tell). Threads directly through Vfs::read_ino
   * / Vfs::write_ino at the explicit offset so entry.offset
   * stays untouched — a pread/pwrite pair does not disturb a
   * subsequent fd_read / fd_seek that uses the seekable-fd
   * position. */
  FD_PREAD: 42,
  FD_PWRITE: 45,
  /** Unused by the WASI shim today; the tests probe it to verify
   * the dispatcher's `ENOSYS` path still fires for opcodes the
   * kernel doesn't yet handle. Was `FD_PREAD` before that handler
   * landed; swap to whichever WASI opcode is still unhandled as
   * the implementation catches up. */
  SOCK_SHUTDOWN: 115,
  /** Wire-format identity for `fd_readdir`. Directory-listing
   * opcode. args[0..4] = fd (u32); args[4..12] = cookie (u64
   * LE; 0 = start from beginning); heap = caller's output buffer
   * with capacity heap_len bytes. Kernel writes 24-byte dirent_t
   * records + inline name bytes into the buffer until it fills
   * or entries exhaust. value / extraLen = bytes written. Entries
   * pack back-to-back with no padding; a buffer that fills mid-
   * entry signals "more may exist" by returning value == heap_len
   * and the caller re-issues with the last d_next as the cookie. */
  FD_READDIR: 47,
  /** Wire-format identity for `poll_oneoff`. The shim packs
   * `(n_subs, n_events_cap)` into the inline args window (u32 each
   * at offsets 0 / 4) and puts the subscription list followed by
   * an events output window in the heap — subs at [0..n_subs*48],
   * events at [n_subs*48..n_subs*48 + n_events_cap*32]. The kernel
   * returns the actual event count in `response.value` and echoes
   * it in `response.extraLen`. v1 is non-blocking: CLOCK fires only
   * if the target time is already past; FD_READ / FD_WRITE fire
   * only if the op would make progress right now. */
  POLL_ONEOFF: 80
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
  ECONNREFUSED: 14,
  EEXIST: 20,
  EINVAL: 28,
  EISDIR: 31,
  ENOENT: 44,
  ENOSYS: 52,
  ENOTDIR: 54,
  ENOTEMPTY: 55,
  ENOTSUP: 58,
  EROFS: 69
};
var POLL_SUBSCRIPTION_SIZE = 48;
var POLL_EVENT_SIZE = 32;
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
      fd_seek: (fd, offset, whence, newOffsetPtr) => {
        if (this.memory === void 0) return ERRNO.EINVAL;
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
          heapLen: 0
        });
        if (response.status !== 0) return -response.status;
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
      fd_tell: (fd, offsetPtr) => {
        if (this.memory === void 0) return ERRNO.EINVAL;
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.FD_TELL,
          requestId: 0,
          arg0: fd,
          heapPtr: 0,
          heapLen: 0
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
      fd_advise: (_fd, _offset, _len, _advice) => {
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.FD_ADVISE,
          requestId: 0,
          arg0: _fd,
          heapPtr: 0,
          heapLen: 0
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
      fd_allocate: (_fd, _offset, _len) => {
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.FD_ALLOCATE,
          requestId: 0,
          arg0: _fd,
          heapPtr: 0,
          heapLen: 0
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
      fd_sync: (fd) => {
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.FD_SYNC,
          requestId: 0,
          arg0: fd,
          heapPtr: 0,
          heapLen: 0
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
      fd_datasync: (fd) => {
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.FD_DATASYNC,
          requestId: 0,
          arg0: fd,
          heapPtr: 0,
          heapLen: 0
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
      proc_exit: (rval) => {
        this.backend.dispatch({
          opcode: OP_WASI.PROC_EXIT,
          requestId: 0,
          arg0: rval >>> 0
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
      fd_filestat_get: (fd, bufPtr) => {
        if (this.memory === void 0) return ERRNO.EINVAL;
        const { response, heapOut } = this.backend.dispatch({
          opcode: OP_WASI.FD_FILESTAT_GET,
          requestId: 0,
          arg0: fd,
          heapPtr: 0,
          heapLen: 64
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
      path_filestat_get: (_dirfd, _flags, pathPtr, pathLen, bufPtr) => {
        if (this.memory === void 0) return ERRNO.EINVAL;
        const pathBytes = new Uint8Array(
          this.memory.buffer,
          pathPtr,
          pathLen
        );
        const pathCopy = new Uint8Array(pathBytes);
        const { response, heapOut } = this.backend.dispatch(
          {
            opcode: OP_WASI.PATH_FILESTAT_GET,
            requestId: 0,
            arg0: 0,
            // dir_fd — ignored
            heapPtr: 0,
            heapLen: pathLen
          },
          pathCopy
        );
        if (response.status !== 0) return -response.status;
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
      path_filestat_set_times: (_dirfd, _flags, pathPtr, pathLen, atim, mtim, fstflags) => {
        if (this.memory === void 0) return ERRNO.EINVAL;
        const pathBytes = new Uint8Array(
          this.memory.buffer,
          pathPtr,
          pathLen
        );
        const heap = new Uint8Array(16 + pathLen);
        const heapView = new DataView(heap.buffer);
        heapView.setBigUint64(0, atim, true);
        heapView.setBigUint64(8, mtim, true);
        heap.set(pathBytes, 16);
        const args = new Uint8Array(16);
        const argsView = new DataView(args.buffer);
        argsView.setUint32(0, 0, true);
        argsView.setUint32(4, 0, true);
        argsView.setUint32(8, fstflags, true);
        const { response } = this.backend.dispatch(
          {
            opcode: OP_WASI.PATH_FILESTAT_SET_TIMES,
            requestId: 0,
            args,
            heapPtr: 0,
            heapLen: heap.length
          },
          heap
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
      fd_readdir: (fd, bufPtr, bufLen, cookie, bufusedPtr) => {
        if (this.memory === void 0) return ERRNO.EINVAL;
        const args = new Uint8Array(16);
        const argsView = new DataView(args.buffer);
        argsView.setUint32(0, fd, true);
        argsView.setBigUint64(4, cookie, true);
        const heap = new Uint8Array(bufLen);
        const { response, heapOut } = this.backend.dispatch(
          {
            opcode: OP_WASI.FD_READDIR,
            requestId: 0,
            args,
            heapPtr: 0,
            heapLen: heap.length
          },
          heap
        );
        if (response.status !== 0) return -response.status;
        const written = Number(response.value);
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
      path_unlink_file: (_dirfd, pathPtr, pathLen) => {
        if (this.memory === void 0) return ERRNO.EINVAL;
        const pathBytes = new Uint8Array(this.memory.buffer, pathPtr, pathLen);
        const heap = new Uint8Array(pathLen);
        heap.set(pathBytes, 0);
        const { response } = this.backend.dispatch(
          {
            opcode: OP_WASI.PATH_UNLINK_FILE,
            requestId: 0,
            arg0: 0,
            // dir_fd ignored in v1
            heapPtr: 0,
            heapLen: heap.length
          },
          heap
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
      path_rename: (_oldFd, oldPathPtr, oldPathLen, _newFd, newPathPtr, newPathLen) => {
        if (this.memory === void 0) return ERRNO.EINVAL;
        const oldBytes = new Uint8Array(
          this.memory.buffer,
          oldPathPtr,
          oldPathLen
        );
        const newBytes = new Uint8Array(
          this.memory.buffer,
          newPathPtr,
          newPathLen
        );
        const heap = new Uint8Array(oldPathLen + newPathLen);
        heap.set(oldBytes, 0);
        heap.set(newBytes, oldPathLen);
        const args = new Uint8Array(16);
        const argsView = new DataView(args.buffer);
        argsView.setUint32(0, 0, true);
        argsView.setUint32(4, 0, true);
        argsView.setUint32(8, oldPathLen, true);
        const { response } = this.backend.dispatch(
          {
            opcode: OP_WASI.PATH_RENAME,
            requestId: 0,
            args,
            heapPtr: 0,
            heapLen: heap.length
          },
          heap
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
      path_create_directory: (_dirfd, pathPtr, pathLen) => {
        if (this.memory === void 0) return ERRNO.EINVAL;
        const pathBytes = new Uint8Array(this.memory.buffer, pathPtr, pathLen);
        const heap = new Uint8Array(pathLen);
        heap.set(pathBytes, 0);
        const { response } = this.backend.dispatch(
          {
            opcode: OP_WASI.PATH_CREATE_DIRECTORY,
            requestId: 0,
            arg0: 0,
            // dir_fd ignored in v1
            heapPtr: 0,
            heapLen: heap.length
          },
          heap
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
      path_remove_directory: (_dirfd, pathPtr, pathLen) => {
        if (this.memory === void 0) return ERRNO.EINVAL;
        const pathBytes = new Uint8Array(this.memory.buffer, pathPtr, pathLen);
        const heap = new Uint8Array(pathLen);
        heap.set(pathBytes, 0);
        const { response } = this.backend.dispatch(
          {
            opcode: OP_WASI.PATH_REMOVE_DIRECTORY,
            requestId: 0,
            arg0: 0,
            // dir_fd ignored in v1
            heapPtr: 0,
            heapLen: heap.length
          },
          heap
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
      fd_renumber: (from, to) => {
        const args = new Uint8Array(16);
        const argsView = new DataView(args.buffer);
        argsView.setUint32(0, from, true);
        argsView.setUint32(4, to, true);
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.FD_RENUMBER,
          requestId: 0,
          args,
          heapPtr: 0,
          heapLen: 0
        });
        return response.status !== 0 ? -response.status : 0;
      },
      // WASI `fd_pread`.
      //
      // Signature (lowered):
      //   (fd: i32, iovs_ptr: i32, iovs_len: i32, offset: i64,
      //    nread_ptr: i32) -> errno: i32
      //
      // Positional read: read from an explicit offset without
      // advancing FdEntry.offset. v1 lowers a single-iovec shape
      // (the iovec struct is `(buf_ptr: u32, buf_len: u32)` at
      // iovs_ptr; iovs_len is usually 1 for println!-style libc
      // callers). Copies the read bytes back into user memory at
      // iovec.buf_ptr and writes the byte count as a u32 at
      // nread_ptr. Vnode-only — non-Vnode fds return -EINVAL;
      // unopened fds return -EBADF.
      fd_pread: (fd, iovsPtr, iovsLen, offset, nreadPtr) => {
        if (this.memory === void 0) return ERRNO.EINVAL;
        if (iovsLen !== 1) return ERRNO.EINVAL;
        const iovView = new DataView(this.memory.buffer, iovsPtr, 8);
        const bufPtr = iovView.getUint32(0, true);
        const bufLen = iovView.getUint32(4, true);
        const args = new Uint8Array(16);
        const argsView = new DataView(args.buffer);
        argsView.setUint32(0, fd, true);
        argsView.setBigUint64(4, offset, true);
        const heapOut = new Uint8Array(bufLen);
        const { response } = this.backend.dispatch(
          {
            opcode: OP_WASI.FD_PREAD,
            requestId: 0,
            args,
            heapPtr: 0,
            heapLen: bufLen
          },
          heapOut
        );
        if (response.status !== 0) return -response.status;
        const read = Number(response.value);
        const memBytes = new Uint8Array(this.memory.buffer);
        memBytes.set(heapOut.subarray(0, read), bufPtr);
        const memView = new DataView(this.memory.buffer);
        memView.setUint32(nreadPtr, read, true);
        return 0;
      },
      // WASI `fd_pwrite`.
      //
      // Signature (lowered):
      //   (fd: i32, iovs_ptr: i32, iovs_len: i32, offset: i64,
      //    nwritten_ptr: i32) -> errno: i32
      //
      // Positional write: write at an explicit offset without
      // advancing FdEntry.offset. Mirrors fd_pread's iovec lowering
      // — single-iovec only in v1; multi-iovec callers that libc
      // might emit lower to multiple single-iovec pwrites.
      fd_pwrite: (fd, iovsPtr, iovsLen, offset, nwrittenPtr) => {
        if (this.memory === void 0) return ERRNO.EINVAL;
        if (iovsLen !== 1) return ERRNO.EINVAL;
        const iovView = new DataView(this.memory.buffer, iovsPtr, 8);
        const bufPtr = iovView.getUint32(0, true);
        const bufLen = iovView.getUint32(4, true);
        const bytes = new Uint8Array(this.memory.buffer, bufPtr, bufLen);
        const heap = new Uint8Array(bufLen);
        heap.set(bytes, 0);
        const args = new Uint8Array(16);
        const argsView = new DataView(args.buffer);
        argsView.setUint32(0, fd, true);
        argsView.setBigUint64(4, offset, true);
        const { response } = this.backend.dispatch(
          {
            opcode: OP_WASI.FD_PWRITE,
            requestId: 0,
            args,
            heapPtr: 0,
            heapLen: bufLen
          },
          heap
        );
        if (response.status !== 0) return -response.status;
        const written = Number(response.value);
        const memView = new DataView(this.memory.buffer);
        memView.setUint32(nwrittenPtr, written, true);
        return 0;
      },
      // WASI `fd_filestat_set_size`.
      //
      // Signature (lowered):
      //   (fd: i32, new_size: i64) -> errno: i32
      //
      // WASI's equivalent of POSIX ftruncate: truncate / zero-extend
      // a seekable fd to an exact byte count. Vnode-only — non-Vnode
      // fds reject with EINVAL. Directory targets reject with EISDIR
      // (tmpfs.truncate returns IsADirectory). Read-only filesystems
      // return EROFS. Wire: fd + new_size in the inline args window
      // (u32 + u64 LE); no heap round-trip.
      fd_filestat_set_size: (fd, new_size) => {
        const args = new Uint8Array(16);
        const argsView = new DataView(args.buffer);
        argsView.setUint32(0, fd, true);
        argsView.setBigUint64(4, new_size, true);
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.FD_FILESTAT_SET_SIZE,
          requestId: 0,
          args,
          heapPtr: 0,
          heapLen: 0
        });
        return response.status !== 0 ? -response.status : 0;
      },
      // WASI `fd_fdstat_set_flags`.
      //
      // Signature (lowered):
      //   (fd: i32, fdflags: i32) -> errno: i32
      //
      // WASI's equivalent of POSIX fcntl(F_SETFL): overwrites the fd's
      // file-status flags (NONBLOCK / APPEND / DSYNC / RSYNC / SYNC).
      // v1 recognises NONBLOCK + APPEND; sync-family bits are accepted
      // + ignored. CLOEXEC is preserved on the fd across the call. No
      // FdObject-variant rejection — WASI permits the call on any fd
      // type. Dispatches FD_FDSTAT_SET_FLAGS packing (fd, fdflags) as
      // two u32s in the inline args window. No heap round-trip.
      fd_fdstat_set_flags: (fd, fdflags) => {
        const args = new Uint8Array(16);
        const argsView = new DataView(args.buffer);
        argsView.setUint32(0, fd, true);
        argsView.setUint32(4, fdflags, true);
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.FD_FDSTAT_SET_FLAGS,
          requestId: 0,
          args,
          heapPtr: 0,
          heapLen: 0
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
      fd_filestat_set_times: (fd, atim, mtim, fstflags) => {
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
            heapLen: heap.length
          },
          heap
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
      poll_oneoff: (inPtr, outPtr, nsubscriptions, neventsPtr) => {
        if (this.memory === void 0) return ERRNO.EINVAL;
        if (nsubscriptions === 0) return ERRNO.EINVAL;
        const subsBytes = nsubscriptions * POLL_SUBSCRIPTION_SIZE;
        const heap = new Uint8Array(subsBytes);
        heap.set(
          new Uint8Array(this.memory.buffer, inPtr, subsBytes),
          0
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
            heapLen: heap.length
          },
          heap
        );
        if (response.status !== 0) return -response.status;
        const nEvents = Number(response.value);
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
      clock_res_get: (clockId, resolutionPtr) => {
        if (this.memory === void 0) return ERRNO.EINVAL;
        const { response } = this.backend.dispatch({
          opcode: OP_WASI.CLOCK_RES_GET,
          requestId: 0,
          arg0: clockId,
          heapPtr: 0,
          heapLen: 0
        });
        if (response.status !== 0) return -response.status;
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
