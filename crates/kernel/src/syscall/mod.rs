//! Syscall dispatcher: SAB ring request → Kernel method → Response.
//!
//! The dispatcher is the bridge between the opcode-level binary
//! interface (see [`abi::wasi`] and [`abi::ext`]) and the Rust-level
//! method surface on [`crate::sys::Kernel`]. It lives inside the
//! kernel crate because it needs mutable access to every subsystem
//! the `Kernel` owns, and because its tests build against the
//! `native-platform` feature gate just like the rest of the kernel.
//!
//! ## Architecture
//!
//! * [`dispatch`] — top-level [`Dispatcher`] struct and the
//!   opcode → handler routing, plus helpers shared by both the
//!   WASI and extension sides (arg decoders, heap slice lookup,
//!   [`KernelError`](crate::sys::KernelError) → errno mapping).
//! * [`wasi`] — WASI preview 1 opcode handlers (range 0x0001..0x0080).
//! * [`ext`] — PMos extension opcode handlers (range 0x1000..0x1501).
//!
//! ## Scope today (first T073 landing)
//!
//! Nine opcodes have working handlers — the ones a minimal userland
//! program needs to speak to the kernel. On the WASI side:
//! `FD_READ`, `FD_WRITE`, `FD_CLOSE`, `PATH_OPEN`, `PROC_EXIT`. On
//! the extension side: `PROC_SELF`, `PROC_PARENT`, `CAP_CHECK`,
//! `CAP_LIST`. Every other opcode — known-but-unimplemented
//! (e.g. `PROC_SPAWN`, `IPC_SOCKET`, `CLOCK_TIME_GET`) and truly
//! unknown opcodes alike — gets back `ENOSYS` with the request_id
//! echoed, so userland sees a consistent "function not supported"
//! response rather than a hang.
//!
//! Expansion is mechanical: each new opcode adds one `match` arm
//! in the wasi or ext dispatch function, one handler, and one
//! isolation test. The dispatcher infrastructure itself does not
//! need to change for opcode width.

pub mod dispatch;
pub mod ext;
pub mod wasi;

pub use dispatch::{kerr_to_errno, Dispatcher, ServiceOutcome};

/// Convenience wrapper over [`dispatch::dispatch`] that unwraps a
/// [`ServiceOutcome::Done`] response. Tests and call-sites that do not
/// need to distinguish `Parked` from `Done` use this; call-sites that
/// DO care (e.g. `wasm_entry::kernel_dispatch`) call
/// `dispatch::dispatch` directly and match on the outcome.
///
/// Panics if the outcome is `Parked` — that variant is not reachable
/// until Task 5 lands `handle_ipc_accept`'s park path, so a panic
/// here is the right canary.
pub fn dispatch(
    kernel: &mut crate::sys::Kernel,
    pid: abi::ext::Pid,
    req: &abi::ring::Request,
    heap: &mut [u8],
) -> abi::ring::Response {
    match dispatch::dispatch(kernel, pid, req, heap) {
        ServiceOutcome::Done(resp) => resp,
        ServiceOutcome::Parked => {
            panic!("dispatch: unexpected Parked outcome before park-on-accept lands")
        }
    }
}
