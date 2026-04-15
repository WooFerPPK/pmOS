//! `#[no_mangle] extern "C"` entry points for the
//! wasm32-unknown-unknown kernel cdylib.
//!
//! This module is the narrow seam between the browser and the
//! rest of the kernel crate. It owns three things:
//!
//! 1. A static `Kernel` singleton that lives for the lifetime of
//!    the kernel Worker and is initialised by the first
//!    `kernel_init` call from the host side.
//! 2. Three static scratch regions — a 32-byte request slot, a
//!    32-byte response slot, and a 4 KiB heap scratch buffer —
//!    whose addresses are handed to the host via pointer getters
//!    so the TS side can `DataView`-write into them.
//! 3. A set of `extern "C"` exports that the host calls to drive
//!    the dispatcher: init, register-a-process, install-a-fd,
//!    mark-running, and the core dispatch call.
//!
//! ## Scope today (T091-adjacent: the exports exist)
//!
//! Without these exports, `kernel.wasm` compiles to 86 bytes
//! because LLVM's dead-code elimination strips every symbol
//! unreachable from an export. With them, the whole syscall
//! dispatcher + `Kernel` method surface gets pulled in, and the
//! resulting module is ~60 KB — the real kernel.
//!
//! The surface is deliberately small. The "full" production
//! entry point — one that manages multiple user-process SABs
//! concurrently, routes into different Dispatcher instances,
//! and threads driver events through a wake path — lives with
//! T091's kernel-worker.ts integration slice. What's here is
//! enough for an integration test to prove the dispatcher can
//! be called from outside the kernel crate.
//!
//! ## Thread model
//!
//! The kernel Worker is single-threaded: exactly one dedicated
//! Web Worker hosts the kernel. `static mut` globals are
//! therefore sound to access from any exported function as long
//! as the JS side doesn't recurse through a host import back
//! into a different export. Every export in this module holds
//! a `&'static mut` to the kernel for the duration of the call
//! and relinquishes it before returning, which is the discipline
//! the host side has to honour.
//!
//! ## Cfg gating
//!
//! The module is only compiled for `target_arch = "wasm32"`
//! without the `native-platform` feature. Native tests
//! (`cargo test` on the host target) don't see any of this —
//! `static mut` + raw pointer arithmetic + host imports are
//! all browser-specific. Native isolation coverage for the
//! dispatcher lives in `crates/kernel/tests/syscall.rs`, which
//! calls `kernel::syscall::dispatch` directly.

#![cfg(all(not(feature = "native-platform"), target_arch = "wasm32"))]
#![allow(static_mut_refs)]

extern crate alloc;

use alloc::boxed::Box;

use abi::cap::CapSet;
use abi::ext::Pid;
use abi::ring::{Request, SLOT_SIZE};

use crate::fd::{FdFlags, FdObject};
use crate::fs::devfs::{DevFs, DEV_CONSOLE};
use crate::fs::procfs::ProcFs;
use crate::fs::tmpfs::TmpFs;
use crate::proc::ProcState;
use crate::sys::{Kernel, RegisterArgs};
use crate::syscall::dispatch;

/// Size of the heap scratch region the dispatcher reads/writes
/// variable-length payloads through. Picked as a round number
/// that comfortably fits a PATH_OPEN path + the longest
/// FD_WRITE buffer an app likely needs for stdin/stdout echoing.
/// Can be grown at the cost of more static memory if a future
/// opcode needs it.
const HEAP_SCRATCH_SIZE: usize = 4096;

/// Storage for the 32-byte request slot the host writes before
/// calling [`kernel_dispatch`]. The host gets its linear-memory
/// address via [`kernel_req_ptr`].
static mut REQ_SCRATCH: [u8; SLOT_SIZE] = [0u8; SLOT_SIZE];

/// Storage for the 32-byte response slot the dispatcher writes
/// and the host reads after [`kernel_dispatch`] returns.
static mut RESP_SCRATCH: [u8; SLOT_SIZE] = [0u8; SLOT_SIZE];

/// Heap scratch region used for variable-length payloads
/// (FD_WRITE bytes, PATH_OPEN path strings, FD_READ destination
/// buffer). See [`kernel_heap_ptr`] / [`kernel_heap_len`].
static mut HEAP_SCRATCH: [u8; HEAP_SCRATCH_SIZE] = [0u8; HEAP_SCRATCH_SIZE];

/// The global kernel singleton. `None` until [`kernel_init`] is
/// called; `Some` afterwards for the lifetime of the Worker.
static mut KERNEL: Option<Kernel> = None;

/// Borrow the global kernel mutably. Panics if `kernel_init`
/// hasn't been called yet — the host side MUST call it before
/// any other export.
fn kernel_mut() -> &'static mut Kernel {
    unsafe {
        KERNEL
            .as_mut()
            .expect("kernel_init must be called before any other kernel_* export")
    }
}

// ---- init --------------------------------------------------------------

/// Initialise the global kernel with the v1 default mount
/// layout:
///
/// * `/`     — tmpfs
/// * `/dev`  — devfs (null/zero/random/console/fb0/input_*)
/// * `/proc` — procfs
///
/// Returns `0` on success. A second call after a successful
/// init is a no-op and also returns `0` — idempotency makes the
/// host side simpler (a reloaded Worker can always call init
/// without checking whether it's already initialised).
///
/// Returns `-1` if any mount step fails. That should never
/// happen in practice: the default mounts mount empty
/// filesystems at absolute paths, and none of the error paths
/// in `Vfs::mount` are reachable from this call.
#[no_mangle]
pub extern "C" fn kernel_init() -> i32 {
    unsafe {
        if KERNEL.is_some() {
            return 0;
        }
        let mut k = Kernel::new();
        if k.vfs.mount("/", Box::new(TmpFs::new())).is_err() {
            return -1;
        }
        if k.vfs.mount("/dev", Box::new(DevFs::new())).is_err() {
            return -1;
        }
        if k.vfs.mount("/proc", Box::new(ProcFs::with_static())).is_err() {
            return -1;
        }
        KERNEL = Some(k);
    }
    0
}

// ---- scratch pointer getters -------------------------------------------
//
// The host calls these once after `kernel_init` and caches the
// values. They return pointers into the kernel's own linear
// memory, which the host accesses through a `DataView` backed by
// the kernel Worker's `WebAssembly.Memory.buffer`.

/// Pointer to the 32-byte request slot in the kernel's linear
/// memory. The host writes a [`Request`] here (little-endian,
/// the layout from [`Request::to_le_bytes`]) before calling
/// [`kernel_dispatch`].
#[no_mangle]
pub extern "C" fn kernel_req_ptr() -> u32 {
    unsafe { REQ_SCRATCH.as_ptr() as u32 }
}

/// Pointer to the 32-byte response slot. The host reads the
/// dispatched [`Response`] from here after [`kernel_dispatch`]
/// returns.
#[no_mangle]
pub extern "C" fn kernel_resp_ptr() -> u32 {
    unsafe { RESP_SCRATCH.as_ptr() as u32 }
}

/// Pointer to the heap scratch region. The host places
/// variable-length payloads (write bytes, path strings, the
/// destination window for fd_read, etc.) at this address and
/// sets `Request.heap_ptr` to an offset relative to the start
/// of the region, NOT the absolute linear-memory address.
#[no_mangle]
pub extern "C" fn kernel_heap_ptr() -> u32 {
    unsafe { HEAP_SCRATCH.as_ptr() as u32 }
}

/// Capacity of the heap scratch region in bytes.
#[no_mangle]
pub extern "C" fn kernel_heap_len() -> u32 {
    HEAP_SCRATCH_SIZE as u32
}

// ---- process lifecycle (thin wrappers over Kernel methods) -------------

/// Register a process with the given capability bitset. The
/// process's name is a fixed "proc" placeholder — this export
/// is intentionally minimal; the full `RegisterArgs` surface
/// (name strings, cwd strings, parent pid) belongs to the
/// T091 integration slice where the host side has real proc
/// identity to plumb.
///
/// Returns the new pid (positive) or `-1` on any error.
#[no_mangle]
pub extern "C" fn kernel_register_process(caps_bits: u64) -> i32 {
    let kernel = kernel_mut();
    match kernel.register_process(RegisterArgs {
        name: "proc",
        ppid: 0,
        caps: CapSet(caps_bits),
        cwd: "/",
    }) {
        Ok(pid) => pid as i32,
        Err(_) => -1,
    }
}

/// Install `/dev/console` as an fd in `pid`'s fd table.
/// Convenience wrapper so the host can set up stdin/stdout/stderr
/// without having to decode `FdObject` on the TS side.
///
/// Returns `0` on success, `-1` on error (bad pid, fd-table full,
/// etc.).
#[no_mangle]
pub extern "C" fn kernel_install_console_fd(pid: Pid, fd: u32) -> i32 {
    let kernel = kernel_mut();
    match kernel.install_fd(pid, fd, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// Transition a freshly-registered process from `Starting`
/// through `Ready` to `Running`. Needed because several
/// opcodes (notably `PROC_EXIT`) require the caller to be in
/// `Running` state — the dispatcher does not handle
/// state transitions itself; the host has to stage them.
///
/// Returns `0` on success, `-1` on any state-machine error.
#[no_mangle]
pub extern "C" fn kernel_mark_running(pid: Pid) -> i32 {
    let kernel = kernel_mut();
    if kernel.mark_ready(pid).is_err() {
        return -1;
    }
    if kernel.procs.transition(pid, ProcState::Running).is_err() {
        return -1;
    }
    0
}

// ---- dispatch ----------------------------------------------------------

/// Dispatch the request currently sitting in [`REQ_SCRATCH`] on
/// behalf of `pid`, writing the response into [`RESP_SCRATCH`].
///
/// The host-side protocol is:
///
/// 1. Serialise a `Request` into the 32 bytes at
///    [`kernel_req_ptr`] in the kernel's linear memory.
/// 2. If the request has a heap payload (FD_WRITE bytes,
///    PATH_OPEN path, ...), write it to
///    `kernel_heap_ptr()[req.heap_ptr .. req.heap_ptr + req.heap_len]`.
/// 3. Call `kernel_dispatch(pid)`.
/// 4. Read the 32-byte response from [`kernel_resp_ptr`].
/// 5. If the response populated the heap (FD_READ output,
///    etc.), read `resp.extra_len` bytes starting at
///    `kernel_heap_ptr()[req.heap_ptr]`.
///
/// Always returns `0`. The actual syscall status rides on
/// `Response::status`.
#[no_mangle]
pub extern "C" fn kernel_dispatch(pid: Pid) -> i32 {
    let kernel = kernel_mut();
    let req = unsafe { Request::from_le_bytes(&REQ_SCRATCH) };
    let heap = unsafe { &mut HEAP_SCRATCH[..] };
    let resp = dispatch(kernel, pid, &req, heap);
    unsafe {
        RESP_SCRATCH = resp.to_le_bytes();
    }
    0
}

// ---- device-input injection --------------------------------------------
//
// The kernel's device dispatcher has internal helpers
// (`DeviceDispatcher::inject_console_input` and friends) used by the
// native test harness to push bytes into a device's input ring. The
// production path uses them too: the TS console driver forwards
// keystrokes into the kernel by writing them into the heap scratch
// region and calling the export below.
//
// Input injection is the "TS driver → kernel" direction, the
// complement of `pmos_host_driver_call`, which is the "kernel → TS
// driver" direction.

/// Push `len` bytes of console input into the kernel's `/dev/console`
/// input ring. The bytes are read from the start of the heap scratch
/// region (offset 0), so the host side writes them there first via a
/// `DataView` on the exported memory, then calls this function.
///
/// Returns `0` on success, `-1` if `len` exceeds the heap scratch
/// capacity.
#[no_mangle]
pub extern "C" fn kernel_inject_console_input(len: u32) -> i32 {
    let len = len as usize;
    if len > HEAP_SCRATCH_SIZE {
        return -1;
    }
    let kernel = kernel_mut();
    unsafe {
        kernel.devs.inject_console_input(&HEAP_SCRATCH[..len]);
    }
    0
}
