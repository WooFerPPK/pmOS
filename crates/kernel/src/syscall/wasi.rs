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
        op::FD_PRESTAT_GET => handle_fd_prestat_get(req),
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
//   args[0..4]  = fd flags (u32, as raw `FdFlags::from_bits` bits)
//   heap_ptr    = start of UTF-8 path
//   heap_len    = path length in bytes
// Response:
//   value       = freshly-allocated fd number (u32 widened to i64)
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
    let flags_bits = args_u32(req, 0);
    let flags = FdFlags::from_bits(flags_bits);
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
//   args[4..8] = lookup flags (u32, ignored — only
//                LOOKUP_SYMLINK_FOLLOW=0x1 is defined, and v1's VFS
//                doesn't follow symlinks either way, so the flag
//                is a no-op regardless of its value)
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
    // dir_fd + lookup flags are part of the documented wire layout
    // but unused in v1 (no preopens, no symlink following); the
    // reads pin the layout so a future slice can use them without
    // a wire-format break.
    let _dir_fd = args_u32(req, 0);
    let _lookup_flags = args_u32(req, 4);
    let Some(path_bytes) = heap_in(req, heap) else {
        return Response::err(req.request_id, EINVAL);
    };
    let Ok(path) = core::str::from_utf8(path_bytes) else {
        return Response::err(req.request_id, EINVAL);
    };
    let (mount_id, ino) = match kernel.vfs.resolve(path) {
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
