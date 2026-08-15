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
//! As of the HOST_FILE_RECV slice, the dispatcher routes every
//! extension opcode (`contracts/syscalls.md §3`):
//! `IPC_SOCKET`, `IPC_BIND`, `IPC_LISTEN`, `IPC_CONNECT`,
//! `IPC_ACCEPT`, `IPC_SEND`, `IPC_RECV`, `IPC_PIPE`, `IPC_PEER_CAPS`,
//! `IPC_PEER_PID`, `PROC_SELF`,
//! `PROC_PARENT`, `PROC_SPAWN`, `PROC_WAIT`, `PROC_KILL`,
//! `PROC_CAPS_GET`, `DISPLAY_CONNECT`, `DISPLAY_BIND`,
//! `CAP_CHECK`, `CAP_LIST`, `CAP_GRANT`, `MOUNT`, `UMOUNT`,
//! `FS_WATCH`, `HOST_FILE_RECV`.
//!
//! Every opcode in `abi::ext::FIRST..LAST_EXCL` now has a
//! handler — the `_ =>` arm exists only to catch reserved-but-
//! unallocated opcode numbers in the gaps between the subsystem
//! groups (e.g. `0x100a` between `IPC_PEER_PID` and `PROC_SPAWN`,
//! `0x1106..0x11ff` between the last `proc_*` opcode and
//! `DISPLAY_CONNECT`, etc.). The
//! `known_ext_opcode_without_handler_returns_enosys` test was
//! rotated to a synthetic unallocated extension-range opcode
//! (`0x100a`) so the `_ =>` arm stays covered against future
//! regressions; once a future opcode lands at any unallocated
//! number, that test should be re-rotated to a still-unused
//! number in `FIRST..LAST_EXCL`.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use abi::cap::{Cap, CapSet};
use abi::errno::{EAGAIN, EBADF, EBUSY, ECHILD, EINVAL, ENOSYS, ENOTCONN, ENOTSOCK, ESRCH};
use abi::ext::WATCH_MASK_ALL;
use abi::ext::{self as op, Pid};
use abi::ring::{Request, Response};

use crate::fd::{FdObject, FdTable, FD_SOFT_LIMIT};
use crate::ipc::SocketType;
use crate::platform;
use crate::proc::{ExitStatus, Signal};
use crate::sys::{Kernel, RecvParkRequest, SpawnArgs, WaitOutcome, WaitTarget};

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
        op::IPC_SEND => ServiceOutcome::Done(handle_ipc_send(kernel, pid, req, heap)),
        op::IPC_RECV => handle_ipc_recv(kernel, pid, req, heap),
        op::IPC_PIPE => ServiceOutcome::Done(handle_ipc_pipe(kernel, pid, req, heap)),
        op::IPC_PEER_CAPS => ServiceOutcome::Done(handle_ipc_peer_caps(kernel, pid, req)),
        op::IPC_PEER_PID => ServiceOutcome::Done(handle_ipc_peer_pid(kernel, pid, req)),
        op::PROC_SELF => ServiceOutcome::Done(handle_proc_self(pid, req)),
        op::PROC_PARENT => ServiceOutcome::Done(handle_proc_parent(kernel, pid, req)),
        op::CAP_CHECK => ServiceOutcome::Done(handle_cap_check(kernel, pid, req)),
        op::CAP_LIST => ServiceOutcome::Done(handle_cap_list(kernel, pid, req)),
        op::CAP_GRANT => ServiceOutcome::Done(handle_cap_grant(kernel, pid, req)),
        op::PROC_SPAWN => ServiceOutcome::Done(handle_proc_spawn(kernel, pid, req, heap)),
        op::PROC_WAIT => handle_proc_wait(kernel, pid, req, heap),
        op::PROC_KILL => ServiceOutcome::Done(handle_proc_kill(kernel, pid, req)),
        op::PROC_CAPS_GET => ServiceOutcome::Done(handle_proc_caps_get(kernel, pid, req)),
        op::DISPLAY_CONNECT => ServiceOutcome::Done(handle_display_connect(kernel, pid, req)),
        op::DISPLAY_BIND => ServiceOutcome::Done(handle_display_bind(kernel, pid, req)),
        op::MOUNT => ServiceOutcome::Done(handle_mount(kernel, pid, req, heap)),
        op::UMOUNT => ServiceOutcome::Done(handle_umount(kernel, pid, req, heap)),
        op::FS_WATCH => ServiceOutcome::Done(handle_fs_watch(kernel, pid, req, heap)),
        op::FS_CHMOD => ServiceOutcome::Done(handle_fs_chmod(kernel, pid, req, heap)),
        op::HOST_FILE_RECV => ServiceOutcome::Done(handle_host_file_recv(kernel, pid, req)),
        op::HOST_FILE_PICK => ServiceOutcome::Done(handle_host_file_pick(kernel, pid, req)),
        op::HOST_FILE_SEND => ServiceOutcome::Done(handle_host_file_send(kernel, pid, req, heap)),
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

// ---- cap_grant ---------------------------------------------------------
//
// Layout:
//   args[0..4]  = target_pid (u32)
//   args[4..12] = caps_to_grant (u64 LE bitset)
// Response:
//   value       = 0 on success
//   status      = -ENOTCAPABLE if the caller does not hold CAP_GRANT
//                 or if `caps_to_grant` is not a subset of the caller's
//                 own cap set (the no-privilege-escalation guard);
//                 -ESRCH if the target pid is absent from the
//                 process table.
//
// Semantics live in `CapTable::grant` (`crates/kernel/src/cap/mod.rs`)
// and match `contracts/syscalls.md §3.5` + `data-model.md §5`:
//
//   1. Caller must hold `Cap::CapGrant` — otherwise NotPermitted →
//      ENOTCAPABLE. In v1 only init holds CAP_GRANT by default, so
//      every other process sees ENOTCAPABLE here unless init has
//      already delegated CAP_GRANT down via this same opcode.
//   2. `caps_to_grant` must be a subset of the caller's own cap
//      set — otherwise NotASubset → ENOTCAPABLE. This is the
//      no-widening rule: a process cannot grant a cap it does
//      not itself possess. The check is atomic — partial subset
//      violations reject the entire grant; the target's caps are
//      never mutated when any bit of `caps_to_grant` is missing
//      from the caller.
//   3. Target pid must exist — otherwise NoSuchPid → ESRCH.
//
// On success the target's cap set becomes
// `target.caps ∪ caps_to_grant` (union — never strips caps from
// the target; cap removal is `CapTable::drop_caps`'s job, exposed
// to userland via a separate future opcode).
//
// The `From<CapError> for KernelError` impl in `sys.rs` collapses
// NotPermitted and NotASubset into the same `NotCapable` variant
// since both surface to userland as ENOTCAPABLE — userland gets
// the same errno regardless of whether the caller lacked the
// capability cap entirely or tried to grant a cap they didn't
// hold. Distinguishing the two would invite probe-driven
// permission-set inference, which the cap model is built to
// frustrate.

fn handle_cap_grant(kernel: &mut Kernel, pid: Pid, req: &Request) -> Response {
    let target = args_u32(req, 0) as Pid;
    let caps = CapSet(args_u64(req, 4));
    match kernel.caps.grant(pid, target, caps) {
        Ok(()) => Response::ok(req.request_id, 0),
        Err(e) => Response::err(req.request_id, kerr_to_errno(e.into())),
    }
}

// ---- ipc_socket --------------------------------------------------------
//
// Layout:
//   args[0..4] = socket type (u32): 0 = Stream; 1 is the reserved Dgram
//                discriminant and returns ENOTSUP in v1
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
//                 listener can queue before a connect is refused. v1 admits
//                 only stream sockets; Dgram creation returns ENOTSUP.
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

fn handle_ipc_connect(kernel: &mut Kernel, pid: Pid, req: &Request, heap: &[u8]) -> Response {
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
    if flags & !accept_flags::NONBLOCK != 0 {
        return ServiceOutcome::Done(Response::err(req.request_id, EINVAL));
    }
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
                Err(e) => ServiceOutcome::Done(Response::err(req.request_id, kerr_to_errno(e))),
            }
        }
        Err(e) => ServiceOutcome::Done(Response::err(req.request_id, kerr_to_errno(e))),
    }
}

// ---- ipc_peer_caps ----------------------------------------------------
//
// Layout:
//   args[0..4] = connected socket fd (u32)
// Response:
//   value = the peer's kernel-captured CapSet bitmask as i64.
//
// This is intentionally fd-scoped rather than pid-scoped. The caller
// may inspect only the authenticated identity on an IPC connection it
// already possesses; no `PROC_INSPECT` authority and no client claim
// are involved.

fn handle_ipc_peer_caps(kernel: &Kernel, pid: Pid, req: &Request) -> Response {
    let fd = args_u32(req, 0);
    match kernel.ipc_peer_caps(pid, fd) {
        Ok(caps) => Response::ok(req.request_id, caps.0 as i64),
        Err(crate::sys::KernelError::NotSupportedOnFd) => Response::err(req.request_id, ENOTSOCK),
        Err(crate::sys::KernelError::InvalidArgument) => Response::err(req.request_id, ENOTCONN),
        Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
    }
}

// ---- ipc_peer_pid -----------------------------------------------------
//
// Layout:
//   args[0..4] = connected socket fd (u32)
// Response:
//   value = the peer's kernel-captured pid, widened to i64.
//
// Error semantics deliberately match `ipc_peer_caps`: the query is
// fd-scoped, pointer-free, and reads the same immutable credential
// snapshot without accepting any identity bytes from the peer.

fn handle_ipc_peer_pid(kernel: &Kernel, pid: Pid, req: &Request) -> Response {
    let fd = args_u32(req, 0);
    match kernel.ipc_peer_pid(pid, fd) {
        Ok(peer_pid) => Response::ok(req.request_id, peer_pid as i64),
        Err(crate::sys::KernelError::NotSupportedOnFd) => Response::err(req.request_id, ENOTSOCK),
        Err(crate::sys::KernelError::InvalidArgument) => Response::err(req.request_id, ENOTCONN),
        Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
    }
}

// ---- ipc_send ---------------------------------------------------------
//
// Layout (per `contracts/syscalls.md §3.1`):
//   args[0..4]   = fd (u32) — the connected socket OR pipe-write fd
//                  the payload should land on.
//   args[4..8]   = len (u32) — number of payload bytes living at
//                  heap[heap_ptr .. heap_ptr + len].
//   args[8..12]  = fd_to_pass (i32 LE) — optional ancillary fd to
//                  attach. -1 = "no ancillary fd"; any non-negative
//                  value is interpreted as an fd number that must
//                  exist in the caller's fd table at call time.
//   args[12..16] = flags (u32) — reserved. v1 accepts only flags=0
//                  (any other value → EINVAL before any side effect).
//                  Future: MSG_DONTWAIT, MSG_PEEK, etc.
//   heap_ptr / heap_len = the payload bytes. heap_len MUST equal
//                  the args[4..8] `len` field — a mismatch → EINVAL.
//                  The duplication is deliberate: `len` lives in the
//                  inline args window so a userland caller doesn't
//                  have to peek at the heap descriptor to know the
//                  payload size; the cross-check keeps a corrupt
//                  shim from desynchronising the two.
// Response:
//   value = number of bytes accepted (>= 0). May be smaller than
//           `len` if the peer's rx ring is partially full.
//   status = -EBADF if `fd` is unknown, not a socket / pipe-write,
//                  or `fd_to_pass >= 0` and not in the caller's
//                  table.
//            -EINVAL if `flags != 0`, `len` mismatches `heap_len`,
//                  the heap range is out of bounds, OR `fd_to_pass
//                  >= 0` is paired with a pipe-write fd (pipes have
//                  no ancillary channel).
//            -EPIPE if the socket peer or pipe reader has closed.
//                  Mirrors `fd_write`'s SIGPIPE delivery — the kernel
//                  posts `Signal::Pipe` to the caller's signal inbox
//                  on this path so userland's POSIX-shaped EPIPE
//                  handler runs.
//            -EAGAIN if the underlying ring is full (v1 blocks via
//                  the dispatcher only on accept; send currently
//                  surfaces WouldBlock so userland can retry).
//
// Routing decision lives in `Kernel::ipc_send`; this handler is a
// thin adapter that decodes the inline args window + heap-scratch
// payload and folds NotSupportedOnFd into EBADF (rather than the
// generic EINVAL `kerr_to_errno` would produce). The spec says
// "wrong fd kind" on an IPC opcode is EBADF — distinct from the
// generic FD_WRITE arm that uses EINVAL — so the wire surface
// matches `send(2)` / `sendmsg(2)`'s documented contract.

fn handle_ipc_send(kernel: &mut Kernel, pid: Pid, req: &Request, heap: &[u8]) -> Response {
    let fd = args_u32(req, 0);
    let len = args_u32(req, 4) as usize;
    let fd_to_pass_raw = args_u32(req, 8) as i32;
    let flags = args_u32(req, 12);

    if flags != 0 {
        return Response::err(req.request_id, EINVAL);
    }
    if len != req.heap_len as usize {
        return Response::err(req.request_id, EINVAL);
    }
    let Some(buf) = heap_in(req, heap) else {
        return Response::err(req.request_id, EINVAL);
    };
    let fd_to_pass = if fd_to_pass_raw < 0 {
        None
    } else {
        Some(fd_to_pass_raw as u32)
    };

    match kernel.ipc_send(pid, fd, buf, fd_to_pass) {
        Ok(n) => Response::ok(req.request_id, n as i64),
        Err(crate::sys::KernelError::NotSupportedOnFd) => Response::err(req.request_id, EBADF),
        Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
    }
}

// ---- ipc_recv ---------------------------------------------------------
//
// Layout (per `contracts/syscalls.md §3.1`, mirroring `ipc_send`):
//   args[0..4]   = fd (u32) — the connected socket the caller wants
//                  to drain bytes (and optionally one passed fd) from.
//   args[4..8]   = len (u32) — the maximum payload byte count the
//                  caller will accept this call. The handler reserves
//                  a fresh `Vec<u8>` of this size in `Kernel::ipc_recv`
//                  and copies whatever's drained back into the
//                  caller's heap_out window.
//   args[8..12]  = recv_fd_slot (i32 LE) — fd-handoff opt-in:
//                  -1 = "don't install a passed fd; leave any queued
//                       fds on the socket's rx_fds for a later call".
//                  >= 0 = "install one queued fd in the lowest-free
//                       slot of my fd table; v1 ignores the actual
//                       value beyond the sign — slot-targeted install
//                       is a future extension."
//   args[12..16] = flags (u32) — bit 0 (`0x01`) toggles non-blocking
//                  semantics; every other bit is reserved (any set
//                  → -EINVAL before any side effect):
//                    flags & 0x01 == 0 (default) → blocking. An
//                      empty rx_buf + rx_fds parks the caller via
//                      `Kernel::park_on_recv`; a future ipc_send
//                      from the peer wakes them with the bytes /
//                      fds the non-blocking path would have produced.
//                    flags & 0x01 != 0           → non-blocking.
//                      An empty rx_buf + rx_fds returns -EAGAIN
//                      (the f00e559 default; preserved here for
//                      callers that explicitly want the legacy
//                      semantic, e.g. an event-loop poller).
//                  Future flag bits: MSG_PEEK, MSG_CMSG_CLOEXEC.
//                  Bit 0 mirrors `accept_flags::NONBLOCK` (bit 0 of
//                  the ipc_accept flags u16) so the same convention
//                  applies across both blocking-recv-style opcodes.
//   heap_ptr / heap_len = the OUTPUT region the kernel writes payload
//                  bytes into. heap_len MUST be >= 4 if recv_fd_slot
//                  >= 0 (the first 4 bytes hold the new fd-number
//                  when one was installed; payload starts at offset
//                  4). When recv_fd_slot < 0, heap_len just needs to
//                  cover the requested payload bytes.
//
// Response:
//   value     = number of payload bytes drained (>= 0). Independent
//               of whether an fd was installed — even a 0-byte recv
//               that drained one fd reports value=0 and extra_len=4.
//   extra_len = total bytes the kernel wrote into heap_out:
//                 - recv_fd_slot < 0 OR no fd installed: bytes_drained
//                 - fd installed: 4 + bytes_drained (the leading 4
//                   bytes are the new receiver-side fd-number as
//                   little-endian u32; payload follows from offset 4).
//   status    = -EBADF if the main fd is unknown or not a socket.
//               -EINVAL if any flag bit other than 0x01 is set, the
//                  heap range is out of bounds, or recv_fd_slot >= 0
//                  with heap_len < 4.
//               -EAGAIN if the socket has no bytes AND no queued
//                  fds AND `flags & 0x01 != 0` (non-blocking mode);
//                  blocking mode parks the caller instead (no
//                  Response is produced — `ServiceOutcome::Parked`).
// Parked:
//   No response is produced when `flags & 0x01 == 0` AND the
//   socket has no bytes AND no queued fds. The caller is parked
//   on the socket via `Kernel::park_on_recv`; a later `ipc_send`
//   from the peer (or `fd_write` on the peer's socket fd) drains
//   the parker via `Kernel.pending_wakes` +
//   `kernel_take_next_wake_for_pid`, the same machinery
//   `handle_ipc_accept`'s park path uses.
//
// Heap layout decision — fd-leading: when an fd is installed, the
// 4-byte fd-number lives at heap[0..4] and the payload at
// heap[4..4+value]. The alternative (payload first, fd at
// heap[value..value+4]) requires the caller to compute the offset
// from `value`, which forces a two-pass parse. Fixed-offset fd
// matches `proc_wait`'s child-pid-at-heap[0..4] convention and the
// `ipc_pipe` dual-fd-at-heap[0..8] convention — every existing
// multi-value response in the dispatcher uses fixed offsets, never
// variable ones. The `value`-as-byte-count split keeps the most
// common path (POSIX-shaped `recv(fd, buf, len)`) reading naturally
// — a caller that ignores `recv_fd_slot` reads `value` and walks
// `heap[..value]`.
//
// Why not pack the fd into Response.value alongside the byte count?
// Response.value is i64 with conventional semantics "primary scalar
// return". Squeezing two scalars into one field by bit-packing
// would let `value` carry both, but it'd break the read pattern
// userland already learned from FD_READ / IPC_SEND ("value == bytes").
// The heap-extra path costs 4 bytes per recv — negligible.

fn handle_ipc_recv(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> ServiceOutcome {
    /// `flags & RECV_NONBLOCK` selects the non-blocking semantic
    /// (legacy f00e559 behavior — empty socket → -EAGAIN). Bit 0;
    /// every other bit is reserved.
    const RECV_NONBLOCK: u32 = 0x01;
    let fd = args_u32(req, 0);
    let len = args_u32(req, 4) as usize;
    let recv_fd_slot = args_u32(req, 8) as i32;
    let flags = args_u32(req, 12);

    if (flags & !RECV_NONBLOCK) != 0 {
        return ServiceOutcome::Done(Response::err(req.request_id, EINVAL));
    }
    let nonblock = (flags & RECV_NONBLOCK) != 0;
    let want_fd = recv_fd_slot >= 0;
    let fd_prefix = if want_fd { 4 } else { 0 };
    let needed = len.saturating_add(fd_prefix);
    if (req.heap_len as usize) < needed {
        return ServiceOutcome::Done(Response::err(req.request_id, EINVAL));
    }
    // Validate the concrete scratch-memory range before touching socket or fd
    // state. `heap_len` is guest-provided capacity metadata; it does not prove
    // that `heap_ptr..heap_ptr + needed` exists in this request's heap.
    // Receiving may drain bytes and install an ancillary fd, so discovering an
    // invalid pointer after `Kernel::ipc_recv` would make EINVAL non-atomic.
    if heap_out_mut(req, heap, needed).is_none() {
        return ServiceOutcome::Done(Response::err(req.request_id, EINVAL));
    }

    let (bytes, new_fd) = match kernel.ipc_recv(pid, fd, len, want_fd) {
        Ok(pair) => pair,
        Err(crate::sys::KernelError::NotSupportedOnFd) => {
            return ServiceOutcome::Done(Response::err(req.request_id, EBADF))
        }
        Err(crate::sys::KernelError::WouldBlock) if nonblock => {
            return ServiceOutcome::Done(Response::err(req.request_id, EAGAIN))
        }
        Err(crate::sys::KernelError::WouldBlock) => {
            // Blocking-recv path: park the caller on the socket and
            // return without producing a Response. A future ipc_send
            // from the peer will queue the wake response via
            // `wake_parked_recver_if_any`. If the park itself fails
            // (one-parker-per-pid invariant), surface the errno —
            // userland sees -EAGAIN for that race rather than a
            // silent never-wake.
            return match kernel.park_on_recv(
                pid,
                fd,
                RecvParkRequest {
                    request_id: req.request_id,
                    max_len: len as u32,
                    recv_fd_slot,
                    heap_ptr: req.heap_ptr,
                    heap_len: req.heap_len,
                },
            ) {
                Ok(()) => ServiceOutcome::Parked,
                Err(crate::sys::KernelError::NotSupportedOnFd) => {
                    ServiceOutcome::Done(Response::err(req.request_id, EBADF))
                }
                Err(e) => ServiceOutcome::Done(Response::err(req.request_id, kerr_to_errno(e))),
            };
        }
        Err(e) => return ServiceOutcome::Done(Response::err(req.request_id, kerr_to_errno(e))),
    };

    let installed = new_fd.is_some();
    let payload_offset = if installed { 4 } else { 0 };
    let total = payload_offset + bytes.len();
    let out = heap_out_mut(req, heap, total)
        .expect("ipc_recv output range was validated before committing receive state");
    if let Some(new_fd_num) = new_fd {
        out[0..4].copy_from_slice(&new_fd_num.to_le_bytes());
    }
    out[payload_offset..payload_offset + bytes.len()].copy_from_slice(&bytes);

    ServiceOutcome::Done(Response {
        request_id: req.request_id,
        status: 0,
        value: bytes.len() as i64,
        extra_len: total as u32,
        _pad: [0u8; 12],
    })
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

fn handle_ipc_pipe(kernel: &mut Kernel, pid: Pid, req: &Request, heap: &mut [u8]) -> Response {
    if (req.heap_len as usize) < 8 {
        return Response::err(req.request_id, EINVAL);
    }
    if heap_out_mut(req, heap, 8).is_none() {
        return Response::err(req.request_id, EINVAL);
    }
    let (read_fd, write_fd) = match kernel.create_pipe_fds(pid) {
        Ok(pair) => pair,
        Err(e) => return Response::err(req.request_id, kerr_to_errno(e)),
    };
    let out = heap_out_mut(req, heap, 8)
        .expect("ipc_pipe output range was validated before allocating the pipe");
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

/// Parse a NUL-terminated UTF-8 byte stream into a `Vec<String>`.
/// Each entry is everything up to the next `\0`; the stream MUST
/// end with a `\0` (i.e. be empty or be of shape `e0\0e1\0...\0`).
/// Returns `None` on invalid UTF-8 or a missing trailing NUL.
fn parse_nul_separated(bytes: &[u8]) -> Option<Vec<alloc::string::String>> {
    if bytes.is_empty() {
        return Some(Vec::new());
    }
    if *bytes.last().unwrap() != 0 {
        return None;
    }
    let mut out = Vec::new();
    for chunk in bytes[..bytes.len() - 1].split(|b| *b == 0) {
        out.push(core::str::from_utf8(chunk).ok()?.into());
    }
    Some(out)
}

/// Parse a NUL-terminated `KEY=VALUE` byte stream into a sorted
/// `BTreeMap`. Entries without `=` are rejected. Duplicate keys
/// are accepted; the last value wins (matches POSIX
/// putenv-by-overwrite semantics).
fn parse_envp(
    bytes: &[u8],
) -> Option<alloc::collections::BTreeMap<alloc::string::String, alloc::string::String>> {
    let entries = parse_nul_separated(bytes)?;
    let mut out = alloc::collections::BTreeMap::new();
    for entry in entries {
        let eq = entry.find('=')?;
        let key = entry[..eq].to_string();
        let value = entry[eq + 1..].to_string();
        out.insert(key, value);
    }
    Some(out)
}

struct DecodedSpawn {
    path: String,
    argv: Vec<String>,
    envp: BTreeMap<String, String>,
    stdin_fd: Option<u32>,
    stdout_fd: Option<u32>,
    stderr_fd: Option<u32>,
    extra_fds: Vec<(u32, u32)>,
    cwd: Option<String>,
    caps: Option<CapSet>,
}

struct BlobCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> BlobCursor<'a> {
    fn new(bytes: &'a [u8], offset: usize) -> Self {
        Self { bytes, offset }
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.offset.checked_add(len)?;
        let out = self.bytes.get(self.offset..end)?;
        self.offset = end;
        Some(out)
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn text(&mut self, len: usize, allow_empty: bool) -> Option<String> {
        let bytes = self.take(len)?;
        if (!allow_empty && bytes.is_empty()) || bytes.contains(&0) {
            return None;
        }
        Some(core::str::from_utf8(bytes).ok()?.to_string())
    }

    fn is_finished(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

fn read_u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn read_u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_i32_at(bytes: &[u8], offset: usize) -> Option<i32> {
    Some(i32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

fn decode_optional_fd(raw: i32) -> Option<Option<u32>> {
    match raw {
        op::spawn_v1::INHERIT_FD => Some(None),
        0..=i32::MAX => Some(Some(raw as u32)),
        _ => None,
    }
}

fn decode_spawn_v1(req: &Request, payload: &[u8]) -> Option<DecodedSpawn> {
    use op::spawn_v1 as wire;

    if args_u32(req, 4) as usize != payload.len()
        || args_u16(req, 8) != wire::VERSION
        || req.args[10..].iter().any(|byte| *byte != 0)
        || payload.len() < wire::HEADER_LEN
        || payload.len() > abi::ring::HEAP_SCRATCH_BYTES
        || read_u32_at(payload, wire::OFF_MAGIC)? != wire::MAGIC
        || read_u16_at(payload, wire::OFF_VERSION)? != wire::VERSION
        || read_u32_at(payload, wire::OFF_TOTAL_LEN)? as usize != payload.len()
        || read_u16_at(payload, wire::OFF_RESERVED_U16)? != 0
        || read_u32_at(payload, wire::OFF_RESERVED_U32)? != 0
    {
        return None;
    }

    let flags = read_u16_at(payload, wire::OFF_FLAGS)?;
    if flags & !wire::KNOWN_FLAGS != 0 {
        return None;
    }
    let path_len = read_u16_at(payload, wire::OFF_PATH_LEN)? as usize;
    let cwd_len = read_u16_at(payload, wire::OFF_CWD_LEN)? as usize;
    let argc = read_u16_at(payload, wire::OFF_ARGC)? as usize;
    let envc = read_u16_at(payload, wire::OFF_ENVC)? as usize;
    let extra_count = read_u16_at(payload, wire::OFF_EXTRA_FD_COUNT)? as usize;
    let stdin_fd = decode_optional_fd(read_i32_at(payload, wire::OFF_STDIN_FD)?)?;
    let stdout_fd = decode_optional_fd(read_i32_at(payload, wire::OFF_STDOUT_FD)?)?;
    let stderr_fd = decode_optional_fd(read_i32_at(payload, wire::OFF_STDERR_FD)?)?;
    let raw_caps = read_u64_at(payload, wire::OFF_CAPS)?;
    let caps = if flags & wire::FLAG_CAPS != 0 {
        Some(CapSet(raw_caps))
    } else {
        if raw_caps != 0 {
            return None;
        }
        None
    };
    if (flags & wire::FLAG_CWD == 0 && cwd_len != 0)
        || (flags & wire::FLAG_CWD != 0 && cwd_len == 0)
    {
        return None;
    }

    let mut cursor = BlobCursor::new(payload, wire::HEADER_LEN);
    let path = cursor.text(path_len, false)?;
    if !path.starts_with('/') {
        return None;
    }
    let cwd = if flags & wire::FLAG_CWD != 0 {
        let cwd = cursor.text(cwd_len, false)?;
        if !cwd.starts_with('/') {
            return None;
        }
        Some(cwd)
    } else {
        None
    };

    let mut argv = Vec::with_capacity(argc);
    for _ in 0..argc {
        let len = cursor.u16()? as usize;
        argv.push(cursor.text(len, true)?);
    }

    let mut envp = BTreeMap::new();
    for _ in 0..envc {
        let key_len = cursor.u16()? as usize;
        let value_len = cursor.u16()? as usize;
        let key = cursor.text(key_len, false)?;
        let value = cursor.text(value_len, true)?;
        if key.contains('=') || envp.insert(key, value).is_some() {
            return None;
        }
    }

    let mut extra_fds = Vec::with_capacity(extra_count);
    for _ in 0..extra_count {
        let parent_fd = cursor.u32()?;
        let child_fd = cursor.u32()?;
        if child_fd < abi::fd::FIRST_DYNAMIC
            || child_fd as usize >= FD_SOFT_LIMIT
            || extra_fds
                .iter()
                .any(|(_, existing_child)| *existing_child == child_fd)
        {
            return None;
        }
        extra_fds.push((parent_fd, child_fd));
    }
    if !cursor.is_finished() {
        return None;
    }

    Some(DecodedSpawn {
        path,
        argv,
        envp,
        stdin_fd,
        stdout_fd,
        stderr_fd,
        extra_fds,
        cwd,
        caps,
    })
}

fn decode_spawn_legacy(req: &Request, payload: &[u8]) -> Option<DecodedSpawn> {
    let path_len = args_u32(req, 0) as usize;
    let caps = CapSet(args_u64(req, 4));
    let argv_buf_len = u16::from_le_bytes([req.args[12], req.args[13]]) as usize;
    let envp_buf_len = u16::from_le_bytes([req.args[14], req.args[15]]) as usize;
    let total_heap = path_len
        .checked_add(argv_buf_len)?
        .checked_add(envp_buf_len)?;
    if payload.len() != total_heap {
        return None;
    }
    let path_bytes = &payload[..path_len];
    let argv_bytes = &payload[path_len..path_len + argv_buf_len];
    let envp_bytes = &payload[path_len + argv_buf_len..];
    let path = core::str::from_utf8(path_bytes).ok()?.to_string();
    let argv = parse_nul_separated(argv_bytes)?;
    let envp = parse_envp(envp_bytes)?;
    Some(DecodedSpawn {
        path,
        argv,
        envp,
        stdin_fd: None,
        stdout_fd: None,
        stderr_fd: None,
        extra_fds: Vec::new(),
        cwd: None,
        caps: Some(caps),
    })
}

fn selected_fd(table: &FdTable, requested: Option<u32>, inherited: u32) -> Option<FdObject> {
    table
        .get(requested.unwrap_or(inherited))
        .map(|entry| entry.object)
}

fn handle_proc_spawn(kernel: &mut Kernel, pid: Pid, req: &Request, heap: &[u8]) -> Response {
    let Some(payload) = heap_in(req, heap) else {
        return Response::err(req.request_id, EINVAL);
    };
    let decoded = if args_u32(req, 0) == op::spawn_v1::MAGIC {
        decode_spawn_v1(req, payload)
    } else {
        decode_spawn_legacy(req, payload)
    };
    let Some(decoded) = decoded else {
        return Response::err(req.request_id, EINVAL);
    };

    let parent_caps = match kernel.caps.list(pid) {
        Ok(caps) => caps,
        Err(_) => return Response::err(req.request_id, ESRCH),
    };
    let parent_cwd = match kernel.procs.get(pid) {
        Some(process) => process.cwd.clone(),
        None => return Response::err(req.request_id, ESRCH),
    };
    let parent_fds = match kernel.fds(pid) {
        Ok(t) => t,
        Err(_) => return Response::err(req.request_id, ESRCH),
    };
    let Some(stdin) = selected_fd(parent_fds, decoded.stdin_fd, abi::fd::STDIN) else {
        return Response::err(req.request_id, EINVAL);
    };
    let Some(stdout) = selected_fd(parent_fds, decoded.stdout_fd, abi::fd::STDOUT) else {
        return Response::err(req.request_id, EINVAL);
    };
    let Some(stderr) = selected_fd(parent_fds, decoded.stderr_fd, abi::fd::STDERR) else {
        return Response::err(req.request_id, EINVAL);
    };
    let mut extra_objects = Vec::with_capacity(decoded.extra_fds.len());
    for (parent_fd, child_fd) in &decoded.extra_fds {
        let Some(object) = parent_fds.get(*parent_fd).map(|entry| entry.object) else {
            return Response::err(req.request_id, EBADF);
        };
        extra_objects.push((*child_fd, object));
    }
    if extra_objects
        .iter()
        .any(|(_, object)| matches!(object, FdObject::Socket(_)))
    {
        return Response::err(req.request_id, abi::errno::ENOTSUP);
    }

    let spawn_args = SpawnArgs {
        name: &decoded.path,
        caps: decoded.caps.unwrap_or(parent_caps),
        cwd: decoded.cwd.as_deref().unwrap_or(&parent_cwd),
        argv: decoded.argv,
        envp: decoded.envp,
        stdin,
        stdout,
        stderr,
    };
    let new_pid = match kernel.proc_spawn(pid, spawn_args) {
        Ok(p) => p,
        Err(e) => return Response::err(req.request_id, kerr_to_errno(e)),
    };

    for (child_fd, object) in extra_objects {
        if let Err(error) = kernel.install_spawn_extra_fd(new_pid, child_fd, object) {
            let _ = kernel.proc_exit(new_pid, ExitStatus::Exited(-1));
            let _ = kernel.reap(new_pid);
            return Response::err(req.request_id, kerr_to_errno(error));
        }
    }

    let executable = match kernel.load_vfs_executable(&decoded.path) {
        Ok(executable) => executable,
        Err(error) => {
            let _ = kernel.proc_exit(new_pid, ExitStatus::Exited(-1));
            let _ = kernel.reap(new_pid);
            return Response::err(req.request_id, kerr_to_errno(error));
        }
    };

    // Ask the host to actually spawn a Worker for `new_pid`. On
    // failure, roll back by marking the process `Zombie` and
    // reaping it so no pid is leaked on a half-done spawn.
    if let Err(error) =
        platform::current().spawn_process(new_pid, &decoded.path, executable.as_deref())
    {
        let _ = kernel.proc_exit(new_pid, ExitStatus::Exited(-1));
        let _ = kernel.reap(new_pid);
        let errno = match error {
            crate::platform::DriverError::Errno(errno) if errno > 0 => errno,
            crate::platform::DriverError::Errno(_)
            | crate::platform::DriverError::NotReady
            | crate::platform::DriverError::Transport => abi::errno::EIO,
        };
        return Response::err(req.request_id, errno);
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
//   EAGAIN     = live children exist but none are zombies and WNOHANG
//                was requested. With options=0 the caller parks.
//   ECHILD     = no child matching the target, or target == sender pid.
//   EINVAL     = malformed target or unknown options bits.
//
// Self-wait is rejected with ECHILD rather than EDEADLK because POSIX
// waitpid(pid == self) semantically means "a pid that can't be my
// child" — ECHILD matches what Linux returns. Process-group wait
// (target < -1) is out of scope for v1's flat process model; returning
// EINVAL steers userland away from a feature we don't implement.

pub(crate) fn pack_exit_status(status: ExitStatus) -> i64 {
    match status {
        ExitStatus::Exited(code) => {
            let low = (code as u32) as u64;
            ((0x01_u64) << 40) | low
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
) -> ServiceOutcome {
    let target_pid = i32::from_le_bytes([req.args[0], req.args[1], req.args[2], req.args[3]]);
    let options = args_u32(req, 4);
    if options & !abi::ext::wait_opts::WNOHANG != 0 {
        return ServiceOutcome::Done(Response::err(req.request_id, EINVAL));
    }
    if target_pid < -1 {
        return ServiceOutcome::Done(Response::err(req.request_id, EINVAL));
    }
    let target = if target_pid == 0 || target_pid == -1 {
        WaitTarget::Any
    } else {
        if target_pid == pid {
            return ServiceOutcome::Done(Response::err(req.request_id, ECHILD));
        }
        WaitTarget::Specific(target_pid)
    };
    let nohang = (options & abi::ext::wait_opts::WNOHANG) != 0;

    match kernel.proc_wait(pid, target) {
        Ok(WaitOutcome::Reaped(child, status)) => {
            // Synchronous-reap path — unchanged from pre-2c.1.
            let packed = pack_exit_status(status);
            let mut resp = Response::ok(req.request_id, packed);
            if (req.heap_len as usize) >= 4 {
                if let Some(out) = heap_out_mut(req, heap, 4) {
                    out[0..4].copy_from_slice(&(child as u32).to_le_bytes());
                    resp.extra_len = 4;
                }
            }
            ServiceOutcome::Done(resp)
        }
        Ok(WaitOutcome::WouldBlock) if nohang => {
            ServiceOutcome::Done(Response::err(req.request_id, EAGAIN))
        }
        Ok(WaitOutcome::WouldBlock) => {
            // Blocking path: park the caller. If park_on_wait
            // reports WouldBlock (one-waiter-per-parent
            // invariant), surface EAGAIN.
            match kernel.park_on_wait(pid, req.request_id, target, req.heap_ptr, req.heap_len) {
                Ok(()) => ServiceOutcome::Parked,
                Err(crate::sys::KernelError::WouldBlock) => {
                    ServiceOutcome::Done(Response::err(req.request_id, EAGAIN))
                }
                Err(e) => ServiceOutcome::Done(Response::err(req.request_id, kerr_to_errno(e))),
            }
        }
        Ok(WaitOutcome::NoChildren) => ServiceOutcome::Done(Response::err(req.request_id, ECHILD)),
        Err(e) => ServiceOutcome::Done(Response::err(req.request_id, kerr_to_errno(e))),
    }
}

// ---- proc_kill --------------------------------------------------------
//
// Layout:
//   args[0..4] = target_pid (i32).
//   args[4..6] = signum (u16). v1 accepts
//                {0, 2, 9, 10, 12, 13, 15, 17}:
//                0 = POSIX kill(pid, 0) existence + permission
//                    probe — runs every precondition proc_kill
//                    would run but delivers no signal.
//                2 = SIGINT, 9 = SIGKILL, 10 = SIGUSR1,
//                12 = SIGUSR2, 13 = SIGPIPE, 15 = SIGTERM,
//                17 = SIGCHLD. Any other number, including SIGHUP
//                and SIGQUIT, → EINVAL before the kernel is touched.
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
        // Never touches the park interrupt — a probe against a
        // BlockedOnIpc pid leaves the park intact.
        return match kernel.proc_check_signal(pid, target_pid) {
            Ok(()) => Response::ok(req.request_id, 0),
            Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
        };
    }
    let signal = match signum {
        2 => Signal::Interrupt,
        9 => Signal::Kill,
        10 => Signal::User1,
        12 => Signal::User2,
        13 => Signal::Pipe,
        15 => Signal::Term,
        17 => Signal::Child,
        _ => return Response::err(req.request_id, EINVAL),
    };
    match kernel.proc_kill(pid, target_pid, signal) {
        Ok(()) => {
            // The catchable-signal interrupt set is {Term, Interrupt}.
            // Both are POSIX-canonical interrupters of blocking syscalls
            // (SIGTERM = "please shut down", SIGINT = ctrl-c) — they
            // additionally fire `interrupt_parked_accept` /
            // `interrupt_parked_wait` / `interrupt_parked_recv` on the
            // target with -EINTR after the signal-inbox delivery inside
            // `Kernel::proc_kill`, so userland draining fd 3 on the
            // EINTR wake finds the signal queued.
            //
            // The non-interrupter set is {Pipe, Child, User1, User2} —
            // these are inbox-only and leave any parked syscall asleep:
            //
            //   * Pipe is a write-side notification with no POSIX
            //     analogue for interrupting blocking read syscalls.
            //   * Child is a child-exit notification delivered TO the
            //     parent — interrupting a parent's unrelated park on
            //     Child arrival would be surprising (POSIX leaves
            //     SIGCHLD's blocking-syscall interruption gated by
            //     SA_RESTART, which v1 doesn't model).
            //   * User1 / User2 are user-defined signals; POSIX gates
            //     their interruption of blocking syscalls behind the
            //     same SA_RESTART knob (or its inverse) which v1
            //     doesn't model, so the safe default is "deliver to
            //     inbox, don't disturb the parked syscall." A future
            //     slice can flip this conditionally once a use case
            //     appears (e.g. a daemon that wants SIGUSR1 to
            //     trigger a config reload mid-blocking-recv).
            if signal == Signal::Term || signal == Signal::Interrupt {
                let _ = kernel.interrupt_parked_accept(target_pid);
                let _ = kernel.interrupt_parked_wait(target_pid);
                let _ = kernel.interrupt_parked_recv(target_pid);
                let _ = kernel.interrupt_parked_pipe_read(target_pid);
                let _ = kernel.interrupt_parked_poll(target_pid);
            }
            Response::ok(req.request_id, 0)
        }
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

// ---- mount ------------------------------------------------------------
//
// Layout (per `contracts/syscalls.md §3.5`):
//   args[0..4]   = path_ptr (u32) — heap offset where the absolute
//                  mount path bytes live.
//   args[4..8]   = path_len (u32) — length of the mount path bytes.
//   args[8..12]  = fstype_ptr (u32) — heap offset where the fstype
//                  name lives. v1 only accepts `"tmpfs"`.
//   args[12..16] = fstype_len (u32) — length of the fstype name.
//   heap input: path bytes at heap[path_ptr..path_ptr+path_len] +
//                  fstype bytes at heap[fstype_ptr..fstype_ptr+fstype_len].
//                  The two regions MAY overlap or alias — the handler
//                  reads each independently and never holds both
//                  borrows simultaneously past the UTF-8 decode.
//
// Response:
//   value = 0 on success
//   status = -ENOTCAPABLE if the caller does not hold Cap::Mount.
//            -EINVAL if either heap range is out of bounds, the path
//                  is not valid UTF-8, the path is not absolute, the
//                  path is `/` (root remount is a future slice), the
//                  fstype is not valid UTF-8, the fstype is anything
//                  other than `"tmpfs"`, OR the target directory is
//                  non-empty (a fresh mount over a directory with
//                  existing entries would shadow them irreversibly
//                  until umount; v1 rejects outright).
//            -ENOENT if the target path doesn't resolve.
//            -ENOTDIR if the target exists but isn't a directory.
//            -EBUSY if the target is already a mount point.
//
// Why the handler maps `KernelError::Fs(FsError::AlreadyExists)`
// (which the dispatcher's default `kerr_to_errno` would render as
// EEXIST) to EBUSY: `Vfs::mount` reuses `AlreadyExists` for the
// duplicate-mountpoint guard because no mount-specific FS error
// variant exists, but the §3.5 wire surface specifies EBUSY for
// "target is already a mount point" — same condition, different
// error vocabulary at the user-facing layer. Translating in the
// handler keeps `kerr_to_errno` exhaustive (every FsError variant
// has its own canonical errno) without adding a special-case to
// the bridge for one syscall.
//
// Wire format note — fstype is heap-pointed rather than inline:
// the inline args window is 16 bytes; v1's only fstype `"tmpfs"`
// is 5 bytes, but the spec leaves room for `"devfs"` / `"procfs"`
// / future names that may exceed 16 bytes minus the path-pointer
// payload. Heap-pointing both strings keeps the wire format
// stable as the fstype factory grows.

fn handle_mount(kernel: &mut Kernel, pid: Pid, req: &Request, heap: &[u8]) -> Response {
    let path_ptr = args_u32(req, 0) as usize;
    let path_len = args_u32(req, 4) as usize;
    let fstype_ptr = args_u32(req, 8) as usize;
    let fstype_len = args_u32(req, 12) as usize;

    let Some(path_end) = path_ptr.checked_add(path_len) else {
        return Response::err(req.request_id, EINVAL);
    };
    let Some(fstype_end) = fstype_ptr.checked_add(fstype_len) else {
        return Response::err(req.request_id, EINVAL);
    };
    if path_end > heap.len() || fstype_end > heap.len() {
        return Response::err(req.request_id, EINVAL);
    }
    let Ok(path) = core::str::from_utf8(&heap[path_ptr..path_end]) else {
        return Response::err(req.request_id, EINVAL);
    };
    let Ok(fstype) = core::str::from_utf8(&heap[fstype_ptr..fstype_end]) else {
        return Response::err(req.request_id, EINVAL);
    };

    // The `mount` opcode encodes mount-flag bits (MOUNT_REMOUNT et
    // al.) in the per-request `flags: u16` field — the args window
    // is already 16 bytes full (4×u32 for path/fstype ptr+len) so
    // there's no inline space for an extra u32. The Request struct
    // already carries `flags: u16` documented as "reserved in v1,
    // MUST be zero", so this is the natural home for the new
    // mount-flag bitset. Widened to u32 for the Kernel::mount
    // signature; the high 16 bits are always zero in v1.
    let flags = req.flags as u32;

    match kernel.mount(pid, path, fstype, flags) {
        Ok(()) => Response::ok(req.request_id, 0),
        Err(crate::sys::KernelError::Fs(crate::vfs::FsError::AlreadyExists)) => {
            Response::err(req.request_id, EBUSY)
        }
        Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
    }
}

// ---- umount -----------------------------------------------------------
//
// Layout (per `contracts/syscalls.md §3.5`):
//   args[0..4] = path_ptr (u32) — heap offset where the absolute
//                mount path bytes live.
//   args[4..8] = path_len (u32) — length of the mount path bytes.
//   heap input: path bytes only (no fstype — umount identifies the
//                target by mountpoint).
//
// Response:
//   value = 0 on success
//   status = -ENOTCAPABLE if the caller does not hold Cap::Mount.
//            -EINVAL if the heap range is out of bounds, the path is
//                  not valid UTF-8, the path is not absolute, OR the
//                  path is not currently a mount point.
//            -EBUSY if any process holds an open Vnode fd whose
//                  mount_id matches the target mount (busy-mount
//                  guard; mirror of POSIX umount(2) EBUSY).
//
// Why the handler maps `KernelError::WouldBlock` (which the
// dispatcher's default `kerr_to_errno` would render as EAGAIN) to
// EBUSY: `Kernel::umount` reuses `WouldBlock` for the busy-fd
// guard because no umount-specific kernel error variant exists,
// but the §3.5 wire surface specifies EBUSY for "mount is busy".
// Same translation rationale as the mount handler's
// `AlreadyExists → EBUSY` map.

fn handle_umount(kernel: &mut Kernel, pid: Pid, req: &Request, heap: &[u8]) -> Response {
    let path_ptr = args_u32(req, 0) as usize;
    let path_len = args_u32(req, 4) as usize;

    let Some(path_end) = path_ptr.checked_add(path_len) else {
        return Response::err(req.request_id, EINVAL);
    };
    if path_end > heap.len() {
        return Response::err(req.request_id, EINVAL);
    }
    let Ok(path) = core::str::from_utf8(&heap[path_ptr..path_end]) else {
        return Response::err(req.request_id, EINVAL);
    };

    match kernel.umount(pid, path) {
        Ok(()) => Response::ok(req.request_id, 0),
        Err(crate::sys::KernelError::WouldBlock) => Response::err(req.request_id, EBUSY),
        Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
    }
}

// ---- fs_chmod ---------------------------------------------------------
//
// Layout:
//   args[0..4]  = path_len (u32)
//   args[4..8]  = replacement permission bits (u32, 0..=0o777)
//   args[8..16] = zero
//   heap input  = exact UTF-8 path bytes

fn handle_fs_chmod(kernel: &mut Kernel, pid: Pid, req: &Request, heap: &[u8]) -> Response {
    let path_len = args_u32(req, 0) as usize;
    let mode = args_u32(req, 4);
    if args_u32(req, 8) != 0 || args_u32(req, 12) != 0 || mode & !0o777 != 0 {
        return Response::err(req.request_id, EINVAL);
    }
    let Some(payload) = heap_in(req, heap) else {
        return Response::err(req.request_id, EINVAL);
    };
    if payload.len() != path_len {
        return Response::err(req.request_id, EINVAL);
    }
    let Ok(path) = core::str::from_utf8(payload) else {
        return Response::err(req.request_id, EINVAL);
    };
    if !path.starts_with('/') {
        return Response::err(req.request_id, EINVAL);
    }
    match kernel.fs_chmod(pid, path, mode) {
        Ok(()) => Response::ok(req.request_id, 0),
        Err(error) => Response::err(req.request_id, kerr_to_errno(error)),
    }
}

// ---- fs_watch ---------------------------------------------------------
//
// Layout (per `contracts/syscalls.md §3.7`):
//   args[0..4]   = path_ptr (u32) — heap offset where the absolute
//                  watch path bytes live.
//   args[4..8]   = path_len (u32) — length of the watch path bytes.
//   args[8..12]  = mask (u32) — bit-OR of `WATCH_CREATE` /
//                  `WATCH_DELETE` / `WATCH_MODIFY`. Zero or any
//                  unknown bit → -EINVAL atomic-reject (no watch
//                  installed; no fd allocated).
//   args[12..16] = flags (u32) — v1 only accepts 0. The wire spec
//                  documents two flag bits (`RECURSIVE`,
//                  `COALESCE_MODIFY`) but neither is implemented in
//                  v1; any non-zero value → -EINVAL BEFORE any
//                  side effect, mirroring the strict-flag invariant
//                  of `ipc_send` / `ipc_recv` / `mount`.
//   heap input: path bytes at heap[path_ptr..path_ptr+path_len].
//
// Response:
//   value = freshly-allocated watch fd-number on success (>= 0).
//           Userland reads this fd via `fd_read` to drain queued
//           [`WatchEvent`]s as 8-byte fixed-size records (mask u32
//           LE + inode u32 LE).
//   status = -EINVAL if the heap range is out of bounds, the path
//                  is not valid UTF-8, the path is not absolute, the
//                  mask is zero, the mask contains unknown bits,
//                  OR flags != 0.
//            -ENOENT if `path` doesn't resolve through the VFS.
//            -EMFILE if the caller's fd table is full.
//            -ENOSPC if the per-process, per-target, or kernel-wide
//                  watch admission budget is full.
//            -ENOTSUP if the resolved filesystem cannot emit watches.
//
// No capability check in v1. The §3.7 spec doesn't gate `fs_watch`
// on a cap; a future slice may add `Cap::FsWatch` for paths that
// would otherwise leak metadata about other users' filesystems —
// but v1 is a single-user OS and every process can already
// `path_open + readdir` any directory it can name, so an extra cap
// would be ceremony without protection.
//
// Watch-side state lives on the `Vfs::watches` registry (keyed by
// `(MountId, Inode)`), not on the per-process fd table beyond the
// `FdObject::Watch` discriminator. Closing the fd via `fd_close`
// runs `release_object`'s Watch arm which calls
// `Vfs::unregister_watch`, so a process exit (which drains every
// fd) cleans up watches automatically.

fn handle_fs_watch(kernel: &mut Kernel, pid: Pid, req: &Request, heap: &[u8]) -> Response {
    let path_ptr = args_u32(req, 0) as usize;
    let path_len = args_u32(req, 4) as usize;
    let mask = args_u32(req, 8);
    let flags = args_u32(req, 12);

    // Atomic-reject: every wire-level invariant runs BEFORE we
    // touch the watch registry. A malformed call leaves the
    // kernel state untouched.
    if flags != 0 {
        return Response::err(req.request_id, EINVAL);
    }
    if mask == 0 || (mask & !WATCH_MASK_ALL) != 0 {
        return Response::err(req.request_id, EINVAL);
    }
    let Some(path_end) = path_ptr.checked_add(path_len) else {
        return Response::err(req.request_id, EINVAL);
    };
    if path_end > heap.len() {
        return Response::err(req.request_id, EINVAL);
    }
    let Ok(path) = core::str::from_utf8(&heap[path_ptr..path_end]) else {
        return Response::err(req.request_id, EINVAL);
    };
    if !path.starts_with('/') {
        return Response::err(req.request_id, EINVAL);
    }

    match kernel.fs_watch(pid, path, mask) {
        Ok(fd) => Response::ok(req.request_id, fd as i64),
        Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
    }
}

// ---- host_file_recv ---------------------------------------------------
//
// Layout (per `contracts/syscalls.md §3.6`):
//   args[0..4] = token (u32) — bootstrap-minted handle that
//                identifies the host file the caller wants to
//                receive. The bootstrap mints tokens when a user
//                drops a file on the browser tab (DOM `drop`
//                event) or picks one via the file-manager
//                `Import…` menu (`<input type="file">` change),
//                and posts a `host_file_dropped(token, name,
//                size, mime)` notification on the kernel-side
//                IPC endpoint `/run/host-files`. A subscribed
//                userland process (typically the file manager)
//                reads the notification and calls this opcode
//                with the carried token.
//   No heap input/output (the bytes are streamed via subsequent
//   `fd_read` calls on the returned fd, not crammed into the
//   syscall response).
//
// Response:
//   value      = freshly-allocated host-file fd-number on success.
//                Userland reads the file's bytes via `fd_read` on
//                this fd and closes via `fd_close` (which drops
//                the kernel-side bytes — the spec spells this
//                "Closing the fd releases the browser-side `File`
//                reference").
//   status     = -EBADF if `token` is unknown OR has already been
//                consumed by a prior recv (the spec collapses
//                both into EBADF — "Exactly one `host_file_recv`
//                call is permitted per token; a second call with
//                the same token returns `EBADF`. ... any other
//                caller [unknown token] receives `EBADF`").
//                -ESRCH if the caller's pid has been removed from
//                the process table mid-flight (defensive — in
//                normal execution the dispatcher requires the pid
//                to exist, so this arm is a guard for tests that
//                deliberately remove the process).
//                -EMFILE if the caller's fd table is full. The
//                host-file payload is rolled back into the
//                pending table so a fd-limit failure doesn't
//                burn the token (userland can free up an fd slot
//                and retry).
//
// **No capability check** — per spec §3.6: "no capability is
// required beyond ordinary IPC-endpoint subscription. The
// bootstrap is trusted to only produce tokens for files the
// *user* chose via explicit DOM events. Only processes that
// subscribe to `/run/host-files` and receive an unsolicited
// `host_file_dropped` notification have a meaningful reason to
// call this syscall; any other caller receives `EBADF` for an
// unknown token." The EBADF-on-unknown-token arm is the
// effective access control.
//
// The opcode is a thin adapter over `Kernel::host_file_recv`
// which owns the token-lookup + fd-alloc + rollback semantics.
// `kerr_to_errno` maps `KernelError::BadFd` → `-EBADF`,
// `OutOfFds` → `-EMFILE`, `NoSuchPid` → `-ESRCH` — every error
// arm of the semantic method has a canonical errno on the wire
// surface.

fn handle_host_file_recv(kernel: &mut Kernel, pid: Pid, req: &Request) -> Response {
    let token = args_u32(req, 0);
    match kernel.host_file_recv(pid, token) {
        Ok(fd) => Response::ok(req.request_id, fd as i64),
        Err(e) => Response::err(req.request_id, kerr_to_errno(e)),
    }
}

fn handle_host_file_pick(kernel: &Kernel, pid: Pid, req: &Request) -> Response {
    match kernel.host_file_pick(pid) {
        Ok(()) => Response::ok(req.request_id, 0),
        Err(error) => Response::err(req.request_id, kerr_to_errno(error)),
    }
}

// args[0..4] = name_len, args[4..8] = mime_len; heap is the exact
// concatenation `name || mime`. Bulk file bytes subsequently use FD_WRITE.
fn handle_host_file_send(kernel: &mut Kernel, pid: Pid, req: &Request, heap: &[u8]) -> Response {
    let name_len = args_u32(req, 0) as usize;
    let mime_len = args_u32(req, 4) as usize;
    let Some(expected_len) = name_len.checked_add(mime_len) else {
        return Response::err(req.request_id, EINVAL);
    };
    let Some(input) = heap_in(req, heap) else {
        return Response::err(req.request_id, EINVAL);
    };
    if input.len() != expected_len {
        return Response::err(req.request_id, EINVAL);
    }
    let Ok(name) = core::str::from_utf8(&input[..name_len]) else {
        return Response::err(req.request_id, EINVAL);
    };
    let Ok(mime) = core::str::from_utf8(&input[name_len..]) else {
        return Response::err(req.request_id, EINVAL);
    };
    match kernel.host_file_send(pid, name, mime) {
        Ok(fd) => Response::ok(req.request_id, fd as i64),
        Err(error) => Response::err(req.request_id, kerr_to_errno(error)),
    }
}
