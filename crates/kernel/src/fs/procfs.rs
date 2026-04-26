//! procfs — synthetic process-introspection filesystem.
//!
//! Top-level directory skeleton — `/proc/version`, `/proc/uptime`,
//! `/proc/meminfo`, `/proc/loadavg`, `/proc/storage` — plus a
//! per-pid subtree under `/proc/<pid>/` that surfaces live
//! process-table fields. Populated in this slice:
//! `/proc/<pid>/status` (Name / State / Pid / PPid / VmSize / VmPeak),
//! `/proc/<pid>/fd/<n>` symlinks describing each open file
//! descriptor, and `/proc/<pid>/cmdline` with the process's
//! NUL-separated argv. Follow-up slices add `stat`, `maps`,
//! `environ`, etc.
//!
//! Data sources are injected through the [`ProcFsSource`] trait.
//! The v1 slice provides a [`StaticProcFsSource`] that returns
//! canned values and an explicitly-populated per-pid status map —
//! it's what the kernel VFS tests use. A kernel boot path bridge
//! (a `KernelProcFsSource` reading the live `ProcessTable`) is
//! a follow-up; the trait shape here is what that bridge will
//! implement.
//!
//! # Inode layout
//!
//! * `1` — `/proc` root directory.
//! * `2..=6` — the fixed top-level files (version, uptime,
//!   meminfo, loadavg, storage).
//! * `PID_DIR_BASE + pid * PID_STRIDE + offset` — per-pid nodes.
//!   Stride (16) is chosen large enough to admit future fields
//!   (`stat`, `maps`, ...) without renumbering. A
//!   `pid_of(ino)` helper rounds back to the pid; an out-of-range
//!   offset yields `FsError::NotFound`. Slot offsets in use:
//!     * `0` — the pid directory itself.
//!     * `1` — `status`.
//!     * `2` — `fd/` directory.
//!     * `3` — `cmdline`.
//!     * `4..15` — reserved for `maps`, `environ`, ...
//! * `FD_INO_BASE + pid * FD_PID_STRIDE + fd_number` — per-fd
//!   symlink nodes beneath `/proc/<pid>/fd/<n>`. A dedicated
//!   region is required because stride-16 cannot hold arbitrary
//!   fd counts; `FD_PID_STRIDE = 1024` gives room for the kernel's
//!   fd-per-process cap without colliding. `FD_INO_BASE` is chosen
//!   well above `PID_DIR_BASE + PID_STRIDE * MAX_PID` so the two
//!   regions never overlap — see
//!   `fd_ino_region_does_not_collide_with_pid_subtree` in
//!   `tests/procfs.rs`.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use abi::ext::Pid;

use crate::platform;
use crate::proc::{ProcState, ProcessTable};
use crate::vfs::{DirEntry, FileStat, Filesystem, FsError, Ino, Mode, NodeType};

/// Per-file snapshot as a plain string that procfs serves on `read`.
///
/// Top-level files return their content as a `String`. Per-pid
/// fields return structured snapshots that procfs formats into
/// `Key: Value` bytes — keeping the formatter inside `ProcFs`
/// means every backend produces byte-identical output.
pub trait ProcFsSource: Send + Sync {
    fn version(&self) -> String;
    fn uptime(&self) -> String;
    fn meminfo(&self) -> String;
    fn loadavg(&self) -> String;

    /// Pre-formatted `/proc/storage` body.
    ///
    /// Default: if [`storage_info`](Self::storage_info) returns
    /// `Some`, format it as `"{quota_bytes} {used_bytes} {file_count}\n"`;
    /// otherwise return the canned placeholder `"0 0 0\n"`. Sources
    /// that carry a fully-formatted line (the canned test source)
    /// may override this directly.
    fn storage(&self) -> String {
        match self.storage_info() {
            Some(snap) => format_storage_snapshot(&snap),
            None => String::from("0 0 0\n"),
        }
    }

    /// Structured `/proc/storage` snapshot, or `None` if the
    /// source has no live storage counters to project.
    ///
    /// Default: `None`. The future kernel-bridge
    /// (`KernelProcFsSource`) will override this to project the
    /// block driver's quota + used + file-count counters.
    fn storage_info(&self) -> Option<StorageSnapshot> {
        None
    }

    /// Return the process-table snapshot for `pid`, or `None` if
    /// no such live pid exists.
    ///
    /// Default: no live pids. Tests and the future
    /// `KernelProcFsSource` override this.
    fn pid_status(&self, _pid: Pid) -> Option<ProcStatusSnapshot> {
        None
    }

    /// Return the open-fd snapshots for `pid`, in ascending fd
    /// order. Backs the `/proc/<pid>/fd/` directory: one symlink
    /// per entry, name = decimal fd number, target = path or
    /// pseudo-path the fd refers to (e.g. `/etc/preferences.toml`,
    /// `pipe:[42]`, `socket:[17]`, `/dev/console`).
    ///
    /// Default: empty. The future `KernelProcFsSource` will project
    /// the process's descriptor table; a pid that exists but has no
    /// open fds (rare for a live process, but possible during
    /// bring-up) yields an empty vec — the VFS turns that into an
    /// empty directory listing.
    fn pid_fds(&self, _pid: Pid) -> Vec<ProcFdSnapshot> {
        Vec::new()
    }

    /// Return the raw `/proc/<pid>/cmdline` bytes: argv elements
    /// joined with NUL separators and a trailing NUL, matching the
    /// Linux convention. `None` if the pid has no cmdline recorded
    /// (or if the source doesn't know). An `Some(Vec::new())` is
    /// distinct from `None` and represents a live pid with an empty
    /// argv — in that case the cmdline file exists but reads as
    /// zero bytes.
    ///
    /// Default: `None`. `StaticProcFsSource` populates this via
    /// `set_pid_cmdline`; `KernelProcFsSource` projects the
    /// [`Process::argv`](crate::proc::Process) list.
    fn pid_cmdline(&self, _pid: Pid) -> Option<Vec<u8>> {
        None
    }

    /// Ascending list of live pids used by readdir on `/proc`.
    ///
    /// Default: empty. Overriding this + `pid_status` together is
    /// what makes per-pid procfs entries visible.
    fn live_pids(&self) -> Vec<Pid> {
        Vec::new()
    }
}

/// Structured counters that back `/proc/storage`.
///
/// Mirrors the block driver's quota / used / file-count triad; the
/// future `KernelProcFsSource` will project the live driver
/// counters into this struct at snapshot time. Owned values (no
/// borrows) so that an emit-time read can release the driver
/// borrow before formatting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageSnapshot {
    pub quota_bytes: u64,
    pub used_bytes: u64,
    pub file_count: u64,
}

/// Format a [`StorageSnapshot`] into the exact bytes
/// `/proc/storage` serves: three decimal fields on one line,
/// trailing newline. Kept inside the module so every backend
/// produces byte-identical output — callers that want different
/// output must override [`ProcFsSource::storage`] directly.
fn format_storage_snapshot(snap: &StorageSnapshot) -> String {
    format!(
        "{} {} {}\n",
        snap.quota_bytes, snap.used_bytes, snap.file_count,
    )
}

/// Canned values — used in tests and as the default before the
/// kernel's real data source is installed.
pub struct StaticProcFsSource {
    pub version_line: String,
    pub uptime_line: String,
    pub meminfo_line: String,
    pub loadavg_line: String,
    pub storage_line: String,
    pub storage_info: Option<StorageSnapshot>,
    pub pid_statuses: BTreeMap<Pid, ProcStatusSnapshot>,
    pub pid_fds: BTreeMap<Pid, Vec<ProcFdSnapshot>>,
    pub pid_cmdlines: BTreeMap<Pid, Vec<u8>>,
}

impl Default for StaticProcFsSource {
    fn default() -> Self {
        StaticProcFsSource {
            version_line: String::from("PMos 0.1.0 (native-test)\n"),
            uptime_line: String::from("0 0\n"),
            meminfo_line: String::from("0 0 0\n"),
            loadavg_line: String::from("0.00 0.00 0.00 0/0 0\n"),
            storage_line: String::from("0 0 0\n"),
            storage_info: None,
            pid_statuses: BTreeMap::new(),
            pid_fds: BTreeMap::new(),
            pid_cmdlines: BTreeMap::new(),
        }
    }
}

impl StaticProcFsSource {
    /// Register or replace the snapshot for `pid`. Returns `self`
    /// by `&mut` so tests can chain several inserts on a builder
    /// expression.
    pub fn set_pid_status(&mut self, snap: ProcStatusSnapshot) -> &mut Self {
        self.pid_statuses.insert(snap.pid, snap);
        self
    }

    /// Register or replace the open-fd list for `pid`. Returns
    /// `self` by `&mut` for chaining.
    pub fn set_pid_fds(&mut self, pid: Pid, fds: Vec<ProcFdSnapshot>) -> &mut Self {
        self.pid_fds.insert(pid, fds);
        self
    }

    /// Register or replace the raw cmdline bytes for `pid`. Callers
    /// pass the fully NUL-joined / NUL-terminated byte sequence
    /// `/proc/<pid>/cmdline` will serve; `format_argv_cmdline` is
    /// the canonical builder for that form. Returns `self` by `&mut`
    /// for chaining.
    pub fn set_pid_cmdline(&mut self, pid: Pid, cmdline: Vec<u8>) -> &mut Self {
        self.pid_cmdlines.insert(pid, cmdline);
        self
    }
}

impl ProcFsSource for StaticProcFsSource {
    fn version(&self) -> String {
        self.version_line.clone()
    }
    fn uptime(&self) -> String {
        self.uptime_line.clone()
    }
    fn meminfo(&self) -> String {
        self.meminfo_line.clone()
    }
    fn loadavg(&self) -> String {
        self.loadavg_line.clone()
    }
    fn storage(&self) -> String {
        self.storage_line.clone()
    }
    fn storage_info(&self) -> Option<StorageSnapshot> {
        self.storage_info.clone()
    }
    fn pid_status(&self, pid: Pid) -> Option<ProcStatusSnapshot> {
        self.pid_statuses.get(&pid).cloned()
    }
    fn pid_fds(&self, pid: Pid) -> Vec<ProcFdSnapshot> {
        self.pid_fds.get(&pid).cloned().unwrap_or_default()
    }
    fn pid_cmdline(&self, pid: Pid) -> Option<Vec<u8>> {
        self.pid_cmdlines.get(&pid).cloned()
    }
    fn live_pids(&self) -> Vec<Pid> {
        self.pid_statuses.keys().copied().collect()
    }
}

/// Live-kernel bridge: projects the running [`ProcessTable`] into
/// the [`ProcFsSource`] trait so procfs reflects real state instead
/// of canned [`StaticProcFsSource`] values.
///
/// Scope of this slice (T169 partial):
///
/// * `pid_status`, `live_pids` — implemented against the borrowed
///   [`ProcessTable`]. `/proc/<pid>/status` now shows whatever the
///   kernel actually holds for that pid, including live state
///   transitions (Running → Zombie, etc.) and process memory
///   counters (VmSize / VmPeak).
/// * `pid_cmdline` — projects [`Process::argv`](crate::proc::Process)
///   through [`format_argv_cmdline`]. Returns `None` for a dead or
///   never-spawned pid; returns `Some(vec![])` for a live pid whose
///   argv is empty (the file exists but reads as zero bytes).
/// * `version` — hardcoded `"PMos 0.1.0-alpha (native-test)\n"`
///   until a build-time banner is plumbed through.
/// * `uptime`, `meminfo`, `loadavg` — placeholders (`"0 0\n"`,
///   `"0 0 0\n"`, `"0.00 0.00 0.00 0/0 0\n"`). Real values need a
///   clock source + system-wide memory accounting, both of which
///   are follow-ups.
/// * `storage_info` — returns `None`; the block-driver counter
///   wiring is a separate follow-up slice, and the default
///   `storage()` impl falls back to the `"0 0 0\n"` placeholder.
/// * `pid_fds` — inherits the default empty impl. The
///   per-process fd tables live inside the `Kernel` struct
///   (`Kernel.fds: BTreeMap<Pid, FdTable>`, private), not on the
///   `ProcessTable`, and a `Vnode`-to-path reverse lookup does not
///   yet exist in the VFS. Wiring this requires either widening
///   the bridge to hold a full `&Kernel` or adding a path-cache to
///   the VFS — both are separate slices.
///
/// Integration wiring (mounting this source at `/proc` in the
/// real boot path) is deliberately deferred: the module doc and
/// T169 itself call out that the bridge is landed first as a unit
/// then snapped into the boot path once the fd-table + storage
/// follow-ups land.
pub struct KernelProcFsSource<'a> {
    process_table: &'a ProcessTable,
}

impl<'a> KernelProcFsSource<'a> {
    /// Wrap a borrowed [`ProcessTable`]. The borrow must outlive
    /// any call to a trait method; this is always the case inside
    /// the kernel's single-threaded syscall dispatch because
    /// procfs reads happen on the same tick as the read-lock.
    pub fn new(process_table: &'a ProcessTable) -> Self {
        KernelProcFsSource { process_table }
    }
}

/// Map the kernel's [`ProcState`] enum to the coarser
/// [`ProcStatusState`] the `/proc/<pid>/status` file serves. The
/// mapping matches `project_state` in `tests/procfs.rs`.
#[inline]
pub fn proc_state_to_status(state: ProcState) -> ProcStatusState {
    match state {
        ProcState::Running => ProcStatusState::Running,
        ProcState::Starting
        | ProcState::Ready
        | ProcState::BlockedOnSyscall
        | ProcState::BlockedOnIpc
        | ProcState::BlockedOnWait => ProcStatusState::Sleeping,
        ProcState::Zombie | ProcState::Dead => ProcStatusState::Zombie,
    }
}

impl<'a> ProcFsSource for KernelProcFsSource<'a> {
    fn version(&self) -> String {
        String::from("PMos 0.1.0-alpha (native-test)\n")
    }

    fn uptime(&self) -> String {
        String::from("0 0\n")
    }

    fn meminfo(&self) -> String {
        String::from("0 0 0\n")
    }

    fn loadavg(&self) -> String {
        String::from("0.00 0.00 0.00 0/0 0\n")
    }

    fn pid_status(&self, pid: Pid) -> Option<ProcStatusSnapshot> {
        let proc = self.process_table.get(pid)?;
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
    }

    fn pid_cmdline(&self, pid: Pid) -> Option<Vec<u8>> {
        let proc = self.process_table.get(pid)?;
        if proc.state == ProcState::Dead {
            return None;
        }
        Some(format_argv_cmdline(&proc.argv))
    }

    fn live_pids(&self) -> Vec<Pid> {
        self.process_table.live_pids()
    }
}

/// Letter-coded process state that `/proc/<pid>/status` serves.
///
/// Mirrors the Linux `/proc/<pid>/status` convention: one letter
/// plus a parenthesised human-readable name. Maps from the
/// kernel's [`crate::proc::ProcState`] variants at snapshot time.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ProcStatusState {
    /// Currently Running (scheduler has the cpu on it).
    Running,
    /// Any Blocked-on-* variant or Ready/Starting — i.e. "alive
    /// and waiting for an event", which is all one bucket at
    /// `/proc` resolution.
    Sleeping,
    /// Exited but not yet reaped.
    Zombie,
    /// Stopped (e.g. by SIGSTOP). Reserved for future slices —
    /// v1 has no SIGSTOP delivery, so the kernel never hands us
    /// this at snapshot time. Present so the formatter is total.
    Stopped,
}

impl ProcStatusState {
    /// Linux-style one-letter code.
    #[inline]
    pub const fn letter(self) -> char {
        match self {
            ProcStatusState::Running => 'R',
            ProcStatusState::Sleeping => 'S',
            ProcStatusState::Zombie => 'Z',
            ProcStatusState::Stopped => 'T',
        }
    }

    /// Human-readable name that follows the letter in parentheses.
    #[inline]
    pub const fn name(self) -> &'static str {
        match self {
            ProcStatusState::Running => "running",
            ProcStatusState::Sleeping => "sleeping",
            ProcStatusState::Zombie => "zombie",
            ProcStatusState::Stopped => "stopped",
        }
    }
}

/// A snapshot of one process's `/proc/<pid>/status` fields.
///
/// Owned values (not borrows) so that an emit-time read can
/// release the source borrow before formatting; simplifies the
/// kernel-bridge backend where the source borrow is the
/// `ProcessTable` itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcStatusSnapshot {
    pub pid: Pid,
    pub ppid: Pid,
    pub name: String,
    pub state: ProcStatusState,
    pub vm_size_bytes: u64,
    pub vm_peak_bytes: u64,
}

/// A snapshot of one open file descriptor in a process, as
/// surfaced under `/proc/<pid>/fd/<n>`.
///
/// * `fd` — the descriptor number; appears verbatim as the entry
///   name (decimal, no leading zeros), matching Linux procfs.
/// * `target` — the path or pseudo-path the fd refers to. Regular
///   files and devices carry an absolute path; pipes and sockets
///   carry the Linux-style pseudo form `pipe:[N]` / `socket:[N]`
///   where `N` is the kernel-assigned pipe/socket ino.
///
/// Owned values so an emit-time read can release the source borrow
/// before formatting — same contract as [`ProcStatusSnapshot`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcFdSnapshot {
    pub fd: u32,
    pub target: String,
}

// Inode layout — ino 1 is the root directory; each top-level file
// has a fixed ino so tests can assert on them.
const INO_ROOT: Ino = 1;
const INO_VERSION: Ino = 2;
const INO_UPTIME: Ino = 3;
const INO_MEMINFO: Ino = 4;
const INO_LOADAVG: Ino = 5;
const INO_STORAGE: Ino = 6;

/// Start of the per-pid inode range. Chosen well above the fixed
/// top-level inos so a stray `2..=6` never aliases a per-pid ino.
pub const PID_DIR_BASE: Ino = 1_000_000;
/// Bytes reserved per pid. Slot 0 is the pid's directory itself;
/// slot 1 is `status`; slot 2 is `fd/`; further slots are reserved
/// for future per-pid files (`cmdline`, `maps`, `environ`, ...).
pub const PID_STRIDE: Ino = 16;
/// Offset within a pid's stride for the `status` file.
const PID_OFFSET_STATUS: Ino = 1;
/// Offset within a pid's stride for the `fd/` directory.
const PID_OFFSET_FD_DIR: Ino = 2;
/// Offset within a pid's stride for the `cmdline` file.
const PID_OFFSET_CMDLINE: Ino = 3;
/// Offset within a pid's stride for the `maps` file.
const PID_OFFSET_MAPS: Ino = 4;

/// Start of the per-fd symlink inode range — one stride per pid,
/// within that the raw fd number is the offset. Chosen well above
/// `PID_DIR_BASE + PID_STRIDE * MAX_PID` so the two regions never
/// overlap; asserted by
/// `fd_ino_region_does_not_collide_with_pid_subtree` in
/// `tests/procfs.rs`.
pub const FD_INO_BASE: Ino = 100_000_000;
/// Bytes reserved per pid inside the fd region. Must be larger
/// than the kernel's per-process fd cap — 1024 covers every
/// WASI-facing cap we plan to ship in v1 (RLIMIT_NOFILE default is
/// 256, so 1024 is 4× headroom).
pub const FD_PID_STRIDE: Ino = 1024;

/// Map a pid to its directory ino (slot 0 within its stride).
#[inline]
pub const fn pid_dir_ino(pid: Pid) -> Ino {
    PID_DIR_BASE + (pid as u64) * PID_STRIDE
}

/// Map a pid to its `status` file ino.
#[inline]
pub const fn pid_status_ino(pid: Pid) -> Ino {
    pid_dir_ino(pid) + PID_OFFSET_STATUS
}

/// Map a pid to its `fd/` directory ino.
#[inline]
pub const fn pid_fd_dir_ino(pid: Pid) -> Ino {
    pid_dir_ino(pid) + PID_OFFSET_FD_DIR
}

/// Map a pid to its `cmdline` file ino.
#[inline]
pub const fn pid_cmdline_ino(pid: Pid) -> Ino {
    pid_dir_ino(pid) + PID_OFFSET_CMDLINE
}

/// Map a pid to its `maps` file ino.
#[inline]
pub const fn pid_maps_ino(pid: Pid) -> Ino {
    pid_dir_ino(pid) + PID_OFFSET_MAPS
}

/// Map a `(pid, fd)` pair to the symlink ino that represents
/// `/proc/<pid>/fd/<fd>`. Lives in a separate region from the
/// per-pid stride; see module doc for the numbering rationale.
#[inline]
pub const fn fd_symlink_ino(pid: Pid, fd: u32) -> Ino {
    FD_INO_BASE + (pid as u64) * FD_PID_STRIDE + (fd as u64)
}

/// Map a pid and per-stride `offset` to the pid-subtree ino at
/// that slot. Used by tests to assert the fd region never overlaps
/// with any `PID_DIR_BASE + pid * PID_STRIDE + offset` for
/// `offset in 0..PID_STRIDE`.
#[inline]
pub const fn pid_subtree_ino(pid: Pid, offset: Ino) -> Ino {
    pid_dir_ino(pid) + offset
}

/// Inverse of `fd_symlink_ino`. Returns the `(pid, fd)` pair, or
/// `None` if `ino` is not in the fd region.
fn decode_fd_ino(ino: Ino) -> Option<(Pid, u32)> {
    if ino < FD_INO_BASE {
        return None;
    }
    let rel = ino - FD_INO_BASE;
    let pid = (rel / FD_PID_STRIDE) as Pid;
    let fd = (rel % FD_PID_STRIDE) as u32;
    Some((pid, fd))
}

/// Inverse of `pid_dir_ino` + `pid_status_ino`. Returns the pid
/// and the slot within its stride, or `None` if `ino` is not in
/// the per-pid range.
fn decode_pid_ino(ino: Ino) -> Option<(Pid, Ino)> {
    if ino < PID_DIR_BASE || ino >= FD_INO_BASE {
        return None;
    }
    let rel = ino - PID_DIR_BASE;
    let pid = (rel / PID_STRIDE) as Pid;
    let slot = rel % PID_STRIDE;
    Some((pid, slot))
}

/// Serialise a [`ProcStatusSnapshot`] into the exact byte layout
/// `/proc/<pid>/status` serves — one `Key:\tValue\n` per line, in
/// a fixed order, with a trailing newline. Callers receive a
/// `Vec<u8>` sized for the snapshot (no heap allocation beyond
/// the one `format!` that backs it).
///
/// `Threads` and `TracerPid` are hardcoded constants because v1
/// has no thread or ptrace concepts; the lines exist for parity
/// with Linux `/proc/<pid>/status` parsers (sysmon, htop, etc.)
/// that expect them.
fn format_pid_status(snap: &ProcStatusSnapshot) -> Vec<u8> {
    let text = format!(
        "Name:\t{}\nState:\t{} ({})\nTgid:\t{}\nPid:\t{}\nPPid:\t{}\nTracerPid:\t0\nVmSize:\t{} kB\nVmPeak:\t{} kB\nThreads:\t1\n",
        snap.name,
        snap.state.letter(),
        snap.state.name(),
        snap.pid,
        snap.pid,
        snap.ppid,
        bytes_to_status_kib(snap.vm_size_bytes),
        bytes_to_status_kib(snap.vm_peak_bytes),
    );
    text.into_bytes()
}

#[inline]
fn bytes_to_status_kib(bytes: u64) -> u64 {
    if bytes == 0 {
        0
    } else {
        ((bytes - 1) / 1024) + 1
    }
}

/// Serialise a `/proc/<pid>/maps` line for the process's wasm
/// linear memory.
///
/// Linux `/proc/<pid>/maps` lists every memory region in the
/// address space — code, data, heap, stack, shared libraries —
/// each on its own line:
///
///   `<start_hex>-<end_hex> <perms> <offset_hex> <dev>:<inode> <inum> <pathname>`
///
/// PMos's wasm32 process model has a SINGLE region per process:
/// the `WebAssembly.Memory` object the user Worker owns. The
/// kernel cannot introspect into wasm-internal subdivisions
/// (text vs data vs stack vs heap) — those are an artifact of
/// the linker that's lost by the time the binary executes. So
/// we surface one line per process, spanning the full linear
/// memory at addresses `0x00000000..0x<vm_size_bytes>`, with
/// permissions `rw-p` (the wasm runtime always grants the user
/// process read+write access to its own memory; no `x` bit
/// because wasm execution rights live in the module table, not
/// in linear memory) and the pathname `[wasm-memory]` —
/// matching Linux's `[stack]` / `[heap]` convention for
/// kernel-managed regions with no on-disk file.
///
/// `dev:inode` is `00:00 0` because there's no backing file.
/// Off is `00000000` because the region maps from offset zero
/// of the (virtual) backing.
///
/// Empty (zero-length) regions emit a single line with end
/// equal to start — sysmon can still walk the file deterministically
/// without special-casing pre-spawn processes.
pub fn format_pid_maps(snap: &ProcStatusSnapshot) -> Vec<u8> {
    use alloc::format;
    let end = snap.vm_size_bytes;
    let line = format!(
        "{:08x}-{:08x} rw-p 00000000 00:00 0                          [wasm-memory]\n",
        0u64, end,
    );
    line.into_bytes()
}

/// Serialise an `argv` slice into the exact byte layout
/// `/proc/<pid>/cmdline` serves — each element followed by a NUL
/// byte, matching the Linux convention (`argv[0]\0argv[1]\0...\0`).
/// An empty `argv` yields an empty byte vector (Linux behaviour for
/// kernel threads / pre-exec processes). No terminating newline;
/// downstream readers rely on NUL boundaries.
pub fn format_argv_cmdline(argv: &[String]) -> Vec<u8> {
    let total: usize = argv.iter().map(|s| s.len() + 1).sum();
    let mut out = Vec::with_capacity(total);
    for arg in argv {
        out.extend_from_slice(arg.as_bytes());
        out.push(0);
    }
    out
}

pub struct ProcFs {
    source: Box<dyn ProcFsSource>,
    /// File inode → canned-name lookup for top-level files. Used
    /// by readdir and stat; the per-pid subtree is resolved
    /// dynamically against `source.live_pids()` + `source.pid_status(pid)`.
    entries: BTreeMap<Ino, &'static str>,
    /// Name → inode lookup for top-level `lookup(dir, name)`.
    name_to_ino: BTreeMap<&'static str, Ino>,
}

impl ProcFs {
    pub fn new(source: Box<dyn ProcFsSource>) -> Self {
        let pairs: &[(Ino, &'static str)] = &[
            (INO_VERSION, "version"),
            (INO_UPTIME, "uptime"),
            (INO_MEMINFO, "meminfo"),
            (INO_LOADAVG, "loadavg"),
            (INO_STORAGE, "storage"),
        ];
        let mut entries = BTreeMap::new();
        let mut name_to_ino = BTreeMap::new();
        for (ino, name) in pairs {
            entries.insert(*ino, *name);
            name_to_ino.insert(*name, *ino);
        }
        ProcFs {
            source,
            entries,
            name_to_ino,
        }
    }

    /// Convenience constructor with canned test values.
    pub fn with_static() -> Self {
        ProcFs::new(Box::new(StaticProcFsSource::default()))
    }

    fn contents_for(&self, ino: Ino) -> Option<Vec<u8>> {
        // Top-level canned files.
        let static_text: Option<String> = match ino {
            INO_VERSION => Some(self.source.version()),
            INO_UPTIME => Some(self.source.uptime()),
            INO_MEMINFO => Some(self.source.meminfo()),
            INO_LOADAVG => Some(self.source.loadavg()),
            INO_STORAGE => Some(self.source.storage()),
            _ => None,
        };
        if let Some(s) = static_text {
            return Some(s.into_bytes());
        }
        // Per-fd symlink targets — checked before the per-pid
        // range so the fd-region dispatch short-circuits cleanly.
        if let Some((pid, fd)) = decode_fd_ino(ino) {
            let snap = self.source.pid_fds(pid).into_iter().find(|s| s.fd == fd)?;
            return Some(snap.target.into_bytes());
        }
        // Per-pid files.
        let (pid, slot) = decode_pid_ino(ino)?;
        if slot == PID_OFFSET_STATUS {
            let snap = self.source.pid_status(pid)?;
            return Some(format_pid_status(&snap));
        }
        if slot == PID_OFFSET_CMDLINE {
            return self.source.pid_cmdline(pid);
        }
        if slot == PID_OFFSET_MAPS {
            let snap = self.source.pid_status(pid)?;
            return Some(format_pid_maps(&snap));
        }
        None
    }

    /// True iff `ino` is a per-pid directory for a currently-live pid.
    fn is_live_pid_dir(&self, ino: Ino) -> bool {
        let Some((pid, slot)) = decode_pid_ino(ino) else {
            return false;
        };
        slot == 0 && self.source.pid_status(pid).is_some()
    }

    /// True iff `ino` is the `/proc/<pid>/fd` directory for a
    /// currently-live pid.
    fn is_live_fd_dir(&self, ino: Ino) -> bool {
        let Some((pid, slot)) = decode_pid_ino(ino) else {
            return false;
        };
        slot == PID_OFFSET_FD_DIR && self.source.pid_status(pid).is_some()
    }
}

impl Filesystem for ProcFs {
    fn root(&self) -> Ino {
        INO_ROOT
    }

    fn lookup(&mut self, dir: Ino, name: &str) -> Result<Ino, FsError> {
        if dir == INO_ROOT {
            // Top-level: either a canned name or a numeric pid.
            if let Some(ino) = self.name_to_ino.get(name).copied() {
                return Ok(ino);
            }
            if let Ok(pid) = name.parse::<Pid>() {
                if self.source.pid_status(pid).is_some() {
                    return Ok(pid_dir_ino(pid));
                }
            }
            return Err(FsError::NotFound);
        }
        // Per-pid directory: `status` file + `fd/` directory +
        // optional `cmdline` file (present only when the source
        // records one for this pid).
        if self.is_live_pid_dir(dir) {
            let (pid, _) = decode_pid_ino(dir).expect("is_live_pid_dir implies decode ok");
            return match name {
                "status" => Ok(pid_status_ino(pid)),
                "fd" => Ok(pid_fd_dir_ino(pid)),
                "cmdline" => {
                    if self.source.pid_cmdline(pid).is_some() {
                        Ok(pid_cmdline_ino(pid))
                    } else {
                        Err(FsError::NotFound)
                    }
                }
                "maps" => Ok(pid_maps_ino(pid)),
                _ => Err(FsError::NotFound),
            };
        }
        // `/proc/<pid>/fd/<n>`: numeric fd names only; must match a
        // live snapshot entry.
        if self.is_live_fd_dir(dir) {
            let (pid, _) = decode_pid_ino(dir).expect("is_live_fd_dir implies decode ok");
            let fd = name.parse::<u32>().map_err(|_| FsError::NotFound)?;
            if self.source.pid_fds(pid).iter().any(|s| s.fd == fd) {
                return Ok(fd_symlink_ino(pid, fd));
            }
            return Err(FsError::NotFound);
        }
        Err(FsError::NotADirectory)
    }

    fn read(&mut self, ino: Ino, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        if ino == INO_ROOT {
            return Err(FsError::IsADirectory);
        }
        if self.is_live_pid_dir(ino) || self.is_live_fd_dir(ino) {
            return Err(FsError::IsADirectory);
        }
        let bytes = self.contents_for(ino).ok_or(FsError::NotFound)?;
        let start = offset as usize;
        if start >= bytes.len() {
            return Ok(0);
        }
        let end = core::cmp::min(bytes.len(), start + buf.len());
        let n = end - start;
        buf[..n].copy_from_slice(&bytes[start..end]);
        Ok(n)
    }

    fn write(&mut self, _ino: Ino, _offset: u64, _buf: &[u8]) -> Result<usize, FsError> {
        Err(FsError::ReadOnly)
    }

    fn readdir(&mut self, dir: Ino, out: &mut Vec<DirEntry>) -> Result<(), FsError> {
        if dir == INO_ROOT {
            for (name, ino) in &self.name_to_ino {
                out.push(DirEntry {
                    name: (*name).to_string(),
                    ino: *ino,
                    ty: NodeType::RegularFile,
                });
            }
            for pid in self.source.live_pids() {
                out.push(DirEntry {
                    name: pid.to_string(),
                    ino: pid_dir_ino(pid),
                    ty: NodeType::Directory,
                });
            }
            return Ok(());
        }
        if self.is_live_pid_dir(dir) {
            let (pid, _) = decode_pid_ino(dir).expect("is_live_pid_dir implies decode ok");
            out.push(DirEntry {
                name: "status".to_string(),
                ino: pid_status_ino(pid),
                ty: NodeType::RegularFile,
            });
            out.push(DirEntry {
                name: "fd".to_string(),
                ino: pid_fd_dir_ino(pid),
                ty: NodeType::Directory,
            });
            if self.source.pid_cmdline(pid).is_some() {
                out.push(DirEntry {
                    name: "cmdline".to_string(),
                    ino: pid_cmdline_ino(pid),
                    ty: NodeType::RegularFile,
                });
            }
            out.push(DirEntry {
                name: "maps".to_string(),
                ino: pid_maps_ino(pid),
                ty: NodeType::RegularFile,
            });
            return Ok(());
        }
        if self.is_live_fd_dir(dir) {
            let (pid, _) = decode_pid_ino(dir).expect("is_live_fd_dir implies decode ok");
            for snap in self.source.pid_fds(pid) {
                out.push(DirEntry {
                    name: snap.fd.to_string(),
                    ino: fd_symlink_ino(pid, snap.fd),
                    ty: NodeType::SymLink,
                });
            }
            return Ok(());
        }
        Err(FsError::NotADirectory)
    }

    fn create(&mut self, _dir: Ino, _name: &str, _mode: Mode) -> Result<Ino, FsError> {
        Err(FsError::ReadOnly)
    }

    fn mkdir(&mut self, _dir: Ino, _name: &str, _mode: Mode) -> Result<Ino, FsError> {
        Err(FsError::ReadOnly)
    }

    fn unlink(&mut self, _dir: Ino, _name: &str) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }

    fn rmdir(&mut self, _dir: Ino, _name: &str) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }

    fn rename(
        &mut self,
        _from_dir: Ino,
        _from_name: &str,
        _to_dir: Ino,
        _to_name: &str,
    ) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }

    fn stat(&mut self, ino: Ino) -> Result<FileStat, FsError> {
        // Procfs content is synthesised per-call — the "file" is
        // generated fresh from the ProcFsSource each read — so the
        // semantic truth for every timestamp is "now". Per-call
        // evaluation means successive stats on the same ino may
        // report different values; that's consistent with how the
        // content itself can change across calls (uptime, loadavg,
        // meminfo all update every tick).
        let now = platform::current().now_realtime_ns();
        if ino == INO_ROOT {
            return Ok(FileStat {
                ino: INO_ROOT,
                ty: NodeType::Directory,
                mode: 0o555,
                nlink: 1,
                size: 0,
                atime_ns: now,
                mtime_ns: now,
                ctime_ns: now,
            });
        }
        if self.is_live_pid_dir(ino) || self.is_live_fd_dir(ino) {
            return Ok(FileStat {
                ino,
                ty: NodeType::Directory,
                mode: 0o555,
                nlink: 1,
                size: 0,
                atime_ns: now,
                mtime_ns: now,
                ctime_ns: now,
            });
        }
        if self.entries.contains_key(&ino) {
            let content = self.contents_for(ino).ok_or(FsError::NotFound)?;
            return Ok(FileStat {
                ino,
                ty: NodeType::RegularFile,
                mode: 0o444,
                nlink: 1,
                size: content.len() as u64,
                atime_ns: now,
                mtime_ns: now,
                ctime_ns: now,
            });
        }
        // Per-fd symlinks — checked before the per-pid range since
        // the two regions do not overlap. Mode 0o777 matches Linux
        // procfs convention (symlink permission bits are advisory;
        // the target's own mode governs any follow).
        if let Some((pid, fd)) = decode_fd_ino(ino) {
            let snap = self
                .source
                .pid_fds(pid)
                .into_iter()
                .find(|s| s.fd == fd)
                .ok_or(FsError::NotFound)?;
            return Ok(FileStat {
                ino,
                ty: NodeType::SymLink,
                mode: 0o777,
                nlink: 1,
                size: snap.target.len() as u64,
                atime_ns: now,
                mtime_ns: now,
                ctime_ns: now,
            });
        }
        // Per-pid regular files: recognise the stride offset.
        if let Some((_pid, slot)) = decode_pid_ino(ino) {
            if slot == PID_OFFSET_STATUS
                || slot == PID_OFFSET_CMDLINE
                || slot == PID_OFFSET_MAPS
            {
                let content = self.contents_for(ino).ok_or(FsError::NotFound)?;
                return Ok(FileStat {
                    ino,
                    ty: NodeType::RegularFile,
                    mode: 0o444,
                    nlink: 1,
                    size: content.len() as u64,
                    atime_ns: now,
                    mtime_ns: now,
                    ctime_ns: now,
                });
            }
        }
        Err(FsError::NotFound)
    }

    fn readlink(&mut self, ino: Ino, out: &mut [u8]) -> Result<usize, FsError> {
        // Only fd-region inos are symlinks; anything else is either
        // a directory or a regular file, both of which are EINVAL
        // per POSIX.
        let (pid, fd) = decode_fd_ino(ino).ok_or(FsError::InvalidArgument)?;
        let snap = self
            .source
            .pid_fds(pid)
            .into_iter()
            .find(|s| s.fd == fd)
            .ok_or(FsError::NotFound)?;
        let target = snap.target.as_bytes();
        let n = core::cmp::min(out.len(), target.len());
        out[..n].copy_from_slice(&target[..n]);
        Ok(n)
    }

    fn truncate(&mut self, _ino: Ino, _new_size: u64) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }

    fn kind_name(&self) -> &'static str {
        "procfs"
    }
}
