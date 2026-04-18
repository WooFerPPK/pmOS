//! WASI preview 1 opcode handlers.
//!
//! Each handler in here decodes a [`Request`]'s inline args and
//! heap-scratch payload, calls one method on [`crate::sys::Kernel`],
//! and encodes a [`Response`]. The `Kernel` method is always the
//! semantic truth — these handlers are adapters, not logic.
//!
//! ## Argument layouts
//!
//! The 16-byte `req.args` inline region is read per-opcode. A
//! handler that needs more data reads it out of the caller's heap
//! scratch region via `heap_in` / `heap_out_mut`. Each opcode's
//! layout is documented at the top of its handler in terms that
//! match `contracts/syscalls.md §2`.
//!
//! ## Not-yet-implemented opcodes
//!
//! Every WASI opcode that doesn't have a handler below falls
//! through the `_ =>` arm and returns `ENOSYS` with the request_id
//! echoed. Expansion is purely additive — adding `CLOCK_TIME_GET`,
//! `RANDOM_GET`, etc. is one `match` arm + one handler function +
//! isolation tests.

use abi::errno::{EBADF, EINVAL, ENOSYS, ENOTSUP};
use abi::ext::Pid;
use abi::ring::{Request, Response};
use abi::wasi as op;
use abi::wasi::filestat as fs_off;

use crate::fd::{FdFlags, FdObject};
use crate::platform;
use crate::proc::ExitStatus;
use crate::sys::{Kernel, KernelError};
use crate::vfs::NodeType;

use super::dispatch::{args_u32, args_u64, heap_in, heap_out_mut, kerr_to_errno};

/// WASI lookupflags bit that requests symlink dereferencing on the
/// final path component. Intermediate components always follow
/// symlinks in [`Vfs::resolve`]; this flag only governs the last
/// component. Unset → lstat-like (don't follow final symlink).
const LOOKUP_SYMLINK_FOLLOW: u32 = 0x1;

/// Dispatch a request whose opcode is in the WASI preview 1 range.
/// The caller has already guarded with [`abi::wasi::is_wasi`].
pub fn dispatch_wasi(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> Response {
    match req.opcode {
        op::FD_WRITE => handle_fd_write(kernel, pid, req, heap),
        op::FD_READ => handle_fd_read(kernel, pid, req, heap),
        op::FD_SEEK => handle_fd_seek(kernel, pid, req),
        op::FD_TELL => handle_fd_tell(kernel, pid, req),
        op::FD_ADVISE => handle_fd_advise(kernel, pid, req),
        op::FD_ALLOCATE => handle_fd_allocate(kernel, pid, req),
        op::FD_SYNC => handle_fd_sync(kernel, pid, req),
        op::FD_DATASYNC => handle_fd_datasync(kernel, pid, req),
        op::FD_CLOSE => handle_fd_close(kernel, pid, req),
        op::PATH_OPEN => handle_path_open(kernel, pid, req, heap),
        op::PROC_EXIT => handle_proc_exit(kernel, pid, req),
        op::CLOCK_TIME_GET => handle_clock_time_get(req),
        op::CLOCK_RES_GET => handle_clock_res_get(req),
        op::RANDOM_GET => handle_random_get(req, heap),
        op::SCHED_YIELD => handle_sched_yield(req),
        op::ARGS_SIZES_GET => handle_args_sizes_get(req, heap),
        op::ARGS_GET => handle_args_get(req),
        op::ENVIRON_SIZES_GET => handle_environ_sizes_get(req, heap),
        op::ENVIRON_GET => handle_environ_get(req),
        op::FD_FDSTAT_GET => handle_fd_fdstat_get(kernel, pid, req, heap),
        op::FD_FILESTAT_GET => handle_fd_filestat_get(kernel, pid, req, heap),
        op::PATH_FILESTAT_GET => handle_path_filestat_get(kernel, pid, req, heap),
        op::PATH_FILESTAT_SET_TIMES => handle_path_filestat_set_times(kernel, pid, req, heap),
        op::FD_FILESTAT_SET_TIMES => handle_fd_filestat_set_times(kernel, pid, req, heap),
        op::FD_RENUMBER => handle_fd_renumber(kernel, pid, req),
        op::PATH_UNLINK_FILE => handle_path_unlink_file(kernel, pid, req, heap),
        op::PATH_RENAME => handle_path_rename(kernel, pid, req, heap),
        op::PATH_LINK => handle_path_link(kernel, pid, req, heap),
        op::PATH_SYMLINK => handle_path_symlink(kernel, pid, req, heap),
        op::PATH_READLINK => handle_path_readlink(kernel, pid, req, heap),
        op::PATH_CREATE_DIRECTORY => handle_path_create_directory(kernel, pid, req, heap),
        op::PATH_REMOVE_DIRECTORY => handle_path_remove_directory(kernel, pid, req, heap),
        op::FD_FDSTAT_SET_FLAGS => handle_fd_fdstat_set_flags(kernel, pid, req),
        op::FD_FILESTAT_SET_SIZE => handle_fd_filestat_set_size(kernel, pid, req),
        op::FD_PREAD => handle_fd_pread(kernel, pid, req, heap),
        op::FD_PWRITE => handle_fd_pwrite(kernel, pid, req, heap),
        op::SOCK_SEND => handle_sock_send(kernel, pid, req, heap),
        op::SOCK_RECV => handle_sock_recv(kernel, pid, req, heap),
        op::SOCK_ACCEPT => handle_sock_accept(kernel, pid, req),
        op::SOCK_SHUTDOWN => handle_sock_shutdown(kernel, pid, req),
        op::FD_READDIR => handle_fd_readdir(kernel, pid, req, heap),
        op::POLL_ONEOFF => handle_poll_oneoff(kernel, pid, req, heap),
        op::FD_PRESTAT_GET => handle_fd_prestat_get(req),
        op::FD_PRESTAT_DIR_NAME => handle_fd_prestat_dir_name(req),
        _ => Response::err(req.request_id, ENOSYS),
    }
}

// ---- fd_write ----------------------------------------------------------
//
// Layout:
//   args[0..4]  = fd (u32)
//   heap_ptr    = start of bytes to write (in caller's heap scratch)
//   heap_len    = number of bytes to write
// Response:
//   value       = bytes actually written (always == heap_len on
//                 success for the destinations in v1; the contract
//                 leaves room for short writes when a pipe / socket
//                 partial-consumes)

fn handle_fd_write(kernel: &mut Kernel, pid: Pid, req: &Request, heap: &[u8]) -> Response {
    let fd = args_u32(req, 0);
    let Some(bytes) = heap_in(req, heap) else {
        return Response::err(req.request_id, EINVAL);
    };
    match kernel.fd_write(pid, fd, bytes) {
        Ok(n) => Response::ok(req.request_id, n as i64),
        Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
    }
}

// ---- fd_read -----------------------------------------------------------
//
// Layout:
//   args[0..4]  = fd (u32)
//   heap_ptr    = start of the destination window in caller's heap
//                 scratch
//   heap_len    = MAX bytes to read (buffer capacity, not desired
//                 byte count — the kernel returns whatever is
//                 available up to this limit)
// Response:
//   value       = bytes actually read (0 on EOF)
//   extra_len   = bytes actually read, echoed into the response
//                 header so the userland side can tell how much
//                 of the heap window was populated without
//                 looking at `value` first

fn handle_fd_read(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> Response {
    let fd = args_u32(req, 0);
    let max_len = req.heap_len as usize;
    let Some(buf) = heap_out_mut(req, heap, max_len) else {
        return Response::err(req.request_id, EINVAL);
    };
    match kernel.fd_read(pid, fd, buf) {
        Ok(n) => Response {
            request_id: req.request_id,
            status: 0,
            value: n as i64,
            extra_len: n as u32,
            _pad: [0u8; 12],
        },
        Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
    }
}

// ---- fd_seek -----------------------------------------------------------
//
// Layout:
//   args[0..4]  = fd (u32)
//   args[4..8]  = whence (u32; only the low byte is meaningful, mapping
//                 to abi::wasi::Whence: Set=0, Cur=1, End=2)
//   args[8..16] = offset (i64; bit pattern read as u64 then cast)
// Response:
//   value       = new absolute offset (u64 widened to i64; bit-exact)
//
// WASI's seek combines three normally-distinct file-position operations
// into a single opcode whose meaning depends on `whence`:
//
//   * Set → new position is exactly `offset` (offset must be >= 0;
//           a negative SeekSet is meaningless and returns EINVAL).
//   * Cur → new position is `current + offset`; both directions are
//           legal as long as the result doesn't underflow past 0
//           (a Cur seek that would land below 0 returns EINVAL). The
//           idiomatic `fd_tell` fold lives here as `Cur 0` — no
//           change to the offset, just read it back.
//   * End → new position is `file_size + offset`; offset is typically
//           negative (seek-into-file from the end). EINVAL on
//           underflow same as Cur.
//
// Per-fd, not per-vnode: two fds pointing at the same vnode have
// independent positions. This matches POSIX + WASI semantics. v1
// rejects fd_seek on every non-Vnode FdObject with EINVAL — whence
// has no meaning on a char device, socket, pipe, or signal channel.
// Seeking past EOF on a regular file is allowed (POSIX/WASI both
// permit it; v1's tmpfs read returns 0 bytes from such an offset and
// a write would extend the file).
//
// The new offset travels back in `response.value` rather than via a
// heap write, mirroring `clock_time_get`'s shape — both opcodes
// compute one u64 and the shim writes it straight to a user pointer
// via `setBigInt64`. No heap round-trip needed.

fn handle_fd_seek(kernel: &mut Kernel, pid: Pid, req: &Request) -> Response {
    let fd = args_u32(req, 0);
    let whence_byte = (args_u32(req, 4) & 0xff) as u8;
    let offset = args_u64(req, 8) as i64;

    let entry = match kernel.fds(pid) {
        Ok(t) => match t.get(fd) {
            Some(e) => *e,
            None => return Response::err(req.request_id, EBADF),
        },
        Err(e) => return Response::err(req.request_id, kerr_to_errno(e)),
    };
    let (mount_id, ino) = match entry.object {
        FdObject::Vnode { mount_id, ino } => (mount_id, ino),
        _ => return Response::err(req.request_id, EINVAL),
    };

    // Pattern-matching on the u8 against the enum's discriminants
    // keeps the abi::wasi::Whence variants as the single source of
    // truth without an unsafe transmute. A future variant added to
    // the enum needs a matching arm here, which is what we want.
    let new_offset = match whence_byte {
        x if x == abi::wasi::Whence::Set as u8 => {
            if offset < 0 {
                return Response::err(req.request_id, EINVAL);
            }
            offset as u64
        }
        x if x == abi::wasi::Whence::Cur as u8 => {
            match entry.offset.checked_add_signed(offset) {
                Some(n) => n,
                None => return Response::err(req.request_id, EINVAL),
            }
        }
        x if x == abi::wasi::Whence::End as u8 => {
            let st = match kernel.vfs.stat_ino(mount_id, ino) {
                Ok(s) => s,
                Err(e) => {
                    return Response::err(
                        req.request_id,
                        kerr_to_errno(KernelError::Fs(e)),
                    )
                }
            };
            match st.size.checked_add_signed(offset) {
                Some(n) => n,
                None => return Response::err(req.request_id, EINVAL),
            }
        }
        _ => return Response::err(req.request_id, EINVAL),
    };

    // The fd was present a moment ago; only EBADF can fire here, and
    // only if a concurrent close raced — which the single-threaded
    // dispatcher rules out. The mapping is honest regardless.
    let table = match kernel.fds_mut(pid) {
        Ok(t) => t,
        Err(e) => return Response::err(req.request_id, kerr_to_errno(e)),
    };
    if table.set_offset(fd, new_offset).is_err() {
        return Response::err(req.request_id, EBADF);
    }
    Response::ok(req.request_id, new_offset as i64)
}

// ---- fd_tell -----------------------------------------------------------
//
// Layout:
//   args[0..4] = fd (u32)
// Response:
//   value      = current absolute offset (u64 widened to i64; bit-exact)
//
// The read-only sibling of `fd_seek`: report a seekable fd's current
// position without mutating it. Functionally equivalent to
// `fd_seek(fd, 0, Cur, *)` at the WASI-surface level — the value the
// kernel returns is identical in both paths because both read
// `FdEntry.offset` — but fd_tell is its own opcode (0x0033) because
// it's strictly cheaper: no whence byte to decode, no checked signed
// arithmetic, no `Vfs::stat_ino` call for the End whence branch.
// WASI libc lowers `ftell()` through this opcode.
//
// Shares `fd_seek`'s EBADF + non-Vnode-EINVAL guards: a char device
// / socket / pipe has no meaningful position, so fd_tell on a
// non-Vnode FdObject is EINVAL just like fd_seek. No heap round-trip
// — the u64 offset travels back in `response.value`, mirroring
// `clock_time_get` + `fd_seek`'s response shape.

fn handle_fd_tell(kernel: &mut Kernel, pid: Pid, req: &Request) -> Response {
    let fd = args_u32(req, 0);
    let entry = match kernel.fds(pid) {
        Ok(t) => match t.get(fd) {
            Some(e) => *e,
            None => return Response::err(req.request_id, EBADF),
        },
        Err(e) => return Response::err(req.request_id, kerr_to_errno(e)),
    };
    match entry.object {
        FdObject::Vnode { .. } => Response::ok(req.request_id, entry.offset as i64),
        _ => Response::err(req.request_id, EINVAL),
    }
}

// ---- fd-state opcodes (fd_advise / fd_allocate / fd_sync / fd_datasync) ----
//
// Four "fd-state" opcodes that collapse to trivial semantics in v1's
// tmpfs-backed VFS. All four share the same guards — EBADF on an
// unopened fd; EINVAL on every non-Vnode FdObject (the state these
// opcodes touch only has meaning for seekable regular files).
//
//   fd_advise   = no-op success (the advice is taken, then discarded;
//                 POSIX and WASI both permit this, the opcode is a
//                 hint with no reserved behaviour).
//   fd_sync     = no-op success (nothing to flush; v1 writes are
//                 synchronous into the vfs state).
//   fd_datasync = no-op success (same reason).
//   fd_allocate = ENOTSUP (v1 tmpfs has no preallocation primitive;
//                 a success response would lie about reserved space,
//                 ENOTSUP is the honest answer).
//
// Handlers mirror `handle_fd_tell`'s shape verbatim — one fd read,
// one table lookup, one match on the FdObject variant. Offset bytes,
// length bytes, and the advice byte in the WASI signature are all
// ignored in v1: the semantics collapse regardless of their values.

fn handle_fd_advise(kernel: &mut Kernel, pid: Pid, req: &Request) -> Response {
    let fd = args_u32(req, 0);
    let entry = match kernel.fds(pid) {
        Ok(t) => match t.get(fd) {
            Some(e) => *e,
            None => return Response::err(req.request_id, EBADF),
        },
        Err(e) => return Response::err(req.request_id, kerr_to_errno(e)),
    };
    match entry.object {
        FdObject::Vnode { .. } => Response::ok(req.request_id, 0),
        _ => Response::err(req.request_id, EINVAL),
    }
}

fn handle_fd_allocate(kernel: &mut Kernel, pid: Pid, req: &Request) -> Response {
    let fd = args_u32(req, 0);
    let entry = match kernel.fds(pid) {
        Ok(t) => match t.get(fd) {
            Some(e) => *e,
            None => return Response::err(req.request_id, EBADF),
        },
        Err(e) => return Response::err(req.request_id, kerr_to_errno(e)),
    };
    match entry.object {
        FdObject::Vnode { .. } => Response::err(req.request_id, ENOTSUP),
        _ => Response::err(req.request_id, EINVAL),
    }
}

fn handle_fd_sync(kernel: &mut Kernel, pid: Pid, req: &Request) -> Response {
    let fd = args_u32(req, 0);
    let entry = match kernel.fds(pid) {
        Ok(t) => match t.get(fd) {
            Some(e) => *e,
            None => return Response::err(req.request_id, EBADF),
        },
        Err(e) => return Response::err(req.request_id, kerr_to_errno(e)),
    };
    match entry.object {
        FdObject::Vnode { .. } => Response::ok(req.request_id, 0),
        _ => Response::err(req.request_id, EINVAL),
    }
}

fn handle_fd_datasync(kernel: &mut Kernel, pid: Pid, req: &Request) -> Response {
    let fd = args_u32(req, 0);
    let entry = match kernel.fds(pid) {
        Ok(t) => match t.get(fd) {
            Some(e) => *e,
            None => return Response::err(req.request_id, EBADF),
        },
        Err(e) => return Response::err(req.request_id, kerr_to_errno(e)),
    };
    match entry.object {
        FdObject::Vnode { .. } => Response::ok(req.request_id, 0),
        _ => Response::err(req.request_id, EINVAL),
    }
}

// ---- fd_renumber -----------------------------------------------------
//
// Layout:
//   args[0..4] = from (u32)
//   args[4..8] = to   (u32)
// Response:
//   value = 0 on success; status = -errno on error.
//
// WASI's dup2-spelling: atomically move `from` to `to`, closing
// whatever was at `to` first. Semantics (matching wasmtime's
// reading of WASI preview 1):
//
//   * from == to on an open fd: no-op success. The fd stays open,
//     entry unchanged. Mirrors POSIX's dup2(fd, fd) = fd.
//   * from == to on an unopened fd: EBADF. Mirrors POSIX's
//     dup2(bad, bad) = EBADF.
//   * from != to and from is open: `from` is closed, `to` is
//     replaced with `from`'s entry (offset + flags preserved
//     verbatim). If `to` was open pre-call, its entry's object
//     is released via `release_object` — important for pipe /
//     socket ref counts.
//   * from != to and from is not open: EBADF, `to` untouched.
//
// The move is a single-process operation — no fs, no ipc, no
// platform calls. The only reason the kernel wrapper exists (rather
// than calling FdTable::renumber directly from the handler) is the
// release_object path for the prior `to` entry.

fn handle_fd_renumber(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
) -> Response {
    let from = args_u32(req, 0);
    let to = args_u32(req, 4);
    match kernel.fd_renumber(pid, from, to) {
        Ok(()) => Response::ok(req.request_id, 0),
        Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
    }
}

// ---- fd_close ----------------------------------------------------------
//
// Layout:
//   args[0..4] = fd (u32)
// Response:
//   value      = 0

fn handle_fd_close(kernel: &mut Kernel, pid: Pid, req: &Request) -> Response {
    let fd = args_u32(req, 0);
    match kernel.fd_close(pid, fd) {
        Ok(()) => Response::ok(req.request_id, 0),
        Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
    }
}

// ---- path_open ---------------------------------------------------------
//
// Layout:
//   args[0..4]  = fd flags (u32, in WASI `fdflags` encoding —
//                 APPEND=0x01, DSYNC=0x02, NONBLOCK=0x04,
//                 RSYNC=0x08, SYNC=0x10)
//   heap_ptr    = start of UTF-8 path
//   heap_len    = path length in bytes
// Response:
//   value       = freshly-allocated fd number (u32 widened to i64)
//
// The fdflags u32 is translated through `FdFlags::from_wasi_bits`
// into PMos's internal encoding (which differs — PMos CLOEXEC=0x01,
// NONBLOCK=0x02, APPEND=0x04). Prior to that helper landing, this
// handler passed the WASI bits directly to `FdFlags::from_bits`,
// which silently mis-mapped APPEND → CLOEXEC (dropping the fd on
// proc_spawn), NONBLOCK → APPEND (mis-routing writes to EOF), etc.
// DSYNC / RSYNC / SYNC are accepted and discarded since tmpfs is
// synchronous; the helper ignores them.
//
// oflags (CREAT/TRUNC/DIRECTORY/EXCL) are not yet wired on the
// Kernel side (`Kernel::path_open` takes only `FdFlags`), so the
// dispatcher doesn't decode them either. When the path-resolution
// layer grows oflag support, this handler gains one more u32 at
// args[4..8] without a wire-format break.

fn handle_path_open(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
    heap: &[u8],
) -> Response {
    let wasi_bits = args_u32(req, 0);
    let flags = FdFlags::from_wasi_bits(wasi_bits);
    let Some(path_bytes) = heap_in(req, heap) else {
        return Response::err(req.request_id, EINVAL);
    };
    let Ok(path) = core::str::from_utf8(path_bytes) else {
        return Response::err(req.request_id, EINVAL);
    };
    match kernel.path_open(pid, path, flags) {
        Ok(fd) => Response::ok(req.request_id, fd as i64),
        Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
    }
}

// ---- proc_exit ---------------------------------------------------------
//
// Layout:
//   args[0..4] = exit code (u32, interpreted as i32)
// Response:
//   value      = 0
//
// WASI's contract says proc_exit never returns to the caller. PMos
// can't quite honour that from inside a shared dispatcher — the
// syscall runs to completion on the kernel side and the process
// becomes a zombie, but we still post a response so the ring
// transport stays consistent (the caller Worker is torn down by
// the platform layer after the zombie transition; it won't see
// the response). Userland never polls for this response.

fn handle_proc_exit(kernel: &mut Kernel, pid: Pid, req: &Request) -> Response {
    let exit_code = args_u32(req, 0) as i32;
    let status = ExitStatus::Exited(exit_code);
    match kernel.proc_exit(pid, status) {
        Ok(()) => Response::ok(req.request_id, 0),
        Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
    }
}

// ---- clock_time_get ---------------------------------------------------
//
// Layout:
//   args[0..4] = clock_id (u32)
//   args[4..12] = precision (u64, ignored in v1 — the Platform
//                 clock has nanosecond granularity already)
// Response:
//   value       = nanoseconds (u64 widened to i64;
//                 `i64::from_ne_bytes(u64::to_ne_bytes(...))`
//                 to preserve every bit, which userland reads
//                 back as u64)
//
// WASI defines four clock IDs: realtime (0), monotonic (1),
// process cputime (2), thread cputime (3).
//
//   * CLOCKID_MONOTONIC → `Platform::now_ns()`, the kernel's
//     strictly-increasing clock. Userland `Instant::now()`
//     ultimately lands here.
//   * CLOCKID_REALTIME  → `Platform::now_realtime_ns()`, the
//     wall-clock-ns-since-Unix-epoch. Userland
//     `SystemTime::now()` lands here.
//   * CLOCKID_PROCESS_CPUTIME_ID / CLOCKID_THREAD_CPUTIME_ID →
//     ENOTSUP. The v1 kernel doesn't model per-process CPU
//     accounting. Returning ENOTSUP (rather than a fake value
//     or routing to the monotonic clock) is the honest answer;
//     a libc that probes for cpu-time support sees a clean "no"
//     and falls back to monotonic.
//   * Anything else → EINVAL.
//
// Time, unlike every other syscall, runs forward even across
// panics + reboots. That matters because userland's HashMap
// seeding uses `clock_time_get` to get entropy — if the kernel
// ever returns a constant, HashMap-based protocols degrade
// predictably. `Platform::now_ns` is documented to be strictly
// increasing, which is strong enough; `Platform::now_realtime_ns`
// is not required to be monotonic but is required to be
// wall-clock-sourced (so it reliably seeds entropy even though
// it may step backwards across an NTP adjustment).

fn handle_clock_time_get(req: &Request) -> Response {
    let clock_id = args_u32(req, 0);
    // `Platform::now_*()` return u64; cast to i64 preserves every
    // bit (u64::MAX becomes -1). Userland bigint code reinterprets
    // the bits back to u64.
    let ns = match clock_id {
        abi::wasi::CLOCKID_MONOTONIC => platform::current().now_ns(),
        abi::wasi::CLOCKID_REALTIME => platform::current().now_realtime_ns(),
        abi::wasi::CLOCKID_PROCESS_CPUTIME_ID | abi::wasi::CLOCKID_THREAD_CPUTIME_ID => {
            return Response::err(req.request_id, ENOTSUP);
        }
        _ => return Response::err(req.request_id, EINVAL),
    };
    Response::ok(req.request_id, ns as i64)
}

// ---- clock_res_get ----------------------------------------------------
//
// Layout:
//   args[0..4]  = clock_id (u32)
// Response:
//   value       = resolution in nanoseconds (u64 widened to i64)
//
// The precision-query sibling to `clock_time_get`. Given a clock
// id, report the finest tick the clock can resolve. PMos's
// `Platform::now_ns` + `Platform::now_realtime_ns` both sit on top
// of nanosecond-granular sources (native: `Instant::now()` +
// `SystemTime::now()`; wasm host: `performance.now()` * 1_000_000
// + `Date.now()` * 1_000_000), so 1 ns is the honest answer for
// both supported clocks.
//
//   * CLOCKID_MONOTONIC / CLOCKID_REALTIME → 1 ns.
//   * CLOCKID_PROCESS_CPUTIME_ID / CLOCKID_THREAD_CPUTIME_ID →
//     ENOTSUP. Same split as the time handler: known clock id,
//     not implemented here.
//   * Anything else → EINVAL.
//
// Unlike `clock_time_get`, the result is a compile-time constant
// per clock id — the handler doesn't touch any Platform method.
// That is a deliberate property of v1: if the Platform clock's
// underlying resolution ever degrades (e.g. a future `wasm32` host
// that only exposes microseconds), the handler is the single place
// to widen the answer without rippling through every userland
// binary that pre-computed 1 ns.

fn handle_clock_res_get(req: &Request) -> Response {
    let clock_id = args_u32(req, 0);
    let resolution_ns: u64 = match clock_id {
        abi::wasi::CLOCKID_MONOTONIC | abi::wasi::CLOCKID_REALTIME => 1,
        abi::wasi::CLOCKID_PROCESS_CPUTIME_ID | abi::wasi::CLOCKID_THREAD_CPUTIME_ID => {
            return Response::err(req.request_id, ENOTSUP);
        }
        _ => return Response::err(req.request_id, EINVAL),
    };
    Response::ok(req.request_id, resolution_ns as i64)
}

// ---- random_get -------------------------------------------------------
//
// Layout:
//   args       = unused (the buffer is addressed entirely via
//                 heap_ptr / heap_len)
//   heap_ptr    = destination offset in caller's heap scratch
//   heap_len    = number of bytes to fill
// Response:
//   value       = bytes filled (== heap_len on success)
//   extra_len   = bytes filled (same value, mirrored so userland
//                 can read it out of the response header
//                 directly)
//
// Filling straight into the heap scratch region means the
// handler doesn't need a temporary buffer. `Platform::random_bytes`
// writes into a `&mut [u8]` slice of any length; we hand it the
// heap window.

fn handle_random_get(req: &Request, heap: &mut [u8]) -> Response {
    let len = req.heap_len as usize;
    let Some(buf) = heap_out_mut(req, heap, len) else {
        return Response::err(req.request_id, EINVAL);
    };
    platform::current().random_bytes(buf);
    Response {
        request_id: req.request_id,
        status: 0,
        value: len as i64,
        extra_len: len as u32,
        _pad: [0u8; 12],
    }
}

// ---- sched_yield ------------------------------------------------------
//
// Layout: no args, no heap.
// Response: status = 0.
//
// WASI's `sched_yield` is a cooperative-scheduling hint: "I
// have no more work to do right now; please let someone else
// run." PMos's current scheduler is a single-threaded
// round-robin that runs each dispatch to completion, so yield
// has no behavioural effect — every syscall already "yields"
// in the sense that the kernel can pick the next runnable
// process on the next dispatch loop iteration. The handler
// exists anyway because Rust's std WASI libc calls
// `sched_yield` in a few spots (notably lock busy-wait loops)
// and would panic on -ENOSYS.

fn handle_sched_yield(req: &Request) -> Response {
    Response::ok(req.request_id, 0)
}

// ---- args_sizes_get / args_get / environ_sizes_get / environ_get ------
//
// PMos doesn't pass argv or envp to user wasm in v1 — every binary
// starts with an empty argument list and an empty environment. These
// handlers exist so that a Rust `std` binary (whose libc init probes
// all four of these opcodes before `main` runs) doesn't panic on
// `-ENOSYS`. Userland sees `(argc=0, buf_size=0)` and `(envc=0,
// buf_size=0)` and moves on.
//
// A future slice can attach a real argv/envp to `SpawnArgs`, thread
// them through `Kernel::proc_spawn` into the child's Process record,
// and have these handlers read from there. The wire format stays the
// same — *_SIZES_GET returns two u32s, *_GET writes N byte-blobs +
// N pointers into the caller's heap scratch.
//
// Layout for the _SIZES_GET pair:
//   args      = unused
//   heap_ptr  = output offset (8 bytes: (count u32, buf_size u32) LE)
//   heap_len  = 8 (or >=8; only the first 8 bytes are written)
// Response:
//   value     = 0 (success)
//   extra_len = 8 (bytes written)
//
// Layout for the _GET pair (with count == 0, nothing to write):
//   args      = unused
//   heap_ptr  = unused
//   heap_len  = unused
// Response:
//   value     = 0 (success)

fn write_two_zero_u32s(req: &Request, heap: &mut [u8]) -> Response {
    let Some(buf) = heap_out_mut(req, heap, 8) else {
        return Response::err(req.request_id, EINVAL);
    };
    buf[..8].copy_from_slice(&[0u8; 8]);
    Response {
        request_id: req.request_id,
        status: 0,
        value: 0,
        extra_len: 8,
        _pad: [0u8; 12],
    }
}

fn handle_args_sizes_get(req: &Request, heap: &mut [u8]) -> Response {
    write_two_zero_u32s(req, heap)
}

fn handle_args_get(req: &Request) -> Response {
    Response::ok(req.request_id, 0)
}

fn handle_environ_sizes_get(req: &Request, heap: &mut [u8]) -> Response {
    write_two_zero_u32s(req, heap)
}

fn handle_environ_get(req: &Request) -> Response {
    Response::ok(req.request_id, 0)
}

// ---- fd_fdstat_get ----------------------------------------------------
//
// Layout:
//   args[0..4] = fd (u32)
//   heap_ptr   = output offset for the 24-byte fdstat_t
//   heap_len   = 24 (or >=24; only 24 bytes are written)
// Response:
//   value     = 0 (success)
//   extra_len = 24
//
// fdstat_t wire layout (matches WASI preview 1):
//   offset 0:  filetype        u8
//   offset 1:  _pad            u8
//   offset 2:  fs_flags        u16
//   offset 4:  _pad            u32  (the Rust-side generated bindings
//                                    insist on this alignment gap)
//   offset 8:  fs_rights_base  u64
//   offset 16: fs_rights_inh   u64
//
// PMos's v1 answer: filetype is derived from the FdObject variant;
// fs_flags = 0; rights are set to all-bits-on for stdio-style fds so
// `println!`'s isatty-ish probes succeed without the kernel having
// to model rights properly. When a future slice introduces real WASI
// rights tracking, this handler becomes the single place to wire it.

use abi::wasi::filetype as ft;

fn filetype_for(object: FdObject) -> u8 {
    match object {
        FdObject::Vnode { .. } => ft::REGULAR_FILE,
        FdObject::CharDevice(_) => ft::CHARACTER_DEVICE,
        FdObject::Socket(_) | FdObject::DisplayConn(_) => ft::SOCKET_STREAM,
        FdObject::PipeRead(_)
        | FdObject::PipeWrite(_)
        | FdObject::SignalChannel => ft::UNKNOWN,
    }
}

fn filetype_from_nodetype(ty: NodeType) -> u8 {
    match ty {
        NodeType::RegularFile     => ft::REGULAR_FILE,
        NodeType::Directory       => ft::DIRECTORY,
        NodeType::CharDevice(_)   => ft::CHARACTER_DEVICE,
        NodeType::Socket          => ft::SOCKET_STREAM,
        NodeType::SymLink         => ft::SYMBOLIC_LINK,
        // WASI has no FIFO filetype; UNKNOWN is the honest answer.
        NodeType::Fifo            => ft::UNKNOWN,
    }
}

fn handle_fd_fdstat_get(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> Response {
    let fd = args_u32(req, 0);
    let table = match kernel.fds(pid) {
        Ok(t) => t,
        Err(e) => return Response::err(req.request_id, kerr_to_errno(e)),
    };
    let Some(entry) = table.get(fd) else {
        return Response::err(req.request_id, EBADF);
    };
    let filetype = filetype_for(entry.object);

    let Some(buf) = heap_out_mut(req, heap, 24) else {
        return Response::err(req.request_id, EINVAL);
    };
    buf[..24].copy_from_slice(&[0u8; 24]);
    buf[0] = filetype;
    // fs_flags at offset 2 stays 0.
    // fs_rights_base at offset 8: all-bits-on so userland doesn't
    // reject an op on rights grounds before the kernel itself does.
    buf[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
    buf[16..24].copy_from_slice(&u64::MAX.to_le_bytes());

    Response {
        request_id: req.request_id,
        status: 0,
        value: 0,
        extra_len: 24,
        _pad: [0u8; 12],
    }
}

// ---- fd_filestat_get --------------------------------------------------
//
// Layout:
//   args[0..4] = fd (u32)
//   heap_ptr   = output offset for the 64-byte filestat_t
//   heap_len   = 64
// Response:
//   value     = 0
//   extra_len = 64
//
// Writes a 64-byte WASI preview 1 `filestat_t`:
//
//   offset  0: dev       u64
//   offset  8: ino       u64
//   offset 16: filetype  u8   (+ 7 bytes of struct-alignment padding
//                               before nlink — kept at zero)
//   offset 24: nlink     u64
//   offset 32: size      u64
//   offset 40: atim      u64  (nanoseconds since the epoch)
//   offset 48: mtim      u64
//   offset 56: ctim      u64
//
// For `Vnode` fds the handler queries [`Vfs::stat_ino`] so a vnode
// that points at a directory reports filetype=3 (DIRECTORY) and
// propagates the filesystem-reported size/ino/times. `dev` is the
// mount id — a stable v1-wide filesystem identifier.
//
// For non-Vnode fds the handler synthesises the fields: filetype
// comes from [`filetype_for`], `nlink=1`, `dev=0`, `ino=<opaque id>`
// (the devnum for `CharDevice`, socket/pipe id for sockets/pipes,
// 0 for `SignalChannel`), `size=0`, times zero. This is honest
// behaviour for v1 — a future slice that threads Platform timestamps
// into vnode metadata via a vfs side-channel is what grows real
// mtime/atime/ctime numbers.
fn handle_fd_filestat_get(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> Response {
    let fd = args_u32(req, 0);
    let entry = match kernel.fds(pid) {
        Ok(t) => match t.get(fd) {
            Some(e) => *e,
            None => return Response::err(req.request_id, EBADF),
        },
        Err(e) => return Response::err(req.request_id, kerr_to_errno(e)),
    };

    let (filetype, dev, ino, size, nlink, atim, mtim, ctim) = match entry.object {
        FdObject::Vnode { mount_id, ino } => {
            let st = match kernel.vfs.stat_ino(mount_id, ino) {
                Ok(s) => s,
                Err(e) => {
                    return Response::err(
                        req.request_id,
                        kerr_to_errno(KernelError::Fs(e)),
                    )
                }
            };
            (
                filetype_from_nodetype(st.ty),
                mount_id.0 as u64,
                st.ino,
                st.size,
                st.nlink as u64,
                st.atime_ns,
                st.mtime_ns,
                st.ctime_ns,
            )
        }
        FdObject::CharDevice(devnum) => {
            (ft::CHARACTER_DEVICE, 0, devnum as u64, 0, 1, 0, 0, 0)
        }
        FdObject::Socket(id) | FdObject::DisplayConn(id) => {
            (ft::SOCKET_STREAM, 0, id as u64, 0, 1, 0, 0, 0)
        }
        FdObject::PipeRead(id) | FdObject::PipeWrite(id) => {
            (ft::UNKNOWN, 0, id as u64, 0, 1, 0, 0, 0)
        }
        FdObject::SignalChannel => (ft::UNKNOWN, 0, 0, 0, 1, 0, 0, 0),
    };

    let Some(buf) = heap_out_mut(req, heap, fs_off::SIZE) else {
        return Response::err(req.request_id, EINVAL);
    };
    buf[..fs_off::SIZE].copy_from_slice(&[0u8; fs_off::SIZE]);
    buf[fs_off::OFF_DEV..fs_off::OFF_DEV + 8].copy_from_slice(&dev.to_le_bytes());
    buf[fs_off::OFF_INO..fs_off::OFF_INO + 8].copy_from_slice(&ino.to_le_bytes());
    buf[fs_off::OFF_FILETYPE] = filetype;
    // bytes fs_off::OFF_FILETYPE+1 .. fs_off::OFF_NLINK stay zero
    // (struct-alignment padding before the next u64 field).
    buf[fs_off::OFF_NLINK..fs_off::OFF_NLINK + 8].copy_from_slice(&nlink.to_le_bytes());
    buf[fs_off::OFF_SIZE..fs_off::OFF_SIZE + 8].copy_from_slice(&size.to_le_bytes());
    buf[fs_off::OFF_ATIM..fs_off::OFF_ATIM + 8].copy_from_slice(&atim.to_le_bytes());
    buf[fs_off::OFF_MTIM..fs_off::OFF_MTIM + 8].copy_from_slice(&mtim.to_le_bytes());
    buf[fs_off::OFF_CTIM..fs_off::OFF_CTIM + 8].copy_from_slice(&ctim.to_le_bytes());

    Response {
        request_id: req.request_id,
        status: 0,
        value: 0,
        extra_len: fs_off::SIZE as u32,
        _pad: [0u8; 12],
    }
}

// ---- path_filestat_get ------------------------------------------------
//
// Layout:
//   args[0..4] = dir_fd (u32, ignored — v1 has no preopens, every
//                path is treated as absolute)
//   args[4..8] = lookup flags (u32; the only defined bit is
//                LOOKUP_SYMLINK_FOLLOW=0x1. When set, the final
//                path component is dereferenced if it's a symlink
//                — POSIX stat semantics. When unset, the final
//                component is NOT dereferenced — POSIX lstat
//                semantics. Intermediate components always follow
//                symlinks; only the final one is flag-governed.)
//   heap_ptr   = offset of the path bytes (input) AND of the 64-byte
//                filestat_t (output). The handler reads the path
//                first, then overwrites the same region with the
//                filestat_t — the two windows share the heap ptr
//                because [`Request`] only carries one heap region.
//   heap_len   = length of the path bytes on input. The output is
//                always `fs_off::SIZE` (64) bytes regardless of
//                heap_len; the caller sizes heap_cap for >= 64.
// Response:
//   value     = 0
//   extra_len = 64
//
// The path-based sibling of [`handle_fd_filestat_get`]. Reuses the
// same `filestat_t` wire layout + [`filetype_from_nodetype`] helper
// + [`Vfs::stat_ino`] accessor; the only genuinely new surface is
// the path-string input decoding (same as [`handle_path_open`]) and
// the different error path for a missing path (FsError::NotFound →
// ENOENT via [`kerr_to_errno`]).
//
// Userland std code compiled for wasm32-wasip1 lowers
// `std::fs::metadata(path)` through this opcode; PMos has no
// preopens, so the path that arrives at the kernel is already the
// absolute form the caller wrote in its source.
fn handle_path_filestat_get(
    kernel: &mut Kernel,
    _pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> Response {
    // dir_fd still unused in v1 (no preopens). lookup_flags drives
    // symlink-follow behaviour per WASI: bit 0 set → follow final
    // symlink (stat), unset → don't (lstat). Intermediate
    // symlinks always follow regardless — that's implicit in
    // Vfs::resolve vs. resolve_nofollow.
    let _dir_fd = args_u32(req, 0);
    let lookup_flags = args_u32(req, 4);
    let Some(path_bytes) = heap_in(req, heap) else {
        return Response::err(req.request_id, EINVAL);
    };
    let Ok(path) = core::str::from_utf8(path_bytes) else {
        return Response::err(req.request_id, EINVAL);
    };
    let follow = (lookup_flags & LOOKUP_SYMLINK_FOLLOW) != 0;
    let resolved = if follow {
        kernel.vfs.resolve(path)
    } else {
        kernel.vfs.resolve_nofollow(path)
    };
    let (mount_id, ino) = match resolved {
        Ok(p) => p,
        Err(e) => {
            return Response::err(
                req.request_id,
                kerr_to_errno(KernelError::Fs(e)),
            )
        }
    };
    let st = match kernel.vfs.stat_ino(mount_id, ino) {
        Ok(s) => s,
        Err(e) => {
            return Response::err(
                req.request_id,
                kerr_to_errno(KernelError::Fs(e)),
            )
        }
    };

    let Some(buf) = heap_out_mut(req, heap, fs_off::SIZE) else {
        return Response::err(req.request_id, EINVAL);
    };
    buf[..fs_off::SIZE].copy_from_slice(&[0u8; fs_off::SIZE]);
    buf[fs_off::OFF_DEV..fs_off::OFF_DEV + 8]
        .copy_from_slice(&(mount_id.0 as u64).to_le_bytes());
    buf[fs_off::OFF_INO..fs_off::OFF_INO + 8].copy_from_slice(&st.ino.to_le_bytes());
    buf[fs_off::OFF_FILETYPE] = filetype_from_nodetype(st.ty);
    // bytes fs_off::OFF_FILETYPE+1 .. fs_off::OFF_NLINK stay zero
    // (struct-alignment padding before the next u64 field).
    buf[fs_off::OFF_NLINK..fs_off::OFF_NLINK + 8]
        .copy_from_slice(&(st.nlink as u64).to_le_bytes());
    buf[fs_off::OFF_SIZE..fs_off::OFF_SIZE + 8].copy_from_slice(&st.size.to_le_bytes());
    buf[fs_off::OFF_ATIM..fs_off::OFF_ATIM + 8].copy_from_slice(&st.atime_ns.to_le_bytes());
    buf[fs_off::OFF_MTIM..fs_off::OFF_MTIM + 8].copy_from_slice(&st.mtime_ns.to_le_bytes());
    buf[fs_off::OFF_CTIM..fs_off::OFF_CTIM + 8].copy_from_slice(&st.ctime_ns.to_le_bytes());

    Response {
        request_id: req.request_id,
        status: 0,
        value: 0,
        extra_len: fs_off::SIZE as u32,
        _pad: [0u8; 12],
    }
}

// ---- path_filestat_set_times -----------------------------------------
//
// Layout:
//   args[0..4]    = dir_fd (u32, ignored — v1 has no preopens)
//   args[4..8]    = lookup_flags (u32, ignored — v1 doesn't follow
//                   symlinks either way)
//   args[8..12]   = fstflags (u32; low 4 bits: SET_ATIM=0x1,
//                   SET_ATIM_NOW=0x2, SET_MTIM=0x4, SET_MTIM_NOW=0x8;
//                   upper bits unused)
//   args[12..16]  = reserved (must be 0; ignored in v1)
//   heap[0..8]    = atim (u64 LE ns-since-epoch; ignored unless
//                   SET_ATIM is set)
//   heap[8..16]   = mtim (u64 LE ns-since-epoch; ignored unless
//                   SET_MTIM is set)
//   heap[16..]    = UTF-8 path bytes
//   heap_len      = 16 + path.len()
// Response:
//   value = 0 on success; status = -errno on error.
//
// Unblocked by the real-vnode-timestamps slice (before which every
// filesystem reported 0 for all three times, so setting them via
// this opcode would have just ratcheted a zero-initialised field).
// The dispatcher decodes fstflags into a pair of Option values and
// hands them to `Vfs::set_times`; the filesystem never sees the
// raw fstflags bits, only "this is the new atim" / "don't touch
// mtim". SET_ATIM_NOW + SET_MTIM_NOW substitute
// `Platform::now_realtime_ns()` at this layer, not at the
// filesystem. ctime is always bumped by the filesystem's
// `set_times` impl when the call has any effect — metadata change.
//
// The heap layout puts the two u64 timestamps in the first 16
// bytes then the path. Picking this over "atim/mtim inline in
// args + path in heap" keeps the 16-byte inline args window
// identical to path_filestat_get's (dir_fd + lookup_flags at the
// same offsets) and leaves args[8..] free for fstflags + future
// growth.

// ---- fd_filestat_set_times -------------------------------------------
//
// Layout:
//   args[0..4]    = fd (u32)
//   args[4..8]    = fstflags (u32; SET_ATIM / SET_ATIM_NOW / SET_MTIM /
//                   SET_MTIM_NOW — same bits as path_filestat_set_times)
//   heap[0..8]    = atim (u64 LE ns-since-epoch; ignored unless
//                   SET_ATIM is set)
//   heap[8..16]   = mtim (u64 LE ns-since-epoch; ignored unless
//                   SET_MTIM is set)
//   heap_len      = 16
// Response:
//   value = 0 on success; status = -errno on error.
//
// Fd-based sibling of `path_filestat_set_times`: same fstflags + same
// Option-materialisation for the _NOW variants + same zero-flags no-op
// semantics. Differs only in how the `(mount_id, ino)` pair is resolved
// — the fd table lookup replaces the path-resolve step. Reuses
// `Vfs::set_times_ino` (the direct-ino mirror of `Vfs::set_times`) so
// the underlying `Filesystem::set_times` call is byte-identical to the
// path variant.
//
// Guards in order:
//   1. Exclusive flag-pair validation fires BEFORE any fd lookup or
//      heap read — an invalid-flags probe gets the same EINVAL errno
//      regardless of fd state, mirroring the path variant.
//   2. Heap shorter than 16 → EINVAL (malformed shim).
//   3. Unopened fd → EBADF.
//   4. Non-Vnode FdObject → EINVAL (char devices, sockets, pipes,
//      signal channels carry no time metadata to mutate; guard mirrors
//      `fd_seek` / `fd_tell` / the fd-state bundle).
//   5. Filesystem rejection (EROFS for devfs / procfs etc.) is passed
//      through via `kerr_to_errno(KernelError::Fs(_))`.

fn handle_fd_filestat_set_times(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> Response {
    let fd = args_u32(req, 0);
    let fstflags = args_u32(req, 4);

    let a_now = fstflags & abi::wasi::fstflags::SET_ATIM_NOW as u32 != 0;
    let a_set = fstflags & abi::wasi::fstflags::SET_ATIM as u32 != 0;
    let m_now = fstflags & abi::wasi::fstflags::SET_MTIM_NOW as u32 != 0;
    let m_set = fstflags & abi::wasi::fstflags::SET_MTIM as u32 != 0;
    if a_now && a_set {
        return Response::err(req.request_id, EINVAL);
    }
    if m_now && m_set {
        return Response::err(req.request_id, EINVAL);
    }

    let Some(heap_bytes) = heap_in(req, heap) else {
        return Response::err(req.request_id, EINVAL);
    };
    if heap_bytes.len() < 16 {
        return Response::err(req.request_id, EINVAL);
    }
    let mut atim_buf = [0u8; 8];
    atim_buf.copy_from_slice(&heap_bytes[0..8]);
    let atim = u64::from_le_bytes(atim_buf);
    let mut mtim_buf = [0u8; 8];
    mtim_buf.copy_from_slice(&heap_bytes[8..16]);
    let mtim = u64::from_le_bytes(mtim_buf);

    let entry = match kernel.fds(pid) {
        Ok(t) => match t.get(fd) {
            Some(e) => *e,
            None => return Response::err(req.request_id, EBADF),
        },
        Err(e) => return Response::err(req.request_id, kerr_to_errno(e)),
    };
    let (mount_id, ino) = match entry.object {
        FdObject::Vnode { mount_id, ino } => (mount_id, ino),
        _ => return Response::err(req.request_id, EINVAL),
    };

    let atim_opt = if a_now {
        Some(platform::current().now_realtime_ns())
    } else if a_set {
        Some(atim)
    } else {
        None
    };
    let mtim_opt = if m_now {
        Some(platform::current().now_realtime_ns())
    } else if m_set {
        Some(mtim)
    } else {
        None
    };

    match kernel.vfs.set_times_ino(mount_id, ino, atim_opt, mtim_opt) {
        Ok(()) => Response::ok(req.request_id, 0),
        Err(e) => Response::err(
            req.request_id,
            kerr_to_errno(KernelError::Fs(e)),
        ),
    }
}

fn handle_path_filestat_set_times(
    kernel: &mut Kernel,
    _pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> Response {
    let _dir_fd = args_u32(req, 0);
    let _lookup_flags = args_u32(req, 4);
    let fstflags = args_u32(req, 8);

    // Validate the exclusive pairs: SET_ATIM + SET_ATIM_NOW is
    // EINVAL per WASI, same for the mtim pair. These fire BEFORE
    // the heap-length check so a caller with invalid flags gets a
    // consistent errno regardless of whether they also sent a
    // well-formed heap.
    let a_now = fstflags & abi::wasi::fstflags::SET_ATIM_NOW as u32 != 0;
    let a_set = fstflags & abi::wasi::fstflags::SET_ATIM as u32 != 0;
    let m_now = fstflags & abi::wasi::fstflags::SET_MTIM_NOW as u32 != 0;
    let m_set = fstflags & abi::wasi::fstflags::SET_MTIM as u32 != 0;
    if a_now && a_set {
        return Response::err(req.request_id, EINVAL);
    }
    if m_now && m_set {
        return Response::err(req.request_id, EINVAL);
    }

    let Some(heap_bytes) = heap_in(req, heap) else {
        return Response::err(req.request_id, EINVAL);
    };
    if heap_bytes.len() < 16 {
        return Response::err(req.request_id, EINVAL);
    }
    let mut atim_buf = [0u8; 8];
    atim_buf.copy_from_slice(&heap_bytes[0..8]);
    let atim = u64::from_le_bytes(atim_buf);
    let mut mtim_buf = [0u8; 8];
    mtim_buf.copy_from_slice(&heap_bytes[8..16]);
    let mtim = u64::from_le_bytes(mtim_buf);
    let Ok(path) = core::str::from_utf8(&heap_bytes[16..]) else {
        return Response::err(req.request_id, EINVAL);
    };

    let atim_opt = if a_now {
        Some(platform::current().now_realtime_ns())
    } else if a_set {
        Some(atim)
    } else {
        None
    };
    let mtim_opt = if m_now {
        Some(platform::current().now_realtime_ns())
    } else if m_set {
        Some(mtim)
    } else {
        None
    };

    match kernel.vfs.set_times(path, atim_opt, mtim_opt) {
        Ok(()) => Response::ok(req.request_id, 0),
        Err(e) => Response::err(
            req.request_id,
            kerr_to_errno(KernelError::Fs(e)),
        ),
    }
}

// ---- fd_prestat_get ---------------------------------------------------
//
// Layout:
//   args[0..4] = fd (u32)
// Response:
//   status    = -EBADF (always)
//
// WASI's preopen-dir discovery loop starts at fd 3 and walks up
// until the runtime returns EBADF; that's how libcs know there are
// no more preopens. PMos doesn't expose any preopen directories
// (every path resolves through the VFS by absolute name), so the
// honest answer for every fd is EBADF. Returning that turns a Rust
// std binary's startup probe into a two-iteration loop that exits
// cleanly.

fn handle_fd_prestat_get(req: &Request) -> Response {
    Response::err(req.request_id, EBADF)
}

// ---- fd_prestat_dir_name ---------------------------------------------
//
// Layout:
//   args[0..4] = fd (u32; ignored — same EBADF for every fd)
//   heap       = caller's output buffer (ignored — nothing is written
//                on the EBADF path)
// Response:
//   status    = -EBADF (always)
//
// Companion to fd_prestat_get. Userland's preopen-discovery loops
// iterate fd 3/4/5 calling both fd_prestat_get and fd_prestat_dir_name
// until both return EBADF. Pre-slice, v1 returned EBADF from get but
// ENOSYS from dir_name, which broke the loop — callers saw "no
// preopens" for get and "this syscall doesn't exist" for dir_name,
// which is inconsistent. Post-slice, both agree on EBADF and the
// loop terminates cleanly.

fn handle_fd_prestat_dir_name(req: &Request) -> Response {
    Response::err(req.request_id, EBADF)
}

// ---- fd_readdir ------------------------------------------------------
//
// Layout:
//   args[0..4]  = fd (u32)
//   args[4..12] = cookie (u64 LE; 0 = start from beginning)
//   heap_ptr    = start of caller's output buffer
//   heap_len    = buffer capacity in bytes
// Response:
//   value     = bytes actually written (0..=heap_len)
//   extra_len = mirrors value (bytes-written convention)
//
// WASI's directory listing. Each entry in the output buffer is a
// 24-byte dirent header (d_next / d_ino / d_namlen / d_type; see
// `abi::wasi::dirent`) followed immediately by d_namlen bytes of
// UTF-8 name. Entries pack back-to-back with no inter-entry
// padding — a buffer that fills mid-entry receives a truncated
// final entry. The caller signals "more entries may exist" by
// observing value == heap_len and re-issuing with the last d_next
// cookie they successfully decoded.
//
// Cookie semantics: 0 = start from the beginning, N = resume AFTER
// the entry whose d_next was last observed as N-1. The kernel
// assigns d_next = (index_in_listing + 1), so cookie = 3 means
// "skip entries 0, 1, 2 and start from entry 3".
//
// Guards in order:
//   1. Unopened fd → EBADF.
//   2. Non-Vnode FdObject → EINVAL. Only directories can be
//      readdir'd; char devices / sockets / pipes / signal channels
//      have no directory listing.
//   3. Vnode pointing at a non-directory → ENOTDIR (passed through
//      from Filesystem::readdir via kerr_to_errno).
//   4. heap_len == 0 → value = 0, extra_len = 0 (success with
//      nothing written; a caller probing for sizing).
//
// v1 does NOT inject `.` / `..` entries. WASI doesn't require them,
// and the v1 VFS doesn't track parent inodes — a synthesised `..`
// would need a separate parent-tracking pass on every dir op.

use abi::wasi::dirent as de;

fn handle_fd_readdir(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> Response {
    let fd = args_u32(req, 0);
    let cookie = args_u64(req, 4);
    let buf_len = req.heap_len as usize;

    let entry = match kernel.fds(pid) {
        Ok(t) => match t.get(fd) {
            Some(e) => *e,
            None => return Response::err(req.request_id, EBADF),
        },
        Err(e) => return Response::err(req.request_id, kerr_to_errno(e)),
    };
    let (mount_id, ino) = match entry.object {
        FdObject::Vnode { mount_id, ino } => (mount_id, ino),
        _ => return Response::err(req.request_id, EINVAL),
    };

    let entries = match kernel.vfs.readdir_ino(mount_id, ino) {
        Ok(v) => v,
        Err(e) => {
            return Response::err(
                req.request_id,
                kerr_to_errno(KernelError::Fs(e)),
            )
        }
    };

    let Some(buf) = heap_out_mut(req, heap, buf_len) else {
        return Response::err(req.request_id, EINVAL);
    };

    let mut written = 0usize;
    let start = cookie as usize;
    for (i, ent) in entries.iter().enumerate().skip(start) {
        if written >= buf_len {
            break;
        }
        let name_bytes = ent.name.as_bytes();
        let d_type = filetype_from_nodetype(ent.ty);
        let d_next = (i as u64) + 1;

        // Write the header byte-by-byte into what remains. If the
        // header itself doesn't fit, write what does and stop —
        // truncation is allowed at any point.
        let remaining = buf_len - written;
        let header_end = core::cmp::min(de::HEADER_SIZE, remaining);
        let mut header = [0u8; de::HEADER_SIZE];
        header[de::OFF_D_NEXT..de::OFF_D_NEXT + 8]
            .copy_from_slice(&d_next.to_le_bytes());
        header[de::OFF_D_INO..de::OFF_D_INO + 8]
            .copy_from_slice(&ent.ino.to_le_bytes());
        header[de::OFF_D_NAMLEN..de::OFF_D_NAMLEN + 4]
            .copy_from_slice(&(name_bytes.len() as u32).to_le_bytes());
        header[de::OFF_D_TYPE] = d_type;
        buf[written..written + header_end].copy_from_slice(&header[..header_end]);
        written += header_end;
        if header_end < de::HEADER_SIZE {
            break;
        }

        // Write as much of the name as fits.
        let remaining = buf_len - written;
        let name_fit = core::cmp::min(name_bytes.len(), remaining);
        buf[written..written + name_fit].copy_from_slice(&name_bytes[..name_fit]);
        written += name_fit;
        if name_fit < name_bytes.len() {
            break;
        }
    }

    Response {
        request_id: req.request_id,
        status: 0,
        value: written as i64,
        extra_len: written as u32,
        _pad: [0u8; 12],
    }
}

// ---- path_unlink_file ------------------------------------------------
//
// Layout:
//   args[0..4]  = dir_fd (u32, ignored — v1 has no preopens, every
//                 path is treated as absolute)
//   heap_ptr    = offset of UTF-8 path bytes
//   heap_len    = length of the path
// Response:
//   value = 0 on success; status = -errno on error.
//
// WASI's path_unlink_file is strictly for regular files — unlinking
// a directory returns EISDIR (the caller should use
// path_remove_directory instead). This is the first write-side
// filesystem-mutation WASI opcode in PMos's surface: the handler
// threads the path through `Vfs::unlink`, which in turn calls
// `Filesystem::unlink` on the mount owning the path. tmpfs overrides
// with the real removal path; devfs / procfs inherit the default
// (returns NotSupported → ENOTSUP). A future OPFS slice will
// override with the journal-backed path.

fn handle_path_unlink_file(
    kernel: &mut Kernel,
    _pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> Response {
    let _dir_fd = args_u32(req, 0);
    let Some(path_bytes) = heap_in(req, heap) else {
        return Response::err(req.request_id, EINVAL);
    };
    let Ok(path) = core::str::from_utf8(path_bytes) else {
        return Response::err(req.request_id, EINVAL);
    };
    match kernel.vfs.unlink(path) {
        Ok(()) => Response::ok(req.request_id, 0),
        Err(e) => Response::err(
            req.request_id,
            kerr_to_errno(KernelError::Fs(e)),
        ),
    }
}

// ---- path_rename -----------------------------------------------------
//
// Layout:
//   args[0..4]   = from_dir_fd (u32, ignored — v1 has no preopens)
//   args[4..8]   = to_dir_fd   (u32, ignored)
//   args[8..12]  = old_len (u32; split point in the heap window —
//                  heap[0..old_len] is the old UTF-8 path,
//                  heap[old_len..heap_len] is the new UTF-8 path)
//   args[12..16] = reserved (must be 0; ignored in v1)
//   heap[0..old_len]        = UTF-8 old path
//   heap[old_len..heap_len] = UTF-8 new path
//   heap_len                = old_len + new_len (enforced by the
//                             dispatcher; a shorter heap_len rejects
//                             with EINVAL)
// Response:
//   value = 0 on success; status = -errno on error.
//
// Unusual two-heap-strings wire format: [`Request`] carries only one
// heap region, so the kernel packs both paths into a single window
// and reads old_len from the inline args to know where to split.
// Picking the u32-in-args layout over "null-separated concatenation"
// keeps the dispatcher branch simple (no in-band scan for a
// separator byte, no path-contains-null concern) and matches the way
// `path_open` threads a path-length hint through args today.
//
// Semantics pass through `Vfs::rename` — which rejects cross-mount
// renames with `FsError::NotSupported` (→ ENOTSUP) so userland uses
// create+write+unlink for those cases instead. Within a single mount,
// tmpfs replaces any existing destination per POSIX rename, and the
// other in-tree filesystems inherit the trait default (ReadOnly /
// NotSupported depending on the impl).

fn handle_path_rename(
    kernel: &mut Kernel,
    _pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> Response {
    let _from_dir_fd = args_u32(req, 0);
    let _to_dir_fd = args_u32(req, 4);
    let old_len = args_u32(req, 8) as usize;

    let Some(heap_bytes) = heap_in(req, heap) else {
        return Response::err(req.request_id, EINVAL);
    };
    if old_len == 0 || old_len >= heap_bytes.len() {
        return Response::err(req.request_id, EINVAL);
    }
    let (old_bytes, new_bytes) = heap_bytes.split_at(old_len);
    let Ok(old_path) = core::str::from_utf8(old_bytes) else {
        return Response::err(req.request_id, EINVAL);
    };
    let Ok(new_path) = core::str::from_utf8(new_bytes) else {
        return Response::err(req.request_id, EINVAL);
    };
    match kernel.vfs.rename(old_path, new_path) {
        Ok(()) => Response::ok(req.request_id, 0),
        Err(e) => Response::err(
            req.request_id,
            kerr_to_errno(KernelError::Fs(e)),
        ),
    }
}

// ---- path_link -------------------------------------------------------
//
// Layout:
//   args[0..4]   = old_fd      (u32; ignored in v1 — no preopens)
//   args[4..8]   = old_flags   (u32; lookup flags — ignored in v1,
//                               symlinks aren't followed by the path
//                               resolver regardless)
//   args[8..12]  = new_fd      (u32; ignored in v1)
//   args[12..16] = old_len     (u32; split point in heap window —
//                               heap[0..old_len] is the source path,
//                               heap[old_len..heap_len] is the new
//                               hardlink-target path)
//   heap[0..old_len]        = UTF-8 source path (must already exist)
//   heap[old_len..heap_len] = UTF-8 new path (must NOT already exist)
//   heap_len                = old_len + new_len (a shorter heap_len
//                             rejects with EINVAL)
// Response:
//   value = 0 on success; status = -errno on error.
//
// Creates a hardlink — a second directory entry pointing at the same
// inode. After success, writes through either name are visible through
// the other, and stat().nlink reports the updated count. Mirrors
// PATH_RENAME's two-heap-strings packing but with old_len at
// args[12..16] because path_link carries three integer-shaped args
// before the path-length fields at the WASI level.
//
// Threads through `Vfs::link` → `Filesystem::link` on the owning mount.
// tmpfs overrides with the real nlink++ + dir-entry-insert path;
// devfs / procfs / opfs inherit the trait default (`ReadOnly` → EROFS)
// exactly the way [`Filesystem::set_times`]'s default behaves. Cross-
// mount links are rejected at the VFS layer with `NotSupported` →
// ENOTSUP — a hardlink can't span filesystems because inode numbers
// are per-mount.

fn handle_path_link(
    kernel: &mut Kernel,
    _pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> Response {
    let _old_fd = args_u32(req, 0);
    let _old_flags = args_u32(req, 4);
    let _new_fd = args_u32(req, 8);
    let old_len = args_u32(req, 12) as usize;

    let Some(heap_bytes) = heap_in(req, heap) else {
        return Response::err(req.request_id, EINVAL);
    };
    if old_len == 0 || old_len >= heap_bytes.len() {
        return Response::err(req.request_id, EINVAL);
    }
    let (old_bytes, new_bytes) = heap_bytes.split_at(old_len);
    let Ok(old_path) = core::str::from_utf8(old_bytes) else {
        return Response::err(req.request_id, EINVAL);
    };
    let Ok(new_path) = core::str::from_utf8(new_bytes) else {
        return Response::err(req.request_id, EINVAL);
    };
    match kernel.vfs.link(old_path, new_path) {
        Ok(()) => Response::ok(req.request_id, 0),
        Err(e) => Response::err(
            req.request_id,
            kerr_to_errno(KernelError::Fs(e)),
        ),
    }
}

// ---- path_symlink ----------------------------------------------------
//
// Layout:
//   args[0..4]  = old_len (u32; split point in heap window —
//                 heap[0..old_len] is the target UTF-8 string that
//                 the symlink holds, heap[old_len..heap_len] is the
//                 new path to create as a symlink)
//   heap[0..old_len]        = UTF-8 target string (arbitrary; need
//                             not exist — dangling symlinks are fine)
//   heap[old_len..heap_len] = UTF-8 new path (the path of the symlink
//                             the caller is creating)
//   heap_len                = old_len + new_len (a shorter heap_len
//                             rejects with EINVAL)
// Response:
//   value = 0 on success; status = -errno on error.
//
// Cleaner than PATH_LINK's packing because WASI path_symlink has only
// one integer-shaped arg (new_fd) in its signature and v1 ignores it
// anyway — only old_len is decoded.
//
// Creates a vnode whose "content" is the target string rather than
// regular-file bytes. The target is NOT resolved or validated at
// creation — POSIX/WASI explicitly allow dangling symlinks, and v1's
// `Vfs::resolve` doesn't follow symlinks regardless. A future
// path_readlink slice will retrieve the target; a future
// resolve-follows-symlinks slice (separate) can teach the path walker
// to dereference.
//
// Threads through `Vfs::symlink` → `Filesystem::symlink` on the
// owning mount. tmpfs overrides with the real TmpNode::SymLink
// allocation; devfs / procfs / opfs inherit the trait default
// (`NotSupported` → ENOTSUP). The default differs from link's
// `ReadOnly` default because symlink semantics are a capability
// (does this filesystem know what a symlink is?) rather than a write
// permission — devfs / procfs never gain symlinks in any v1-era
// future slice.

fn handle_path_symlink(
    kernel: &mut Kernel,
    _pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> Response {
    let old_len = args_u32(req, 0) as usize;

    let Some(heap_bytes) = heap_in(req, heap) else {
        return Response::err(req.request_id, EINVAL);
    };
    if old_len == 0 || old_len >= heap_bytes.len() {
        return Response::err(req.request_id, EINVAL);
    }
    let (target_bytes, new_bytes) = heap_bytes.split_at(old_len);
    let Ok(target) = core::str::from_utf8(target_bytes) else {
        return Response::err(req.request_id, EINVAL);
    };
    let Ok(new_path) = core::str::from_utf8(new_bytes) else {
        return Response::err(req.request_id, EINVAL);
    };
    match kernel.vfs.symlink(target, new_path) {
        Ok(_ino) => Response::ok(req.request_id, 0),
        Err(e) => Response::err(
            req.request_id,
            kerr_to_errno(KernelError::Fs(e)),
        ),
    }
}

// ---- path_readlink --------------------------------------------------
//
// Layout:
//   args[0..4] = dir_fd (u32; ignored in v1 — no preopens)
//   args[4..8] = path_len (u32; the first path_len bytes of heap are
//                the UTF-8 input path; the full heap_len doubles as
//                the output buffer capacity)
//   heap[0..path_len] on request = UTF-8 input path
//   heap[0..n] on response       = target bytes written by the kernel
// Response:
//   value     = bytes actually written (n; 0..=heap_len)
//   extra_len = mirrors value (bytes-written convention, so the host
//               test harness's `heapOut = heap[0..extra_len]` surface
//               yields the target bytes directly)
//
// The kernel snapshots the path out of heap[0..path_len] first so it
// can reuse the entire heap as the output buffer. Truncates silently
// if the target exceeds heap_len — matches POSIX readlink(2)'s
// documented behaviour; the caller distinguishes "exact fit" from
// "truncated" by reissuing with a larger heap when value == heap_len.
//
// Threads through `Vfs::readlink` → `Filesystem::readlink` on the
// owning mount. tmpfs's override pattern-matches on `TmpNode::SymLink`
// and copies the target bytes; non-symlink targets return
// `InvalidArgument` → EINVAL. Filesystems that don't know what a
// symlink is (devfs, procfs, opfs) inherit the trait default
// `NotSupported` → ENOTSUP.

fn handle_path_readlink(
    kernel: &mut Kernel,
    _pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> Response {
    let _dir_fd = args_u32(req, 0);
    let path_len = args_u32(req, 4) as usize;

    let heap_len = req.heap_len as usize;
    let Some(full) = heap_out_mut(req, heap, heap_len) else {
        return Response::err(req.request_id, EINVAL);
    };
    if path_len == 0 || path_len > full.len() {
        return Response::err(req.request_id, EINVAL);
    }
    // Snapshot the path out of the input region so the full heap can
    // be reused as the target output buffer.
    let path_owned: alloc::string::String = match core::str::from_utf8(&full[..path_len]) {
        Ok(s) => s.into(),
        Err(_) => return Response::err(req.request_id, EINVAL),
    };
    match kernel.vfs.readlink(&path_owned, full) {
        Ok(n) => Response {
            request_id: req.request_id,
            status: 0,
            value: n as i64,
            extra_len: n as u32,
            _pad: [0u8; 12],
        },
        Err(e) => Response::err(
            req.request_id,
            kerr_to_errno(KernelError::Fs(e)),
        ),
    }
}

// ---- path_create_directory / path_remove_directory ----------------
//
// mkdir + rmdir wire layout (both identical to path_unlink_file's):
//   args[0..4]  = dir_fd (u32, ignored — v1 has no preopens)
//   heap_ptr    = offset of UTF-8 path bytes
//   heap_len    = length of the path
// Response:
//   value = 0 on success; status = -errno on error.
//
// WASI's path_create_directory signature has no mode argument, so
// v1 hard-codes mode 0o755 on the Vfs::mkdir call — standard
// "owner rwx, everyone rx" for new directories. Userland that
// wants a different mode uses path_filestat_set_times (WASI has
// no chmod syscall in preview 1).
//
// Semantics thread through Vfs::mkdir / Vfs::rmdir: tmpfs mutates
// its in-memory dir-tree (AlreadyExists → EEXIST, NotFound on
// parent → ENOENT, NotEmpty → ENOTEMPTY, NotADirectory → ENOTDIR
// for rmdir on a regular file); devfs + procfs return ReadOnly →
// EROFS.

fn handle_path_create_directory(
    kernel: &mut Kernel,
    _pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> Response {
    let _dir_fd = args_u32(req, 0);
    let Some(path_bytes) = heap_in(req, heap) else {
        return Response::err(req.request_id, EINVAL);
    };
    let Ok(path) = core::str::from_utf8(path_bytes) else {
        return Response::err(req.request_id, EINVAL);
    };
    match kernel.vfs.mkdir(path, 0o755) {
        Ok(_) => Response::ok(req.request_id, 0),
        Err(e) => Response::err(
            req.request_id,
            kerr_to_errno(KernelError::Fs(e)),
        ),
    }
}

fn handle_path_remove_directory(
    kernel: &mut Kernel,
    _pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> Response {
    let _dir_fd = args_u32(req, 0);
    let Some(path_bytes) = heap_in(req, heap) else {
        return Response::err(req.request_id, EINVAL);
    };
    let Ok(path) = core::str::from_utf8(path_bytes) else {
        return Response::err(req.request_id, EINVAL);
    };
    match kernel.vfs.rmdir(path) {
        Ok(()) => Response::ok(req.request_id, 0),
        Err(e) => Response::err(
            req.request_id,
            kerr_to_errno(KernelError::Fs(e)),
        ),
    }
}

// ---- fd_fdstat_set_flags -------------------------------------------
//
// Layout:
//   args[0..4] = fd (u32)
//   args[4..8] = new_fdflags (u32; WASI encoding from abi::wasi::fdflags:
//                APPEND=0x01, DSYNC=0x02, NONBLOCK=0x04, RSYNC=0x08,
//                SYNC=0x10)
// Response:
//   value = 0 on success; status = -errno on error.
//
// WASI's equivalent of POSIX `fcntl(F_SETFL)`: overwrites the fd's
// file-status flags. v1 recognises two WASI bits meaningfully —
// NONBLOCK and APPEND — and accepts the three sync-family bits
// (DSYNC/RSYNC/SYNC) as a no-op (tmpfs writes are already
// synchronous into in-memory state, so there is nothing to flush).
//
// Bit translation: WASI's fdflags encoding differs from PMos's
// internal FdFlags (APPEND=0x01 vs CLOEXEC=0x01 on the PMos side;
// NONBLOCK=0x04 vs APPEND=0x04, etc.). The handler reads the WASI
// u32 and sets the corresponding PMos bits. CLOEXEC is the
// descriptor-level flag POSIX `F_SETFD` owns, not `F_SETFL`, so
// this handler preserves any prior CLOEXEC bit on the fd verbatim
// — a userland call that asks for NONBLOCK on a CLOEXEC-marked fd
// keeps CLOEXEC set without any extra opcode dance.
//
// Rejects only EBADF (unopened fd). WASI permits `fd_fdstat_set_flags`
// on any fd type — a socket can be made NONBLOCK, a char device can
// be APPEND'd — so the handler does not check the FdObject variant.

fn handle_fd_fdstat_set_flags(kernel: &mut Kernel, pid: Pid, req: &Request) -> Response {
    let fd = args_u32(req, 0);
    let wasi_bits = args_u32(req, 4);

    let table = match kernel.fds_mut(pid) {
        Ok(t) => t,
        Err(e) => return Response::err(req.request_id, kerr_to_errno(e)),
    };
    let Some(entry) = table.get_mut(fd) else {
        return Response::err(req.request_id, EBADF);
    };

    // Preserve CLOEXEC (and any future non-F_SETFL flag) because
    // those never appear in the WASI fdflags u32. Clear the
    // F_SETFL-owned bits (NONBLOCK + APPEND), then OR in whatever
    // `FdFlags::from_wasi_bits` materialised from the caller's
    // argument — the same translation path_open uses for its
    // fdflags.
    entry.flags.remove(FdFlags::NONBLOCK);
    entry.flags.remove(FdFlags::APPEND);
    let new_bits = FdFlags::from_wasi_bits(wasi_bits);
    entry.flags.insert(new_bits);
    Response::ok(req.request_id, 0)
}

// ---- fd_filestat_set_size ------------------------------------------
//
// Layout:
//   args[0..4]  = fd (u32)
//   args[4..12] = new_size (u64 LE)
// Response:
//   value = 0 on success; status = -errno on error.
//
// Truncate — or extend with zero-fill — a seekable fd to an exact
// byte count. POSIX ftruncate / WASI fd_filestat_set_size share the
// same "bytes" semantics: shrinking discards tail bytes, extending
// past EOF zero-fills the gap. The vnode's mtime + ctime both
// advance per POSIX (the filesystem impl handles that — tmpfs
// updates both on every resize).
//
// Vnode-only. The operation has no meaning on a char device
// (bytes are produced on demand, not stored), a socket (no
// seekable storage), a pipe (same), a signal channel, or a
// display connection. Non-Vnode FdObject variants reject with
// EINVAL — same guard shape as fd_seek / fd_tell.
//
// Threads through the new Vfs::truncate_ino helper, mirroring
// how fd_filestat_set_times reached tmpfs.set_times via
// Vfs::set_times_ino. tmpfs returns IsADirectory → EISDIR for a
// directory vnode; procfs returns ReadOnly → EROFS; devfs returns
// NotSupported → ENOTSUP (but devfs has no regular-file vnodes in
// v1, so that branch is unreachable from the WASI surface).

fn handle_fd_filestat_set_size(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
) -> Response {
    let fd = args_u32(req, 0);
    let new_size = args_u64(req, 4);

    let entry = match kernel.fds(pid) {
        Ok(t) => match t.get(fd) {
            Some(e) => *e,
            None => return Response::err(req.request_id, EBADF),
        },
        Err(e) => return Response::err(req.request_id, kerr_to_errno(e)),
    };
    let (mount_id, ino) = match entry.object {
        FdObject::Vnode { mount_id, ino } => (mount_id, ino),
        _ => return Response::err(req.request_id, EINVAL),
    };
    match kernel.vfs.truncate_ino(mount_id, ino, new_size) {
        Ok(()) => Response::ok(req.request_id, 0),
        Err(e) => Response::err(
            req.request_id,
            kerr_to_errno(KernelError::Fs(e)),
        ),
    }
}

// ---- fd_pread / fd_pwrite ------------------------------------------
//
// Wire layout (both shapes identical except for the heap direction):
//
//   args[0..4]  = fd (u32)
//   args[4..12] = offset (u64 LE)
//   heap_ptr    = source (pwrite) or destination (pread)
//   heap_len    = byte count
//
// Response (pread):
//   value     = bytes actually read (0 on EOF)
//   extra_len = bytes actually read (mirrors fd_read's convention)
// Response (pwrite):
//   value = bytes actually written
//
// POSIX + WASI semantics: positional I/O takes an explicit offset
// and must NOT update the seekable-fd's position. A pread+pwrite
// pair is observable through FdEntry.offset staying unchanged
// after the call. The implementation reaches Vfs::read_ino /
// Vfs::write_ino directly with the explicit offset rather than
// routing through Kernel::fd_read / Kernel::fd_write (which also
// advance the offset on Vnode branches).
//
// Vnode-only: char device / socket / pipe / signal-channel /
// display-connection fds reject with EINVAL (same non-Vnode guard
// shape as fd_seek / fd_tell / fd_filestat_set_size). Positional
// I/O has no meaning on those object types — a socket's byte
// stream isn't seekable, a char device's bytes are produced on
// demand, etc.

fn handle_fd_pread(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> Response {
    let fd = args_u32(req, 0);
    let offset = args_u64(req, 4);

    let entry = match kernel.fds(pid) {
        Ok(t) => match t.get(fd) {
            Some(e) => *e,
            None => return Response::err(req.request_id, EBADF),
        },
        Err(e) => return Response::err(req.request_id, kerr_to_errno(e)),
    };
    let (mount_id, ino) = match entry.object {
        FdObject::Vnode { mount_id, ino } => (mount_id, ino),
        _ => return Response::err(req.request_id, EINVAL),
    };
    let max_len = req.heap_len as usize;
    let Some(buf) = heap_out_mut(req, heap, max_len) else {
        return Response::err(req.request_id, EINVAL);
    };
    match kernel.vfs.read_ino(mount_id, ino, offset, buf) {
        Ok(n) => Response {
            request_id: req.request_id,
            status: 0,
            value: n as i64,
            extra_len: n as u32,
            _pad: [0u8; 12],
        },
        Err(e) => Response::err(
            req.request_id,
            kerr_to_errno(KernelError::Fs(e)),
        ),
    }
}

fn handle_fd_pwrite(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
    heap: &[u8],
) -> Response {
    let fd = args_u32(req, 0);
    let offset = args_u64(req, 4);

    let entry = match kernel.fds(pid) {
        Ok(t) => match t.get(fd) {
            Some(e) => *e,
            None => return Response::err(req.request_id, EBADF),
        },
        Err(e) => return Response::err(req.request_id, kerr_to_errno(e)),
    };
    let (mount_id, ino) = match entry.object {
        FdObject::Vnode { mount_id, ino } => (mount_id, ino),
        _ => return Response::err(req.request_id, EINVAL),
    };
    let Some(bytes) = heap_in(req, heap) else {
        return Response::err(req.request_id, EINVAL);
    };
    match kernel.vfs.write_ino(mount_id, ino, offset, bytes) {
        Ok(n) => Response::ok(req.request_id, n as i64),
        Err(e) => Response::err(
            req.request_id,
            kerr_to_errno(KernelError::Fs(e)),
        ),
    }
}

// ---- sock_send / sock_recv -----------------------------------------
//
// WASI socket aliases of FD_WRITE / FD_READ on Socket fds. Wire:
//
//   SOCK_SEND: args[0..4] = fd (u32)
//              args[4..8] = si_flags (u16 in the low bits; v1 ignores)
//              heap_in    = bytes to send
//   SOCK_RECV: args[0..4] = fd (u32)
//              args[4..8] = ri_flags (u16 in the low bits; v1 ignores)
//              heap_out   = destination buffer, heap_len = capacity
//
// Both reject non-Socket FdObject with EINVAL — PMos has no
// ENOTSOCK errno; EINVAL matches the non-Vnode guard shape used by
// every other fd-type-specific opcode. Unopened fd is EBADF. The
// handlers reuse kernel.ipc.send_on_socket / kernel.ipc.recv_on_socket
// directly; IpcError converts to KernelError via the existing
// From<IpcError> impl and then to errno via kerr_to_errno. An
// InvalidState IpcError (e.g. sending on an unconnected socket)
// surfaces as EINVAL, a ConnectionRefused surfaces as ECONNREFUSED,
// etc. — the mapping is consistent with what FD_READ / FD_WRITE
// report on the same fd.
//
// The si_flags / ri_flags WASI fields encode out-of-band data bits,
// MSG_PEEK, MSG_DONTWAIT etc. None of those are meaningful on v1's
// single-threaded kernel with memory-backed socket buffers, so the
// handler accepts whatever low 16 bits the caller supplies and
// discards them — a userland caller that asks for SO_OOB gets a
// plain in-band send, no error.

fn handle_sock_send(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
    heap: &[u8],
) -> Response {
    let fd = args_u32(req, 0);
    let _si_flags = args_u32(req, 4) & 0xFFFF;

    let entry = match kernel.fds(pid) {
        Ok(t) => match t.get(fd) {
            Some(e) => *e,
            None => return Response::err(req.request_id, EBADF),
        },
        Err(e) => return Response::err(req.request_id, kerr_to_errno(e)),
    };
    let socket_id = match entry.object {
        FdObject::Socket(id) => id,
        _ => return Response::err(req.request_id, EINVAL),
    };
    let Some(bytes) = heap_in(req, heap) else {
        return Response::err(req.request_id, EINVAL);
    };
    match kernel
        .ipc
        .send_on_socket(crate::ipc::SocketId(socket_id), bytes, alloc::vec::Vec::new())
    {
        Ok(n) => Response::ok(req.request_id, n as i64),
        Err(e) => Response::err(
            req.request_id,
            kerr_to_errno(KernelError::from(e)),
        ),
    }
}

// ---- sock_accept ---------------------------------------------------
//
// Layout:
//   args[0..4] = listener_fd (u32)
//   args[4..8] = fdflags (u32, WASI encoding — applied to the new
//                fd via FdFlags::from_wasi_bits; typically NONBLOCK)
// Response:
//   value = freshly-allocated fd for the accepted connection.
//
// WASI alias of the existing IPC_ACCEPT (ext 0x1004). The handler
// forwards to Kernel::accept_socket — the same semantic the
// extension opcode reaches — and then applies the WASI fdflags to
// the new fd's FdEntry. Kernel::accept_socket already handles:
//   - non-Socket fd → NotSupportedOnFd → EINVAL
//   - unopened fd → BadFd → EBADF
//   - listener not in Listening state → InvalidState → EINVAL
//   - empty backlog → WouldBlock → EAGAIN
// so this handler is an extremely thin wrapper.

// ---- sock_shutdown -------------------------------------------------
//
// Layout:
//   args[0..4] = fd (u32)
//   args[4..8] = how (u32, low 8 bits = WASI sdflags: RD=0x1, WR=0x2)
// Response:
//   value = 0 on success; EINVAL on zero `how` or on any bits
//           beyond RD | WR; EINVAL on a non-Socket fd; EBADF on an
//           unopened fd.
//
// Half-close is now a first-class IpcTable primitive: v1 tracks
// `shutdown_read` and `shutdown_write` per Socket, and updates
// `send_on_socket` / `recv_on_socket` to honour them. Dispatch
// decodes the two sdflags bits and calls
// `IpcTable::shutdown_socket(id, read, write)`. After a successful
// call the fd stays open — subsequent fd_close still tears down
// via close_socket, which is distinct from a shutdown and is the
// path that actually reaps the kernel-side Socket entry.
//
// Post-shutdown semantics (per POSIX shutdown(2)):
//   * RD sets shutdown_read → `recv_on_socket` unconditionally
//     returns `(0, Vec::new())` EOF on this fd; peer's
//     `send_on_socket` returns `PipeBroken` since there's no reader.
//   * WR sets shutdown_write → `send_on_socket` returns
//     `PipeBroken` on this fd; peer's `recv_on_socket` observes
//     EOF once its rx buffer drains.
//   * RD | WR sets both of the above.
//
// Non-Socket FdObject → EINVAL; unopened fd → EBADF.

fn handle_sock_shutdown(kernel: &mut Kernel, pid: Pid, req: &Request) -> Response {
    let fd = args_u32(req, 0);
    let how = args_u32(req, 4) & 0xFF;

    let entry = match kernel.fds(pid) {
        Ok(t) => match t.get(fd) {
            Some(e) => *e,
            None => return Response::err(req.request_id, EBADF),
        },
        Err(e) => return Response::err(req.request_id, kerr_to_errno(e)),
    };
    let socket_id = match entry.object {
        FdObject::Socket(id) => id,
        _ => return Response::err(req.request_id, EINVAL),
    };

    let rd = abi::wasi::sdflags::RD as u32;
    let wr = abi::wasi::sdflags::WR as u32;
    // Zero how or bits beyond RD | WR = malformed request.
    if how == 0 || (how & !(rd | wr)) != 0 {
        return Response::err(req.request_id, EINVAL);
    }
    let read = (how & rd) != 0;
    let write = (how & wr) != 0;
    match kernel
        .ipc
        .shutdown_socket(crate::ipc::SocketId(socket_id), read, write)
    {
        Ok(()) => Response::ok(req.request_id, 0),
        Err(e) => Response::err(
            req.request_id,
            kerr_to_errno(KernelError::from(e)),
        ),
    }
}

fn handle_sock_accept(kernel: &mut Kernel, pid: Pid, req: &Request) -> Response {
    let listener_fd = args_u32(req, 0);
    let wasi_fdflags = args_u32(req, 4);

    let new_fd = match kernel.accept_socket(pid, listener_fd) {
        Ok(fd) => fd,
        Err(e) => return Response::err(req.request_id, kerr_to_errno(e)),
    };
    // Apply the WASI fdflags (APPEND/NONBLOCK after translation;
    // sync-family bits discarded; CLOEXEC never set). A zero
    // fdflags argument is a no-op — the new fd starts with
    // FdFlags::EMPTY from FdEntry::new inside accept_socket.
    if wasi_fdflags != 0 {
        let flags = FdFlags::from_wasi_bits(wasi_fdflags);
        if let Ok(table) = kernel.fds_mut(pid) {
            if let Some(entry) = table.get_mut(new_fd) {
                entry.flags = flags;
            }
        }
    }
    Response::ok(req.request_id, new_fd as i64)
}

fn handle_sock_recv(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> Response {
    let fd = args_u32(req, 0);
    let _ri_flags = args_u32(req, 4) & 0xFFFF;

    let entry = match kernel.fds(pid) {
        Ok(t) => match t.get(fd) {
            Some(e) => *e,
            None => return Response::err(req.request_id, EBADF),
        },
        Err(e) => return Response::err(req.request_id, kerr_to_errno(e)),
    };
    let socket_id = match entry.object {
        FdObject::Socket(id) => id,
        _ => return Response::err(req.request_id, EINVAL),
    };
    let max_len = req.heap_len as usize;
    let Some(buf) = heap_out_mut(req, heap, max_len) else {
        return Response::err(req.request_id, EINVAL);
    };
    match kernel
        .ipc
        .recv_on_socket(crate::ipc::SocketId(socket_id), buf, 0)
    {
        Ok((n, _fds)) => Response {
            request_id: req.request_id,
            status: 0,
            value: n as i64,
            extra_len: n as u32,
            _pad: [0u8; 12],
        },
        Err(e) => Response::err(
            req.request_id,
            kerr_to_errno(KernelError::from(e)),
        ),
    }
}

// ---- poll_oneoff -----------------------------------------------------
//
// Layout:
//   args[0..4]  = n_subs (u32; MUST be >= 1)
//   args[4..8]  = n_events_cap (u32; caller-provided max number of
//                 events the kernel may write back)
//   heap[0..n_subs*48]                        = input subscriptions
//                                                (48 bytes each; see
//                                                `abi::wasi::poll` for
//                                                the field offsets)
//   heap[n_subs*48..n_subs*48 + n_events*32]  = output events (32 bytes
//                                                each; kernel writes
//                                                up to n_events_cap)
//   heap_len    = n_subs*48 + n_events_cap*32 (caller sizes this)
// Response:
//   value     = n_events actually emitted (u32 widened to i64)
//   extra_len = same (mirrored so the shim can read it without
//               reinterpreting `value` as BigInt)
//
// ## v1 semantics: non-blocking only
//
// The real POSIX `poll()` sleeps the calling thread until at least one
// subscription fires or a timeout elapses. PMos's kernel is strictly
// single-threaded — the dispatcher runs each request to completion
// before picking up the next one — so a handler that blocks would
// stall every process sharing the kernel Worker. Instead, this
// handler does a one-shot readiness check and returns immediately:
//
//   * CLOCK subscriptions fire only if the target time has already
//     been reached. `ABSTIME` with a timeout in the past is ready;
//     a relative timeout of 0 is ready; any relative non-zero
//     timeout is reported as "not ready yet".
//   * FD_READ / FD_WRITE subscriptions fire only if the corresponding
//     operation would make progress right now — rx_buf non-empty for
//     a readable socket, peer_closed for EOF, available bytes < size
//     for a Vnode, etc.
//
// Userland is expected to spin: repeatedly call poll_oneoff until at
// least one event fires. This is the documented weaker-than-POSIX
// contract; a future slice that introduces real blocking (parking
// the process on the subscription's target condition and waking via
// the scheduler) keeps the same opcode shape and wire layout.
//
// ## Per-subscription errors vs syscall-level errors
//
// A bad subscription (unopened fd, unsupported clock id, bogus tag,
// unsupported fd-object-type combination) emits ONE event with
// `event.error` set to the appropriate errno. The whole syscall
// still returns success — a caller with a mix of good and bad subs
// still gets their good events alongside the bad ones. This matches
// WASI's documented semantics.
//
// Syscall-level EINVAL fires only for shape errors that prevent the
// handler from decoding the subscription list at all: n_subs == 0,
// or a heap that's too short to hold the declared subs + the caller-
// requested event window.

use crate::fd::FdEntry;
use crate::fs::devfs::{
    DEV_CONSOLE, DEV_FB0, DEV_INPUT_KBD, DEV_INPUT_MOUSE, DEV_NULL, DEV_RANDOM, DEV_ZERO,
};
use crate::ipc::{SocketId, SocketState};
use abi::errno::ENOENT;
use abi::wasi::eventrwflags::FD_READWRITE_HANGUP;
use abi::wasi::eventtype as et;
use abi::wasi::poll as pl;
use abi::wasi::subclockflags::ABSTIME;
use alloc::vec::Vec;

/// Build one 32-byte event slot from its logical fields.
fn build_event(userdata: u64, error: u16, ty: u8, nbytes: u64, flags: u16) -> [u8; pl::EVENT_SIZE] {
    let mut e = [0u8; pl::EVENT_SIZE];
    e[pl::EVENT_OFF_USERDATA..pl::EVENT_OFF_USERDATA + 8]
        .copy_from_slice(&userdata.to_le_bytes());
    e[pl::EVENT_OFF_ERROR..pl::EVENT_OFF_ERROR + 2]
        .copy_from_slice(&error.to_le_bytes());
    e[pl::EVENT_OFF_TYPE] = ty;
    e[pl::EVENT_OFF_RW_NBYTES..pl::EVENT_OFF_RW_NBYTES + 8]
        .copy_from_slice(&nbytes.to_le_bytes());
    e[pl::EVENT_OFF_RW_FLAGS..pl::EVENT_OFF_RW_FLAGS + 2]
        .copy_from_slice(&flags.to_le_bytes());
    e
}

/// Per-subscription readability / writability check. Returns
/// `(is_ready, nbytes, rwflags, per_sub_error)`. If `per_sub_error`
/// is `Some`, the caller emits an event with that errno regardless of
/// the boolean; it's a "meaningless subscription" signal (e.g. FD_READ
/// on a SignalChannel). If `is_ready` is true, the caller emits a
/// success event with the returned nbytes + flags.
fn fd_readiness(
    kernel: &mut Kernel,
    entry: &FdEntry,
    is_read: bool,
) -> (bool, u64, u16, Option<i32>) {
    match entry.object {
        FdObject::Vnode { mount_id, ino } => {
            let st = match kernel.vfs.stat_ino(mount_id, ino) {
                Ok(s) => s,
                Err(_) => return (false, 0, 0, Some(ENOENT)),
            };
            if is_read {
                if entry.offset < st.size {
                    (true, st.size - entry.offset, 0, None)
                } else {
                    // Past EOF is still "ready to read" per WASI —
                    // the caller's read() returns 0. Signal EOF via
                    // the HANGUP flag so the caller doesn't spin.
                    (true, 0, FD_READWRITE_HANGUP, None)
                }
            } else {
                (true, 0, 0, None)
            }
        }
        FdObject::CharDevice(devnum) => char_device_readiness(kernel, devnum, is_read),
        FdObject::Socket(id) => socket_readiness(kernel, SocketId(id), is_read),
        FdObject::PipeRead(_)
        | FdObject::PipeWrite(_)
        | FdObject::DisplayConn(_)
        | FdObject::SignalChannel => {
            // v1 doesn't wire these through fd_read / fd_write; poll
            // on them is meaningless → per-subscription EINVAL.
            (false, 0, 0, Some(EINVAL))
        }
    }
}

fn char_device_readiness(
    kernel: &mut Kernel,
    devnum: u32,
    is_read: bool,
) -> (bool, u64, u16, Option<i32>) {
    if is_read {
        match devnum {
            // null always returns 0 (EOF) — ready-with-hangup would be
            // a legal shape but most callers that poll /dev/null want
            // to know it's readable; keep it simple and flag hangup.
            DEV_NULL => (true, 0, FD_READWRITE_HANGUP, None),
            // zero / random are infinite readable sources.
            DEV_ZERO | DEV_RANDOM => (true, 0, 0, None),
            DEV_CONSOLE => {
                let len = kernel.devs.console_input_len();
                if len > 0 {
                    (true, len as u64, 0, None)
                } else {
                    (false, 0, 0, None)
                }
            }
            // No public length accessor for the input rings; report
            // not-ready. A future slice can expose a length on
            // DeviceDispatcher and wire them here.
            DEV_INPUT_KBD | DEV_INPUT_MOUSE => (false, 0, 0, None),
            // fb0 is write-only.
            DEV_FB0 => (false, 0, 0, Some(EINVAL)),
            _ => (false, 0, 0, Some(EINVAL)),
        }
    } else {
        match devnum {
            // null / zero / console / fb0 all accept writes without blocking.
            DEV_NULL | DEV_ZERO | DEV_CONSOLE | DEV_FB0 => (true, 0, 0, None),
            DEV_RANDOM | DEV_INPUT_KBD | DEV_INPUT_MOUSE => {
                (false, 0, 0, Some(EINVAL))
            }
            _ => (false, 0, 0, Some(EINVAL)),
        }
    }
}

fn socket_readiness(
    kernel: &mut Kernel,
    id: SocketId,
    is_read: bool,
) -> (bool, u64, u16, Option<i32>) {
    // Snapshot the fields we need under a scoped mut borrow so we
    // can release the borrow before looking at the peer.
    let (state, peer_opt, rx_len) = {
        let sock = match kernel.ipc.socket_mut(id) {
            Ok(s) => s,
            Err(_) => return (false, 0, 0, Some(EBADF)),
        };
        (sock.state, sock.peer, sock.rx_buf.len())
    };
    if state != SocketState::Connected {
        return (false, 0, 0, Some(EINVAL));
    }
    let (peer_closed, peer_free) = match peer_opt {
        Some(peer_id) => match kernel.ipc.socket_mut(peer_id) {
            Ok(p) => (p.closed, p.rx_cap.saturating_sub(p.rx_buf.len())),
            Err(_) => (true, 0),
        },
        None => (true, 0),
    };
    if is_read {
        if rx_len > 0 {
            (true, rx_len as u64, 0, None)
        } else if peer_closed {
            (true, 0, FD_READWRITE_HANGUP, None)
        } else {
            (false, 0, 0, None)
        }
    } else if peer_closed {
        (true, 0, FD_READWRITE_HANGUP, None)
    } else if peer_free > 0 {
        (true, peer_free as u64, 0, None)
    } else {
        (false, 0, 0, None)
    }
}

fn handle_poll_oneoff(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> Response {
    let n_subs = args_u32(req, 0);
    let n_events_cap = args_u32(req, 4);
    if n_subs == 0 {
        return Response::err(req.request_id, EINVAL);
    }
    let subs_bytes = match (n_subs as usize).checked_mul(pl::SUBSCRIPTION_SIZE) {
        Some(n) => n,
        None => return Response::err(req.request_id, EINVAL),
    };
    let events_bytes = match (n_events_cap as usize).checked_mul(pl::EVENT_SIZE) {
        Some(n) => n,
        None => return Response::err(req.request_id, EINVAL),
    };
    let total_bytes = match subs_bytes.checked_add(events_bytes) {
        Some(n) => n,
        None => return Response::err(req.request_id, EINVAL),
    };
    let heap_len = req.heap_len as usize;
    let heap_ptr = req.heap_ptr as usize;
    // Events are written starting at `heap_ptr + 0`, overwriting the
    // subscriptions (which the handler copies out into `subs` before
    // touching the output region). The heap therefore only needs to
    // fit the larger of the two windows — in practice that's always
    // `subs_bytes`, since WASI's signature sizes the output buffer as
    // `nsubscriptions` events and a 48-byte sub is bigger than a
    // 32-byte event — but the check is written against `max` so a
    // caller that passes `n_events_cap > n_subs` is still rejected
    // cleanly when the events window doesn't fit.
    let needed = core::cmp::max(subs_bytes, events_bytes);
    if heap_len < needed {
        return Response::err(req.request_id, EINVAL);
    }
    let _ = total_bytes; // silence unused — kept for future growth.
    let heap_end = match heap_ptr.checked_add(needed) {
        Some(n) => n,
        None => return Response::err(req.request_id, EINVAL),
    };
    if heap_end > heap.len() {
        return Response::err(req.request_id, EINVAL);
    }

    // Copy the subscription bytes out before we touch the output
    // region, so writing events doesn't alias the input reads.
    let mut subs: Vec<[u8; pl::SUBSCRIPTION_SIZE]> = Vec::with_capacity(n_subs as usize);
    for i in 0..(n_subs as usize) {
        let base = heap_ptr + i * pl::SUBSCRIPTION_SIZE;
        let mut arr = [0u8; pl::SUBSCRIPTION_SIZE];
        arr.copy_from_slice(&heap[base..base + pl::SUBSCRIPTION_SIZE]);
        subs.push(arr);
    }

    let mut events: Vec<[u8; pl::EVENT_SIZE]> = Vec::with_capacity(n_subs as usize);
    for sub in &subs {
        if events.len() >= n_events_cap as usize {
            break;
        }
        let mut ud_bytes = [0u8; 8];
        ud_bytes.copy_from_slice(&sub[pl::SUB_OFF_USERDATA..pl::SUB_OFF_USERDATA + 8]);
        let userdata = u64::from_le_bytes(ud_bytes);
        let tag = sub[pl::SUB_OFF_TAG];
        match tag {
            et::CLOCK => {
                let mut cid_bytes = [0u8; 4];
                cid_bytes.copy_from_slice(&sub[pl::SUB_CLOCK_OFF_ID..pl::SUB_CLOCK_OFF_ID + 4]);
                let clock_id = u32::from_le_bytes(cid_bytes);
                let mut to_bytes = [0u8; 8];
                to_bytes.copy_from_slice(
                    &sub[pl::SUB_CLOCK_OFF_TIMEOUT..pl::SUB_CLOCK_OFF_TIMEOUT + 8],
                );
                let timeout = u64::from_le_bytes(to_bytes);
                let mut fl_bytes = [0u8; 2];
                fl_bytes.copy_from_slice(
                    &sub[pl::SUB_CLOCK_OFF_FLAGS..pl::SUB_CLOCK_OFF_FLAGS + 2],
                );
                let flags = u16::from_le_bytes(fl_bytes);
                let now_opt = match clock_id {
                    abi::wasi::CLOCKID_MONOTONIC => Some(platform::current().now_ns()),
                    abi::wasi::CLOCKID_REALTIME => Some(platform::current().now_realtime_ns()),
                    abi::wasi::CLOCKID_PROCESS_CPUTIME_ID
                    | abi::wasi::CLOCKID_THREAD_CPUTIME_ID => {
                        events.push(build_event(userdata, ENOTSUP as u16, et::CLOCK, 0, 0));
                        continue;
                    }
                    _ => {
                        events.push(build_event(userdata, EINVAL as u16, et::CLOCK, 0, 0));
                        continue;
                    }
                };
                let now = now_opt.unwrap();
                let is_ready = if flags & ABSTIME != 0 {
                    now >= timeout
                } else {
                    timeout == 0
                };
                if is_ready {
                    events.push(build_event(userdata, 0, et::CLOCK, 0, 0));
                }
            }
            et::FD_READ | et::FD_WRITE => {
                let mut fd_bytes = [0u8; 4];
                fd_bytes.copy_from_slice(&sub[pl::SUB_FDRW_OFF_FD..pl::SUB_FDRW_OFF_FD + 4]);
                let fd = u32::from_le_bytes(fd_bytes);
                let entry_opt = match kernel.fds(pid) {
                    Ok(t) => t.get(fd).copied(),
                    Err(_) => None,
                };
                let Some(entry) = entry_opt else {
                    events.push(build_event(userdata, EBADF as u16, tag, 0, 0));
                    continue;
                };
                let (ready, nbytes, rwflags, err_opt) =
                    fd_readiness(kernel, &entry, tag == et::FD_READ);
                if let Some(errno_code) = err_opt {
                    events.push(build_event(userdata, errno_code as u16, tag, 0, 0));
                } else if ready {
                    events.push(build_event(userdata, 0, tag, nbytes, rwflags));
                }
            }
            _ => {
                events.push(build_event(userdata, EINVAL as u16, tag, 0, 0));
            }
        }
    }

    // Write events sequentially starting at `heap_ptr + 0`,
    // overwriting the subscription window (which is no longer live
    // because `subs` holds the copy). The caller's shim reads the
    // events straight out of the same heap region.
    let n_events = events.len();
    for (i, e) in events.iter().enumerate() {
        let start = heap_ptr + i * pl::EVENT_SIZE;
        let end = start + pl::EVENT_SIZE;
        heap[start..end].copy_from_slice(e);
    }

    Response {
        request_id: req.request_id,
        status: 0,
        value: n_events as i64,
        extra_len: (n_events * pl::EVENT_SIZE) as u32,
        _pad: [0u8; 12],
    }
}
