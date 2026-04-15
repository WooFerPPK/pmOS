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
//! defines, four have handlers below: `PROC_SELF`, `PROC_PARENT`,
//! `CAP_CHECK`, `CAP_LIST`. Every other extension opcode
//! (`PROC_SPAWN`, `PROC_WAIT`, `PROC_KILL`, `IPC_*`, `DISPLAY_CONNECT`,
//! `CAP_GRANT`, `MOUNT`, `FS_WATCH`, `HOST_FILE_RECV`, ...) falls
//! through the `_ =>` arm and returns `ENOSYS`. Those semantic
//! surfaces exist on `Kernel` already (`Kernel::proc_spawn`,
//! `Kernel::display_connect`, etc.) — the dispatcher just hasn't
//! learned their argument shapes yet. Each one is a future slice.

use abi::cap::Cap;
use abi::errno::{EINVAL, ENOSYS, ESRCH};
use abi::ext::{self as op, Pid};
use abi::ring::{Request, Response};

use crate::sys::Kernel;

use super::dispatch::args_u32;

/// Dispatch a request whose opcode is in the PMos extension range.
/// The caller has already guarded with [`abi::ext::is_ext`].
pub fn dispatch_ext(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
    _heap: &mut [u8],
) -> Response {
    match req.opcode {
        op::PROC_SELF => handle_proc_self(pid, req),
        op::PROC_PARENT => handle_proc_parent(kernel, pid, req),
        op::CAP_CHECK => handle_cap_check(kernel, pid, req),
        op::CAP_LIST => handle_cap_list(kernel, pid, req),
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
