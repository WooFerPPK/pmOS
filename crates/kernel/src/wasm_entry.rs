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
use alloc::collections::{BTreeMap, BTreeSet};

use abi::cap::CapSet;
use abi::ext::Pid;
use abi::ring::{Request, SLOT_SIZE};

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::fd::{FdFlags, FdObject};
use crate::fs::devfs::{DevFs, DEV_CONSOLE};
#[cfg(target_arch = "wasm32")]
use crate::fs::opfs::{block::WasmBlockDevice, OpfsFs};
use crate::fs::procfs::{
    classify_procfs_ino, classify_procfs_path, classify_procfs_relative, format_argv_cmdline,
    proc_state_to_status, ProcFdSnapshot, ProcFs, ProcFsNode, ProcFsSource, ProcStatusSnapshot,
    StorageSnapshot,
};
use crate::fs::tmpfs::TmpFs;
use crate::proc::{ExitStatus, ProcState};
use crate::sys::{Kernel, RegisterArgs};
use crate::syscall::dispatch::{self, ServiceOutcome};

/// Size of the heap scratch region the dispatcher reads/writes
/// variable-length payloads through. Picked as a round number
/// that comfortably fits a PATH_OPEN path + the longest
/// FD_WRITE buffer an app likely needs for stdin/stdout echoing.
/// Can be grown at the cost of more static memory if a future
/// opcode needs it.
/// Size of the per-syscall heap scratch window in bytes.
/// 64 KiB is enough to handle the chunked-blit op sequence
/// (`OP_BLIT_CHUNK` payloads cap at 24 KiB + a 4-byte offset
/// header) plus every other userland syscall (path strings,
/// console output, env vars). Larger frames present via
/// multi-call chunking rather than one giant fd_write.
const HEAP_SCRATCH_SIZE: usize = 64 * 1024;

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

/// Dispatch-scoped, owned projection used by the procfs mount. It is a
/// separate allocation from [`KERNEL`]: snapshot preparation finishes before
/// syscall dispatch starts, and procfs reads never reach back into the live
/// kernel while `dispatch` holds its exclusive borrow.
static mut LIVE_PROCFS_VIEW: Option<LiveProcFsView> = None;

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

// ---- live procfs source ------------------------------------------------

#[derive(Default)]
struct LiveProcFsView {
    boot_time_ns: u64,
    live_pids: Option<Vec<Pid>>,
    meminfo: Option<String>,
    loadavg: Option<String>,
    storage: Option<StorageSnapshot>,
    statuses: BTreeMap<Pid, Option<ProcStatusSnapshot>>,
    cmdlines: BTreeMap<Pid, Option<Vec<u8>>>,
    fds: BTreeMap<Pid, Vec<ProcFdSnapshot>>,
}

fn live_procfs_view() -> &'static LiveProcFsView {
    unsafe {
        LIVE_PROCFS_VIEW
            .as_ref()
            .expect("kernel_init must prepare procfs before it can be read")
    }
}

/// `ProcFsSource` serving the independently-owned projection prepared at the
/// syscall boundary. No method borrows [`KERNEL`], so a VFS call made through
/// `dispatch(&mut Kernel, ..)` cannot create an aliased kernel reference.
///
/// Replaces the `ProcFs::with_static()` placeholder in
/// `kernel_init` so `/proc/<pid>/status`, `/proc/<pid>/cmdline`,
/// and the top-level `/proc/version` reflect the running kernel
/// instead of canned test data.
///
pub struct LiveProcFsSource;

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
        // `<seconds_since_boot> <idle_seconds>\n` — Linux serves
        // both as floats with two decimals; v1 emits integer
        // seconds because the only consumer is sysmon. Idle time
        // stays 0 until the scheduler tracks per-tick idle/busy.
        let now = crate::platform::current().now_ns();
        let elapsed = now.saturating_sub(live_procfs_view().boot_time_ns);
        let secs = elapsed / 1_000_000_000;
        format!("{} 0\n", secs)
    }

    fn meminfo(&self) -> String {
        // System-wide totals derived from per-process VM
        // accounting that landed in T168. Format mirrors the
        // existing placeholder shape ("total peak available")
        // in bytes, sourced from the live process table.
        live_procfs_view()
            .meminfo
            .clone()
            .unwrap_or_else(|| String::from("0 0 0\n"))
    }

    fn loadavg(&self) -> String {
        // Linux format: "<1m> <5m> <15m> <running>/<total> <last_pid>\n".
        //
        // The three averages come from `Scheduler::load_averages`
        // (a Linux CALC_LOAD-style EMA over `Running + Ready`
        // process counts; see `proc/loadavg.rs`). Snapshot
        // preparation projects elapsed samples into an owned
        // copy; dispatch commits that copy only after the syscall
        // succeeds. This callback only reads the projection.
        //
        // `running/total` and `last_pid` project live state
        // through the process table — same as before the live
        // averaging landed.
        live_procfs_view()
            .loadavg
            .clone()
            .unwrap_or_else(|| String::from("0.00 0.00 0.00 0/0 0\n"))
    }

    fn pid_status(&self, pid: Pid) -> Option<ProcStatusSnapshot> {
        live_procfs_view().statuses.get(&pid).cloned().flatten()
    }

    fn live_pids(&self) -> Vec<Pid> {
        live_procfs_view().live_pids.clone().unwrap_or_default()
    }

    fn pid_cmdline(&self, pid: Pid) -> Option<Vec<u8>> {
        live_procfs_view().cmdlines.get(&pid).cloned().flatten()
    }

    fn pid_fds(&self, pid: Pid) -> Vec<ProcFdSnapshot> {
        live_procfs_view()
            .fds
            .get(&pid)
            .cloned()
            .unwrap_or_default()
    }

    fn storage_info(&self) -> Option<StorageSnapshot> {
        // /proc/storage projects the persistent root's OPFS quota
        // counters. When boot has fallen back to a volatile tmpfs
        // root (no FileSystemSyncAccessHandle, private mode, or an
        // invalid existing image), the procfs default formats
        // `0 0 0\n` so userspace parsers do not fail.
        live_procfs_view().storage.clone()
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ProcFsAccess {
    Metadata,
    Readdir,
}

#[derive(Default)]
struct ProcFsTargets {
    nodes: Vec<(ProcFsNode, ProcFsAccess)>,
}

impl ProcFsTargets {
    fn add(&mut self, node: ProcFsNode, access: ProcFsAccess) {
        if let Some((_, existing)) = self.nodes.iter_mut().find(|(item, _)| *item == node) {
            if access == ProcFsAccess::Readdir {
                *existing = ProcFsAccess::Readdir;
            }
            return;
        }
        self.nodes.push((node, access));
    }

    fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

fn snapshot_pid_status(kernel: &Kernel, pid: Pid) -> Option<ProcStatusSnapshot> {
    let process = kernel.procs.get(pid)?;
    if process.state == ProcState::Dead {
        return None;
    }
    Some(ProcStatusSnapshot {
        pid: process.pid,
        ppid: process.ppid,
        name: process.name.clone(),
        state: proc_state_to_status(process.state),
        vm_size_bytes: process.vm_size_bytes,
        vm_peak_bytes: process.vm_peak_bytes,
        open_fds: Some(
            kernel
                .fds(pid)
                .expect("every live process must own an fd table")
                .open_count(),
        ),
    })
}

fn snapshot_pid_cmdline(kernel: &Kernel, pid: Pid) -> Option<Vec<u8>> {
    let process = kernel.procs.get(pid)?;
    if process.state == ProcState::Dead {
        return None;
    }
    Some(format_argv_cmdline(&process.argv))
}

fn snapshot_fd(kernel: &Kernel, fd: u32, object: FdObject) -> ProcFdSnapshot {
    let target = match object {
        FdObject::Vnode { mount_id, ino } => {
            let mountpoint = kernel.vfs.mountpoint_of(mount_id).unwrap_or("?");
            format!("{}#{}", mountpoint, ino)
        }
        FdObject::CharDevice(devnum) => devnum_to_path(devnum),
        FdObject::PipeRead(id) => format!("pipe:[{}r]", id),
        FdObject::PipeWrite(id) => format!("pipe:[{}w]", id),
        FdObject::Socket(id) => format!("socket:[{}]", id),
        FdObject::DisplayConn(id) => format!("display:[{}]", id),
        FdObject::SignalChannel => String::from("signal:"),
        FdObject::Watch { watch_id } => format!("watch:[{}]", watch_id.0),
        FdObject::HostFile { token } => format!("host_file:[{}]", token),
        FdObject::HostDownload { id } => format!("host_download:[{}]", id),
    };
    ProcFdSnapshot { fd, target }
}

fn live_pid_fd_table(kernel: &Kernel, pid: Pid) -> Option<&crate::fd::FdTable> {
    let process = kernel.procs.get(pid)?;
    if process.state == ProcState::Dead {
        return None;
    }
    Some(
        kernel
            .fds(pid)
            .expect("every live process must own an fd table"),
    )
}

fn snapshot_pid_fds(kernel: &Kernel, pid: Pid) -> Vec<ProcFdSnapshot> {
    let Some(table) = live_pid_fd_table(kernel, pid) else {
        return Vec::new();
    };
    table
        .iter()
        .map(|(fd, entry)| snapshot_fd(kernel, fd, entry.object))
        .collect()
}

fn snapshot_pid_fd(kernel: &Kernel, pid: Pid, fd: u32) -> Option<ProcFdSnapshot> {
    let table = live_pid_fd_table(kernel, pid)?;
    let entry = table.get(fd)?;
    Some(snapshot_fd(kernel, fd, entry.object))
}

fn prepare_live_procfs_view(
    kernel: &mut Kernel,
    targets: &ProcFsTargets,
) -> Option<crate::proc::loadavg::LoadAverages> {
    if targets.is_empty() {
        return None;
    }
    let mut projected_load_averages = None;
    let mut view = LiveProcFsView {
        boot_time_ns: kernel.boot_time_ns,
        ..LiveProcFsView::default()
    };
    let mut full_fd_snapshots = BTreeSet::new();
    for (node, access) in &targets.nodes {
        match *node {
            ProcFsNode::Root if *access == ProcFsAccess::Readdir => {
                view.live_pids = Some(kernel.procs.live_pids());
            }
            ProcFsNode::Root | ProcFsNode::Version | ProcFsNode::Uptime => {}
            ProcFsNode::Meminfo => {
                if view.meminfo.is_none() {
                    let mut total = 0u64;
                    let mut peak = 0u64;
                    for pid in kernel.procs.live_pids() {
                        if let Some(process) = kernel.procs.get(pid) {
                            total = total.saturating_add(process.vm_size_bytes);
                            peak = peak.saturating_add(process.vm_peak_bytes);
                        }
                    }
                    view.meminfo = Some(format!("{} {} {}\n", total, peak, total));
                }
            }
            ProcFsNode::Loadavg => {
                if view.loadavg.is_none() {
                    let live = kernel.procs.live_pids();
                    let total = live.len();
                    let mut running = 0usize;
                    let mut runnable = 0u32;
                    for pid in live {
                        if let Some(process) = kernel.procs.get(pid) {
                            if process.state == ProcState::Running {
                                running += 1;
                                runnable = runnable.saturating_add(1);
                            } else if process.state == ProcState::Ready {
                                runnable = runnable.saturating_add(1);
                            }
                        }
                    }
                    let last_pid = kernel.procs.next_pid_peek().saturating_sub(1);
                    let mut projected = kernel.sched.load_averages;
                    projected.tick(crate::platform::current().now_ns(), runnable);
                    let three = projected.format_three();
                    view.loadavg = Some(format!("{} {}/{} {}\n", three, running, total, last_pid));
                    projected_load_averages = Some(projected);
                }
            }
            ProcFsNode::Storage => {
                view.storage =
                    kernel
                        .vfs
                        .storage_usage("/")
                        .ok()
                        .flatten()
                        .map(|usage| StorageSnapshot {
                            quota_bytes: usage.quota_bytes,
                            used_bytes: usage.used_bytes,
                            file_count: usage.file_count,
                        });
            }
            ProcFsNode::PidDir(pid) | ProcFsNode::PidStatus(pid) | ProcFsNode::PidMaps(pid) => {
                view.statuses
                    .entry(pid)
                    .or_insert_with(|| snapshot_pid_status(kernel, pid));
                if matches!(node, ProcFsNode::PidDir(_)) && *access == ProcFsAccess::Readdir {
                    view.cmdlines
                        .entry(pid)
                        .or_insert_with(|| snapshot_pid_cmdline(kernel, pid));
                }
            }
            ProcFsNode::PidCmdline(pid) => {
                view.statuses
                    .entry(pid)
                    .or_insert_with(|| snapshot_pid_status(kernel, pid));
                view.cmdlines
                    .entry(pid)
                    .or_insert_with(|| snapshot_pid_cmdline(kernel, pid));
            }
            ProcFsNode::PidFdDir(pid) => {
                view.statuses
                    .entry(pid)
                    .or_insert_with(|| snapshot_pid_status(kernel, pid));
                if *access == ProcFsAccess::Readdir {
                    view.fds.insert(pid, snapshot_pid_fds(kernel, pid));
                    full_fd_snapshots.insert(pid);
                }
            }
            ProcFsNode::PidFd(pid, fd) => {
                view.statuses
                    .entry(pid)
                    .or_insert_with(|| snapshot_pid_status(kernel, pid));
                if !full_fd_snapshots.contains(&pid) {
                    if let Some(snapshot) = snapshot_pid_fd(kernel, pid, fd) {
                        let snapshots = view.fds.entry(pid).or_default();
                        if snapshots.iter().all(|candidate| candidate.fd != fd) {
                            snapshots.push(snapshot);
                        }
                    }
                }
            }
        }
    }
    unsafe {
        LIVE_PROCFS_VIEW = Some(view);
    }
    projected_load_averages
}

#[inline]
fn request_arg_u32(req: &Request, offset: usize) -> u32 {
    u32::from_le_bytes([
        req.args[offset],
        req.args[offset + 1],
        req.args[offset + 2],
        req.args[offset + 3],
    ])
}

fn request_heap<'a>(req: &Request, heap: &'a [u8]) -> Option<&'a [u8]> {
    let start = req.heap_ptr as usize;
    let end = start.checked_add(req.heap_len as usize)?;
    heap.get(start..end)
}

fn procfs_node_for_fd(kernel: &Kernel, pid: Pid, fd: u32) -> Option<ProcFsNode> {
    let entry = kernel.fds(pid).ok()?.get(fd)?;
    let FdObject::Vnode { mount_id, ino } = entry.object else {
        return None;
    };
    if kernel.vfs.mountpoint_of(mount_id) != Some("/proc") {
        return None;
    }
    classify_procfs_ino(ino)
}

fn classify_procfs_at(base: ProcFsNode, relative: &str) -> Option<ProcFsNode> {
    if relative.starts_with('/') {
        return classify_procfs_path(relative);
    }
    if relative.split('/').any(|component| component == "..") {
        return None;
    }
    if relative.is_empty() || relative == "." {
        return Some(base);
    }
    let prefix = match base {
        ProcFsNode::Root => String::new(),
        ProcFsNode::PidDir(pid) => pid.to_string(),
        ProcFsNode::PidFdDir(pid) => format!("{pid}/fd"),
        other => return Some(other),
    };
    let joined = if prefix.is_empty() {
        String::from(relative)
    } else {
        format!("{prefix}/{relative}")
    };
    classify_procfs_relative(&joined)
}

fn procfs_node_for_path(
    kernel: &mut Kernel,
    pid: Pid,
    dir_fd: Option<u32>,
    path: &str,
    follow_last: bool,
) -> Option<ProcFsNode> {
    if path.starts_with('/') || dir_fd.is_none() {
        if let Some(node) = classify_procfs_path(path) {
            return Some(node);
        }
        let redirected = kernel
            .vfs
            .path_entering_mount(path, "/proc", follow_last)
            .ok()??;
        return classify_procfs_path(&redirected);
    }
    let fd = dir_fd?;
    let entry = kernel.fds(pid).ok()?.get(fd)?;
    let FdObject::Vnode { mount_id, ino } = entry.object else {
        return None;
    };
    match kernel.vfs.mountpoint_of(mount_id)? {
        "/proc" => classify_procfs_at(classify_procfs_ino(ino)?, path),
        "/" if fd == abi::fd::ROOT_PREOPEN => {
            if let Some(node) = classify_procfs_path(path) {
                Some(node)
            } else {
                let redirected = kernel
                    .vfs
                    .path_entering_mount(path, "/proc", follow_last)
                    .ok()??;
                classify_procfs_path(&redirected)
            }
        }
        _ => {
            let redirected = kernel
                .vfs
                .path_entering_mount_at(mount_id, ino, path, "/proc", follow_last)
                .ok()??;
            classify_procfs_path(&redirected)
        }
    }
}

fn add_utf8_path(
    targets: &mut ProcFsTargets,
    kernel: &mut Kernel,
    pid: Pid,
    dir_fd: Option<u32>,
    bytes: &[u8],
    follow_last: bool,
    access: ProcFsAccess,
) {
    let Ok(path) = core::str::from_utf8(bytes) else {
        return;
    };
    if let Some(node) = procfs_node_for_path(kernel, pid, dir_fd, path, follow_last) {
        targets.add(node, access);
    }
}

fn collect_mount_procfs_target(
    targets: &mut ProcFsTargets,
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
    heap: &[u8],
) {
    if u32::from(req.flags) & abi::ext::mount_flags::MOUNT_REMOUNT != 0
        || !matches!(kernel.caps.check(pid, abi::cap::Cap::Mount), Ok(true))
    {
        return;
    }

    let path_start = request_arg_u32(req, 0) as usize;
    let path_len = request_arg_u32(req, 4) as usize;
    let fstype_start = request_arg_u32(req, 8) as usize;
    let fstype_len = request_arg_u32(req, 12) as usize;
    let Some(path) = path_start
        .checked_add(path_len)
        .and_then(|end| heap.get(path_start..end))
    else {
        return;
    };
    let Some(fstype) = fstype_start
        .checked_add(fstype_len)
        .and_then(|end| heap.get(fstype_start..end))
    else {
        return;
    };
    let (Ok(path_text), Ok(fstype_text)) =
        (core::str::from_utf8(path), core::str::from_utf8(fstype))
    else {
        return;
    };
    if fstype_text != "tmpfs" || !path_text.starts_with('/') {
        return;
    }
    let normalised = crate::vfs::path::normalize(path_text);
    if normalised == "/"
        || kernel
            .vfs
            .mountpoints()
            .iter()
            .any(|(_, mountpoint)| mountpoint == &normalised)
    {
        return;
    }
    add_utf8_path(
        targets,
        kernel,
        pid,
        None,
        path,
        true,
        ProcFsAccess::Readdir,
    );
}

fn collect_procfs_targets(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
    heap: &[u8],
) -> ProcFsTargets {
    use abi::wasi as op;

    let mut targets = ProcFsTargets::default();
    match req.opcode {
        op::FD_READDIR => {
            if let Some(node) = procfs_node_for_fd(kernel, pid, request_arg_u32(req, 0)) {
                // Preserve the handler's fd/type error precedence for a zero
                // or malformed output window, but do not materialise up to a
                // full 1,024-entry `/proc/<pid>/fd` snapshot when no dirent
                // byte can be returned.
                let access = if req.heap_len != 0 && request_heap(req, heap).is_some() {
                    ProcFsAccess::Readdir
                } else {
                    ProcFsAccess::Metadata
                };
                targets.add(node, access);
            }
        }
        op::FD_ALLOCATE
        | op::FD_FDSTAT_GET
        | op::FD_FILESTAT_GET
        | op::FD_FILESTAT_SET_SIZE
        | op::FD_FILESTAT_SET_TIMES
        | op::FD_PREAD
        | op::FD_READ
        | op::FD_SEEK => {
            if let Some(node) = procfs_node_for_fd(kernel, pid, request_arg_u32(req, 0)) {
                targets.add(node, ProcFsAccess::Metadata);
            }
        }
        op::POLL_ONEOFF => {
            if let Some(payload) = request_heap(req, heap) {
                let count = request_arg_u32(req, 0) as usize;
                let event_cap = request_arg_u32(req, 4) as usize;
                let Ok(admission) = kernel.poll_admission_class(pid) else {
                    return targets;
                };
                let limit = admission.per_call_limit();
                if count == 0 || count > limit || event_cap == 0 || event_cap > limit {
                    return targets;
                }
                let stride = abi::wasi::poll::SUBSCRIPTION_SIZE;
                let Some(subscriptions_len) = count.checked_mul(stride) else {
                    return targets;
                };
                let Some(events_len) = event_cap.checked_mul(abi::wasi::poll::EVENT_SIZE) else {
                    return targets;
                };
                if payload.len() < core::cmp::max(subscriptions_len, events_len) {
                    return targets;
                }
                for index in 0..count {
                    let subscription = &payload[index * stride..(index + 1) * stride];
                    let tag = subscription[abi::wasi::poll::SUB_OFF_TAG];
                    if tag != abi::wasi::eventtype::FD_READ && tag != abi::wasi::eventtype::FD_WRITE
                    {
                        continue;
                    }
                    let offset = abi::wasi::poll::SUB_FDRW_OFF_FD;
                    let fd = u32::from_le_bytes([
                        subscription[offset],
                        subscription[offset + 1],
                        subscription[offset + 2],
                        subscription[offset + 3],
                    ]);
                    if let Some(node) = procfs_node_for_fd(kernel, pid, fd) {
                        targets.add(node, ProcFsAccess::Metadata);
                    }
                }
            }
        }
        op::PATH_OPEN => {
            if let Some(payload) = request_heap(req, heap) {
                add_utf8_path(
                    &mut targets,
                    kernel,
                    pid,
                    Some(request_arg_u32(req, 12)),
                    payload,
                    request_arg_u32(req, 8) & abi::wasi::lookupflags::SYMLINK_FOLLOW != 0,
                    ProcFsAccess::Metadata,
                );
            }
        }
        op::PATH_FILESTAT_GET => {
            if let Some(payload) = request_heap(req, heap) {
                add_utf8_path(
                    &mut targets,
                    kernel,
                    pid,
                    None,
                    payload,
                    request_arg_u32(req, 4) & abi::wasi::lookupflags::SYMLINK_FOLLOW != 0,
                    ProcFsAccess::Metadata,
                );
            }
        }
        op::PATH_FILESTAT_SET_TIMES => {
            if let Some(payload) = request_heap(req, heap).and_then(|bytes| bytes.get(16..)) {
                add_utf8_path(
                    &mut targets,
                    kernel,
                    pid,
                    None,
                    payload,
                    true,
                    ProcFsAccess::Metadata,
                );
            }
        }
        op::PATH_CREATE_DIRECTORY => {
            if let Some(payload) = request_heap(req, heap) {
                add_utf8_path(
                    &mut targets,
                    kernel,
                    pid,
                    None,
                    payload,
                    false,
                    ProcFsAccess::Metadata,
                );
            }
        }
        op::PATH_UNLINK_FILE | op::PATH_REMOVE_DIRECTORY => {
            if let Some(payload) = request_heap(req, heap) {
                add_utf8_path(
                    &mut targets,
                    kernel,
                    pid,
                    Some(request_arg_u32(req, 0)),
                    payload,
                    false,
                    ProcFsAccess::Metadata,
                );
            }
        }
        op::PATH_RENAME | op::PATH_LINK => {
            if let Some(payload) = request_heap(req, heap) {
                let split = if req.opcode == op::PATH_RENAME {
                    request_arg_u32(req, 8) as usize
                } else {
                    request_arg_u32(req, 12) as usize
                };
                if split > 0 && split < payload.len() {
                    let (old_path, new_path) = payload.split_at(split);
                    add_utf8_path(
                        &mut targets,
                        kernel,
                        pid,
                        None,
                        old_path,
                        false,
                        ProcFsAccess::Metadata,
                    );
                    add_utf8_path(
                        &mut targets,
                        kernel,
                        pid,
                        None,
                        new_path,
                        false,
                        ProcFsAccess::Metadata,
                    );
                }
            }
        }
        op::PATH_SYMLINK => {
            if let Some(payload) = request_heap(req, heap) {
                let split = request_arg_u32(req, 0) as usize;
                if split > 0 && split < payload.len() {
                    add_utf8_path(
                        &mut targets,
                        kernel,
                        pid,
                        None,
                        &payload[split..],
                        false,
                        ProcFsAccess::Metadata,
                    );
                }
            }
        }
        op::PATH_READLINK => {
            if let Some(payload) = request_heap(req, heap) {
                let path_len = request_arg_u32(req, 4) as usize;
                if let Some(path) = payload.get(..path_len) {
                    add_utf8_path(
                        &mut targets,
                        kernel,
                        pid,
                        None,
                        path,
                        false,
                        ProcFsAccess::Metadata,
                    );
                }
            }
        }
        abi::ext::MOUNT => {
            collect_mount_procfs_target(&mut targets, kernel, pid, req, heap);
        }
        abi::ext::UMOUNT | abi::ext::FS_WATCH => {
            let start = request_arg_u32(req, 0) as usize;
            let len = request_arg_u32(req, 4) as usize;
            if let Some(path) = start.checked_add(len).and_then(|end| heap.get(start..end)) {
                add_utf8_path(
                    &mut targets,
                    kernel,
                    pid,
                    None,
                    path,
                    true,
                    ProcFsAccess::Metadata,
                );
            }
        }
        abi::ext::FS_CHMOD => {
            if let Some(payload) = request_heap(req, heap) {
                let path_len = request_arg_u32(req, 0) as usize;
                if let Some(path) = payload.get(..path_len) {
                    add_utf8_path(
                        &mut targets,
                        kernel,
                        pid,
                        None,
                        path,
                        true,
                        ProcFsAccess::Metadata,
                    );
                }
            }
        }
        _ => {}
    }
    targets
}

/// Map a device number to its canonical path for `/proc/<pid>/fd/<n>`
/// symlink targets. Mirrors the names in the devfs init layout
/// (`crates/kernel/src/fs/devfs.rs`); unknown devnums fall back to
/// `dev:[<n>]` so an unrecognised device doesn't crash the
/// projection.
fn devnum_to_path(devnum: u32) -> String {
    use crate::fs::devfs::{
        DEV_CONSOLE, DEV_FB0, DEV_INPUT_KBD, DEV_INPUT_MOUSE, DEV_NULL, DEV_RANDOM, DEV_ZERO,
    };
    match devnum {
        DEV_NULL => String::from("/dev/null"),
        DEV_ZERO => String::from("/dev/zero"),
        DEV_RANDOM => String::from("/dev/random"),
        DEV_CONSOLE => String::from("/dev/console"),
        DEV_FB0 => String::from("/dev/fb0"),
        DEV_INPUT_KBD => String::from("/dev/input_kbd"),
        DEV_INPUT_MOUSE => String::from("/dev/input_mouse"),
        n => format!("dev:[{}]", n),
    }
}

// ---- init --------------------------------------------------------------

/// Initialise the global kernel with the v1 default mount
/// layout:
///
/// * `/`     — OPFS when available and valid, otherwise tmpfs
/// * `/tmp`  — tmpfs
/// * `/run`  — tmpfs
/// * `/dev`  — devfs (null/zero/random/console/fb0/input_*)
/// * `/proc` — procfs
///
/// Returns `0` on success. A second call after a successful
/// init is a no-op and also returns `0` — idempotency makes the
/// host side simpler (a reloaded Worker can always call init
/// without checking whether it's already initialised).
///
/// Returns `-1` if the root or a required virtual mount cannot be
/// installed. An unavailable or invalid persistent image is not a
/// fatal boot error: the existing bytes are left untouched and the
/// kernel makes the degraded tmpfs-root mode observable in the boot
/// log and through `/proc/storage` (`0 0 0`).
#[no_mangle]
pub extern "C" fn kernel_init() -> i32 {
    unsafe {
        if KERNEL.is_some() {
            return 0;
        }
        let mut k = Kernel::new();

        // T084: the browser-side block driver owns a
        // FileSystemSyncAccessHandle over `pmos.img`. A
        // driver-proven newly-created image may be formatted; an
        // existing image is mount-only, and a validation failure is
        // never treated as permission to overwrite it.
        let mut persistent_root_mounted = false;
        #[cfg(target_arch = "wasm32")]
        if let Ok(device) = WasmBlockDevice::open() {
            let image_state = device.image_state();
            match OpfsFs::open_image(alloc::boxed::Box::new(device), image_state) {
                Ok(opfs) => {
                    if k.vfs.mount("/", Box::new(opfs)).is_ok() {
                        persistent_root_mounted = true;
                        let _ = k
                            .devs
                            .write(DEV_CONSOLE, b"[pmos] persistent OPFS root mounted at /\n");
                    } else {
                        let _ = k.devs.write(
                            DEV_CONSOLE,
                            b"[pmos] persistent root unavailable; storage left untouched; using volatile tmpfs root\n",
                        );
                    }
                }
                Err(_) => {
                    let _ = k.devs.write(
                        DEV_CONSOLE,
                        b"[pmos] persistent root unavailable or invalid; storage left untouched; using volatile tmpfs root\n",
                    );
                }
            }
        }

        if !persistent_root_mounted && k.vfs.mount("/", Box::new(TmpFs::new())).is_err() {
            return -1;
        }
        // A valid older OPFS image may predate newly bundled system files;
        // migrate only missing defaults. The volatile fallback also needs a
        // coherent OS tree and starter home so applications remain usable
        // even though reload persistence is unavailable.
        if crate::fs::seed::seed_system_defaults(&mut k.vfs).is_err() {
            return -1;
        }
        if !persistent_root_mounted && crate::fs::seed::seed_volatile_user_home(&mut k.vfs).is_err()
        {
            return -1;
        }
        if k.vfs.mount("/tmp", Box::new(TmpFs::new())).is_err() {
            return -1;
        }
        if k.vfs.mount("/run", Box::new(TmpFs::new())).is_err() {
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
        LIVE_PROCFS_VIEW = Some(LiveProcFsView {
            boot_time_ns: k.boot_time_ns,
            ..LiveProcFsView::default()
        });
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
    kernel
        .register_process(RegisterArgs {
            name: "proc",
            ppid: 0,
            caps: CapSet(caps_bits),
            cwd: "/",
        })
        .unwrap_or(-1)
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
    kernel
        .proc_spawn(
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
        )
        .unwrap_or(-1)
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

/// Install the WASI `/` directory preopen in a process created through the
/// low-level [`kernel_register_process`] host path. Processes created by
/// `proc_spawn` receive this descriptor automatically.
#[no_mangle]
pub extern "C" fn kernel_install_root_preopen_fd(pid: Pid, fd: u32) -> i32 {
    let kernel = kernel_mut();
    match kernel.install_root_preopen_fd(pid, fd) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// Install [`FdObject::SignalChannel`] as an fd in `pid`'s fd
/// table. Companion to [`kernel_install_console_fd`] that gives
/// the host a way to install the per-process signal channel
/// without decoding `FdObject` on the TS side. Normally the
/// kernel auto-installs SignalChannel at [`abi::fd::SIGNAL`] on every
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
    if !already_ready && kernel.mark_ready(pid).is_err() {
        return -1;
    }
    if kernel.procs.transition(pid, ProcState::Running).is_err() {
        return -1;
    }
    0
}

/// Reconcile a host-observed user Worker termination with the
/// authoritative process table.
///
/// A clean WASI `proc_exit` normally transitions the pid before the
/// Worker reports its final message, so terminal pids are accepted as
/// an idempotent no-op. A wasm trap, Worker script error, or failure
/// to construct/boot the Worker has no syscall path back into the
/// kernel; those cases transition the still-live pid here and run the
/// same cleanup, SIGCHLD, and parked-parent wake path as `proc_exit`.
///
/// Returns `0` when the pid is known (newly reconciled or already
/// terminal), `1` when it is unknown/reaped, and `-1` on an unexpected
/// transition error.
#[no_mangle]
pub extern "C" fn kernel_reconcile_process_exit(pid: Pid, code: i32, crashed: u32) -> i32 {
    let kernel = kernel_mut();
    let Some(process) = kernel.procs.get(pid) else {
        return 1;
    };
    if matches!(process.state, ProcState::Zombie | ProcState::Dead) {
        return 0;
    }
    let status = if crashed != 0 {
        ExitStatus::Crashed
    } else {
        ExitStatus::Exited(code)
    };
    match kernel.proc_exit(pid, status) {
        Ok(()) => {
            kernel.service_poll_waiters();
            0
        }
        Err(_) => -1,
    }
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

/// Register a host-imported file in the kernel's host-file
/// table, keyed by `token`. The TS side has stashed the host
/// `File` (drag-drop or `<input type="file">` source) and chosen
/// `token` as a fresh per-tab handle; this export wires the name
/// + mime + bytes into the kernel-side `HostFile` so a userland
///   `host_file_recv(token)` can later consume them.
///
/// Layout in `HEAP_SCRATCH`:
/// * `[0 .. name_len)`       — UTF-8 file name
/// * `[name_len .. name_len + mime_len)` — UTF-8 mime type
/// * `[name_len + mime_len .. name_len + mime_len + bytes_len)` — raw bytes
///
/// Returns `0` on success; malformed scratch/UTF-8 inputs return `-1`, while
/// kernel policy failures (including import-count or byte-budget exhaustion)
/// return the corresponding negative errno.
///
/// Backs T153 / T154: the bootstrap-side drag-drop handler posts
/// a `host:dropped` MainToKernel message; `kernel-worker-entry.ts`
/// copies the bytes into the kernel heap scratch and calls this
/// export. The userland file-manager flow then calls
/// `host_file_recv(token)` (opcode 0x1500) to obtain a read-only
/// fd for the bytes.
#[no_mangle]
pub extern "C" fn kernel_host_file_dropped(
    token: u32,
    name_len: u32,
    mime_len: u32,
    bytes_len: u32,
) -> i32 {
    use crate::host_file::HostFile;
    let total = (name_len as usize) + (mime_len as usize) + (bytes_len as usize);
    if total > HEAP_SCRATCH_SIZE {
        return -1;
    }

    // SAFETY: kernel Worker is single-threaded; the host has
    // pre-staged the bytes into HEAP_SCRATCH before this call and
    // does not write to it again until we return.
    let scratch = unsafe { &HEAP_SCRATCH[..total] };
    let name_bytes = &scratch[..name_len as usize];
    let mime_bytes = &scratch[name_len as usize..name_len as usize + mime_len as usize];
    let body_bytes = &scratch[name_len as usize + mime_len as usize..];

    let name = match core::str::from_utf8(name_bytes) {
        Ok(s) => s,
        Err(_) => return -1,
    };
    let mime = match core::str::from_utf8(mime_bytes) {
        Ok(s) => s,
        Err(_) => return -1,
    };

    let kernel = kernel_mut();
    let mut owned = Vec::with_capacity(body_bytes.len());
    owned.extend_from_slice(body_bytes);
    let host_file = HostFile::new(name, mime, owned);
    match kernel.host_file_dropped(token, host_file) {
        Ok(()) => {
            kernel.service_poll_waiters();
            0
        }
        Err(error) => -crate::syscall::kerr_to_errno(error),
    }
}

/// Begin a host import whose metadata is staged as `name || mime` in
/// `HEAP_SCRATCH`. File bytes follow through repeated
/// `kernel_host_file_drop_chunk` calls, each independently bounded by the
/// scratch region.
#[no_mangle]
pub extern "C" fn kernel_host_file_drop_begin(
    token: u32,
    name_len: u32,
    mime_len: u32,
    expected_size: u32,
) -> i32 {
    let Some(total) = (name_len as usize).checked_add(mime_len as usize) else {
        return -abi::errno::EINVAL;
    };
    if total > HEAP_SCRATCH_SIZE {
        return -abi::errno::EINVAL;
    }
    // SAFETY: single-threaded kernel Worker; host staged this exact window.
    let scratch = unsafe { &HEAP_SCRATCH[..total] };
    let Ok(name) = core::str::from_utf8(&scratch[..name_len as usize]) else {
        return -abi::errno::EINVAL;
    };
    let Ok(mime) = core::str::from_utf8(&scratch[name_len as usize..]) else {
        return -abi::errno::EINVAL;
    };
    match kernel_mut().host_file_drop_begin(token, name, mime, expected_size as usize) {
        Ok(()) => 0,
        Err(error) => -crate::syscall::kerr_to_errno(error),
    }
}

#[no_mangle]
pub extern "C" fn kernel_host_file_drop_chunk(token: u32, bytes_len: u32) -> i32 {
    if bytes_len as usize > HEAP_SCRATCH_SIZE {
        return -abi::errno::EINVAL;
    }
    // SAFETY: single-threaded kernel Worker; host staged this exact window.
    let bytes = unsafe { &HEAP_SCRATCH[..bytes_len as usize] };
    match kernel_mut().host_file_drop_chunk(token, bytes) {
        Ok(()) => 0,
        Err(error) => -crate::syscall::kerr_to_errno(error),
    }
}

#[no_mangle]
pub extern "C" fn kernel_host_file_drop_end(token: u32) -> i32 {
    let kernel = kernel_mut();
    match kernel.host_file_drop_end(token) {
        Ok(()) => {
            kernel.service_poll_waiters();
            0
        }
        Err(error) => -crate::syscall::kerr_to_errno(error),
    }
}

#[no_mangle]
pub extern "C" fn kernel_host_file_drop_abort(token: u32) {
    kernel_mut().host_file_drop_abort(token);
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
    // T136: route through `flush_policy.flush_now` so the policy
    // state stays in sync (dirty-event counter resets, last_flush_ns
    // advances) — both the periodic-sync tick and the pagehide
    // last-gasp landing land here, and the policy is what tells
    // proc_exit "no flush needed, you just did one 200 ms ago".
    let now = crate::platform::current().now_realtime_ns();
    if kernel.flush_policy.flush_now(&mut kernel.vfs, now).is_ok() {
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
    let procfs_targets = collect_procfs_targets(kernel, pid, &req, heap);
    let projected_load_averages = prepare_live_procfs_view(kernel, &procfs_targets);
    match dispatch::dispatch(kernel, pid, &req, heap) {
        ServiceOutcome::Done(resp) => {
            if resp.status == 0 {
                if let Some(projected) = projected_load_averages {
                    kernel.sched.load_averages = projected;
                }
            }
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

/// Re-check bounded parked poll sets. The Worker calls this immediately before
/// sleeping and after any timer wake, closing the final check-to-park window.
#[no_mangle]
pub extern "C" fn kernel_service_poll_waiters() -> u32 {
    kernel_mut().service_poll_waiters() as u32
}

/// Nanoseconds until the nearest parked poll clock. `u64::MAX` means every
/// parked poll is fd-only (or there are no parked polls), so the Worker may
/// wait indefinitely for a real notification.
#[no_mangle]
pub extern "C" fn kernel_next_poll_timeout_ns() -> u64 {
    kernel_mut().next_poll_timeout_ns().unwrap_or(u64::MAX)
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
    kernel.service_poll_waiters();
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
    kernel.service_poll_waiters();
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
    kernel.service_poll_waiters();
    0
}
