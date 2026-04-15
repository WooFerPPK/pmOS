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

use abi::errno::{EINVAL, ENOSYS};
use abi::ext::Pid;
use abi::ring::{Request, Response};
use abi::wasi as op;

use crate::fd::FdFlags;
use crate::proc::ExitStatus;
use crate::sys::Kernel;

use super::dispatch::{args_u32, heap_in, heap_out_mut, kerr_to_errno};

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
        op::FD_CLOSE => handle_fd_close(kernel, pid, req),
        op::PATH_OPEN => handle_path_open(kernel, pid, req, heap),
        op::PROC_EXIT => handle_proc_exit(kernel, pid, req),
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
