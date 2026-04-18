// Typed wire format for the PMos syscall transport.
//
// Hand-maintained TypeScript mirror of the Rust `abi` crate:
//
//   * `crates/abi/src/ring.rs` — `Request` / `Response` struct layout
//   * `crates/abi/src/wasi.rs` — WASI preview 1 opcode constants
//   * `crates/abi/src/ext.rs`  — PMos extension opcode constants
//   * `crates/abi/src/errno.rs` — errno constants (positive values)
//   * `crates/abi/src/cap.rs`  — capability enum
//   * `crates/kernel/src/platform/mod.rs` — `DevId` enum
//
// Kept hand-written for now because the opcode / errno / cap tables
// are small enough that drift is manageable and a Vitest round-trip
// test at `web/tests/unit/syscall.test.ts` would catch any mismatch
// mechanically. A future slice should promote this file to
// autogeneration by `cargo run -p xtask -- gen-sab-layout` the same
// way `./sab-layout.ts` works today.
//
// The Rust side stays normative: if these constants ever disagree
// with the abi crate, the abi crate wins and this file is wrong.

/* eslint-disable @typescript-eslint/naming-convention */

// ---- Request / Response byte layout ---------------------------------

/** Size of a request / response slot in bytes. */
export const SLOT_SIZE = 32;

/**
 * A syscall request as the caller wants the kernel to see it. Mirrors
 * `abi::ring::Request` but with ergonomic fields instead of the raw
 * 16-byte inline args window.
 *
 * For the common case where a handler reads a single `u32` at
 * `args[0..4]` (almost every WASI opcode we've implemented so far),
 * set `arg0` and leave `args` undefined. If a handler needs a more
 * complex args layout, pass a 16-byte `args` Uint8Array directly and
 * leave `arg0` undefined.
 */
export interface SyscallRequest {
  readonly opcode: number;
  readonly requestId: number;
  /** Reserved in v1; MUST be 0. */
  readonly flags?: number;
  /** Single u32 written at `args[0..4]`. Mutually exclusive with `args`. */
  readonly arg0?: number;
  /** Full 16-byte inline args. Mutually exclusive with `arg0`. */
  readonly args?: Uint8Array;
  /** Offset into the heap scratch region where a payload lives. */
  readonly heapPtr?: number;
  /** Length of the payload at `heapPtr`. */
  readonly heapLen?: number;
}

/** Decoded fields of a [`Response`] slot. */
export interface SyscallResponse {
  readonly requestId: number;
  /** 0 on success, negative errno on failure. */
  readonly status: number;
  /** Primary return value (widened to `bigint` because Rust-side is `i64`). */
  readonly value: bigint;
  /** Length of any payload the handler wrote to the heap scratch region. */
  readonly extraLen: number;
}

/**
 * Encode a [`SyscallRequest`] into the 32-byte little-endian layout
 * the kernel's dispatcher reads from its request slot.
 *
 * Inverse of [`decodeResponse`] / `Request::to_le_bytes` on the Rust
 * side.
 */
export function encodeRequest(req: SyscallRequest): Uint8Array {
  const buf = new Uint8Array(SLOT_SIZE);
  const view = new DataView(buf.buffer);
  view.setUint16(0, req.opcode, true);
  view.setUint16(2, req.flags ?? 0, true);
  view.setUint32(4, req.requestId, true);
  if (req.args !== undefined) {
    if (req.args.length !== 16) {
      throw new Error(`syscall.encodeRequest: args must be 16 bytes, got ${req.args.length}`);
    }
    if (req.arg0 !== undefined) {
      throw new Error("syscall.encodeRequest: pass either args or arg0, not both");
    }
    buf.set(req.args, 8);
  } else if (req.arg0 !== undefined) {
    view.setUint32(8, req.arg0, true);
  }
  view.setUint32(24, req.heapPtr ?? 0, true);
  view.setUint32(28, req.heapLen ?? 0, true);
  return buf;
}

/**
 * Decode a 32-byte [`Response`] slot into the semantically-interesting
 * fields. Padding is dropped because no handler uses it yet.
 *
 * Inverse of `Response::from_le_bytes` on the Rust side.
 */
export function decodeResponse(bytes: Uint8Array): SyscallResponse {
  if (bytes.length !== SLOT_SIZE) {
    throw new Error(`syscall.decodeResponse: expected ${SLOT_SIZE} bytes, got ${bytes.length}`);
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, SLOT_SIZE);
  return {
    requestId: view.getUint32(0, true),
    status: view.getInt32(4, true),
    value: view.getBigInt64(8, true),
    extraLen: view.getUint32(16, true),
  };
}

/**
 * Encode a [`SyscallResponse`] into the 32-byte little-endian layout
 * the kernel produces. Padding bytes are zeroed. Inverse of
 * [`decodeResponse`] and of `Response::to_le_bytes` on the Rust side.
 *
 * Used by the SAB-ring servicing path, which reads a decoded response
 * out of `KernelWasmHost.dispatch` and needs to write it back to the
 * per-pid SAB response ring in the exact byte layout the user-side
 * `Sab::try_pop_response` expects.
 */
export function encodeResponse(res: SyscallResponse): Uint8Array {
  const buf = new Uint8Array(SLOT_SIZE);
  const view = new DataView(buf.buffer);
  view.setUint32(0, res.requestId, true);
  view.setInt32(4, res.status, true);
  view.setBigInt64(8, res.value, true);
  view.setUint32(16, res.extraLen, true);
  return buf;
}

/** Decoded fields of a [`Request`] slot; mirror of [`SyscallRequest`]
 * but with fully-populated `args` (16 bytes, owned) and explicit
 * `flags`/`heapPtr`/`heapLen`. Used by the SAB-ring servicing path. */
export interface DecodedRequest {
  readonly opcode: number;
  readonly flags: number;
  readonly requestId: number;
  /** The full 16-byte inline args window, copied out of the slot. */
  readonly args: Uint8Array;
  readonly heapPtr: number;
  readonly heapLen: number;
}

/**
 * Decode a 32-byte request slot as the inverse of [`encodeRequest`]
 * and of `Request::from_le_bytes` on the Rust side. The returned
 * `args` field is a fresh owned `Uint8Array` — it does not alias the
 * input bytes.
 */
export function decodeRequest(bytes: Uint8Array): DecodedRequest {
  if (bytes.length !== SLOT_SIZE) {
    throw new Error(`syscall.decodeRequest: expected ${SLOT_SIZE} bytes, got ${bytes.length}`);
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, SLOT_SIZE);
  return {
    opcode: view.getUint16(0, true),
    flags: view.getUint16(2, true),
    requestId: view.getUint32(4, true),
    args: bytes.slice(8, 24),
    heapPtr: view.getUint32(24, true),
    heapLen: view.getUint32(28, true),
  };
}

// ---- Opcode constants -----------------------------------------------
//
// Mirror of `abi::wasi` + `abi::ext`. Only the opcodes the dispatcher
// currently implements have named constants; adding a new one here is
// mechanical and happens when the Rust-side handler lands.

/** WASI preview 1 opcodes (0x0001..0x0080). */
export const OP_WASI = {
  ARGS_GET: 0x0001,
  ARGS_SIZES_GET: 0x0002,
  ENVIRON_GET: 0x0003,
  ENVIRON_SIZES_GET: 0x0004,
  FD_CLOSE: 0x0022,
  FD_FDSTAT_GET: 0x0024,
  FD_FILESTAT_GET: 0x0027,
  FD_PRESTAT_GET: 0x002b,
  FD_READ: 0x002e,
  FD_WRITE: 0x0034,
  PATH_FILESTAT_GET: 0x0041,
  /** Wire-format identity for `path_filestat_set_times`. The shim
   * packs `dir_fd`, `lookup_flags`, and `fstflags` into the inline
   * args window (dir_fd + lookup_flags ignored in v1; fstflags is
   * the SET_ATIM / SET_ATIM_NOW / SET_MTIM / SET_MTIM_NOW bitfield)
   * and puts `atim | mtim | path` in the heap (two u64 LE
   * timestamps followed by the UTF-8 path bytes). The kernel
   * returns 0 on success or -errno on error; no heap round-trip. */
  PATH_FILESTAT_SET_TIMES: 0x0042,
  /** Wire-format identity for `fd_filestat_set_times`. Fd-based
   * sibling of `path_filestat_set_times`: the shim packs `fd` at
   * args[0..4] and `fstflags` at args[4..8]; atim + mtim share the
   * heap (two u64 LE at [0..16], heap_len = 16). Guards mirror the
   * path variant for the flag-pair + heap-length checks, then add
   * the fd guards: EBADF on an unopened fd, EINVAL on any non-Vnode
   * FdObject (char devices / sockets / pipes / signal channels
   * carry no time metadata). Filesystem rejections (EROFS from
   * devfs / procfs) pass through unchanged. */
  FD_FILESTAT_SET_TIMES: 0x0029,
  /** Wire-format identity for `fd_renumber`. WASI's dup2-spelling:
   * atomically move the FdEntry at `from` to `to`, closing whatever
   * was at `to` first. Args pack (from, to) as two u32s at offsets
   * 0 / 4; no heap. `from == to` on an open fd is a no-op success;
   * `from == to` on a closed fd is EBADF (mirrors POSIX's
   * dup2(bad, bad)); `from` not open is EBADF with `to` untouched;
   * prior `to`'s object is released via the kernel's
   * release_object path so pipe / socket refs are not leaked. */
  FD_RENUMBER: 0x0030,
  /** Wire-format identity for `path_rename`. Two heap strings (old
   * + new path) packed into a single heap window with an in-band
   * split point: the shim writes old_len at args[8..12] and lays
   * the heap out as `(old_path, new_path)` concatenated; the kernel
   * splits at that offset. from_dir_fd + to_dir_fd at args[0..4] +
   * [4..8] are ignored in v1 (no preopens). Cross-mount rename is
   * rejected with ENOTSUP (use create+write+unlink instead); within
   * a mount, tmpfs replaces any existing destination per POSIX
   * rename semantics. */
  PATH_RENAME: 0x0047,
  /** Wire-format identity for `path_unlink_file`. Strictly for
   * regular files — unlinking a directory returns EISDIR (use
   * path_remove_directory). dir_fd at args[0..4] is ignored in v1;
   * heap holds the UTF-8 path bytes. Threads through Vfs::unlink
   * (→ Filesystem::unlink on the owning mount). */
  PATH_UNLINK_FILE: 0x0049,
  /** Wire-format identity for `path_create_directory`. mkdir
   * opcode; wire layout matches path_unlink_file. dir_fd at
   * args[0..4] is ignored in v1; heap holds the UTF-8 path bytes.
   * The kernel hard-codes mode 0o755 on the Vfs::mkdir call —
   * WASI's mkdir signature has no mode argument. Branches:
   * AlreadyExists → EEXIST, missing parent → ENOENT, devfs/procfs
   * → EROFS, invalid UTF-8 → EINVAL. */
  PATH_CREATE_DIRECTORY: 0x0040,
  /** Wire-format identity for `path_remove_directory`. rmdir
   * opcode; wire layout matches path_unlink_file. dir_fd at
   * args[0..4] is ignored in v1; heap holds the UTF-8 path bytes.
   * Threads through Vfs::rmdir (→ Filesystem::rmdir on the owning
   * mount). Branches: non-empty directory → ENOTEMPTY, regular
   * file target → ENOTDIR (tmpfs.rmdir returns NotADirectory for a
   * non-dir target — callers must use path_unlink_file for regular
   * files), missing → ENOENT, devfs/procfs → EROFS, invalid UTF-8
   * → EINVAL. */
  PATH_REMOVE_DIRECTORY: 0x0046,
  PATH_OPEN: 0x0044,
  PROC_EXIT: 0x0060,
  CLOCK_RES_GET: 0x0010,
  CLOCK_TIME_GET: 0x0011,
  RANDOM_GET: 0x0051,
  SCHED_YIELD: 0x0052,
  /** Wire-format identity for `fd_seek`. The shim packs
   * `(fd, whence, offset)` into the inline args window; the kernel
   * returns the new absolute offset in `response.value`. */
  FD_SEEK: 0x0031,
  /** Wire-format identity for `fd_tell`. The read-only sibling of
   * `fd_seek`: the shim packs `fd` as a single u32 at `arg0`; the
   * kernel returns the current absolute offset in `response.value`
   * without mutating it. Functionally a `fd_seek(fd, 0, Cur, *)` at
   * the WASI surface level. */
  FD_TELL: 0x0033,
  /** Wire-format identity for the four fd-state opcodes. All four
   * take an fd as a u32 at `arg0` and collapse to trivial semantics
   * in v1's tmpfs-backed VFS: advise/sync/datasync = no-op success
   * on a Vnode; allocate = ENOTSUP on a Vnode; EBADF on an unopened
   * fd and EINVAL on every non-Vnode FdObject. */
  FD_ADVISE: 0x0020,
  FD_ALLOCATE: 0x0021,
  FD_SYNC: 0x0032,
  FD_DATASYNC: 0x0023,
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
  FD_FDSTAT_SET_FLAGS: 0x0025,
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
  FD_FILESTAT_SET_SIZE: 0x0028,
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
  FD_PREAD: 0x002a,
  FD_PWRITE: 0x002d,
  /** Wire-format identity for `sock_send` / `sock_recv`. WASI
   * socket aliases of FD_WRITE / FD_READ on Socket fds. Both wire
   * layouts match fd_read / fd_write except for one extra u16 at
   * args[4..6] (si_flags for send, ri_flags for recv — v1 ignores).
   * Socket-only — non-Socket FdObject variants reject with EINVAL
   * (PMos has no ENOTSOCK errno; EINVAL matches the non-Vnode
   * guard shape used by every other fd-type-specific opcode).
   * Unopened fd is EBADF; InvalidState IpcError (sending on an
   * unconnected socket) also surfaces as EINVAL. Reuses
   * kernel.ipc.send_on_socket / recv_on_socket directly. */
  SOCK_SEND: 0x0072,
  SOCK_RECV: 0x0071,
  /** Wire-format identity for `sock_accept`. WASI alias of the
   * existing IPC_ACCEPT (ext 0x1004). Wire: listener_fd at
   * args[0..4], fdflags (WASI encoding) at args[4..8]. The kernel
   * forwards to the existing accept-socket path and applies the
   * fdflags to the new fd via FdFlags::from_wasi_bits. Response.value
   * = freshly-allocated fd for the accepted connection. Error
   * surface: non-Socket fd → EINVAL, unopened → EBADF, listener
   * not in Listening state → EINVAL, empty backlog → EAGAIN. */
  SOCK_ACCEPT: 0x0070,
  /** Wire-format identity for `sock_shutdown`. Shutdown one or
   * both directions of a socket. Wire: fd at args[0..4], how
   * (low 8 bits = WASI sdflags: RD=0x1, WR=0x2) at args[4..8].
   * v1's IpcTable has no half-close primitive, so the handler
   * accepts only how=RD|WR (full close via close_socket) and
   * rejects the half-close combinations with ENOTSUP. Zero how
   * or bits beyond RD|WR reject with EINVAL. Non-Socket fd →
   * EINVAL; unopened fd → EBADF. After a successful shutdown the
   * peer observes EOF on its next recv. */
  SOCK_SHUTDOWN: 0x0073,
  /** Wire-format identity for `path_link`. Hardlink opcode: create a
   * new directory entry pointing at an existing inode. Wire packs
   * (old_fd, old_flags, new_fd, old_len) into the inline args window
   * as four u32s at offsets 0/4/8/12; old_fd/old_flags/new_fd are
   * ignored in v1 (no preopens, no symlink-following in resolve).
   * The heap carries both paths concatenated with the split at
   * args[12..16]: heap[0..old_len] = source path, heap[old_len..] =
   * new hardlink-target path. Threads through Vfs::link → the owning
   * mount's Filesystem::link; tmpfs bumps nlink + adds a dir entry,
   * devfs / procfs inherit the trait default (ReadOnly → EROFS).
   * Cross-mount links return ENOTSUP. */
  PATH_LINK: 0x0043,
  /** Wire-format identity for `path_symlink`. Symlink-creation
   * opcode: creates a vnode whose "content" is an arbitrary UTF-8
   * target string. Wire: args[0..4] = old_len (u32; split point in
   * heap — heap[0..old_len] is the target string the symlink holds,
   * heap[old_len..heap_len] is the new path to create as a symlink).
   * Cleaner packing than PATH_LINK because WASI path_symlink has
   * only one integer-shaped arg (new_fd) and v1 ignores it. The
   * target need not exist (dangling symlinks are fine) and v1's
   * Vfs::resolve does NOT dereference symlinks — stat() on the
   * symlink returns the symlink itself, path_open on the symlink
   * path yields the symlink's fd. Threads through Vfs::symlink →
   * Filesystem::symlink; tmpfs allocates a new ino with a
   * TmpNode::SymLink(target) variant; devfs / procfs / opfs inherit
   * the trait default (NotSupported → ENOTSUP). */
  PATH_SYMLINK: 0x0048,
  /** Wire-format identity for `path_readlink`. Symlink-dereference
   * opcode: copy a symlink's target bytes into a caller-supplied
   * output buffer. Wire: args[0..4] = dir_fd (ignored), args[4..8] =
   * path_len (u32; how many bytes at heap[0..] are the UTF-8 input
   * path; the remainder is the output buffer). Response.value =
   * bytes written. Truncates silently if the target exceeds buf_cap
   * (POSIX readlink(2) semantics — the caller distinguishes exact-
   * fit from truncated by re-issuing with a larger buffer when
   * value == buf_cap). Threads through Vfs::readlink →
   * Filesystem::readlink; tmpfs's TmpNode::SymLink variant yields
   * the target bytes; non-symlink targets → EINVAL; devfs / procfs /
   * opfs inherit the trait default (NotSupported → ENOTSUP). */
  PATH_READLINK: 0x0045,
  /** Wire-format identity for `fd_prestat_dir_name`. Preopen-name
   * companion to fd_prestat_get. Wire: args[0..4] = fd (ignored).
   * v1 has no preopens so the honest answer for every fd is EBADF;
   * post-slice both fd_prestat_get and fd_prestat_dir_name agree on
   * EBADF so userland's libc-style preopen-discovery loops terminate
   * cleanly (pre-slice dir_name returned ENOSYS, which broke the
   * loop). */
  FD_PRESTAT_DIR_NAME: 0x002c,
  /** Unused by the WASI shim today; the tests probe it to verify
   * the dispatcher's `ENOSYS` path still fires for opcodes the
   * kernel doesn't yet handle. Was `FD_PRESTAT_DIR_NAME` before
   * that handler landed; swap to whichever WASI opcode is still
   * unhandled as the implementation catches up. WASI's rights
   * system is un-v1-relevant so FD_FDSTAT_SET_RIGHTS is a reasonable
   * long-term probe target. */
  FD_FDSTAT_SET_RIGHTS: 0x0026,
  /** Wire-format identity for `fd_readdir`. Directory-listing
   * opcode. args[0..4] = fd (u32); args[4..12] = cookie (u64
   * LE; 0 = start from beginning); heap = caller's output buffer
   * with capacity heap_len bytes. Kernel writes 24-byte dirent_t
   * records + inline name bytes into the buffer until it fills
   * or entries exhaust. value / extraLen = bytes written. Entries
   * pack back-to-back with no padding; a buffer that fills mid-
   * entry signals "more may exist" by returning value == heap_len
   * and the caller re-issues with the last d_next as the cookie. */
  FD_READDIR: 0x002f,
  /** Wire-format identity for `poll_oneoff`. The shim packs
   * `(n_subs, n_events_cap)` into the inline args window (u32 each
   * at offsets 0 / 4) and puts the subscription list followed by
   * an events output window in the heap — subs at [0..n_subs*48],
   * events at [n_subs*48..n_subs*48 + n_events_cap*32]. The kernel
   * returns the actual event count in `response.value` and echoes
   * it in `response.extraLen`. v1 is non-blocking: CLOCK fires only
   * if the target time is already past; FD_READ / FD_WRITE fire
   * only if the op would make progress right now. */
  POLL_ONEOFF: 0x0050,
} as const;

/** PMos extension opcodes (0x1000..0x1501). */
export const OP_EXT = {
  IPC_SOCKET: 0x1000,
  IPC_BIND: 0x1001,
  IPC_LISTEN: 0x1002,
  IPC_CONNECT: 0x1003,
  IPC_ACCEPT: 0x1004,
  /** Wire-format identity for `ipc_pipe`. Create a pipe pair: the
   * kernel allocates two fds on the caller — a PipeRead at
   * heap[0..4] and a PipeWrite at heap[4..8] — and returns success
   * with extraLen = 8. No inline args; heap_len must be >= 8 or the
   * dispatcher rejects with EINVAL before allocating any fds. On a
   * failed second-fd alloc mid-call, the kernel rolls back the
   * first install so the fd table never holds a half-installed
   * pair. After a successful call: bytes written to the write fd
   * are readable via the read fd through the existing fd_read /
   * fd_write arms (landed in fbddb91); closing either end
   * propagates to the other (reader closed → subsequent writes
   * EPIPE; writer closed → subsequent reads see (0, []) EOF). */
  IPC_PIPE: 0x1007,
  PROC_SPAWN: 0x1100,
  PROC_SELF: 0x1103,
  PROC_PARENT: 0x1104,
  PROC_WAIT: 0x1101,
  DISPLAY_CONNECT: 0x1200,
  DISPLAY_BIND: 0x1201,
  CAP_CHECK: 0x1300,
  CAP_LIST: 0x1301,
} as const;

// ---- Errno constants -------------------------------------------------
//
// Positive values. A response's `status` field carries the *negated*
// form (`-errno`), so a test asserts `response.status === -ERRNO.EBADF`.

export const ERRNO = {
  EAGAIN: 6,
  EBADF: 8,
  ECONNREFUSED: 14,
  EEXIST: 20,
  EINVAL: 28,
  EISDIR: 31,
  /** Too many levels of symbolic links. Returned from path
   * resolution when a symlink chain exceeds SYMLOOP_MAX (40).
   * Mirrors `abi::errno::ELOOP`. */
  ELOOP: 32,
  ENOENT: 44,
  ENOSYS: 52,
  ENOTDIR: 54,
  ENOTEMPTY: 55,
  ENOTSUP: 58,
  /** Broken pipe / socket. Returned by send / write when the
   * peer has fully closed, the local write side has been shut
   * down via SOCK_SHUTDOWN, or the peer has shut down its read
   * side. Maps to `abi::errno::EPIPE`. */
  EPIPE: 64,
  EROFS: 69,
} as const;

// ---- WASI clock identifiers -----------------------------------------
//
// Mirror of `abi::wasi::CLOCKID_*`. Passed as the u32 at args[0..4] on
// `CLOCK_TIME_GET` + `CLOCK_RES_GET` requests. REALTIME + MONOTONIC are
// supported in v1; PROCESS_CPUTIME + THREAD_CPUTIME return -ENOTSUP on
// both opcodes.

export const CLOCKID = {
  REALTIME: 0,
  MONOTONIC: 1,
  PROCESS_CPUTIME_ID: 2,
  THREAD_CPUTIME_ID: 3,
} as const;

// ---- WASI seek whence -----------------------------------------------
//
// Mirror of `abi::wasi::Whence`. Selects the reference point for an
// `FD_SEEK` opcode's `offset` argument: SET = absolute; CUR = relative
// to the current position (CUR with offset 0 is the `fd_tell` idiom);
// END = relative to file size (typically with a negative offset to
// land inside the file).

export const WHENCE = {
  SET: 0,
  CUR: 1,
  END: 2,
} as const;

// ---- WASI fstflags bitfield -----------------------------------------
//
// Mirror of `abi::wasi::fstflags`. Bit-mask OR'd into the fstflags
// argument of `path_filestat_set_times` / `fd_filestat_set_times`.
// Within each pair (ATIM / ATIM_NOW, MTIM / MTIM_NOW) only one bit
// may be set at a time — setting both is EINVAL.

export const FSTFLAGS = {
  SET_ATIM: 0x1,
  SET_ATIM_NOW: 0x2,
  SET_MTIM: 0x4,
  SET_MTIM_NOW: 0x8,
} as const;

// ---- WASI fdflags bitfield ------------------------------------------
//
// Mirror of `abi::wasi::fdflags`. Bit-mask OR'd into the new_fdflags
// argument of `fd_fdstat_set_flags`. These bit values DIFFER from
// PMos's internal `kernel::fd::FdFlags` (APPEND=0x01 vs CLOEXEC=0x01;
// NONBLOCK=0x04 vs APPEND=0x04 on the internal side) — the kernel
// translates on decode, so TS callers always pass WASI bit values.
// v1 recognises only NONBLOCK + APPEND meaningfully; DSYNC / RSYNC /
// SYNC are accepted and discarded (tmpfs writes are already
// synchronous into in-memory state).

export const FDFLAGS = {
  APPEND: 0x1,
  DSYNC: 0x2,
  NONBLOCK: 0x4,
  RSYNC: 0x8,
  SYNC: 0x10,
} as const;

// ---- WASI sdflags bitfield ------------------------------------------
//
// Mirror of `abi::wasi::sdflags`. Bit-mask OR'd into the `how`
// argument of `sock_shutdown`. v1 accepts only RD|WR (full close)
// and rejects the half-close combinations with ENOTSUP; zero how
// or bits beyond RD|WR reject with EINVAL.

export const SDFLAGS = {
  RD: 0x1,
  WR: 0x2,
} as const;

// ---- WASI filetype bytes --------------------------------------------
//
// Mirror of `abi::wasi::filetype::*`. First byte of the 24-byte
// `fdstat_t` and of the 64-byte `filestat_t` (at its byte-16 offset).
// WASI has no FIFO filetype; PMos maps FIFO vnodes to UNKNOWN.

export const FILETYPE = {
  UNKNOWN: 0,
  BLOCK_DEVICE: 1,
  CHARACTER_DEVICE: 2,
  DIRECTORY: 3,
  REGULAR_FILE: 4,
  SOCKET_DGRAM: 5,
  SOCKET_STREAM: 6,
  SYMBOLIC_LINK: 7,
} as const;

// ---- WASI fd_readdir dirent wire layout ----------------------------
//
// Mirror of `abi::wasi::dirent`. Each entry in an fd_readdir output
// buffer is a 24-byte dirent_t header followed immediately by
// d_namlen bytes of UTF-8 name — no inter-entry padding.

/** Size of one dirent_t header (name bytes follow immediately). */
export const POLL_DIRENT_HEADER_SIZE = 24;

/** Dirent_t header field offsets. */
export const DIRENT_OFF = {
  D_NEXT:   0,  // u64 — resumption cookie after this entry
  D_INO:    8,  // u64
  D_NAMLEN: 16, // u32
  D_TYPE:   20, // u8 (filetype)
} as const;

// ---- WASI poll_oneoff wire layout + flag tables --------------------
//
// Mirror of `abi::wasi::poll`, `abi::wasi::eventtype`,
// `abi::wasi::subclockflags`, and `abi::wasi::eventrwflags`. The
// subscription wire format is 48 bytes; the event wire format is 32
// bytes. Both are laid out in ascending offset order inside the heap
// region `poll_oneoff` hands the kernel — subs at the start, events
// at heap[n_subs*48..].

/** Size of one subscription slot in the `poll_oneoff` input heap. */
export const POLL_SUBSCRIPTION_SIZE = 48;
/** Size of one event slot in the `poll_oneoff` output heap. */
export const POLL_EVENT_SIZE = 32;

/** Subscription wire-offset table (48-byte record). */
export const POLL_SUB_OFF = {
  USERDATA: 0,
  TAG: 8,
  /** CLOCK payload offsets (only meaningful when TAG == EVENTTYPE.CLOCK). */
  CLOCK_ID: 16,
  CLOCK_TIMEOUT: 24,
  CLOCK_PRECISION: 32,
  CLOCK_FLAGS: 40,
  /** FD_READ / FD_WRITE payload offset. */
  FDRW_FD: 16,
} as const;

/** Event wire-offset table (32-byte record). */
export const POLL_EVENT_OFF = {
  USERDATA: 0,
  ERROR: 8,
  TYPE: 10,
  RW_NBYTES: 16,
  RW_FLAGS: 24,
} as const;

/** WASI `eventtype_t` — identifies a subscription / event variant. */
export const EVENTTYPE = {
  CLOCK: 0,
  FD_READ: 1,
  FD_WRITE: 2,
} as const;

/** WASI `subclockflags_t` (applies to CLOCK subscriptions). */
export const SUBCLOCKFLAGS = {
  /** When set, `timeout` is an absolute time; when clear it is relative
   * to the instant the subscription is posted. */
  ABSTIME: 0x1,
} as const;

/** WASI `eventrwflags_t` (applies to FD_READ / FD_WRITE events). */
export const EVENTRWFLAGS = {
  /** Peer end of the fd has hung up (reader closed for a write fd,
   * writer closed for a read fd). Semantically "readable at EOF" for
   * FD_READ, "writing will produce SIGPIPE" for FD_WRITE. */
  FD_READWRITE_HANGUP: 0x1,
} as const;

// ---- Device identifiers ---------------------------------------------
//
// Mirror of `kernel::platform::DevId`. Used in the `pmos_host_driver_call`
// host import to route by device.

export const DEV = {
  FRAMEBUFFER: 0,
  INPUT_KBD: 1,
  INPUT_MOUSE: 2,
  BLOCK: 3,
  NET: 4,
  CONSOLE: 5,
} as const;

// ---- Capability constants -------------------------------------------
//
// Mirror of `abi::cap::Cap`. The u64 bit for a cap is `1 << (cap as u32)`.

export const CAP = {
  DISPLAY_CLIENT: 1,
  DISPLAY_SERVER: 2,
  SHELL: 3,
  PROC_ENUMERATE: 4,
  PROC_KILL_ANY: 5,
  NET: 6,
  MOUNT: 7,
  CAP_GRANT: 8,
  DEV_BLOCK: 9,
  KEYMAP_ADMIN: 10,
} as const;

/** Bit for a cap, as a bigint (matches `CapSet::0` layout). */
export function capBit(cap: number): bigint {
  return 1n << BigInt(cap);
}

// ---- proc_spawn manifest encoding -----------------------------------
//
// Mirror of the [`abi::ext::SpawnManifest`] Rust type + the args /
// heap layout `crates/kernel/src/syscall/ext.rs`'s `handle_proc_spawn`
// parses. The current wire format is minimal: path string + caps
// bitset, with stdio inherited implicitly from the parent's fd table.
// Richer fields (argv, envp, cwd, extra fd dups) will be appended to
// the heap payload in future slices; this helper will grow
// correspondingly.

/** Arguments to a `PROC_SPAWN` syscall (first-landing shape). */
export interface SpawnManifest {
  /** Absolute path of the binary to spawn. */
  readonly path: string;
  /** Capability bitset the child should hold. Must be a subset of the caller's own caps. */
  readonly caps: bigint;
}

/**
 * Build the `(args, heap)` pair for a [`PROC_SPAWN`] syscall from a
 * typed manifest. `args` goes in `SyscallRequest.args` (or pack
 * path_len into `arg0` if preferred), and `heap` goes at the offset
 * `SyscallRequest.heapPtr` points at.
 */
export function encodeSpawnManifest(
  manifest: SpawnManifest,
): { args: Uint8Array; heap: Uint8Array } {
  const path = new TextEncoder().encode(manifest.path);
  const args = new Uint8Array(16);
  const view = new DataView(args.buffer);
  // args[0..4] = path_len
  view.setUint32(0, path.length, true);
  // args[4..12] = caps bitset (u64 LE)
  view.setBigUint64(4, manifest.caps, true);
  // args[12..16] = reserved (zero)
  return { args, heap: path };
}

/** `abi::cap::CapSet::ALL` — every bit set. */
export const CAPSET_ALL = 0xffff_ffff_ffff_ffffn;

/** `abi::cap::initial::DESKTOP_SHELL` — DisplayClient + Shell + ProcEnumerate + KeymapAdmin. */
export const CAPSET_DESKTOP_SHELL =
  capBit(CAP.DISPLAY_CLIENT) |
  capBit(CAP.SHELL) |
  capBit(CAP.PROC_ENUMERATE) |
  capBit(CAP.KEYMAP_ADMIN);

/** `abi::cap::initial::ORDINARY_APP` — just DisplayClient. */
export const CAPSET_ORDINARY_APP = capBit(CAP.DISPLAY_CLIENT);
