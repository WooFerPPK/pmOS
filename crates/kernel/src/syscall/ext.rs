//! PMos extension opcode handlers.
//!
//! Same shape as [`super::wasi`]: each handler decodes a
//! [`Request`], calls one method (or one field accessor) on
//! [`crate::sys::Kernel`], and encodes a [`Response`]. Extension
//! handlers tend to be shorter than WASI handlers because the
//! majority of them either return a single scalar or require no
//! heap payload at all.
//!
//! ## Not-yet-implemented opcodes
//!
//! Of the ~15 extension opcodes `contracts/syscalls.md §3`
//! defines, five have handlers below: `PROC_SELF`, `PROC_PARENT`,
//! `CAP_CHECK`, `CAP_LIST`, and `PROC_SPAWN`. Every other extension
//! opcode (`PROC_WAIT`, `PROC_KILL`, `IPC_*`, `DISPLAY_CONNECT`,
//! `CAP_GRANT`, `MOUNT`, `FS_WATCH`, `HOST_FILE_RECV`, ...) falls
//! through the `_ =>` arm and returns `ENOSYS`. Those semantic
//! surfaces exist on `Kernel` already (`Kernel::proc_wait`,
//! `Kernel::display_connect`, etc.) — the dispatcher just hasn't
//! learned their argument shapes yet. Each one is a future slice.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use abi::cap::{Cap, CapSet};
use abi::errno::{EINVAL, ENOSYS, ESRCH};
use abi::ext::{self as op, Pid};
use abi::ring::{Request, Response};

use crate::platform;
use crate::proc::ExitStatus;
use crate::sys::{Kernel, SpawnArgs};

use super::dispatch::{args_u32, args_u64, heap_in, kerr_to_errno};

/// Dispatch a request whose opcode is in the PMos extension range.
/// The caller has already guarded with [`abi::ext::is_ext`].
pub fn dispatch_ext(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> Response {
    match req.opcode {
        op::PROC_SELF => handle_proc_self(pid, req),
        op::PROC_PARENT => handle_proc_parent(kernel, pid, req),
        op::CAP_CHECK => handle_cap_check(kernel, pid, req),
        op::CAP_LIST => handle_cap_list(kernel, pid, req),
        op::PROC_SPAWN => handle_proc_spawn(kernel, pid, req, heap),
        _ => Response::err(req.request_id, ENOSYS),
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
