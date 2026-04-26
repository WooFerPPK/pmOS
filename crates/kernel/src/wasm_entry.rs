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
//!
//! ## Test-only scaffolding
//!
//! `kernel_register_process_for_spawn` and
//! `kernel_mark_running`'s idempotent-against-Ready branch (the
//! skip-`mark_ready`-when-pid-is-already-Ready arm) exist purely
//! for TS dispatcher tests that compose `spawnChildForTest` with
//! `markRunning`. Production init never takes the idempotent
//! branch because freshly-registered pids start in `Starting`
//! state, not `Ready`. Both live in the production `kernel.wasm`
//! export surface for simplicity (no feature-gated dev build),
//! but neither should be called from production TS code —
//! `PROC_SPAWN` is the real process-creation path.

#![cfg(all(not(feature = "native-platform"), target_arch = "wasm32"))]
#![allow(static_mut_refs)]

extern crate alloc;

use alloc::boxed::Box;

use abi::cap::CapSet;
use abi::ext::Pid;
use abi::ring::{Request, SLOT_SIZE};

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::fd::{FdFlags, FdObject};
use crate::fs::devfs::{DevFs, DEV_CONSOLE};
use crate::fs::procfs::{
    format_argv_cmdline, proc_state_to_status, ProcFdSnapshot, ProcFs, ProcFsSource,
    ProcStatusSnapshot,
};
use crate::fs::tmpfs::TmpFs;
use crate::proc::ProcState;
use crate::sys::{Kernel, RegisterArgs};
use crate::syscall::dispatch::{self, ServiceOutcome};

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

/// User-SAB heap_ptr associated with the most recent wake drained
/// via `kernel_take_next_wake_for_pid`. Readable by the TS side
/// via `kernel_resp_heap_ptr`. Zero when no heap bytes were
/// written. Slice 2c.1.
static mut RESP_HEAP_PTR: u32 = 0;

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

/// Borrow the global kernel immutably. Used by [`LiveProcFsSource`]
/// to project read-only kernel state into `/proc` without taking
/// a fresh `&mut` (which would alias with the dispatch's outer
/// `kernel_mut` borrow). Panics if `kernel_init` hasn't been
/// called yet.
fn kernel_ref() -> &'static Kernel {
    unsafe {
        KERNEL
            .as_ref()
            .expect("kernel_init must be called before any other kernel_* export")
    }
}

// ---- live procfs source ------------------------------------------------

/// `ProcFsSource` projecting the live kernel singleton through
/// `/proc`. Each method call derefs `KERNEL` via `kernel_ref`,
/// snapshots the requested data into owned values, and returns —
/// no reference to kernel state escapes the call.
///
/// Replaces the `ProcFs::with_static()` placeholder in
/// `kernel_init` so `/proc/<pid>/status`, `/proc/<pid>/cmdline`,
/// and the top-level `/proc/version` reflect the running kernel
/// instead of canned test data.
///
/// Safety relies on the wasm32 single-threaded runtime: each
/// procfs read fires inside a syscall whose outer `kernel_mut`
/// has exclusive access; we take a fresh `&Kernel` for the
/// duration of one method call, never store it across call
/// boundaries, and only ever read.
pub struct LiveProcFsSource;

impl LiveProcFsSource {
    fn with_kernel<R>(f: impl FnOnce(&Kernel) -> R) -> R {
        f(kernel_ref())
    }
}

impl ProcFsSource for LiveProcFsSource {
    fn version(&self) -> String {
        format!(
            "PMos {} (wasm32, ABI {}.{})\n",
            env!("CARGO_PKG_VERSION"),
            abi::version::ABI_VERSION.0,
            abi::version::ABI_VERSION.1,
        )
    }

    fn uptime(&self) -> String {
        // Real uptime needs a kernel-side monotonic clock plumbed
        // through `crate::platform::Platform::now_ns`; deferred to
        // a clock-source slice. Until then, mirror the static
        // placeholder so user-space parsers don't fail.
        String::from("0 0\n")
    }

    fn meminfo(&self) -> String {
        // System-wide totals derived from per-process VM
        // accounting that landed in T168. Format mirrors the
        // existing placeholder shape ("total peak available")
        // in bytes, sourced from the live process table.
        Self::with_kernel(|k| {
            let mut total: u64 = 0;
            let mut peak: u64 = 0;
            for pid in k.procs.live_pids() {
                if let Some(proc) = k.procs.get(pid) {
                    total = total.saturating_add(proc.vm_size_bytes);
                    peak = peak.saturating_add(proc.vm_peak_bytes);
                }
            }
            format!("{} {} {}\n", total, peak, total)
        })
    }

    fn loadavg(&self) -> String {
        // Real loadavg needs scheduler-tick averaging; deferred.
        String::from("0.00 0.00 0.00 0/0 0\n")
    }

    fn pid_status(&self, pid: Pid) -> Option<ProcStatusSnapshot> {
        Self::with_kernel(|k| {
            let proc = k.procs.get(pid)?;
            if proc.state == ProcState::Dead {
                return None;
            }
            Some(ProcStatusSnapshot {
                pid: proc.pid,
                ppid: proc.ppid,
                name: proc.name.clone(),
                state: proc_state_to_status(proc.state),
                vm_size_bytes: proc.vm_size_bytes,
                vm_peak_bytes: proc.vm_peak_bytes,
            })
        })
    }

    fn live_pids(&self) -> Vec<Pid> {
        Self::with_kernel(|k| k.procs.live_pids())
    }

    fn pid_cmdline(&self, pid: Pid) -> Option<Vec<u8>> {
        Self::with_kernel(|k| {
            let proc = k.procs.get(pid)?;
            if proc.state == ProcState::Dead {
                return None;
            }
            Some(format_argv_cmdline(&proc.argv))
        })
    }

    fn pid_fds(&self, _pid: Pid) -> Vec<ProcFdSnapshot> {
        // pid_fds projection from the live FdTable through to
        // `/proc/<pid>/fd/<n>` symlinks is the next slice on top
        // of this one — needs the FdObject → path mapping that
        // KernelProcFsSource owns in tests but doesn't yet
        // export through the live source. The default empty Vec
        // matches the codex T168 first-landing slice (876d855).
        Vec::new()
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
        if k.vfs
            .mount("/proc", Box::new(ProcFs::new(Box::new(LiveProcFsSource))))
            .is_err()
        {
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

/// Test-only: register a child process under `parent` with the
/// `ORDINARY_APP` cap set + console stdio, equivalent to the
/// Rust-level `spawn_ordinary_app` test helper but callable from
/// the TS side for dispatcher tests. Returns the child pid on
/// success, -1 on failure.
///
/// The name is read from `HEAP_SCRATCH[0..name_len]` as UTF-8 + ASCII.
/// Typical name_len is 8-16; shorter names are zero-padded by the
/// caller (irrelevant since the exact bytes are copied).
#[no_mangle]
pub extern "C" fn kernel_register_process_for_spawn(
    parent: Pid,
    _name_ptr: u32,
    name_len: u32,
) -> i32 {
    let kernel = kernel_mut();
    let name_bytes = unsafe { &HEAP_SCRATCH[..name_len as usize] };
    let name = match core::str::from_utf8(name_bytes) {
        Ok(s) => s,
        Err(_) => return -1,
    };
    match kernel.proc_spawn(
        parent,
        crate::sys::SpawnArgs {
            name,
            caps: abi::cap::initial::ORDINARY_APP,
            cwd: "/",
            argv: alloc::vec::Vec::new(),
            envp: alloc::collections::BTreeMap::new(),
            stdin: FdObject::CharDevice(DEV_CONSOLE),
            stdout: FdObject::CharDevice(DEV_CONSOLE),
            stderr: FdObject::CharDevice(DEV_CONSOLE),
        },
    ) {
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

/// Install [`FdObject::SignalChannel`] as an fd in `pid`'s fd
/// table. Companion to [`kernel_install_console_fd`] that gives
/// the host a way to install the per-process signal channel
/// without decoding `FdObject` on the TS side. Normally the
/// kernel auto-installs SignalChannel at fd 3 on every
/// `proc_spawn`'d child; this export is for host-side tests
/// that start from a `register_process` primitive and want to
/// exercise the SignalChannel read / poll paths without
/// routing through spawn.
///
/// Returns `0` on success, `-1` on error (bad pid, fd-table
/// full, fd already in use, etc.).
#[no_mangle]
pub extern "C" fn kernel_install_signal_channel_fd(pid: Pid, fd: u32) -> i32 {
    let kernel = kernel_mut();
    match kernel.install_fd(pid, fd, FdObject::SignalChannel, FdFlags::EMPTY) {
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
/// If the pid is already `Ready` (e.g. created via `proc_spawn`
/// which auto-transitions to Ready), the `mark_ready` step is
/// skipped — this allows `spawnChildForTest` + `markRunning`
/// composition to work without the double-Ready error.
///
/// Returns `0` on success, `-1` on any state-machine error.
#[no_mangle]
pub extern "C" fn kernel_mark_running(pid: Pid) -> i32 {
    let kernel = kernel_mut();
    let already_ready = kernel
        .procs
        .get(pid)
        .map(|p| p.state == ProcState::Ready)
        .unwrap_or(false);
    if !already_ready {
        if kernel.mark_ready(pid).is_err() {
            return -1;
        }
    }
    if kernel.procs.transition(pid, ProcState::Running).is_err() {
        return -1;
    }
    0
}

/// Record the current wasm linear-memory size for `pid`.
///
/// The browser-side user Worker owns the process's isolated
/// [`WebAssembly.Memory`], so it reports byte counts through the
/// kernel Worker rather than through a syscall. `bytes_lo` /
/// `bytes_hi` form a little-endian u64 to keep the export ABI stable
/// if a future memory64-capable runtime reports more than 4 GiB.
///
/// Returns `0` on success, `-1` for an unknown pid.
#[no_mangle]
pub extern "C" fn kernel_record_process_memory(pid: Pid, bytes_lo: u32, bytes_hi: u32) -> i32 {
    let bytes = ((bytes_hi as u64) << 32) | (bytes_lo as u64);
    let kernel = kernel_mut();
    if kernel.procs.record_memory_size(pid, bytes) {
        0
    } else {
        -1
    }
}

/// Best-effort flush of every dirty VFS mount.
///
/// Called from the browser on `pagehide` (and other lifecycle
/// transitions where persistence may be the last thing to fire
/// before the page goes away) so OPFS-backed mutations are not
/// lost when the tab closes. Mirrors the proc_exit sync hook —
/// the per-process barrier covers normal exits, this export
/// covers the "user closes the tab while a process is running"
/// case.
///
/// Returns `0` if every dirty mount flushed cleanly, `-1` if
/// any mount returned an error from its `sync` hook (the others
/// are still flushed; mounts whose sync errored stay dirty for
/// the next attempt).
#[no_mangle]
pub extern "C" fn kernel_sync_all() -> i32 {
    let kernel = kernel_mut();
    if kernel.vfs.sync_dirty().is_ok() {
        0
    } else {
        -1
    }
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
    match dispatch::dispatch(kernel, pid, &req, heap) {
        ServiceOutcome::Done(resp) => {
            unsafe {
                RESP_SCRATCH = resp.to_le_bytes();
            }
            0
        }
        ServiceOutcome::Parked => {
            // Caller parked; no response written. JS side must NOT
            // push to the caller's SAB — caller stays on
            // Atomics.wait until a future drainWakesForPid pushes
            // the delayed response.
            1
        }
    }
}

/// Take the next pending wake for `pid` out of `Kernel.pending_wakes`,
/// write its 32-byte Response into RESP_SCRATCH, and if the entry
/// has a heap payload write it into `HEAP_SCRATCH[0..extra_len]`
/// and record the user's original heap_ptr in `RESP_HEAP_PTR`
/// (readable via `kernel_resp_heap_ptr`). Returns 1 if an entry
/// was drained, 0 if nothing is queued for this pid.
#[no_mangle]
pub extern "C" fn kernel_take_next_wake_for_pid(pid: Pid) -> i32 {
    let kernel = kernel_mut();
    let idx = match kernel.pending_wakes.iter().position(|(p, _, _)| *p == pid) {
        Some(i) => i,
        None => return 0,
    };
    let (_, resp, heap) = kernel.pending_wakes.remove(idx);
    unsafe {
        RESP_SCRATCH = resp.to_le_bytes();
    }
    if let Some(h) = heap {
        // Copy heap bytes into HEAP_SCRATCH[0..len]. Capped at
        // HEAP_SCRATCH_SIZE — the TS drainer reads `resp.extra_len`
        // bytes, which the handler layer guaranteed <= heap_len <=
        // HEAP_SCRATCH_SIZE at park time.
        let len = h.bytes.len().min(HEAP_SCRATCH_SIZE);
        unsafe {
            HEAP_SCRATCH[..len].copy_from_slice(&h.bytes[..len]);
            RESP_HEAP_PTR = h.heap_ptr;
        }
    } else {
        unsafe {
            RESP_HEAP_PTR = 0;
        }
    }
    1
}

/// Pointer-equivalent getter: returns the user-SAB heap_ptr
/// recorded by the most recent `kernel_take_next_wake_for_pid`
/// call. Meaningful only when that call returned 1 AND the
/// response's `extra_len > 0`; otherwise zero.
#[no_mangle]
pub extern "C" fn kernel_resp_heap_ptr() -> u32 {
    unsafe { RESP_HEAP_PTR }
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

/// Push `len` bytes of keyboard input into the kernel's
/// `/dev/input/kbd` input ring. Same pattern as
/// `kernel_inject_console_input`: the host writes bytes into the heap
/// scratch region at offset 0, then calls this function.
///
/// Returns `0` on success, `-1` if `len` exceeds heap scratch capacity.
#[no_mangle]
pub extern "C" fn kernel_inject_input_kbd(len: u32) -> i32 {
    let len = len as usize;
    if len > HEAP_SCRATCH_SIZE {
        return -1;
    }
    let kernel = kernel_mut();
    unsafe {
        kernel.devs.inject_kbd_event(&HEAP_SCRATCH[..len]);
    }
    0
}

/// Push `len` bytes of mouse input into the kernel's `/dev/input/mouse`
/// input ring. Same shape as `kernel_inject_input_kbd`.
///
/// Returns `0` on success, `-1` if `len` exceeds heap scratch capacity.
#[no_mangle]
pub extern "C" fn kernel_inject_input_mouse(len: u32) -> i32 {
    let len = len as usize;
    if len > HEAP_SCRATCH_SIZE {
        return -1;
    }
    let kernel = kernel_mut();
    unsafe {
        kernel.devs.inject_mouse_event(&HEAP_SCRATCH[..len]);
    }
    0
}
