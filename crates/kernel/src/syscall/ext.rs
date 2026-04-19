//! PMos extension opcode handlers.
//!
//! Same shape as [`super::wasi`]: each handler decodes a
//! [`Request`], calls one method (or one field accessor) on
//! [`crate::sys::Kernel`], and encodes a [`Response`]. Extension
//! handlers tend to be shorter than WASI handlers because the
//! majority of them either return a single scalar or require no
//! heap payload at all.
//!
//! ## Coverage
//!
//! As of the SIGCHLD delivery batch, the dispatcher routes the
//! following extension opcodes (`contracts/syscalls.md §3`):
//! `IPC_SOCKET`, `IPC_BIND`, `IPC_LISTEN`, `IPC_CONNECT`,
//! `IPC_ACCEPT`, `IPC_PIPE`, `PROC_SELF`, `PROC_PARENT`,
//! `PROC_SPAWN`, `PROC_WAIT`, `PROC_KILL`, `PROC_CAPS_GET`,
//! `DISPLAY_CONNECT`, `DISPLAY_BIND`, `CAP_CHECK`, `CAP_LIST`.
//!
//! The remaining extension opcodes fall through the `_ =>` arm
//! and return `ENOSYS`:
//!
//! * `IPC_SEND` / `IPC_RECV` (0x1005/0x1006) — fd-passing
//!   primitives; deferred until the IpcTable grows per-send fd
//!   queues. `FD_READ` / `FD_WRITE` already cover the
//!   send/recv-without-fd-passing case via the `FdObject::Socket`
//!   arm, so the send/recv opcodes are the fd-passing-only path.
//! * `CAP_GRANT` (0x1302) — intentionally deferred to v2 per the
//!   `known_ext_opcode_without_handler_returns_enosys` probe
//!   comment in `crates/kernel/tests/syscall.rs`; delegating
//!   cap edits to userland is out of scope for v1.
//! * `MOUNT` / `UMOUNT` (0x1400/0x1401) — the semantic surface
//!   exists on `Kernel::vfs.mount` / `umount`, but a fstype →
//!   `Box<dyn Filesystem>` factory and the privilege / config
//!   plumbing isn't yet designed.
//! * `FS_WATCH` (0x1402) — needs an event queue + watch-point
//!   notification machinery that doesn't exist yet.
//! * `HOST_FILE_RECV` (0x1500) — needs the drag-drop token
//!   table + driver plumbing described in
//!   `contracts/syscalls.md §3.6`.
//!
//! `CAP_GRANT` is also the canonical ext-range ENOSYS probe
//! target — see `known_ext_opcode_without_handler_returns_enosys`
//! in the syscall isolation suite.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use abi::cap::{Cap, CapSet};
use abi::errno::{EAGAIN, ECHILD, EINVAL, ENOSYS, ESRCH};
use abi::ext::{self as op, Pid};
use abi::ring::{Request, Response};

use crate::ipc::SocketType;
use crate::platform;
use crate::proc::{ExitStatus, Signal};
use crate::sys::{Kernel, SpawnArgs, WaitOutcome, WaitTarget};

use super::dispatch::{
    args_u16, args_u32, args_u64, heap_in, heap_out_mut, kerr_to_errno, ServiceOutcome,
};

/// Dispatch a request whose opcode is in the PMos extension range.
/// The caller has already guarded with [`abi::ext::is_ext`].
pub fn dispatch_ext(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> ServiceOutcome {
    match req.opcode {
        op::IPC_SOCKET => ServiceOutcome::Done(handle_ipc_socket(kernel, pid, req)),
        op::IPC_BIND => ServiceOutcome::Done(handle_ipc_bind(kernel, pid, req, heap)),
        op::IPC_LISTEN => ServiceOutcome::Done(handle_ipc_listen(kernel, pid, req)),
        op::IPC_CONNECT => ServiceOutcome::Done(handle_ipc_connect(kernel, pid, req, heap)),
        op::IPC_ACCEPT => handle_ipc_accept(kernel, pid, req),
        op::IPC_PIPE => ServiceOutcome::Done(handle_ipc_pipe(kernel, pid, req, heap)),
        op::PROC_SELF => ServiceOutcome::Done(handle_proc_self(pid, req)),
        op::PROC_PARENT => ServiceOutcome::Done(handle_proc_parent(kernel, pid, req)),
        op::CAP_CHECK => ServiceOutcome::Done(handle_cap_check(kernel, pid, req)),
        op::CAP_LIST => ServiceOutcome::Done(handle_cap_list(kernel, pid, req)),
        op::PROC_SPAWN => ServiceOutcome::Done(handle_proc_spawn(kernel, pid, req, heap)),
        op::PROC_WAIT => ServiceOutcome::Done(handle_proc_wait(kernel, pid, req, heap)),
        op::PROC_KILL => ServiceOutcome::Done(handle_proc_kill(kernel, pid, req)),
        op::PROC_CAPS_GET => ServiceOutcome::Done(handle_proc_caps_get(kernel, pid, req)),
        op::DISPLAY_CONNECT => ServiceOutcome::Done(handle_display_connect(kernel, pid, req)),
        op::DISPLAY_BIND => ServiceOutcome::Done(handle_display_bind(kernel, pid, req)),
        _ => ServiceOutcome::Done(Response::err(req.request_id, ENOSYS)),
    }
}

// ---- proc_self ---------------------------------------------------------
//
// Layout: no args, no heap.
// Response: value = caller's pid (widened to i64).
//
// The cheapest possible syscall — every piece of information it
// needs is already in the dispatcher frame.

fn handle_proc_self(pid: Pid, req: &Request) -> Response {
    Response::ok(req.request_id, pid as i64)
}

// ---- proc_parent -------------------------------------------------------
//
// Layout: no args, no heap.
// Response: value = ppid (widened to i64). A process whose parent
//           has been reaped sees `ppid == 0`, same as init's own
//           parent slot.
//
// Returns `ESRCH` only if the caller's own pid has been removed
// from the process table — which in normal execution is
// impossible (the dispatcher itself requires the pid to exist),
// so this arm is a defence-in-depth guard for tests that
// deliberately remove the process mid-flight.

fn handle_proc_parent(kernel: &Kernel, pid: Pid, req: &Request) -> Response {
    match kernel.procs.get(pid) {
        Some(p) => Response::ok(req.request_id, p.ppid as i64),
        None => Response::err(req.request_id, ESRCH),
    }
}

// ---- cap_check ---------------------------------------------------------
//
// Layout:
//   args[0..4] = cap id (u32, one of the `Cap` discriminants)
// Response:
//   value      = 1 if the caller holds that cap, 0 otherwise
//   status     = EINVAL if the cap id doesn't correspond to a known
//                cap variant (guards against userland typos and
//                forward-compatibility probing)

fn handle_cap_check(kernel: &Kernel, pid: Pid, req: &Request) -> Response {
    let cap_id = args_u32(req, 0);
    let Some(cap) = Cap::from_u32(cap_id) else {
        return Response::err(req.request_id, EINVAL);
    };
    match kernel.caps.list(pid) {
        Ok(set) => Response::ok(req.request_id, set.contains(cap) as i64),
        Err(_) => Response::err(req.request_id, ESRCH),
    }
}

// ---- cap_list ----------------------------------------------------------
//
// Layout: no args, no heap.
// Response: value = u64 bitset of the caller's caps, transported
//           as i64 (the high bit is unused — caps fit in the low
//           ~10 bits today and have 54 bits of headroom before
//           the sign bit becomes a concern).
//
// Userland converts back to a `CapSet` by reinterpreting the i64
// value as u64.

fn handle_cap_list(kernel: &Kernel, pid: Pid, req: &Request) -> Response {
    match kernel.caps.list(pid) {
        Ok(set) => Response::ok(req.request_id, set.0 as i64),
        Err(_) => Response::err(req.request_id, ESRCH),
    }
}

// ---- ipc_socket --------------------------------------------------------
//
// Layout:
//   args[0..4] = socket type (u32): 0 = Stream, 1 = Dgram
//                (matches `abi::ext::SocketType` discriminants)
// Response:
//   value = fresh socket fd
//
// No cap check — any process can create sockets. What the socket
// can then bind or connect to is constrained by per-path cap
// gating (today: only `/run/display` has gating via
// `display_bind` / `display_connect`; every other path is open).

fn handle_ipc_socket(kernel: &mut Kernel, pid: Pid, req: &Request) -> Response {
    let ty_bits = args_u32(req, 0);
    let ty = match ty_bits {
        0 => SocketType::Stream,
        1 => SocketType::Dgram,
        _ => return Response::err(req.request_id, EINVAL),
    };
    match kernel.ipc_socket(pid, ty) {
        Ok(fd) => Response::ok(req.request_id, fd as i64),
        Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
    }
}

// ---- ipc_bind ----------------------------------------------------------
//
// Layout:
//   args[0..4] = fd (u32) referring to an unbound socket
//   heap_ptr / heap_len = UTF-8 bind path (not a filesystem path;
//                          lives in the IpcTable bindings map)
// Response:
//   value = 0 on success

fn handle_ipc_bind(kernel: &mut Kernel, pid: Pid, req: &Request, heap: &[u8]) -> Response {
    let fd = args_u32(req, 0);
    let Some(path_bytes) = heap_in(req, heap) else {
        return Response::err(req.request_id, EINVAL);
    };
    let Ok(path) = core::str::from_utf8(path_bytes) else {
        return Response::err(req.request_id, EINVAL);
    };
    match kernel.ipc_bind(pid, fd, path) {
        Ok(()) => Response::ok(req.request_id, 0),
        Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
    }
}

// ---- ipc_listen --------------------------------------------------------
//
// Layout:
//   args[0..4] = fd (u32) referring to a bound socket
//   args[4..8] = backlog (u32) — number of pending connections the
//                 listener can queue before a connect is refused.
//                 Ignored for dgram sockets (they don't listen).
// Response:
//   value = 0 on success

fn handle_ipc_listen(kernel: &mut Kernel, pid: Pid, req: &Request) -> Response {
    let fd = args_u32(req, 0);
    let backlog = args_u32(req, 4) as usize;
    match kernel.ipc_listen(pid, fd, backlog) {
        Ok(()) => Response::ok(req.request_id, 0),
        Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
    }
}

// ---- ipc_connect -------------------------------------------------------
//
// Layout:
//   args[0..4] = fd (u32) referring to an unbound socket
//   heap_ptr / heap_len = UTF-8 path of the target listener
// Response:
//   value = 0 on success
//
// After a successful connect, the fd can be read/written via the
// existing FD_READ / FD_WRITE opcodes; those already handle
// FdObject::Socket correctly.

fn handle_ipc_connect(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
    heap: &[u8],
) -> Response {
    let fd = args_u32(req, 0);
    let Some(path_bytes) = heap_in(req, heap) else {
        return Response::err(req.request_id, EINVAL);
    };
    let Ok(path) = core::str::from_utf8(path_bytes) else {
        return Response::err(req.request_id, EINVAL);
    };
    match kernel.ipc_connect(pid, fd, path) {
        Ok(()) => Response::ok(req.request_id, 0),
        Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
    }
}

// ---- ipc_accept --------------------------------------------------------
//
// Layout:
//   args[0..4] = listener fd (u32)
//   args[4..6] = flags (u16) — accept_flags::NONBLOCK bit toggles
//                the -EAGAIN-on-empty-backlog semantic. flags=0
//                blocks the caller until a client connects.
// Response:
//   value = freshly-allocated server-side fd of the accepted
//           connection, or -EAGAIN if the listener has no pending
//           clients and NONBLOCK is set.
// Parked (new in slice 2a):
//   No response is produced when flags=0 and backlog is empty;
//   instead the caller is parked on the listener via
//   `Kernel::park_on_accept`. A later `ipc_connect` from a peer
//   unparks the caller through `Kernel.pending_wakes` +
//   `kernel_take_next_wake_for_pid`.

fn handle_ipc_accept(kernel: &mut Kernel, pid: Pid, req: &Request) -> ServiceOutcome {
    use abi::ext::accept_flags;
    let listener_fd = args_u32(req, 0);
    let flags = args_u16(req, 4);
    let nonblock = (flags & accept_flags::NONBLOCK) != 0;

    match kernel.accept_socket(pid, listener_fd) {
        Ok(fd) => ServiceOutcome::Done(Response::ok(req.request_id, fd as i64)),
        Err(crate::sys::KernelError::WouldBlock) if nonblock => {
            ServiceOutcome::Done(Response::err(req.request_id, EAGAIN))
        }
        Err(crate::sys::KernelError::WouldBlock) => {
            match kernel.park_on_accept(pid, listener_fd, req.request_id) {
                Ok(()) => ServiceOutcome::Parked,
                Err(crate::sys::KernelError::WouldBlock) => {
                    ServiceOutcome::Done(Response::err(req.request_id, EAGAIN))
                }
                Err(e) => ServiceOutcome::Done(Response::err(
                    req.request_id,
                    kerr_to_errno(e),
                )),
            }
        }
        Err(e) => ServiceOutcome::Done(Response::err(req.request_id, kerr_to_errno(e))),
    }
}

// ---- ipc_pipe ---------------------------------------------------------
//
// Layout: no args. heap[0..8] is an output-only scratch region.
// Response:
//   value     = 0 on success
//   extra_len = 8 (the kernel wrote read_fd at heap[0..4] and
//                write_fd at heap[4..8] as little-endian u32s)
//
// Mirrors POSIX `pipe(2)`: create a pipe, install both ends on the
// caller. After success the caller owns two fds — the PipeRead at
// [0..4] and the PipeWrite at [4..8]. Either end can be dup'd into
// a child via PROC_SPAWN stdio inheritance; both ends honour the
// full fd_read / fd_write / fd_close surface.
//
// heap_len < 8 is rejected BEFORE any fd allocation so a malformed
// shim doesn't leak a half-installed pipe. If the second alloc
// fails mid-call (fd-limit exhaustion), Kernel::create_pipe_fds
// rolls back the first install and drops both pipe refs.

fn handle_ipc_pipe(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> Response {
    if (req.heap_len as usize) < 8 {
        return Response::err(req.request_id, EINVAL);
    }
    let (read_fd, write_fd) = match kernel.create_pipe_fds(pid) {
        Ok(pair) => pair,
        Err(e) => return Response::err(req.request_id, kerr_to_errno(e)),
    };
    let Some(out) = heap_out_mut(req, heap, 8) else {
        // Roll back if heap is out of range post-alloc. In practice
        // the earlier heap_len check should've caught this, but
        // heap_out_mut also guards heap_ptr + len <= heap.len().
        if let Ok(table) = kernel.fds_mut(pid) {
            let _ = table.close(read_fd);
            let _ = table.close(write_fd);
        }
        return Response::err(req.request_id, EINVAL);
    };
    out[0..4].copy_from_slice(&read_fd.to_le_bytes());
    out[4..8].copy_from_slice(&write_fd.to_le_bytes());
    Response {
        request_id: req.request_id,
        status: 0,
        value: 0,
        extra_len: 8,
        _pad: [0u8; 12],
    }
}

// ---- display_connect --------------------------------------------------
//
// Layout: no args, no heap.
// Response: value = freshly-allocated fd of the client-side
//           connection to the display server.
//
// This is a convenience wrapper over `ipc_socket` + `ipc_connect`
// with a hardcoded path (`/run/display`) and a baked-in cap check
// (`Cap::DisplayClient`). A userland toolkit that wants to draw
// windows calls this once at startup; every subsequent message
// goes through `fd_write` on the returned fd.

fn handle_display_connect(kernel: &mut Kernel, pid: Pid, req: &Request) -> Response {
    match kernel.display_connect(pid) {
        Ok(fd) => Response::ok(req.request_id, fd as i64),
        Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
    }
}

// ---- display_bind -----------------------------------------------------
//
// Layout: no args, no heap.
// Response: value = freshly-allocated listener fd bound to
//           `/run/display` and transitioned to `Listening` state
//           with the kernel's `DISPLAY_LISTEN_BACKLOG` depth.
//
// Symmetric with `DISPLAY_CONNECT`: that opcode is
// `socket + connect("/run/display")` with a `DisplayClient`
// cap check. This opcode is
// `socket + bind("/run/display") + listen(backlog)` with a
// `DisplayServer` cap check. Only the display-server userland
// process holds `DisplayServer`, so `/run/display` has exactly
// one owner.
//
// The returned fd is a standard socket fd — the display server
// calls `ipc_accept` on it to pop each new client, then
// `fd_read` / `fd_write` to speak the display protocol over
// each accepted connection.

fn handle_display_bind(kernel: &mut Kernel, pid: Pid, req: &Request) -> Response {
    match kernel.display_bind(pid) {
        Ok(fd) => Response::ok(req.request_id, fd as i64),
        Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
    }
}

// ---- proc_spawn --------------------------------------------------------
//
// Layout:
//   args[0..4]  = path_len (u32)
//   args[4..12] = caps bitset (u64, LE)
//   args[12..16] = reserved (0)
//   heap[req.heap_ptr .. req.heap_ptr + path_len] = UTF-8 binary path
//
// Response:
//   value       = new pid (positive) on success
//
// This slice's manifest is deliberately minimal: path + caps + implicit
// inheritance of the parent's fd 0/1/2 into the child's stdin/stdout/
// stderr. argv / envp / cwd / extra fd dups / explicit stdio-redirect
// all live in the full `SpawnManifest` shape defined in
// `abi::ext::SpawnManifest` and will be added as the first userland
// program that actually needs them lands. The wire format grows
// backwards-compatibly: new fields get appended to the heap payload
// after the path, with a length prefix pattern the existing callers
// can ignore.
//
// Two-stage protocol:
//
//   1. Call `Kernel::proc_spawn` to get a new pid with its
//      process-table entry, cap set, fd table, and stdio fds all
//      wired up. At this point the process is `Ready` on the
//      scheduler but has no Worker backing it.
//   2. Call `Platform::spawn_process(new_pid, path)` to ask the host
//      to actually instantiate a user Worker. If the host accepts,
//      we return the new pid and userland is happy. If the host
//      rejects, we roll back the process-table entry by marking it
//      Zombie and reaping it so no pid is leaked on a failed spawn.

fn handle_proc_spawn(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
    heap: &[u8],
) -> Response {
    let path_len = args_u32(req, 0) as usize;
    let caps = CapSet(args_u64(req, 4));

    let Some(path_bytes) = heap_in(req, heap) else {
        return Response::err(req.request_id, EINVAL);
    };
    if path_bytes.len() != path_len {
        return Response::err(req.request_id, EINVAL);
    }
    let Ok(path) = core::str::from_utf8(path_bytes) else {
        return Response::err(req.request_id, EINVAL);
    };

    // Inherit the parent's stdin/stdout/stderr by cloning its fd
    // objects. If the parent doesn't have one of those slots, the
    // child gets nothing installed there — the `SpawnArgs` contract
    // requires explicit fd objects, so we have to fabricate a
    // sentinel in the "parent doesn't have fd X" case. For this
    // slice we refuse to spawn when stdio is missing so the
    // opcode-level API stays strict; apps that want to inherit
    // partial stdio can install the missing slots on themselves
    // first.
    let parent_fds = match kernel.fds(pid) {
        Ok(t) => t,
        Err(_) => return Response::err(req.request_id, ESRCH),
    };
    let Some(stdin) = parent_fds.get(0).map(|e| e.object) else {
        return Response::err(req.request_id, EINVAL);
    };
    let Some(stdout) = parent_fds.get(1).map(|e| e.object) else {
        return Response::err(req.request_id, EINVAL);
    };
    let Some(stderr) = parent_fds.get(2).map(|e| e.object) else {
        return Response::err(req.request_id, EINVAL);
    };

    let spawn_args = SpawnArgs {
        name: path,
        caps,
        cwd: "/",
        argv: Vec::new(),
        envp: BTreeMap::new(),
        stdin,
        stdout,
        stderr,
    };
    let new_pid = match kernel.proc_spawn(pid, spawn_args) {
        Ok(p) => p,
        Err(e) => return Response::err(req.request_id, kerr_to_errno(e)),
    };

    // Ask the host to actually spawn a Worker for `new_pid`. On
    // failure, roll back by marking the process `Zombie` and
    // reaping it so no pid is leaked on a half-done spawn.
    if platform::current().spawn_process(new_pid, path).is_err() {
        let _ = kernel.proc_exit(new_pid, ExitStatus::Exited(-1));
        let _ = kernel.reap(new_pid);
        return Response::err(req.request_id, abi::errno::EIO);
    }

    Response::ok(req.request_id, new_pid as i64)
}

// ---- proc_wait --------------------------------------------------------
//
// Layout:
//   args[0..4] = target_pid (i32). 0 or -1 → any child; > 0 → specific
//                pid; < -1 → EINVAL.
//   args[4..8] = options (u32). Only WNOHANG is recognised; other bits
//                → EINVAL.
//   heap_ptr   = start of the optional 4-byte child-pid scratch.
//   heap_len   = 0 (caller doesn't need the pid back) or 4 (writes the
//                reaped child's pid as u32 LE).
// Response:
//   value      = packed status: low 32 bits = exit code (i32); bits
//                32..40 = signum (u8, 0 if Exited); bits 40..48 =
//                flags (0x01 Exited, 0x02 Signaled, 0x04 Crashed).
//   extra_len  = 4 when the caller requested the pid readback, 0
//                otherwise.
// Errors:
//   EAGAIN     = live children exist but none are zombies. v1 always
//                returns non-blocking — the caller retries.
//   ECHILD     = no child matching the target, or target == sender pid.
//   EINVAL     = malformed target or unknown options bits.
//
// Self-wait is rejected with ECHILD rather than EDEADLK because POSIX
// waitpid(pid == self) semantically means "a pid that can't be my
// child" — ECHILD matches what Linux returns. Process-group wait
// (target < -1) is out of scope for v1's flat process model; returning
// EINVAL steers userland away from a feature we don't implement.

fn pack_exit_status(status: ExitStatus) -> i64 {
    match status {
        ExitStatus::Exited(code) => {
            let low = (code as u32) as u64;
            ((0x01_u64) << 40) | low as u64
        }
        ExitStatus::Signaled(sig) => {
            // Signum lives in bits 32..40; flags 0x02 in 40..48.
            let sig = (sig as u64) & 0xff;
            ((0x02_u64) << 40) | (sig << 32)
        }
        ExitStatus::Crashed => (0x04_u64) << 40,
    }
    .try_into()
    .unwrap_or(i64::MAX)
}

fn handle_proc_wait(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> Response {
    let target_pid = i32::from_le_bytes([req.args[0], req.args[1], req.args[2], req.args[3]]);
    let options = args_u32(req, 4);
    if options & !abi::ext::wait_opts::WNOHANG != 0 {
        return Response::err(req.request_id, EINVAL);
    }
    if target_pid < -1 {
        return Response::err(req.request_id, EINVAL);
    }
    let target = if target_pid == 0 || target_pid == -1 {
        WaitTarget::Any
    } else {
        if target_pid == pid {
            return Response::err(req.request_id, ECHILD);
        }
        WaitTarget::Specific(target_pid)
    };

    match kernel.proc_wait(pid, target) {
        Ok(WaitOutcome::Reaped(child, status)) => {
            let packed = pack_exit_status(status);
            let mut resp = Response::ok(req.request_id, packed);
            if (req.heap_len as usize) >= 4 {
                if let Some(out) = heap_out_mut(req, heap, 4) {
                    out[0..4].copy_from_slice(&(child as u32).to_le_bytes());
                    resp.extra_len = 4;
                }
            }
            resp
        }
        Ok(WaitOutcome::WouldBlock) => Response::err(req.request_id, EAGAIN),
        Ok(WaitOutcome::NoChildren) => Response::err(req.request_id, ECHILD),
        Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
    }
}

// ---- proc_kill --------------------------------------------------------
//
// Layout:
//   args[0..4] = target_pid (i32).
//   args[4..6] = signum (u16). v1 accepts {0, 2, 9, 13, 15, 17}:
//                0 = POSIX kill(pid, 0) existence + permission
//                    probe — runs every precondition proc_kill
//                    would run but delivers no signal.
//                2 = SIGINT, 9 = SIGKILL, 13 = SIGPIPE,
//                15 = SIGTERM, 17 = SIGCHLD. Any other number →
//                EINVAL before the kernel is touched (a future
//                SIGHUP / SIGQUIT wiring extends this match
//                without a wire-format break).
//
// Response: value = 0 on success; negative errno on failure.
//
// Cap rules are enforced by Kernel::proc_kill (for signum != 0)
// or Kernel::proc_check_signal (for signum == 0): sender must be
// target's parent OR sender == target (self-signal) OR hold
// Cap::ProcKillAny. Other cases → -ENOTCAPABLE. Non-existent or
// already-reaped target → -ESRCH.

fn handle_proc_kill(kernel: &mut Kernel, pid: Pid, req: &Request) -> Response {
    let target_pid = i32::from_le_bytes([req.args[0], req.args[1], req.args[2], req.args[3]]);
    let signum = u16::from_le_bytes([req.args[4], req.args[5]]);
    if signum == 0 {
        // POSIX kill(pid, 0): existence + permission probe only.
        return match kernel.proc_check_signal(pid, target_pid) {
            Ok(()) => Response::ok(req.request_id, 0),
            Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
        };
    }
    let signal = match signum {
        2 => Signal::Interrupt,
        9 => Signal::Kill,
        13 => Signal::Pipe,
        15 => Signal::Term,
        17 => Signal::Child,
        _ => return Response::err(req.request_id, EINVAL),
    };
    match kernel.proc_kill(pid, target_pid, signal) {
        Ok(()) => Response::ok(req.request_id, 0),
        Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
    }
}

// ---- proc_caps_get ----------------------------------------------------
//
// Layout:
//   args[0..4] = target_pid (i32).
// Response: value = CapSet as i64 on success; negative errno on
// failure. The CapSet is always a u64 bitset on the Rust side; the
// cast to i64 preserves every bit (bits 63..0 round-trip verbatim).
//
// Cap rules are enforced by Kernel::proc_caps_get: sender can
// query own caps freely; querying another pid requires parent-
// relationship OR Cap::ProcInspect. Non-existent / reaped target
// → -ESRCH.

fn handle_proc_caps_get(kernel: &Kernel, pid: Pid, req: &Request) -> Response {
    let target_pid = i32::from_le_bytes([req.args[0], req.args[1], req.args[2], req.args[3]]);
    match kernel.proc_caps_get(pid, target_pid) {
        Ok(caps) => Response::ok(req.request_id, caps.0 as i64),
        Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
    }
}
