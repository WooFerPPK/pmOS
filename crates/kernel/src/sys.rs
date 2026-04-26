//! Top-level kernel glue.
//!
//! The [`Kernel`] struct is the composition root of the kernel
//! crate. It owns every top-level subsystem:
//!
//! * [`ProcessTable`](crate::proc::ProcessTable) + [`Scheduler`](crate::proc::Scheduler)
//!   — the process lifecycle and runnable-queue state.
//! * [`CapTable`](crate::cap::CapTable) — capabilities per pid.
//! * [`Vfs`](crate::vfs::Vfs) — mount table + filesystems.
//! * [`IpcTable`](crate::ipc::IpcTable) — pipes, sockets, bindings.
//! * [`DeviceDispatcher`](crate::dev::DeviceDispatcher) — in-kernel
//!   character-device I/O.
//! * Per-process fd tables (one [`FdTable`] per live pid).
//!
//! The eventual numeric-opcode SAB ring syscall dispatcher
//! (T071..T078) wraps this struct and pattern-matches on an
//! opcode enum. The public methods below are the Rust-level
//! versions that syscall dispatch *and* native-test harnesses
//! both drive directly — the SAB transport just translates
//! request payloads into calls to these methods.
//!
//! Splitting the composition out of `lib.rs` keeps the module
//! graph flat and gives the headless-shell gate (T077) a single
//! place to construct a working kernel without pulling in any
//! Worker / SAB / browser glue.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use abi::cap::{Cap, CapSet};
use abi::ext::Pid;

use crate::cap::{CapError, CapTable};
use crate::dev::{DevError, DeviceDispatcher};
use crate::fd::{FdEntry, FdError, FdFlags, FdObject, FdTable};
use crate::host_file::{HostFile, HostFileFd};
use crate::ipc::{IpcError, IpcTable, PipeId, SocketId, SocketType};
use crate::proc::{
    table::{InsertError, ZombieTarget},
    ExitStatus, ProcState, Process, ProcessTable, Scheduler, SignalInbox,
};
pub use crate::proc::Signal;
use crate::vfs::{DirEntry, FsError, Ino, MountId, NodeType, Vfs, WatchEvent, WatchId};

/// Error returned by the high-level kernel API.
///
/// Each variant maps to a specific errno in the eventual
/// numeric syscall layer. Keeping these Rust-level names means
/// the kernel's internal API is self-documenting in a way that
/// a raw `i32` would not be.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum KernelError {
    /// No such pid in the process table.
    NoSuchPid,
    /// Fd number does not refer to an open fd.
    BadFd,
    /// The calling process has hit its fd soft limit.
    OutOfFds,
    /// The fd's object type doesn't support this operation
    /// (e.g. writing to a `PipeRead` fd).
    NotSupportedOnFd,
    /// Not a valid path / argument.
    InvalidArgument,
    /// Capability check failure.
    NotCapable,
    /// Operation would have blocked. The caller turns this
    /// into `EAGAIN` under `O_NONBLOCK`, or parks the
    /// process on the underlying endpoint otherwise.
    WouldBlock,
    /// Connection refused — e.g. `display_connect` with no
    /// display server listening at `/run/display`.
    ConnectionRefused,
    /// Path already bound — e.g. a second `display_bind`
    /// call against the same socket path.
    AddressInUse,
    /// Write attempted on a pipe / stream whose write path is broken
    /// — peer fully closed, local `shutdown(WR)` applied, or peer
    /// `shutdown(RD)` applied. Surfaces to userland as EPIPE (POSIX's
    /// "broken pipe" signal). Distinct from [`KernelError::NotSupportedOnFd`]
    /// (generic "this fd doesn't support this op") so the errno
    /// surface matches `send(2)` / `write(2)`'s documented contract.
    PipeBroken,
    /// Forwarded filesystem error (wraps `FsError`).
    Fs(FsError),
    /// Forwarded device-dispatch error (wraps `DevError`).
    Dev(DevError),
}

impl From<FsError> for KernelError {
    fn from(e: FsError) -> Self {
        KernelError::Fs(e)
    }
}
impl From<DevError> for KernelError {
    fn from(e: DevError) -> Self {
        match e {
            DevError::NotCapable => KernelError::NotCapable,
            other => KernelError::Dev(other),
        }
    }
}
impl From<FdError> for KernelError {
    fn from(e: FdError) -> Self {
        match e {
            FdError::BadFd => KernelError::BadFd,
            FdError::OutOfFds => KernelError::OutOfFds,
        }
    }
}
impl From<CapError> for KernelError {
    fn from(e: CapError) -> Self {
        match e {
            CapError::NoSuchPid => KernelError::NoSuchPid,
            CapError::NotPermitted | CapError::NotASubset => KernelError::NotCapable,
        }
    }
}
impl From<InsertError> for KernelError {
    fn from(_: InsertError) -> Self {
        KernelError::InvalidArgument
    }
}
impl From<IpcError> for KernelError {
    fn from(e: IpcError) -> Self {
        match e {
            IpcError::NoSuchPipe | IpcError::NoSuchSocket => KernelError::BadFd,
            IpcError::AddressInUse => KernelError::AddressInUse,
            IpcError::ConnectionRefused => KernelError::ConnectionRefused,
            IpcError::InvalidState => KernelError::InvalidArgument,
            IpcError::WouldBlock => KernelError::WouldBlock,
            IpcError::PipeBroken => KernelError::PipeBroken,
            IpcError::MsgTooLarge => KernelError::InvalidArgument,
        }
    }
}

/// Path at which the PMos display server listens, per
/// `contracts/display-protocol.md §0`. Exported so userland
/// code and the kernel-level test harness agree on the
/// single string.
pub const DISPLAY_SOCKET_PATH: &str = "/run/display";

/// Default backlog for the display server's listening
/// socket. Matches the v1 target of "a handful of
/// concurrent clients during a desktop session".
pub const DISPLAY_LISTEN_BACKLOG: usize = 16;

/// Configuration for [`Kernel::register_process`].
///
/// Gives tests and the (future) `proc_spawn` path one struct to
/// fill in instead of an eight-argument function.
pub struct RegisterArgs<'a> {
    pub name: &'a str,
    pub ppid: Pid,
    pub caps: CapSet,
    pub cwd: &'a str,
}

/// Configuration for [`Kernel::proc_spawn`]. Populated by the
/// caller (userland `posix_spawn`-equivalent, or by `init` when
/// launching early processes from the root image).
pub struct SpawnArgs<'a> {
    pub name: &'a str,
    pub caps: CapSet,
    pub cwd: &'a str,
    pub argv: Vec<String>,
    pub envp: BTreeMap<String, String>,
    /// Fd objects that will be installed at fd 0, 1, and 2 in
    /// the child's fd table. The kernel automatically bumps
    /// refcounts on pipe-end objects so the child and parent
    /// share the underlying pipe correctly.
    pub stdin: FdObject,
    pub stdout: FdObject,
    pub stderr: FdObject,
}

/// Selector for [`Kernel::proc_wait`]. Mirrors POSIX
/// `waitpid`'s `pid` argument: `-1` → `Any`, positive → `Specific`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WaitTarget {
    Any,
    Specific(Pid),
}

/// Result of [`Kernel::proc_wait`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    /// A zombie child was reaped. Payload: `(child_pid, status)`.
    Reaped(Pid, ExitStatus),
    /// The parent has children matching the target but none
    /// have exited yet. Caller blocks or returns `EAGAIN`.
    WouldBlock,
    /// The parent has no children matching the target.
    NoChildren,
}

// `Signal` is re-exported from `crate::proc::signal` above.
// Its per-process delivery inbox lives in the Kernel struct
// (see `signal_inboxes` field below).

/// Kernel glue layer: the composition root of the kernel crate.
///
/// Sub-system fields are pub so tests and the syscall layer can
/// reach in directly when the high-level helpers are too coarse
/// (e.g. a test that mounts a filesystem on boot, or one that
/// checks scheduler state after a particular wake).
pub struct Kernel {
    pub procs: ProcessTable,
    pub sched: Scheduler,
    pub caps: CapTable,
    pub vfs: Vfs,
    pub ipc: IpcTable,
    pub devs: DeviceDispatcher,
    /// Per-process fd tables. Kept in sync with `procs`: every
    /// live pid has an entry. Reaping a process removes it here
    /// too so the drop-order of object-side resources is
    /// deterministic.
    fds: BTreeMap<Pid, FdTable>,
    /// Per-process signal inboxes. Only holds catchable
    /// signals (Term / Interrupt); SIGKILL is delivered
    /// synchronously without being queued.
    signal_inboxes: BTreeMap<Pid, SignalInbox>,
    /// Responses queued for pids that were parked on a blocking
    /// syscall and have since been unblocked. Drained per-pid by
    /// the dispatcher via `kernel_take_next_wake_for_pid`, which
    /// writes each entry's 32-byte Response into `RESP_SCRATCH`
    /// AND any heap payload into `HEAP_SCRATCH[0..extra_len]`,
    /// surfacing the user's original `heap_ptr` via the companion
    /// `kernel_resp_heap_ptr` export so the TS drainer can write
    /// the heap bytes back into the user's SAB heap scratch.
    ///
    /// `WakeHeap = None` for wakes that don't need a heap readback
    /// (every slice-2a/2b producer). Slice 2c.1's parked-wait wake
    /// sets it to `Some(PendingHeap { heap_ptr, bytes })` with a
    /// 4-byte reaped-child-pid payload when the parker recorded
    /// `heap_len >= 4`.
    pub(crate) pending_wakes: alloc::vec::Vec<(Pid, abi::ring::Response, WakeHeap)>,
    /// Parents parked on a blocking `proc_wait`. Keyed by parent
    /// pid so the child-exit wake path does an O(log n) lookup on
    /// `ppid`.
    ///
    /// v1 invariant: at most one parker per parent. A second
    /// blocking `proc_wait` from a parent that's already parked
    /// returns -EAGAIN regardless of WNOHANG (see design §3.1).
    /// POSIX allows reentrant waits from multiple threads sharing
    /// a pid; PMos v1 rejects them. Future slice can lift this if
    /// a multi-threaded-pid arc lands.
    pub(crate) parked_waiters: alloc::collections::BTreeMap<Pid, WaitParker>,
    /// Pids parked on a blocking `ipc_recv`. Keyed by parker pid so
    /// the proc-exit / SIGKILL cleanup paths do an O(log n) drop;
    /// the wake path (`wake_parked_recver_if_any`) walks the map
    /// looking for the entry whose `socket_id` matches the peer
    /// the bytes just landed on.
    ///
    /// v1 invariant: at most one parker per pid. A second blocking
    /// ipc_recv from an already-parked pid returns -EAGAIN. Mirror
    /// of the `parked_acceptor`/`parked_waiters` one-parker rules.
    pub(crate) parked_recvers: alloc::collections::BTreeMap<Pid, RecvParker>,
    /// Pending host-imported-file payloads keyed by the bootstrap-
    /// minted token (`contracts/syscalls.md §3.6`). Populated by the
    /// future TS-side bootstrap drag-drop notification path via
    /// [`Kernel::host_file_dropped`]; consumed by the userland-facing
    /// [`Kernel::host_file_recv`] which moves the entry to
    /// [`Kernel::host_file_fds`] keyed by the same token. Per spec:
    /// "Exactly one `host_file_recv` call is permitted per token; a
    /// second call with the same token returns `EBADF`." The
    /// pending-table-to-fd-table migration enforces that — once a
    /// token is consumed, the pending entry is gone.
    ///
    /// Kernel-wide rather than per-pid: the bootstrap doesn't know
    /// which pid will eventually call `host_file_recv` (the file
    /// manager subscribes to `/run/host-files` and any pid with a
    /// subscription may pick up a token), and the §3.6 wire surface
    /// passes the token by value rather than by destination-pid.
    pub(crate) host_files: BTreeMap<u32, HostFile>,
    /// Per-fd streaming state for HostFile fds, keyed by the
    /// consumed token. Populated by [`Kernel::host_file_recv`];
    /// torn down by the `release_object` arm on close. The same
    /// token is the discriminator carried inside
    /// [`FdObject::HostFile`], so a single map lookup translates
    /// from the userland-facing fd-number through `FdTable::get`'s
    /// `FdObject` to the streaming state — no per-fd allocation
    /// beyond the table entry.
    pub(crate) host_file_fds: BTreeMap<u32, HostFileFd>,
}

/// Optional heap payload attached to a pending wake. Slice 2c.1.
pub(crate) type WakeHeap = Option<PendingHeap>;

/// Heap bytes the kernel wants the TS drainer to copy into the
/// parker's SAB heap scratch at `heap_ptr`. `bytes` is at most
/// `HEAP_SCRATCH_SIZE` in length; in practice for 2c.1 it's
/// always 4 bytes (the reaped child pid as u32 LE), but the type
/// is general to admit larger heap readbacks in future slices
/// without another shape change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingHeap {
    pub heap_ptr: u32,
    pub bytes: alloc::vec::Vec<u8>,
}

/// Parked-waiter record. One entry per parked parent in
/// `Kernel.parked_waiters`. Constructed by
/// [`Kernel::park_on_wait`]; consumed by child-exit wake paths +
/// [`Kernel::interrupt_parked_wait`] + the SIGKILL arm's surgical
/// `parked_waiters.remove` call.
///
/// `target` determines which child-exit events this parker
/// responds to: `Any` matches every child of the parked pid;
/// `Specific(p)` matches only child pid `p`.
///
/// `heap_ptr` + `heap_len` are captured at park time so the wake
/// path knows where in the user's SAB heap scratch to write the
/// 4-byte reaped-child-pid readback. `heap_len >= 4` means the
/// wake emits `Response.extra_len = 4`; otherwise the wake is
/// status-only.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WaitParker {
    pub req_id: u32,
    pub target: WaitTarget,
    pub heap_ptr: u32,
    pub heap_len: u32,
}

/// Parked-recver record. One entry per parked pid in
/// `Kernel.parked_recvers`. Constructed by [`Kernel::park_on_recv`];
/// consumed by the peer-send wake path
/// ([`Kernel::wake_parked_recver_if_any`]) +
/// [`Kernel::interrupt_parked_recv`] + the SIGKILL/proc_exit
/// surgical `parked_recvers.remove` calls + the socket-close drain
/// in [`Kernel::wake_parked_recvers_on_socket_close`].
///
/// `socket_id` is the receive-side socket the parker is draining.
/// The peer's send hook compares `peer_id == parker.socket_id` to
/// route the wake. `max_len` / `recv_fd_slot` / `heap_ptr` /
/// `heap_len` mirror the in-flight `IPC_RECV` parameters so the
/// wake builds the same fd-leading heap layout f00e559's non-
/// blocking path produces.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RecvParker {
    pub req_id: u32,
    pub socket_id: SocketId,
    pub max_len: u32,
    pub recv_fd_slot: i32,
    pub heap_ptr: u32,
    pub heap_len: u32,
}

/// Test helper: expose the `ext.rs`-private `pack_exit_status`
/// helper so tests can build expected-value assertions
/// identical to what the synchronous-reap path produces.
#[doc(hidden)]
pub fn pack_exit_status_public(status: crate::proc::ExitStatus) -> i64 {
    crate::syscall::ext::pack_exit_status(status)
}

impl Kernel {
    pub fn new() -> Self {
        Kernel {
            procs: ProcessTable::new(),
            sched: Scheduler::new(),
            caps: CapTable::new(),
            vfs: Vfs::new(),
            ipc: IpcTable::new(),
            devs: DeviceDispatcher::new(),
            fds: BTreeMap::new(),
            signal_inboxes: BTreeMap::new(),
            pending_wakes: alloc::vec::Vec::new(),
            parked_waiters: alloc::collections::BTreeMap::new(),
            parked_recvers: alloc::collections::BTreeMap::new(),
            host_files: BTreeMap::new(),
            host_file_fds: BTreeMap::new(),
        }
    }

    // --- Process registration ------------------------------------

    /// Register a process. Creates the process-table entry in
    /// `Starting` state, installs its cap set, and gives it an
    /// empty fd table. The caller is responsible for transitioning
    /// it to `Ready` (once any "boot" work is complete) and
    /// populating stdin/stdout/stderr.
    ///
    /// Returns the assigned pid. Allocating it through
    /// `ProcessTable::allocate_pid` keeps pids monotonic regardless
    /// of which caller goes first.
    pub fn register_process(&mut self, args: RegisterArgs<'_>) -> Result<Pid, KernelError> {
        let pid = self.procs.allocate_pid();
        let proc = Process::new_starting(
            pid,
            args.ppid,
            args.name,
            Vec::new(),
            BTreeMap::new(),
            args.cwd,
            args.caps,
            0,
            0,
            0,
        );
        self.procs.insert(proc)?;
        self.caps.install(pid, args.caps);
        self.fds.insert(pid, FdTable::new());
        self.signal_inboxes.insert(pid, SignalInbox::new());
        Ok(pid)
    }

    /// Borrow a pid's fd table.
    pub fn fds(&self, pid: Pid) -> Result<&FdTable, KernelError> {
        self.fds.get(&pid).ok_or(KernelError::NoSuchPid)
    }

    /// Mutably borrow a pid's fd table.
    pub fn fds_mut(&mut self, pid: Pid) -> Result<&mut FdTable, KernelError> {
        self.fds.get_mut(&pid).ok_or(KernelError::NoSuchPid)
    }

    /// Reap a process's kernel-side state once it has exited.
    /// Removes the per-pid fd table, cap-table entry, and
    /// signal inbox, then promotes the zombie to dead on the
    /// process table. Object-side resources are released
    /// eagerly at `proc_exit`, so the fd-table drain here is
    /// normally a no-op; it still runs as a safety net for
    /// callers that bypass the standard `proc_exit → reap`
    /// flow (e.g. a test that reaps a process registered for
    /// bookkeeping only and never transitioned to Running).
    ///
    /// Returns the exit status recorded on the zombie, or
    /// `NoSuchPid` if the pid is unknown.
    pub fn reap(&mut self, pid: Pid) -> Result<ExitStatus, KernelError> {
        if !self.fds.contains_key(&pid) {
            return Err(KernelError::NoSuchPid);
        }
        self.release_fd_table_resources(pid);
        self.fds.remove(&pid);
        self.caps.remove(pid);
        self.signal_inboxes.remove(&pid);
        self.procs.reap(pid).ok_or(KernelError::NoSuchPid)
    }

    // --- path_open / fd_read / fd_write / fd_close ----------------

    /// `path_open(pid, path, lookup_flags, oflags, mode, flags) -> fd`.
    ///
    /// Resolves `path` through the VFS honouring WASI `oflags`:
    ///
    /// * `CREAT=0x01` — if the path doesn't exist, create a regular
    ///   file with the given `mode` before opening. When `mode` is 0
    ///   the default `0o644` is applied. If the path exists, CREAT
    ///   is a no-op (the file is opened normally).
    /// * `EXCL=0x04` — combined with CREAT, rejects with
    ///   [`FsError::AlreadyExists`] (→ EEXIST) if the path already
    ///   exists. Standalone (without CREAT) it's ignored per POSIX.
    /// * `TRUNC=0x08` — after open, truncate a regular file to
    ///   length 0. Fails with [`FsError::IsADirectory`] (→ EISDIR)
    ///   when applied to a directory and with
    ///   [`FsError::ReadOnly`] (→ EROFS) on a read-only filesystem.
    /// * `DIRECTORY=0x02` — require the final target to be a
    ///   directory; otherwise [`FsError::NotADirectory`] (→
    ///   ENOTDIR).
    /// * `CREAT | DIRECTORY` — [`KernelError::InvalidArgument`] (→
    ///   EINVAL). `path_create_directory` is the correct call.
    ///
    /// `lookup_flags` governs final-component symlink handling:
    /// bit 0 (`LOOKUP_SYMLINK_FOLLOW`) set → follow via
    /// [`Vfs::open`]; clear → do not follow via
    /// [`Vfs::open_nofollow`] so a path whose final component is
    /// itself a symlink returns the symlink's own vnode rather
    /// than the target's. Intermediate components always follow
    /// symlinks — only the final component is flag-governed, per
    /// WASI semantics.
    ///
    /// Caps + [`FdObject`] selection are unchanged. Returns the
    /// fresh fd number on success.
    pub fn path_open(
        &mut self,
        pid: Pid,
        path: &str,
        lookup_flags: u32,
        oflags: u16,
        mode: u16,
        flags: FdFlags,
    ) -> Result<u32, KernelError> {
        use abi::wasi::lookupflags as wasi_lookup;
        use abi::wasi::oflags as wasi_oflags;

        let creat = (oflags & wasi_oflags::CREAT) != 0;
        let excl = (oflags & wasi_oflags::EXCL) != 0;
        let trunc = (oflags & wasi_oflags::TRUNC) != 0;
        let directory = (oflags & wasi_oflags::DIRECTORY) != 0;
        let follow_symlink = (lookup_flags & wasi_lookup::SYMLINK_FOLLOW) != 0;

        if creat && directory {
            return Err(KernelError::InvalidArgument);
        }

        let caps = self.caps.list(pid)?;

        let open_path = |vfs: &mut Vfs, p: &str| -> Result<(MountId, Ino, NodeType), FsError> {
            if follow_symlink {
                vfs.open(p)
            } else {
                vfs.open_nofollow(p)
            }
        };

        // Resolution phase: CREAT runs through a try-open-then-
        // create flow; non-CREAT routes through the standard open.
        let (mount_id, ino, ty) = if creat {
            match open_path(&mut self.vfs, path) {
                Ok((m, i, t)) => {
                    if excl {
                        return Err(KernelError::Fs(crate::vfs::FsError::AlreadyExists));
                    }
                    (m, i, t)
                }
                Err(crate::vfs::FsError::NotFound) => {
                    let effective_mode: u32 = if mode == 0 { 0o644 } else { mode as u32 };
                    // Use the watch-aware wrapper so a watcher on
                    // the parent directory observes the implicit
                    // create that O_CREAT performs.
                    self.vfs_create(path, effective_mode)?;
                    // Freshly-created regular file; follow_symlink
                    // is moot because the target isn't a symlink.
                    self.vfs.open(path)?
                }
                Err(e) => return Err(KernelError::Fs(e)),
            }
        } else {
            open_path(&mut self.vfs, path)?
        };

        if directory && ty != NodeType::Directory {
            return Err(KernelError::Fs(crate::vfs::FsError::NotADirectory));
        }

        if trunc {
            match ty {
                NodeType::Directory => {
                    return Err(KernelError::Fs(crate::vfs::FsError::IsADirectory));
                }
                NodeType::RegularFile => {
                    self.vfs.truncate_ino(mount_id, ino, 0)?;
                }
                _ => {
                    // Non-regular, non-directory targets (char devices,
                    // symlinks, sockets, fifos) silently ignore TRUNC —
                    // WASI doesn't specify behaviour there and POSIX
                    // truncate on those is either a no-op or EINVAL
                    // depending on the system. v1 picks "no-op" since
                    // it matches Linux's open(O_TRUNC) on a char device.
                }
            }
        }

        let object = match ty {
            NodeType::CharDevice(devnum) => {
                DeviceDispatcher::check_open(devnum, caps)?;
                FdObject::CharDevice(devnum)
            }
            NodeType::RegularFile
            | NodeType::Directory
            | NodeType::SymLink
            | NodeType::Fifo
            | NodeType::Socket => FdObject::Vnode { mount_id, ino },
        };
        let table = self.fds.get_mut(&pid).ok_or(KernelError::NoSuchPid)?;
        let fd = table.alloc(FdEntry::with_flags(object, flags))?;
        Ok(fd)
    }

    /// Install a caller-provided fd entry directly. Used by
    /// `proc_spawn` (T074) and the headless-shell gate test to
    /// wire stdin/stdout/stderr to specific character devices
    /// without going through `path_open`.
    pub fn install_fd(
        &mut self,
        pid: Pid,
        fd: u32,
        object: FdObject,
        flags: FdFlags,
    ) -> Result<(), KernelError> {
        let table = self.fds.get_mut(&pid).ok_or(KernelError::NoSuchPid)?;
        let closed = table.install_at(fd, FdEntry::with_flags(object, flags))?;
        if let Some(closed) = closed {
            self.release_object(closed.object);
        }
        Ok(())
    }

    /// `fd_read(pid, fd, buf) -> bytes_read`.
    ///
    /// Routes to the right subsystem based on the fd's object
    /// type:
    ///
    /// * `Vnode` → [`Vfs::read_ino`], advancing the fd's offset.
    /// * `CharDevice` → [`DeviceDispatcher::read`], no offset.
    /// * `PipeRead` / `Socket` → wired in a later slice.
    /// * `PipeWrite` / `DisplayConn` / `SignalChannel` →
    ///   [`KernelError::NotSupportedOnFd`].
    pub fn fd_read(
        &mut self,
        pid: Pid,
        fd: u32,
        buf: &mut [u8],
    ) -> Result<usize, KernelError> {
        let entry = *self
            .fds
            .get(&pid)
            .ok_or(KernelError::NoSuchPid)?
            .get(fd)
            .ok_or(KernelError::BadFd)?;
        match entry.object {
            FdObject::Vnode { mount_id, ino } => {
                let n = self.vfs.read_ino(mount_id, ino, entry.offset, buf)?;
                let slot = self
                    .fds
                    .get_mut(&pid)
                    .and_then(|t| t.get_mut(fd))
                    .ok_or(KernelError::BadFd)?;
                slot.offset = slot.offset.saturating_add(n as u64);
                Ok(n)
            }
            FdObject::CharDevice(devnum) => Ok(self.devs.read(devnum, buf)?),
            FdObject::Socket(id) => {
                let (n, _fds) = self.ipc.recv_on_socket(SocketId(id), buf, 0)?;
                Ok(n)
            }
            FdObject::PipeRead(id) => {
                use crate::ipc::{PipeId, PipeReadResult};
                let pipe = self.ipc.pipe_mut(PipeId(id))?;
                match pipe.try_read(buf) {
                    PipeReadResult::Read(n) => Ok(n),
                    PipeReadResult::Eof => Ok(0),
                    PipeReadResult::WouldBlock => Err(KernelError::WouldBlock),
                }
            }
            FdObject::SignalChannel => {
                // v1 wire format: each pending signal serialises
                // as a u16 LE signum. buf.len() < 2 returns 0
                // without touching the inbox (no room for even
                // one record); empty inbox returns WouldBlock.
                let capacity = buf.len() / 2;
                if capacity == 0 {
                    return Ok(0);
                }
                let pending = self
                    .signal_inboxes
                    .get(&pid)
                    .ok_or(KernelError::NoSuchPid)?
                    .len();
                if pending == 0 {
                    return Err(KernelError::WouldBlock);
                }
                let signals = self.drain_signals_up_to(pid, capacity)?;
                let mut written = 0;
                for sig in signals {
                    let bytes = sig.number().to_le_bytes();
                    buf[written..written + 2].copy_from_slice(&bytes);
                    written += 2;
                }
                Ok(written)
            }
            FdObject::Watch { watch_id } => {
                // Watch fds drain the per-watch event queue into the
                // caller's buffer in 8-byte records (mask u32 LE +
                // inode u32 LE). An empty queue returns 0 bytes
                // (non-blocking — a future slice may park the caller
                // on the watch the way ipc_recv parks on a socket).
                // A buffer shorter than one record returns 0 too,
                // matching `fd_read`'s no-op semantic when the
                // window can't hold a single record.
                let watches = self.vfs.watches_mut();
                let Some(watch) = watches.get_mut(watch_id) else {
                    return Err(KernelError::BadFd);
                };
                Ok(watch.drain_into(buf))
            }
            FdObject::HostFile { token } => {
                // Host-imported file fds stream from the kernel-side
                // `host_file_fds` table keyed by token. Reads at or
                // past EOF return 0 (POSIX-shaped end-of-file). The
                // table entry is created by `Kernel::host_file_recv`
                // and torn down by `release_object` on close.
                let Some(state) = self.host_file_fds.get_mut(&token) else {
                    return Err(KernelError::BadFd);
                };
                Ok(state.read(buf))
            }
            FdObject::PipeWrite(_) | FdObject::DisplayConn(_) => {
                Err(KernelError::NotSupportedOnFd)
            }
        }
    }

    /// `fd_write(pid, fd, buf) -> bytes_written`.
    ///
    /// A broken-pipe outcome on a [`FdObject::PipeWrite`] or
    /// [`FdObject::Socket`] fd additionally posts [`Signal::Pipe`]
    /// to the caller's signal inbox alongside the
    /// [`KernelError::PipeBroken`] return — POSIX `write(2)`
    /// specifies SIGPIPE delivery in lockstep with the EPIPE
    /// errno. A broken write on any other fd variant (Vnode,
    /// CharDevice, pipe-read, display conn, signal channel) does
    /// not post a signal: those either cannot surface
    /// PipeBroken or have their own error paths.
    pub fn fd_write(
        &mut self,
        pid: Pid,
        fd: u32,
        buf: &[u8],
    ) -> Result<usize, KernelError> {
        let entry = *self
            .fds
            .get(&pid)
            .ok_or(KernelError::NoSuchPid)?
            .get(fd)
            .ok_or(KernelError::BadFd)?;
        let result: Result<usize, KernelError> = match entry.object {
            FdObject::Vnode { mount_id, ino } => {
                match self.vfs.write_ino(mount_id, ino, entry.offset, buf) {
                    Ok(n) => {
                        let slot = match self
                            .fds
                            .get_mut(&pid)
                            .and_then(|t| t.get_mut(fd))
                            .ok_or(KernelError::BadFd)
                        {
                            Ok(s) => s,
                            Err(e) => return Err(e),
                        };
                        slot.offset = slot.offset.saturating_add(n as u64);
                        // Watch hook: a successful write fires
                        // WATCH_MODIFY on the file's own inode. Zero-
                        // byte writes still notify — POSIX doesn't
                        // distinguish them and a watcher waiting on
                        // a touch() probe would otherwise miss it.
                        self.notify_modify(mount_id, ino);
                        Ok(n)
                    }
                    Err(e) => Err(KernelError::Fs(e)),
                }
            }
            FdObject::CharDevice(devnum) => {
                self.devs.write(devnum, buf).map_err(KernelError::from)
            }
            FdObject::Socket(id) => {
                let r = self
                    .ipc
                    .send_on_socket(SocketId(id), buf, Vec::new())
                    .map_err(KernelError::from);
                if r.is_ok() {
                    // Bytes landed on the peer's rx_buf — if any pid
                    // is parked on a blocking ipc_recv against the
                    // peer socket, wake it now with the freshly-
                    // available bytes.
                    if let Some(peer_id) = self
                        .ipc
                        .sockets_get(SocketId(id))
                        .and_then(|s| s.peer)
                    {
                        let _ = self.wake_parked_recver_if_any(peer_id);
                    }
                }
                r
            }
            FdObject::PipeWrite(id) => {
                use crate::ipc::{PipeId, PipeWriteResult};
                match self.ipc.pipe_mut(PipeId(id)) {
                    Ok(pipe) => match pipe.try_write(buf) {
                        PipeWriteResult::Wrote(n) => Ok(n),
                        PipeWriteResult::Broken => Err(KernelError::PipeBroken),
                        PipeWriteResult::WouldBlock => Err(KernelError::WouldBlock),
                    },
                    Err(e) => Err(KernelError::from(e)),
                }
            }
            FdObject::PipeRead(_)
            | FdObject::DisplayConn(_)
            | FdObject::SignalChannel
            | FdObject::Watch { .. }
            | FdObject::HostFile { .. } => Err(KernelError::NotSupportedOnFd),
        };
        if matches!(result, Err(KernelError::PipeBroken)) {
            self.post_sigpipe(pid);
        }
        result
    }

    /// Deliver [`Signal::Pipe`] into `pid`'s signal inbox, if
    /// the process has one. Called by every syscall path that
    /// can surface [`KernelError::PipeBroken`] — currently
    /// [`Self::fd_write`] and the `handle_sock_send` opcode arm.
    /// Missing inboxes (a pid whose process table entry is gone)
    /// silently no-op: by the time the write was dispatched the
    /// caller existed; a disappearing inbox between dispatch and
    /// this point is a test-only race, not a delivery failure.
    pub fn post_sigpipe(&mut self, pid: Pid) {
        if let Some(inbox) = self.signal_inboxes.get_mut(&pid) {
            inbox.post(Signal::Pipe);
        }
    }

    /// `fd_renumber(pid, from, to)`. WASI's dup2-spelling. Moves
    /// the FdEntry at `from` to `to`, atomically closing any prior
    /// entry at `to`. If `from == to`, validates that `from` is
    /// open and returns success as a no-op. Releases object-side
    /// resources for the prior `to` entry (pipe / socket ref
    /// counts) before returning, same way `fd_close` does.
    pub fn fd_renumber(
        &mut self,
        pid: Pid,
        from: u32,
        to: u32,
    ) -> Result<(), KernelError> {
        let table = self.fds.get_mut(&pid).ok_or(KernelError::NoSuchPid)?;
        let prior = table.renumber(from, to)?;
        if let Some(entry) = prior {
            self.release_object(entry.object);
        }
        Ok(())
    }

    /// `fd_close(pid, fd)`. Releases any object-side resources
    /// (pipe reference, socket, display connection) as part of
    /// the close, not just the fd-table slot.
    pub fn fd_close(&mut self, pid: Pid, fd: u32) -> Result<(), KernelError> {
        let table = self.fds.get_mut(&pid).ok_or(KernelError::NoSuchPid)?;
        let entry = table.close(fd)?;
        self.release_object(entry.object);
        Ok(())
    }

    /// `fd_readdir(pid, fd) -> Vec<DirEntry>`. Resolves `fd` in
    /// `pid`'s fd table and, if the fd points at a directory
    /// vnode, returns its entries as they live on disk (no `.`
    /// / `..` injection — WASI doesn't require them and the v1
    /// VFS doesn't track parent inodes).
    ///
    /// The caller is responsible for cookie-based pagination
    /// and serialisation into the WASI `dirent_t` wire layout —
    /// those concerns are owned by the syscall adapter
    /// ([`crate::syscall::wasi::dispatch_wasi`]), not the
    /// semantic kernel API. Returning the full `Vec<DirEntry>`
    /// here keeps the kernel method filesystem-agnostic and
    /// mirrors the shape of [`Vfs::readdir_ino`].
    ///
    /// Errors:
    /// * [`KernelError::NoSuchPid`] — unknown pid.
    /// * [`KernelError::BadFd`] — fd is not open.
    /// * [`KernelError::NotSupportedOnFd`] — fd does not point
    ///   at a [`FdObject::Vnode`] (char device / socket / pipe
    ///   / signal channel fds have no directory listing).
    /// * [`KernelError::Fs`]`(FsError::NotADirectory)` — the
    ///   vnode is a regular file / symlink rather than a
    ///   directory; the errno-mapping layer translates this to
    ///   ENOTDIR.
    pub fn fd_readdir(
        &mut self,
        pid: Pid,
        fd: u32,
    ) -> Result<Vec<DirEntry>, KernelError> {
        let entry = *self
            .fds
            .get(&pid)
            .ok_or(KernelError::NoSuchPid)?
            .get(fd)
            .ok_or(KernelError::BadFd)?;
        let (mount_id, ino) = match entry.object {
            FdObject::Vnode { mount_id, ino } => (mount_id, ino),
            _ => return Err(KernelError::NotSupportedOnFd),
        };
        Ok(self.vfs.readdir_ino(mount_id, ino)?)
    }

    // --- Display server IPC --------------------------------------
    //
    // These three methods are the kernel side of the
    // `display_connect` / `display_bind` extension syscalls
    // from `contracts/syscalls.md`. They compose the
    // existing `IpcTable` socket primitives (create_socket,
    // bind_socket, listen_socket, connect_socket,
    // accept_socket) with a cap check and the well-known
    // `/run/display` path string so userland gets a clean
    // two-call interface:
    //
    //   * display server: `display_bind(self_pid) -> fd`
    //     -> listener socket at `/run/display`
    //   * ordinary app: `display_connect(self_pid) -> fd`
    //     -> connected socket (server's accept pops it off)
    //   * display server: `accept_socket(self_pid, listener_fd)
    //     -> fd` -> one new server-side connected fd
    //
    // Socket fds route through `fd_read` / `fd_write`
    // already (see the `FdObject::Socket` arms above), so a
    // paired client/server can round-trip bytes without any
    // extra syscall layer.

    /// Bind and listen on `/run/display`. The caller must
    /// hold `Cap::DisplayServer`. Installs the listener in
    /// `pid`'s fd table and returns the fd number. Fails
    /// with `KernelError::AddressInUse` if the path is
    /// already bound (e.g. a second display server tried to
    /// start).
    pub fn display_bind(&mut self, pid: Pid) -> Result<u32, KernelError> {
        let caps = self.caps.list(pid)?;
        if !caps.contains(Cap::DisplayServer) {
            return Err(KernelError::NotCapable);
        }
        let socket_id = self.ipc.create_socket(SocketType::Stream);
        self.ipc.bind_socket(socket_id, DISPLAY_SOCKET_PATH)?;
        self.ipc.listen_socket(socket_id, DISPLAY_LISTEN_BACKLOG)?;
        let table = self.fds.get_mut(&pid).ok_or(KernelError::NoSuchPid)?;
        let fd = table.alloc(FdEntry::new(FdObject::Socket(socket_id.0)))?;
        Ok(fd)
    }

    /// Connect to `/run/display`. The caller must hold
    /// `Cap::DisplayClient`. Installs the client-side
    /// socket in `pid`'s fd table and returns the fd
    /// number. Fails with `KernelError::ConnectionRefused`
    /// if no display server has bound the path or the
    /// listener's backlog is full.
    pub fn display_connect(&mut self, pid: Pid) -> Result<u32, KernelError> {
        let caps = self.caps.list(pid)?;
        if !caps.contains(Cap::DisplayClient) {
            return Err(KernelError::NotCapable);
        }
        let socket_id = self.ipc.create_socket(SocketType::Stream);
        let parker = self
            .ipc
            .connect_socket(socket_id, DISPLAY_SOCKET_PATH)?;
        let table = self.fds.get_mut(&pid).ok_or(KernelError::NoSuchPid)?;
        let fd = table.alloc(FdEntry::new(FdObject::Socket(socket_id.0)))?;
        self.wake_parked_acceptor_if_any(parker)?;
        Ok(fd)
    }

    /// Accept one pending connection on a listening socket
    /// fd owned by `pid`. Returns the fd of the newly-
    /// connected server-side socket. The listener fd stays
    /// open and can be accepted from again.
    ///
    /// `KernelError::WouldBlock` if no pending client is
    /// waiting; `KernelError::BadFd` if the fd isn't a
    /// socket at all; `KernelError::InvalidArgument` if the
    /// socket exists but isn't in `Listening` state.
    pub fn accept_socket(
        &mut self,
        pid: Pid,
        listener_fd: u32,
    ) -> Result<u32, KernelError> {
        let entry = *self
            .fds
            .get(&pid)
            .ok_or(KernelError::NoSuchPid)?
            .get(listener_fd)
            .ok_or(KernelError::BadFd)?;
        let listener_id = match entry.object {
            FdObject::Socket(id) => SocketId(id),
            _ => return Err(KernelError::NotSupportedOnFd),
        };
        let server_id = self.ipc.accept_socket(listener_id)?;
        let table = self
            .fds
            .get_mut(&pid)
            .ok_or(KernelError::NoSuchPid)?;
        let fd = table.alloc(FdEntry::new(FdObject::Socket(server_id.0)))?;
        Ok(fd)
    }

    /// Park `pid` on the listener socket at `listener_fd`, recording
    /// `request_id` so a later `ipc_connect` can build the accept
    /// response. Transitions `pid` from `Running` to `BlockedOnIpc`.
    ///
    /// Returns `WouldBlock` if the listener already has a parked
    /// acceptor (v1 one-parker invariant), `BadFd` if the fd is
    /// missing, `NotSupportedOnFd` if it isn't a socket,
    /// `InvalidArgument` if the socket isn't in `Listening` state.
    pub fn park_on_accept(
        &mut self,
        pid: Pid,
        listener_fd: u32,
        request_id: u32,
    ) -> Result<(), KernelError> {
        let listener_id = self.socket_id_from_fd(pid, listener_fd)?;
        {
            let sock = self
                .ipc
                .sockets_get_mut(listener_id)
                .ok_or(KernelError::BadFd)?;
            if sock.state != crate::ipc::SocketState::Listening {
                return Err(KernelError::InvalidArgument);
            }
            if sock.parked_acceptor.is_some() {
                return Err(KernelError::WouldBlock);
            }
            sock.parked_acceptor = Some((pid, request_id));
        }
        self.procs
            .transition(pid, ProcState::BlockedOnIpc)
            .map_err(|_| KernelError::NoSuchPid)?;
        self.procs.set_block_reason(
            pid,
            crate::proc::BlockReason::Ipc { endpoint_id: listener_id.0 },
        );
        Ok(())
    }

    /// Companion to `park_on_accept`. Called from `ipc_connect`
    /// (indirectly via `wake_parked_acceptor_if_any`) when a client
    /// connect has just pushed onto the listener's backlog and a
    /// parked acceptor is waiting. Completes the accept inline:
    /// pops the client from the backlog, mints the server-side
    /// socket + fd on `acceptor_pid`'s fd table, pairs the sockets,
    /// returns the new fd.
    ///
    /// Symmetric to `accept_socket`, but the fd is allocated on a
    /// different pid from the one driving the dispatcher cycle.
    pub fn complete_parked_accept(
        &mut self,
        acceptor_pid: Pid,
        listener_id: crate::ipc::SocketId,
    ) -> Result<u32, KernelError> {
        let server_id = self.ipc.accept_socket(listener_id)?;
        let table = self
            .fds
            .get_mut(&acceptor_pid)
            .ok_or(KernelError::NoSuchPid)?;
        let fd = table.alloc(FdEntry::new(FdObject::Socket(server_id.0)))?;
        Ok(fd)
    }

    /// If `connect_socket` returned a parker tuple, complete the
    /// parked accept inline, queue the wake response, and transition
    /// the parker back to `Ready`. Called from `ipc_connect` and
    /// `display_connect`.
    ///
    /// Failures in `complete_parked_accept` (e.g. the parker's fd
    /// table is EMFILE) turn into error wakes — the parker gets
    /// notified with the errno rather than sitting parked forever.
    fn wake_parked_acceptor_if_any(
        &mut self,
        parker: Option<(Pid, u32, crate::ipc::SocketId)>,
    ) -> Result<(), KernelError> {
        let Some((acceptor_pid, req_id, listener_id)) = parker else {
            return Ok(());
        };
        let wake_resp = match self.complete_parked_accept(acceptor_pid, listener_id) {
            Ok(fd) => abi::ring::Response::ok(req_id, fd as i64),
            Err(e) => abi::ring::Response::err(
                req_id,
                crate::syscall::dispatch::kerr_to_errno(e),
            ),
        };
        self.pending_wakes.push((acceptor_pid, wake_resp, None));
        let _ = self.procs.transition(acceptor_pid, ProcState::Ready);
        self.procs.clear_block_reason(acceptor_pid);
        Ok(())
    }

    /// Interrupt any parked `ipc_accept` on `pid` with `-EINTR`.
    /// Clears the listener's `parked_acceptor` slot, queues a wake
    /// with `Response::err(req_id, -EINTR)` onto `pending_wakes`,
    /// transitions `pid` Ready, clears `block_reason`. No-op if
    /// `pid` is not parked on any listener.
    ///
    /// Called from `handle_proc_kill` when a catchable signal
    /// (SIGTERM in v1) targets a BlockedOnIpc pid. The wake runs
    /// AFTER the signal-inbox delivery so userland that drains fd
    /// 3 on observing the EINTR wake finds the queued signal
    /// there — both halves of the contract (inbox + EINTR) fire
    /// for every SIGTERM against a parked pid.
    ///
    /// Returns `true` iff a park was interrupted. Production
    /// callers discard the bool; tests use it to assert the
    /// positive-path invariant.
    pub fn interrupt_parked_accept(&mut self, pid: Pid) -> bool {
        let Some(req_id) = self.ipc.take_parked_acceptor_for_pid(pid) else {
            return false;
        };
        let wake_resp = abi::ring::Response::err(req_id, abi::errno::EINTR);
        self.pending_wakes.push((pid, wake_resp, None));
        // Best-effort state transition. If the pid isn't in
        // BlockedOnIpc for some reason (e.g. a race with another
        // wake path), ignore the transition error — the wake is
        // still queued and the user Worker will observe it on the
        // next dispatch pass.
        let _ = self.procs.transition(pid, ProcState::Ready);
        self.procs.clear_block_reason(pid);
        true
    }

    // --- ipc_recv blocking primitives ----------------------------

    /// Park `pid` on a connected socket waiting for bytes (or fds)
    /// to arrive, recording the in-flight `IPC_RECV` parameters so
    /// a future `ipc_send` from the peer can build the response
    /// the non-blocking path would have produced inline. Transitions
    /// `pid` from `Running` to `BlockedOnIpc`.
    ///
    /// Returns `WouldBlock` if `pid` is already parked on a recv
    /// (v1 one-parker-per-pid invariant — a second blocking recv
    /// from an already-parked pid is an apparent deadlock and the
    /// caller must drain its existing wake first). Returns
    /// `BadFd` / `NotSupportedOnFd` / `InvalidArgument` for the
    /// usual fd-resolution failures (mirrors the non-blocking
    /// recv's `socket_id_from_fd` precondition).
    ///
    /// Called from `handle_ipc_recv` ONLY after the non-blocking
    /// recv path returns `WouldBlock` AND the caller's `flags & 0x01
    /// == 0` (default-blocking). The handler is the single source
    /// of truth for the should-park decision; this method does not
    /// re-check rx_buf / rx_fds emptiness.
    pub fn park_on_recv(
        &mut self,
        pid: Pid,
        fd: u32,
        request_id: u32,
        max_len: u32,
        recv_fd_slot: i32,
        heap_ptr: u32,
        heap_len: u32,
    ) -> Result<(), KernelError> {
        if self.parked_recvers.contains_key(&pid) {
            return Err(KernelError::WouldBlock);
        }
        let socket_id = self.socket_id_from_fd(pid, fd)?;
        // Validate the socket is in a state where a recv could
        // legitimately have parked. `recv_on_socket` would have
        // returned `InvalidState` for non-Connected sockets and
        // the non-blocking handler would have surfaced that as
        // `-EINVAL` rather than parking — guard against the harness
        // mistake of parking on a non-Connected socket.
        let sock = self
            .ipc
            .sockets_get(socket_id)
            .ok_or(KernelError::BadFd)?;
        if sock.state != crate::ipc::SocketState::Connected {
            return Err(KernelError::InvalidArgument);
        }
        self.parked_recvers.insert(
            pid,
            RecvParker {
                req_id: request_id,
                socket_id,
                max_len,
                recv_fd_slot,
                heap_ptr,
                heap_len,
            },
        );
        self.procs
            .transition(pid, ProcState::BlockedOnIpc)
            .map_err(|_| {
                // Roll back the parker insertion on transition
                // failure so we don't leak a parker slot on a
                // now-dead pid.
                self.parked_recvers.remove(&pid);
                KernelError::NoSuchPid
            })?;
        self.procs.set_block_reason(
            pid,
            crate::proc::BlockReason::Ipc { endpoint_id: socket_id.0 },
        );
        Ok(())
    }

    /// Companion to `park_on_recv`. Called from `ipc_send`'s
    /// success path with the PEER socket id (the receive-side
    /// socket the bytes / fds just landed on — NOT the sender's
    /// own socket). If any pid is parked on `socket_id`, this:
    ///
    ///   1. Drains rx_buf + rx_fds inline via `ipc_recv` (which
    ///      reuses the non-blocking path's fd-translation logic)
    ///      with the parker's recorded `max_len` / `recv_fd_slot`.
    ///   2. Builds the same fd-leading heap layout the non-blocking
    ///      `handle_ipc_recv` produces — leading 4-byte fd-number
    ///      when `want_fd && new_fd.is_some()`, payload bytes
    ///      after — and queues a `Response { value = bytes,
    ///      extra_len = total }` on `pending_wakes` together with
    ///      a `PendingHeap { heap_ptr, bytes }` so the TS drainer
    ///      copies the heap bytes back into the parker's SAB
    ///      heap-scratch window.
    ///   3. Transitions the parker Ready, clears block_reason,
    ///      removes the parker slot.
    ///
    /// If the inline recv fails (shouldn't happen — the parker's
    /// socket id was validated at park time and the wake is fired
    /// right after a successful send), the wake is queued as an
    /// errno wake so the parker doesn't sit BlockedOnIpc forever.
    ///
    /// Returns `true` iff a parker was woken. Production callers
    /// discard the bool; tests use it.
    pub(crate) fn wake_parked_recver_if_any(&mut self, socket_id: SocketId) -> bool {
        // Find the parker (if any) by socket_id. The map is keyed
        // by pid for O(log n) cleanup, but the wake path needs a
        // socket-id lookup — a linear walk is fine in v1 (parker
        // count == active blocking recvs, which is small).
        let parker_pid = self
            .parked_recvers
            .iter()
            .find_map(|(pid, parker)| {
                if parker.socket_id == socket_id {
                    Some(*pid)
                } else {
                    None
                }
            });
        let Some(parker_pid) = parker_pid else {
            return false;
        };
        let parker = self
            .parked_recvers
            .remove(&parker_pid)
            .expect("parker entry vanished between find and remove");

        // Reuse the non-blocking semantic: drain bytes + optional
        // fd via `Kernel::ipc_recv`, which routes through the same
        // `IpcTable::recv_on_socket` + `translate_passed_fd` flow
        // the f00e559 handler uses. We need an fd here — derive it
        // by reverse-looking-up the socket id in the parker's fd
        // table. The parker's socket fd may have been closed in
        // the window between park and wake; if so, queue an EBADF
        // wake (the parker's fd is gone) instead of a successful
        // wake against a stale socket.
        let parker_fd = self
            .fds
            .get(&parker_pid)
            .and_then(|table| {
                table.iter().find_map(|(fd, entry)| match entry.object {
                    FdObject::Socket(id) if id == parker.socket_id.0 => Some(fd),
                    _ => None,
                })
            });
        let want_fd = parker.recv_fd_slot >= 0;
        let wake_resp_and_heap = match parker_fd {
            Some(fd) => match self.ipc_recv(parker_pid, fd, parker.max_len as usize, want_fd) {
                Ok((bytes, new_fd)) => {
                    let installed = new_fd.is_some();
                    let payload_offset = if installed { 4 } else { 0 };
                    let total = payload_offset + bytes.len();
                    let mut heap_bytes = alloc::vec![0u8; total];
                    if let Some(n) = new_fd {
                        heap_bytes[0..4].copy_from_slice(&n.to_le_bytes());
                    }
                    heap_bytes[payload_offset..payload_offset + bytes.len()]
                        .copy_from_slice(&bytes);
                    let resp = abi::ring::Response {
                        request_id: parker.req_id,
                        status: 0,
                        value: bytes.len() as i64,
                        extra_len: total as u32,
                        _pad: [0u8; 12],
                    };
                    let heap = if total > 0 {
                        Some(PendingHeap {
                            heap_ptr: parker.heap_ptr,
                            bytes: heap_bytes,
                        })
                    } else {
                        None
                    };
                    (resp, heap)
                }
                Err(crate::sys::KernelError::NotSupportedOnFd) => (
                    abi::ring::Response::err(parker.req_id, abi::errno::EBADF),
                    None,
                ),
                Err(e) => (
                    abi::ring::Response::err(
                        parker.req_id,
                        crate::syscall::dispatch::kerr_to_errno(e),
                    ),
                    None,
                ),
            },
            None => (
                abi::ring::Response::err(parker.req_id, abi::errno::EBADF),
                None,
            ),
        };
        let (wake_resp, wake_heap) = wake_resp_and_heap;
        self.pending_wakes.push((parker_pid, wake_resp, wake_heap));
        let _ = self.procs.transition(parker_pid, ProcState::Ready);
        self.procs.clear_block_reason(parker_pid);
        true
    }

    /// Interrupt any parked `ipc_recv` on `pid` with `-EINTR`.
    /// Removes the `parked_recvers` slot, queues a wake with
    /// `Response::err(req_id, -EINTR)` onto `pending_wakes`,
    /// transitions `pid` Ready, clears `block_reason`. No-op if
    /// `pid` is not parked on recv.
    ///
    /// Called from `handle_proc_kill`'s `Signal::Term` arm
    /// alongside the existing `interrupt_parked_accept` /
    /// `interrupt_parked_wait` calls. The three methods are
    /// idempotent against each other: at most one of them observes
    /// the parker slot (a pid parks on at most one primitive at a
    /// time in v1).
    ///
    /// Returns `true` iff a park was interrupted. Production
    /// callers discard the bool; tests use it.
    pub fn interrupt_parked_recv(&mut self, pid: Pid) -> bool {
        let Some(parker) = self.parked_recvers.remove(&pid) else {
            return false;
        };
        let wake_resp = abi::ring::Response::err(parker.req_id, abi::errno::EINTR);
        self.pending_wakes.push((pid, wake_resp, None));
        // Best-effort state transition (mirrors interrupt_parked_
        // accept's race comment).
        let _ = self.procs.transition(pid, ProcState::Ready);
        self.procs.clear_block_reason(pid);
        true
    }

    /// Test helper: True iff `pid` has a `RecvParker` recorded.
    #[doc(hidden)]
    pub fn parked_recvers_contains(&self, pid: Pid) -> bool {
        self.parked_recvers.contains_key(&pid)
    }

    /// Test helper: clone of a parker entry by pid for assertions.
    #[doc(hidden)]
    pub fn parked_recvers_get(&self, pid: Pid) -> Option<RecvParker> {
        self.parked_recvers.get(&pid).copied()
    }

    // --- Generic IPC sockets -------------------------------------
    //
    // Thin wrappers around the `IpcTable` socket primitives that
    // add fd-table management (allocating fds, looking them up as
    // socket objects) to the opcode surface. No cap checks today:
    // any process can create sockets and bind/connect arbitrary
    // paths. The privileged `/run/display` access is gated through
    // `display_bind` / `display_connect` above; every other path
    // is unrestricted in v1. A future cap-granularity slice can
    // add per-path-prefix gating here without changing the opcode
    // wire format.

    /// Create an unbound socket of the given type and install it at
    /// a fresh fd in `pid`'s fd table. Returns the new fd number.
    pub fn ipc_socket(&mut self, pid: Pid, ty: SocketType) -> Result<u32, KernelError> {
        let socket_id = self.ipc.create_socket(ty);
        let table = self.fds.get_mut(&pid).ok_or(KernelError::NoSuchPid)?;
        let fd = table.alloc(FdEntry::new(FdObject::Socket(socket_id.0)))?;
        Ok(fd)
    }

    /// Create a pipe pair and install both ends on `pid`'s fd table.
    /// Returns `(read_fd, write_fd)`. Mirrors POSIX `pipe(2)`: after
    /// a successful call, bytes written to `write_fd` are readable
    /// from `read_fd` via the existing fd_read / fd_write paths.
    ///
    /// If the second `alloc` fails (fd-limit exhaustion between the
    /// two allocs), the first fd is released to keep the fd table
    /// consistent — a failed IPC_PIPE leaves zero fds installed.
    /// The underlying `Pipe` object is also dropped from the IPC
    /// table's live map since neither end ever got a reference.
    pub fn create_pipe_fds(&mut self, pid: Pid) -> Result<(u32, u32), KernelError> {
        let pipe_id = self.ipc.create_pipe();
        // First fd: read side.
        let table = self.fds.get_mut(&pid).ok_or(KernelError::NoSuchPid)?;
        let read_fd = match table.alloc(FdEntry::new(FdObject::PipeRead(pipe_id.0))) {
            Ok(fd) => fd,
            Err(e) => {
                // Release the pipe's read ref so the pipe can be reaped.
                let _ = self.ipc.drop_pipe_reader(pipe_id);
                let _ = self.ipc.drop_pipe_writer(pipe_id);
                return Err(e.into());
            }
        };
        // Second fd: write side.
        let table = self.fds.get_mut(&pid).ok_or(KernelError::NoSuchPid)?;
        let write_fd = match table.alloc(FdEntry::new(FdObject::PipeWrite(pipe_id.0))) {
            Ok(fd) => fd,
            Err(e) => {
                // Roll back the read fd install, then release both
                // pipe refs so nothing leaks.
                let _ = table.close(read_fd);
                let _ = self.ipc.drop_pipe_reader(pipe_id);
                let _ = self.ipc.drop_pipe_writer(pipe_id);
                return Err(e.into());
            }
        };
        Ok((read_fd, write_fd))
    }

    /// Bind `fd` (which must refer to an unbound socket) to `path`.
    /// The path is a kernel-visible string, not a filesystem path —
    /// it lives in a separate bindings table on `IpcTable`, not in
    /// the VFS.
    pub fn ipc_bind(&mut self, pid: Pid, fd: u32, path: &str) -> Result<(), KernelError> {
        let socket_id = self.socket_id_from_fd(pid, fd)?;
        self.ipc.bind_socket(socket_id, path)?;
        Ok(())
    }

    /// Transition a bound socket fd to listening with a caller-
    /// supplied backlog. Only meaningful for stream sockets; dgram
    /// sockets skip `listen` entirely and go straight to `recv`.
    pub fn ipc_listen(
        &mut self,
        pid: Pid,
        fd: u32,
        backlog: usize,
    ) -> Result<(), KernelError> {
        let socket_id = self.socket_id_from_fd(pid, fd)?;
        self.ipc.listen_socket(socket_id, backlog)?;
        Ok(())
    }

    /// Connect `fd` (which must refer to an unbound socket) to the
    /// listener bound at `path`. After a successful connect, the
    /// caller uses the existing `fd_read` / `fd_write` surface to
    /// exchange bytes — those already route through the socket via
    /// `FdObject::Socket`, so no separate `ipc_send` / `ipc_recv`
    /// plumbing is needed on the Rust side.
    pub fn ipc_connect(
        &mut self,
        pid: Pid,
        fd: u32,
        path: &str,
    ) -> Result<(), KernelError> {
        let socket_id = self.socket_id_from_fd(pid, fd)?;
        let parker = self.ipc.connect_socket(socket_id, path)?;
        self.wake_parked_acceptor_if_any(parker)?;
        Ok(())
    }

    /// `ipc_send(pid, fd, buf, fd_to_pass)` — send `buf` (and an
    /// optional ancillary fd) on a connected socket OR pipe-write fd.
    /// Returns the number of bytes accepted by the underlying ring.
    ///
    /// Routing by fd object:
    /// * [`FdObject::Socket`] — bytes go through
    ///   [`IpcTable::send_on_socket`] together with the optional
    ///   `fd_to_pass` (queued on the peer's ancillary-fd queue for a
    ///   future `ipc_recv` to drain).
    /// * [`FdObject::PipeWrite`] — bytes go through
    ///   [`Pipe::try_write`]. Pipes are byte streams without an
    ///   ancillary channel, so `fd_to_pass.is_some()` on a pipe is
    ///   rejected as [`KernelError::InvalidArgument`] before any
    ///   write is attempted.
    /// * any other fd object — [`KernelError::NotSupportedOnFd`],
    ///   which the dispatcher remaps to `EBADF` for the IPC_SEND
    ///   wire surface (the spec calls "wrong fd kind" EBADF, not
    ///   EINVAL, distinguishing the IPC opcodes from the more
    ///   generic FD_WRITE arm that uses EINVAL).
    ///
    /// `fd_to_pass` validity is checked BEFORE the send — an invalid
    /// ancillary fd causes EBADF without enqueuing the bytes (the
    /// spec mandates the receiver gets nothing on this failure).
    /// Validation is "fd exists in the caller's table"; the value
    /// itself is shipped as a u32 to be re-installed by the eventual
    /// IPC_RECV slice. v1's per-socket `rx_fds` queue stores the
    /// number alone — translation into a fresh receiver-side
    /// FdEntry is the IPC_RECV slice's job, not this one.
    ///
    /// Pipe-broken outcomes additionally post [`Signal::Pipe`] to
    /// the caller's signal inbox via [`Self::post_sigpipe`], same
    /// way [`Self::fd_write`] does — the `write(2)` POSIX contract
    /// pairs EPIPE with SIGPIPE delivery and userland callers
    /// expect that pairing on `ipc_send` too.
    pub fn ipc_send(
        &mut self,
        pid: Pid,
        fd: u32,
        buf: &[u8],
        fd_to_pass: Option<u32>,
    ) -> Result<usize, KernelError> {
        let entry = *self
            .fds
            .get(&pid)
            .ok_or(KernelError::NoSuchPid)?
            .get(fd)
            .ok_or(KernelError::BadFd)?;
        if let Some(ancillary) = fd_to_pass {
            // Verify the ancillary fd exists in the caller's table
            // BEFORE enqueuing any bytes. A bad ancillary fd must
            // not produce a partial send (the receiver would otherwise
            // observe payload bytes without the promised fd).
            let table = self.fds.get(&pid).ok_or(KernelError::NoSuchPid)?;
            if table.get(ancillary).is_none() {
                return Err(KernelError::BadFd);
            }
        }
        let result: Result<usize, KernelError> = match entry.object {
            FdObject::Socket(id) => {
                let passed = match fd_to_pass {
                    Some(f) => alloc::vec![f],
                    None => Vec::new(),
                };
                let r = self
                    .ipc
                    .send_on_socket(SocketId(id), buf, passed)
                    .map_err(KernelError::from);
                if r.is_ok() {
                    // Bytes / fds landed on the peer's rx side — if
                    // any pid is parked on a blocking ipc_recv
                    // against the peer socket, wake it now with the
                    // freshly-available payload + ancillary fd.
                    if let Some(peer_id) = self
                        .ipc
                        .sockets_get(SocketId(id))
                        .and_then(|s| s.peer)
                    {
                        let _ = self.wake_parked_recver_if_any(peer_id);
                    }
                }
                r
            }
            FdObject::PipeWrite(id) => {
                if fd_to_pass.is_some() {
                    return Err(KernelError::InvalidArgument);
                }
                use crate::ipc::{PipeId, PipeWriteResult};
                match self.ipc.pipe_mut(PipeId(id)) {
                    Ok(pipe) => match pipe.try_write(buf) {
                        PipeWriteResult::Wrote(n) => Ok(n),
                        PipeWriteResult::Broken => Err(KernelError::PipeBroken),
                        PipeWriteResult::WouldBlock => Err(KernelError::WouldBlock),
                    },
                    Err(e) => Err(KernelError::from(e)),
                }
            }
            _ => Err(KernelError::NotSupportedOnFd),
        };
        if matches!(result, Err(KernelError::PipeBroken)) {
            self.post_sigpipe(pid);
        }
        result
    }

    /// `ipc_recv(pid, fd, max_len, want_fd)` — drain bytes (and
    /// optionally one ancillary fd) from a connected socket.
    /// Returns `(bytes_read, optionally a freshly-installed
    /// receiver-side fd-number)`.
    ///
    /// The fd-number in the returned tuple is a NEW number in the
    /// caller's fd table — the kernel allocates the lowest free
    /// slot via `FdTable::alloc` and installs a clone of the
    /// underlying [`FdObject`] the sender's fd referred to. This is
    /// the receiver side of the cross-process fd-passing primitive
    /// POSIX `recvmsg(2) + SCM_RIGHTS` provides (Wayland's
    /// `wl_keyboard.keymap` and friends rely on the equivalent).
    ///
    /// Routing by fd object: the caller's `fd` MUST resolve to
    /// [`FdObject::Socket`] — any other kind returns
    /// [`KernelError::NotSupportedOnFd`] which the dispatcher
    /// remaps to EBADF (mirroring `ipc_send`'s wire surface; the
    /// IPC opcodes use POSIX `recv(2)`'s "wrong fd kind" → EBADF
    /// semantic, distinct from FD_READ's EINVAL on a non-readable
    /// fd).
    ///
    /// Empty-socket semantics: an empty rx_buf AND empty rx_fds
    /// surfaces as [`KernelError::WouldBlock`]. The dispatcher
    /// chooses behavior based on the IPC_RECV `flags & 0x01` bit:
    /// non-blocking returns `-EAGAIN`; blocking calls
    /// [`Self::park_on_recv`] to put the caller into BlockedOnIpc
    /// and wakes them via [`Self::wake_parked_recver_if_any`] on
    /// the next peer-side `ipc_send` (or `fd_write`) success.
    ///
    /// `want_fd = false` semantics: the rx_fds queue is left
    /// untouched even if it has entries waiting. A subsequent
    /// `ipc_recv` with `want_fd = true` can drain them. This
    /// matches POSIX `recvmsg(2)` with `msg_control = NULL` —
    /// ancillary data stays queued until a caller asks for it.
    ///
    /// fd-translation: when `want_fd = true` and the rx_fds queue
    /// has at least one entry, the kernel dequeues ONE u32 number
    /// via [`IpcTable::recv_on_socket`] and tries to translate it
    /// into a fresh receiver-side [`FdEntry`]:
    ///
    ///   1. Find the peer's `SocketId` from the receive-side
    ///      socket's `peer` field.
    ///   2. Scan `self.fds` for the pid whose fd table contains an
    ///      `FdObject::Socket(peer_id)` entry — that's the sender.
    ///   3. Look up the queued u32 in the sender's fd table to get
    ///      the underlying `FdObject`.
    ///   4. Allocate a new fd in the receiver's fd table installing
    ///      a clone of that object.
    ///
    /// If any step of the translation fails (sender process exited
    /// between send and recv, sender closed the fd before recv,
    /// peer pointer became `None`), the queued u32 is silently
    /// dropped and the receiver gets `Ok((bytes, None))`. This is
    /// a deliberate v1 degradation: a more rigorous impl would
    /// resolve the underlying object at SEND time and queue the
    /// `FdObject` directly (eliminating the dangling-fd race), but
    /// the IPC_SEND slice committed to queueing only the u32 number
    /// — changing that is a separate IPC-table refactor.
    pub fn ipc_recv(
        &mut self,
        pid: Pid,
        fd: u32,
        max_len: usize,
        want_fd: bool,
    ) -> Result<(Vec<u8>, Option<u32>), KernelError> {
        let socket_id = self.socket_id_from_fd(pid, fd)?;
        let max_fds = if want_fd { 1 } else { 0 };
        let mut buf = alloc::vec![0u8; max_len];
        let (n, fds) = self
            .ipc
            .recv_on_socket(socket_id, &mut buf, max_fds)
            .map_err(KernelError::from)?;
        buf.truncate(n);

        let new_fd = if let Some(&sender_fd_num) = fds.first() {
            self.translate_passed_fd(socket_id, pid, sender_fd_num)
        } else {
            None
        };

        Ok((buf, new_fd))
    }

    /// Resolve a queued u32 fd-number into a receiver-side
    /// [`FdEntry`]. Returns `Some(new_fd)` on success, `None` if
    /// any step fails (sender exited, peer cleared, sender's fd
    /// closed, or fd-table install errored).
    ///
    /// Encapsulated as a separate method so [`Self::ipc_recv`]
    /// reads as a single linear flow — the lookup-and-install dance
    /// is enough machinery to deserve its own name.
    fn translate_passed_fd(
        &mut self,
        receiver_socket: SocketId,
        receiver_pid: Pid,
        sender_fd_num: u32,
    ) -> Option<u32> {
        let peer_id = self.ipc.sockets_get(receiver_socket)?.peer?;
        let sender_pid = self.find_pid_owning_socket(peer_id)?;
        let sender_object = self.fds.get(&sender_pid)?.get(sender_fd_num)?.object;
        let table = self.fds.get_mut(&receiver_pid)?;
        table
            .alloc(FdEntry::new(sender_object))
            .ok()
    }

    /// Linear scan helper: find the pid whose fd table contains an
    /// `FdObject::Socket(socket_id)` entry. v1 invariant: each
    /// socket id is referenced by at most one process's fd table.
    /// Returns `None` if no process owns the socket — this is the
    /// "sender exited between send and recv" path for
    /// [`Self::translate_passed_fd`]. O(P * F) but P and F are
    /// small in v1 (handful of processes, soft-limit ~1024 fds);
    /// a reverse index would optimise this when it matters.
    fn find_pid_owning_socket(&self, socket_id: SocketId) -> Option<Pid> {
        for (pid, table) in &self.fds {
            for (_fd, entry) in table.iter() {
                if let FdObject::Socket(id) = entry.object {
                    if id == socket_id.0 {
                        return Some(*pid);
                    }
                }
            }
        }
        None
    }

    /// Helper: look up `fd` in `pid`'s fd table and return the
    /// `SocketId` the entry refers to, or the appropriate
    /// `KernelError` if the fd is missing / not a socket.
    fn socket_id_from_fd(&self, pid: Pid, fd: u32) -> Result<SocketId, KernelError> {
        let entry = self
            .fds
            .get(&pid)
            .ok_or(KernelError::NoSuchPid)?
            .get(fd)
            .ok_or(KernelError::BadFd)?;
        match entry.object {
            FdObject::Socket(id) => Ok(SocketId(id)),
            _ => Err(KernelError::NotSupportedOnFd),
        }
    }

    /// Public version of [`socket_id_from_fd`] for tests that want
    /// to inspect the IpcTable's per-socket state by fd.
    #[doc(hidden)]
    pub fn socket_id_from_fd_public(
        &self,
        pid: Pid,
        fd: u32,
    ) -> Result<crate::ipc::SocketId, KernelError> {
        self.socket_id_from_fd(pid, fd)
    }

    /// Test helper: True iff `pending_wakes` is empty.
    #[doc(hidden)]
    pub fn pending_wakes_is_empty(&self) -> bool {
        self.pending_wakes.is_empty()
    }

    /// Test helper: clone of `pending_wakes` for assertions.
    #[doc(hidden)]
    pub fn pending_wakes_snapshot(
        &self,
    ) -> alloc::vec::Vec<(Pid, abi::ring::Response, WakeHeap)> {
        self.pending_wakes.clone()
    }

    /// Test helper: look up a parker by parent pid.
    #[doc(hidden)]
    pub fn parked_waiters_get_public(&self, pid: Pid) -> Option<WaitParker> {
        self.parked_waiters.get(&pid).copied()
    }

    /// Drain queued object-release side effects after an fd-table
    /// close has run. Reconciles the IpcTable against the live fd
    /// tables: any socket whose id is still in `self.ipc` but is no
    /// longer referenced by *any* fd in *any* pid's fd table is an
    /// orphan left behind by a direct `FdTable::close` call (see the
    /// `park_on_accept_clears_on_listener_close` test), and gets the
    /// same close-socket side effects `release_object` would've run
    /// synchronously from `fd_close`.
    #[doc(hidden)]
    pub fn drain_closed_object_side_effects(&mut self) {
        use alloc::collections::BTreeSet;
        let mut live: BTreeSet<u32> = BTreeSet::new();
        for table in self.fds.values() {
            for (_fd, entry) in table.iter() {
                if let FdObject::Socket(id) = entry.object {
                    live.insert(id);
                }
            }
        }
        let orphans: alloc::vec::Vec<crate::ipc::SocketId> = self
            .ipc
            .sockets_iter_ids()
            .filter(|id| {
                if live.contains(&id.0) {
                    return false;
                }
                match self.ipc.sockets_get(*id) {
                    Some(s) => !s.closed,
                    None => false,
                }
            })
            .collect();
        for id in orphans {
            // Mirror release_object's recv-parker drain: wake any
            // parked recver on this socket (or its peer) with EBADF
            // BEFORE closing the socket, so the parker observes
            // the error path rather than sitting parked forever.
            self.wake_parked_recvers_on_socket_close(id);
            match self.ipc.close_socket(id) {
                Ok(Some((parker_pid, req_id))) => {
                    self.pending_wakes.push((
                        parker_pid,
                        abi::ring::Response::err(req_id, abi::errno::EBADF),
                        None,
                    ));
                    let _ = self.procs.transition(parker_pid, ProcState::Ready);
                    self.procs.clear_block_reason(parker_pid);
                }
                _ => {}
            }
        }
    }

    // --- Private helpers -----------------------------------------

    /// Release any per-fd-object resources on the kernel side.
    /// Currently a no-op for every variant except the two we
    /// actually wire in v1; extended in subsequent slices as
    /// each object type's resource model settles.
    fn release_object(&mut self, object: FdObject) {
        match object {
            FdObject::PipeRead(id) => {
                let _ = self.ipc.drop_pipe_reader(crate::ipc::PipeId(id));
            }
            FdObject::PipeWrite(id) => {
                let _ = self.ipc.drop_pipe_writer(crate::ipc::PipeId(id));
            }
            FdObject::Socket(id) => {
                // Wake any blocking-recv parker waiting on this
                // socket BEFORE the close — their socket is going
                // away, so the wake response is EBADF (the fd they
                // were parked on no longer exists). Mirror of the
                // close_socket parker drain that handles the accept
                // side.
                self.wake_parked_recvers_on_socket_close(crate::ipc::SocketId(id));
                match self.ipc.close_socket(crate::ipc::SocketId(id)) {
                    Ok(Some((parker_pid, req_id))) => {
                        self.pending_wakes.push((
                            parker_pid,
                            abi::ring::Response::err(req_id, abi::errno::EBADF),
                            None,
                        ));
                        let _ = self.procs.transition(parker_pid, ProcState::Ready);
                        self.procs.clear_block_reason(parker_pid);
                    }
                    Ok(None) => {}
                    Err(_) => {}
                }
            }
            FdObject::Watch { watch_id } => {
                // Unregister the watch from the VFS notifier so future
                // mutations on the watched (mount, inode) pair don't
                // queue events into a queue that will never be drained.
                let _ = self.vfs.unregister_watch(watch_id);
            }
            FdObject::HostFile { token } => {
                // Drop the kernel-side host-file bytes — closing the
                // fd "releases the browser-side `File` reference"
                // per `contracts/syscalls.md §3.6`. The kernel-side
                // bytes stand in for that reference today; the TS-
                // side bootstrap-cleanup hook (which would actually
                // null out the JS-side `File` object) is a future
                // slice. A close on a token that never had a recv
                // (or a token whose state was already drained) is a
                // silent no-op — `BTreeMap::remove` returns None.
                let _ = self.host_file_fds.remove(&token);
            }
            FdObject::Vnode { .. }
            | FdObject::CharDevice(_)
            | FdObject::DisplayConn(_)
            | FdObject::SignalChannel => {}
        }
    }

    /// Drain any parked-recver entries whose `socket_id` matches
    /// `socket_id` (or whose `socket_id` matches the peer of
    /// `socket_id` — closing one end of a connected pair makes the
    /// peer effectively unreadable). For each match, queue a
    /// `Response::err(req_id, -EBADF)` wake on `pending_wakes`,
    /// transition the parker Ready, clear `block_reason`, drop the
    /// parker slot.
    ///
    /// Mirror of `IpcTable::close_socket`'s parked-acceptor drain
    /// for the recv side. v1's one-parker-per-pid invariant means
    /// at most one match per side, but the implementation walks
    /// the map anyway in case future slices lift that invariant.
    fn wake_parked_recvers_on_socket_close(&mut self, socket_id: crate::ipc::SocketId) {
        // Identify the peer (if any) so a close on one end of a
        // connected pair wakes a parker on the other end too.
        let peer_id = self
            .ipc
            .sockets_get(socket_id)
            .and_then(|s| s.peer);
        let to_wake: alloc::vec::Vec<(Pid, u32)> = self
            .parked_recvers
            .iter()
            .filter_map(|(pid, parker)| {
                let matches = parker.socket_id == socket_id
                    || peer_id.map(|p| p == parker.socket_id).unwrap_or(false);
                if matches {
                    Some((*pid, parker.req_id))
                } else {
                    None
                }
            })
            .collect();
        for (parker_pid, req_id) in to_wake {
            self.parked_recvers.remove(&parker_pid);
            let resp = abi::ring::Response::err(req_id, abi::errno::EBADF);
            self.pending_wakes.push((parker_pid, resp, None));
            let _ = self.procs.transition(parker_pid, ProcState::Ready);
            self.procs.clear_block_reason(parker_pid);
        }
    }

    // --- Lifecycle transitions for scheduled processes -----------

    /// Move a process from `Starting` to `Ready` and enqueue it
    /// on the scheduler. Called after `register_process` once the
    /// Worker has instantiated and the process is actually
    /// runnable. Tests call this directly to bypass the
    /// Worker-instantiation step.
    pub fn mark_ready(&mut self, pid: Pid) -> Result<(), KernelError> {
        self.procs
            .transition(pid, ProcState::Ready)
            .map_err(|_| KernelError::NoSuchPid)?;
        self.sched.enqueue(pid);
        Ok(())
    }

    /// Record an exit status on a running or blocked process and
    /// move it to `Zombie`. Does NOT remove the per-pid fd table
    /// or process-table entry — that is what `reap` (via the
    /// parent's `proc_wait`) does. Does, however, release every
    /// object-side resource the fd table refers to (IPC socket
    /// bindings, pipe reader / writer refs, etc.) eagerly, so
    /// stale kernel state does not survive until a parent
    /// happens to reap the zombie.
    ///
    /// The fd-table drain at exit matters because `proc_wait`
    /// (T075) is still deferred: in the current M1 substrate
    /// nothing reaps. Without this eager release, a display
    /// server that `proc_exit`s would leave its `/run/display`
    /// binding behind forever — a follow-up client's
    /// `display_connect` would succeed against that orphan
    /// listener and then hang, instead of returning
    /// `ConnectionRefused` cleanly. Draining here makes exit the
    /// single source of truth for "this process no longer owns
    /// any kernel resources"; the subsequent `reap` walks an
    /// already-empty fd table and is effectively a pid-table +
    /// cap-table + signal-inbox sweep.
    pub fn proc_exit(&mut self, pid: Pid, status: ExitStatus) -> Result<(), KernelError> {
        self.sched.remove(pid);
        // If this pid is parked on any listener, clear the slot so
        // the listener doesn't hold a stale reference after the
        // process dies.
        self.ipc.clear_parked_acceptor_for_pid(pid);
        // If the exiting pid is itself parked on a blocking
        // proc_wait, clear the slot so the exit-time sweep
        // mirrors the ipc_accept side.
        self.parked_waiters.remove(&pid);
        // Same surgical sweep for the blocking ipc_recv parker
        // slot — an exiting pid can't observe a recv wake.
        self.parked_recvers.remove(&pid);
        self.release_fd_table_resources(pid);
        let ppid = self.procs.get(pid).map(|p| p.ppid).unwrap_or(0);
        self.procs
            .exit(pid, status)
            .map_err(|_| KernelError::NoSuchPid)?;
        self.post_sigchld(ppid);
        // If the exiting pid's parent is parked on wait AND the
        // parker's target matches this child, reap + wake inline.
        // The helper is a no-op if either condition fails.
        self.wake_parked_waiter_for_child(pid, ppid, status);
        Ok(())
    }

    /// Deliver [`Signal::Child`] to `ppid`'s signal inbox if
    /// that pid exists and still has one. Orphans (ppid == 0)
    /// and reaped parents silently no-op. Called by every exit
    /// path that transitions a pid to Zombie —
    /// [`Self::proc_exit`] and [`Self::proc_kill`]'s SIGKILL
    /// arm — so a parent polling fd 3 observes each child exit
    /// exactly once.
    pub fn post_sigchld(&mut self, ppid: Pid) {
        if ppid == 0 {
            return;
        }
        if let Some(inbox) = self.signal_inboxes.get_mut(&ppid) {
            inbox.post(Signal::Child);
        }
    }

    /// Drain the named process's fd table, releasing every
    /// object-side resource (pipe ref, socket, display conn)
    /// held by the open fds. The fd table itself is left in
    /// place (now empty) so `reap` still finds it. Called
    /// once by `proc_exit` and once again by `reap`; the second
    /// call is a no-op on the already-empty table.
    fn release_fd_table_resources(&mut self, pid: Pid) {
        let dropped = self
            .fds
            .get_mut(&pid)
            .map(FdTable::drain_all)
            .unwrap_or_default();
        for (_fd, entry) in dropped {
            self.release_object(entry.object);
        }
    }

    // --- Spawn / wait / kill -------------------------------------

    /// Spawn a fresh child process. Creates the pid, installs its
    /// cap set (must be a subset of `parent_pid`'s own cap set —
    /// the privilege-escalation guard), copies stdin/stdout/stderr
    /// from the manifest into the child's fd table (bumping pipe
    /// refcounts when the stdio fd object is a pipe end), and
    /// marks the child `Ready`.
    ///
    /// Returns the child pid.
    pub fn proc_spawn(
        &mut self,
        parent_pid: Pid,
        args: SpawnArgs<'_>,
    ) -> Result<Pid, KernelError> {
        // Privilege-escalation guard: the child's cap set must
        // be a subset of the parent's. This is tighter than the
        // `cap_grant` rule (which allows CapGrant holders to
        // grant any cap they hold) and matches POSIX semantics:
        // a child cannot gain privileges at spawn beyond what
        // the parent already holds.
        let parent_caps = self.caps.list(parent_pid)?;
        if !parent_caps.is_superset_of(args.caps) {
            return Err(KernelError::NotCapable);
        }

        // Allocate + construct the child process.
        let child_pid = self.procs.allocate_pid();
        let proc = Process::new_starting(
            child_pid,
            parent_pid,
            args.name,
            args.argv,
            args.envp,
            args.cwd,
            args.caps,
            0,
            0,
            0,
        );
        self.procs.insert(proc)?;
        self.caps.install(child_pid, args.caps);
        self.fds.insert(child_pid, FdTable::new());
        self.signal_inboxes.insert(child_pid, SignalInbox::new());

        // Wire stdin/stdout/stderr. Each install bumps pipe
        // refcounts when the object is a pipe end so the child
        // and parent share the same underlying kernel object.
        let stdio = [args.stdin, args.stdout, args.stderr];
        for (fd, object) in stdio.iter().enumerate() {
            self.inherit_object(*object);
            let table = self.fds.get_mut(&child_pid).unwrap();
            table
                .install_at(fd as u32, FdEntry::new(*object))
                .expect("stdio install within soft limit");
        }

        // Auto-install the per-process signal channel at fd 3.
        // Matches the POSIX signalfd convention referenced in
        // `crates/kernel/src/proc/signal.rs` ("a `fd_read` on
        // fd 3 drains pending signals"): every proc_spawn'd
        // child can observe its own signal stream via fd_read +
        // POLL_ONEOFF on fd 3 without an explicit install step.
        // Processes built via `register_process` (a lower-level
        // test primitive) do NOT get the auto-install — that
        // primitive is deliberately minimal.
        let table = self.fds.get_mut(&child_pid).unwrap();
        table
            .install_at(3, FdEntry::new(FdObject::SignalChannel))
            .expect("signal channel install within soft limit");

        // Child is ready to run.
        self.procs
            .transition(child_pid, ProcState::Ready)
            .map_err(|_| KernelError::NoSuchPid)?;
        self.sched.enqueue(child_pid);
        Ok(child_pid)
    }

    /// Wait on a child. Non-blocking: returns `WouldBlock` when
    /// matching children exist but none have exited, and
    /// `NoChildren` when the target doesn't match any live
    /// child at all. Real blocking waits are mapped to these
    /// outcomes at the syscall-dispatch layer.
    ///
    /// On a successful reap, the kernel-side resources of the
    /// reaped child are released: its fd table is drained and
    /// its cap set is removed.
    pub fn proc_wait(
        &mut self,
        parent_pid: Pid,
        target: WaitTarget,
    ) -> Result<WaitOutcome, KernelError> {
        // Parent must actually exist. Wait on a nonexistent
        // parent is a programming error.
        if !self.procs.is_alive(parent_pid) {
            return Err(KernelError::NoSuchPid);
        }

        let zt = match target {
            WaitTarget::Any => ZombieTarget::Any,
            WaitTarget::Specific(pid) => ZombieTarget::Specific(pid),
        };
        if let Some(zombie) = self.procs.find_zombie_child(parent_pid, zt) {
            let status = self.reap(zombie)?;
            return Ok(WaitOutcome::Reaped(zombie, status));
        }

        // No zombie yet. Distinguish "has live children matching
        // the target" (WouldBlock) from "no children matching at
        // all" (NoChildren) — important to give userland ECHILD
        // vs. EAGAIN.
        let has_live_match = match target {
            WaitTarget::Any => self.procs.child_count(parent_pid) > 0,
            WaitTarget::Specific(pid) => {
                self.procs
                    .get(pid)
                    .map(|p| p.ppid == parent_pid && p.state != ProcState::Dead)
                    .unwrap_or(false)
            }
        };
        if has_live_match {
            Ok(WaitOutcome::WouldBlock)
        } else {
            Ok(WaitOutcome::NoChildren)
        }
    }

    /// Park `parent_pid` on a blocking `proc_wait`. Records the
    /// parker's request_id + target + heap_ptr + heap_len, then
    /// transitions the parent Running -> BlockedOnWait and sets
    /// `block_reason = BlockReason::Wait { pid }` where `pid =
    /// -1` for `WaitTarget::Any` or the specific child pid
    /// otherwise.
    ///
    /// Precondition: the handler layer has already called
    /// `Kernel::proc_wait` and observed `WaitOutcome::WouldBlock`
    /// (a live matching child exists but no zombie). This method
    /// does NOT re-check that precondition; calling it on a
    /// parent with no live matching child would leave it parked
    /// forever. The handler layer's branching (§3.4 of the
    /// design) is the single source of truth for the
    /// "should-park vs return-ECHILD" decision.
    ///
    /// Returns `WouldBlock` if `parent_pid` is already parked
    /// (v1 one-waiter-per-parent invariant). `NoSuchPid` if the
    /// state transition fails (e.g. the parent was reaped
    /// between the `proc_wait` check and this call — a
    /// harness-only race).
    pub fn park_on_wait(
        &mut self,
        parent_pid: Pid,
        req_id: u32,
        target: WaitTarget,
        heap_ptr: u32,
        heap_len: u32,
    ) -> Result<(), KernelError> {
        if self.parked_waiters.contains_key(&parent_pid) {
            return Err(KernelError::WouldBlock);
        }
        self.parked_waiters.insert(
            parent_pid,
            WaitParker { req_id, target, heap_ptr, heap_len },
        );
        self.procs
            .transition(parent_pid, ProcState::BlockedOnWait)
            .map_err(|_| {
                // Roll back the parker insertion on transition
                // failure so we don't leak a parker slot on a
                // now-dead parent.
                self.parked_waiters.remove(&parent_pid);
                KernelError::NoSuchPid
            })?;
        let reason_pid = match target {
            WaitTarget::Any => -1,
            WaitTarget::Specific(p) => p,
        };
        self.procs
            .set_block_reason(parent_pid, crate::proc::BlockReason::Wait { pid: reason_pid });
        Ok(())
    }

    /// Companion to `park_on_wait`. Invoked from
    /// `Kernel::proc_exit` and `Kernel::proc_kill`'s Signal::Kill
    /// arm when a child transitions to Zombie. If the child's
    /// parent has a parked waiter whose target matches the child,
    /// this:
    ///
    ///   1. Reaps the child inline (releases its kernel-side
    ///      resources, removes it from procs).
    ///   2. Queues the wake on `pending_wakes` with the packed
    ///      exit status + (if the parker's heap_len >= 4) the
    ///      child pid in the heap bytes.
    ///   3. Transitions the parent Ready + clears block_reason.
    ///   4. Removes the parker slot from `parked_waiters`.
    ///
    /// Returns true iff a wake was fired. Production callers
    /// discard the bool; tests use it.
    ///
    /// If the parent has no parked waiter, or the parker's target
    /// doesn't match the child, this is a no-op — the child stays
    /// Zombie (to be reaped by a later non-blocking wait) and no
    /// wake is queued.
    pub(crate) fn wake_parked_waiter_for_child(
        &mut self,
        child_pid: Pid,
        ppid: Pid,
        status: ExitStatus,
    ) -> bool {
        // ppid == 0 = orphan; cannot have a parker.
        if ppid == 0 {
            return false;
        }
        // Target-match check: any parker whose target is Any
        // matches every child; Specific(p) matches only p.
        let matches = match self.parked_waiters.get(&ppid) {
            Some(p) => match p.target {
                WaitTarget::Any => true,
                WaitTarget::Specific(target_pid) => target_pid == child_pid,
            },
            None => return false,
        };
        if !matches {
            return false;
        }

        // Reap inline. The reap removes the child from procs +
        // releases its cap set. Failure shouldn't happen — the
        // caller has just transitioned the child Zombie — but we
        // treat a reap error as a no-op wake (the child's state
        // is inconsistent, but the parent is better off parked
        // than woken with an incorrect status).
        let reap_ok = self.reap(child_pid).is_ok();
        if !reap_ok {
            return false;
        }

        // Dequeue the parker + build the wake response.
        let parker = self
            .parked_waiters
            .remove(&ppid)
            .expect("parked_waiters entry vanished between get and remove");
        let packed = crate::syscall::ext::pack_exit_status(status);
        let mut resp = abi::ring::Response::ok(parker.req_id, packed);
        let heap = if parker.heap_len >= 4 {
            resp.extra_len = 4;
            Some(PendingHeap {
                heap_ptr: parker.heap_ptr,
                bytes: (child_pid as u32).to_le_bytes().to_vec(),
            })
        } else {
            None
        };
        self.pending_wakes.push((ppid, resp, heap));

        // Best-effort state transition. If the parent isn't in
        // BlockedOnWait for some reason (race with another wake
        // path), ignore the transition error — the wake is still
        // queued and the user Worker will observe it.
        let _ = self.procs.transition(ppid, ProcState::Ready);
        self.procs.clear_block_reason(ppid);
        true
    }

    /// Interrupt any parked `proc_wait` on `pid` with `-EINTR`.
    /// Clears `parked_waiters[&pid]`, queues a wake with
    /// `Response::err(req_id, EINTR)` onto `pending_wakes`,
    /// transitions `pid` Ready + clears `block_reason`. No-op if
    /// `pid` is not parked on wait.
    ///
    /// Called from `handle_proc_kill`'s `Signal::Term` arm
    /// alongside the existing `interrupt_parked_accept` call. The
    /// two methods are idempotent against each other: at most one
    /// of them observes the parker slot (a pid parks on at most
    /// one primitive at a time in v1).
    ///
    /// Returns `true` iff a park was interrupted. Production
    /// callers discard the bool; tests use it.
    pub fn interrupt_parked_wait(&mut self, pid: Pid) -> bool {
        let Some(parker) = self.parked_waiters.remove(&pid) else {
            return false;
        };
        let wake_resp = abi::ring::Response::err(parker.req_id, abi::errno::EINTR);
        self.pending_wakes.push((pid, wake_resp, None));
        // Best-effort state transition. If the pid isn't in
        // BlockedOnWait for some reason (race with another wake
        // path), ignore the transition error — the wake is still
        // queued and the user Worker will observe it.
        let _ = self.procs.transition(pid, ProcState::Ready);
        self.procs.clear_block_reason(pid);
        true
    }

    /// Deliver `signal` to `target_pid`. The v1 kernel only
    /// actually terminates on `Kill`; other signals succeed
    /// syntactically (cap checks apply) but are otherwise
    /// buffered for a later slice that wires up the per-process
    /// signal inbox.
    ///
    /// Cap rules: the sender must either be the target's parent
    /// OR hold `Cap::ProcKillAny`. This mirrors the POSIX
    /// "you can signal your own processes freely" rule, modulo
    /// the capability-expressed privileged version.
    pub fn proc_kill(
        &mut self,
        sender_pid: Pid,
        target_pid: Pid,
        signal: Signal,
    ) -> Result<(), KernelError> {
        // Sender must exist.
        let sender_caps = self.caps.list(sender_pid)?;

        // Target must exist and must not already be reaped.
        let target = self
            .procs
            .get(target_pid)
            .ok_or(KernelError::NoSuchPid)?;
        let is_parent = target.ppid == sender_pid;
        let is_self = sender_pid == target_pid;
        if target.state == ProcState::Dead {
            return Err(KernelError::NoSuchPid);
        }

        // Cap check. Parents can signal their own children; any pid
        // can signal itself (POSIX `kill(getpid(), SIG)`); otherwise
        // the caller must hold `Cap::ProcKillAny`.
        if !is_parent && !is_self && !sender_caps.contains(Cap::ProcKillAny) {
            return Err(KernelError::NotCapable);
        }

        // Deliver. SIGKILL terminates synchronously; catchable
        // signals (Term, Interrupt, Pipe) are queued on the
        // target's per-process SignalInbox for `drain_signals`
        // to pick up. Coalescing is handled by
        // `SignalInbox::post`: repeated Terms against a target
        // that already has Term pending do not grow the queue.
        match signal {
            Signal::Kill => {
                // Remove from the scheduler immediately so no
                // pick_next can resurrect the pid after it's
                // been marked zombie.
                self.sched.remove(target_pid);
                // If the SIGKILL'd pid is parked on any listener,
                // clear the slot so the listener doesn't hold a
                // stale reference after the pid transitions Zombie.
                // Mirror of `proc_exit`'s equivalent sweep — the
                // SIGKILL path bypasses `proc_exit` entirely and
                // so needs its own call. No EINTR wake is queued
                // here (the pid is dead, not interrupted).
                self.ipc.clear_parked_acceptor_for_pid(target_pid);
                // Same surgical sweep for parked_waiters. A
                // SIGKILL'd parent parked on wait is dead, not
                // interrupted — no EINTR wake is queued (that's
                // `interrupt_parked_wait`'s job from the
                // catchable-signal arm). This just clears the
                // stale slot.
                self.parked_waiters.remove(&target_pid);
                // Same surgical sweep for parked_recvers. SIGKILL
                // takes the parker out without an EINTR wake — the
                // process is dead, no userland will observe the
                // wake response.
                self.parked_recvers.remove(&target_pid);
                let target_ppid = target.ppid;
                self.procs
                    .exit(target_pid, ExitStatus::Signaled(signal.number()))
                    .map_err(|_| KernelError::NoSuchPid)?;
                // POSIX: the parent observes the child's death
                // via SIGCHLD regardless of whether the child
                // called proc_exit voluntarily or was killed.
                self.post_sigchld(target_ppid);
                // SIGKILL'd target transitioned Zombie — if its
                // parent is parked on wait AND the parker's
                // target matches, reap + wake. Status is
                // Signaled(9) per the exit call above.
                self.wake_parked_waiter_for_child(
                    target_pid,
                    target_ppid,
                    ExitStatus::Signaled(signal.number()),
                );
            }
            Signal::Term
            | Signal::Interrupt
            | Signal::Pipe
            | Signal::Child
            | Signal::User1
            | Signal::User2 => {
                if let Some(inbox) = self.signal_inboxes.get_mut(&target_pid) {
                    inbox.post(signal);
                }
            }
        }
        Ok(())
    }

    /// POSIX `kill(pid, 0)` — the existence + permission probe.
    /// Runs every precondition `proc_kill` would run (sender
    /// exists, target exists, target not Dead, sender permitted
    /// to signal target via parent/self/ProcKillAny) but
    /// delivers no signal. Returns `Ok(())` if a real
    /// `proc_kill(sender, target, ...)` would succeed on this
    /// sender/target pair; otherwise the same `KernelError`
    /// variant `proc_kill` would produce.
    ///
    /// The dispatcher maps `PROC_KILL` with `signum == 0` onto
    /// this method — POSIX-style userland calling `kill(pid, 0)`
    /// to check "is this pid alive and could I signal it" gets
    /// a clean yes/no answer via the ext opcode.
    pub fn proc_check_signal(
        &self,
        sender_pid: Pid,
        target_pid: Pid,
    ) -> Result<(), KernelError> {
        let sender_caps = self.caps.list(sender_pid)?;
        let target = self
            .procs
            .get(target_pid)
            .ok_or(KernelError::NoSuchPid)?;
        if target.state == ProcState::Dead {
            return Err(KernelError::NoSuchPid);
        }
        let is_parent = target.ppid == sender_pid;
        let is_self = sender_pid == target_pid;
        if !is_parent && !is_self && !sender_caps.contains(Cap::ProcKillAny) {
            return Err(KernelError::NotCapable);
        }
        Ok(())
    }

    /// Return the capability set held by `target_pid`.
    ///
    /// Cap rules: querying one's own caps never requires any
    /// extra permission; querying another pid requires the sender
    /// to be the target's parent OR to hold
    /// [`Cap::ProcInspect`]. Otherwise
    /// [`KernelError::NotCapable`].
    ///
    /// Returns [`KernelError::NoSuchPid`] when `target_pid` does
    /// not exist (or has been reaped to `Dead`). The sender must
    /// also exist — `CapError::NoSuchPid` on the sender_caps
    /// fetch bubbles through `From<CapError>` to the same variant.
    pub fn proc_caps_get(
        &self,
        sender_pid: Pid,
        target_pid: Pid,
    ) -> Result<CapSet, KernelError> {
        let sender_caps = self.caps.list(sender_pid)?;
        if sender_pid == target_pid {
            return Ok(sender_caps);
        }
        let target = self
            .procs
            .get(target_pid)
            .ok_or(KernelError::NoSuchPid)?;
        if target.state == ProcState::Dead {
            return Err(KernelError::NoSuchPid);
        }
        let is_parent = target.ppid == sender_pid;
        if !is_parent && !sender_caps.contains(Cap::ProcInspect) {
            return Err(KernelError::NotCapable);
        }
        Ok(self.caps.list(target_pid)?)
    }

    /// Drain the pending signals queued on `pid`'s inbox.
    /// Returns them in delivery order; the inbox is empty after
    /// this call. Used by tests and (once T071/T072 land) by
    /// the WASI `signal_wait`-equivalent extension syscall.
    pub fn drain_signals(&mut self, pid: Pid) -> Result<Vec<Signal>, KernelError> {
        let inbox = self
            .signal_inboxes
            .get_mut(&pid)
            .ok_or(KernelError::NoSuchPid)?;
        Ok(inbox.drain())
    }

    /// Drain up to `max` pending signals from `pid`'s inbox.
    /// Returns them in delivery order; any signals past `max`
    /// stay queued in their original position and surface on the
    /// next drain. Used by the SignalChannel `fd_read` path
    /// (each signal serialises to 2 bytes, so `max = buf.len()
    /// / 2`). See [`SignalInbox::drain_bounded`].
    pub fn drain_signals_up_to(
        &mut self,
        pid: Pid,
        max: usize,
    ) -> Result<Vec<Signal>, KernelError> {
        let inbox = self
            .signal_inboxes
            .get_mut(&pid)
            .ok_or(KernelError::NoSuchPid)?;
        Ok(inbox.drain_bounded(max))
    }

    /// Peek at the inbox without draining. Diagnostic only.
    pub fn pending_signals(&self, pid: Pid) -> Result<usize, KernelError> {
        let inbox = self
            .signal_inboxes
            .get(&pid)
            .ok_or(KernelError::NoSuchPid)?;
        Ok(inbox.len())
    }

    // --- Mount management -----------------------------------------
    //
    // `mount`/`umount` are a thin privileged wrapper over the VFS
    // layer's `Vfs::mount` / `Vfs::umount`. The substrate
    // (mount-table + longest-prefix path resolver) already exists
    // in `crates/kernel/src/vfs/{mod,mount,path}.rs` (T056); this
    // surface adds (a) a `Cap::Mount` check, (b) v1's tmpfs-only
    // fstype factory, (c) the path-validity invariants
    // (`contracts/syscalls.md §3.5`: absolute, non-root, exists,
    // empty dir, not already a mount point), and (d) the umount
    // busy-fd guard (any open `FdObject::Vnode` whose `mount_id`
    // matches the target mount → -EBUSY, mirror of POSIX umount(2)).
    //
    // The fstype-factory is hard-coded to `"tmpfs"` in v1. Other
    // fstypes (`devfs`, `procfs`, `opfs`) are mounted at boot by
    // the kernel itself (see `make_kernel` in tests + the JS
    // bootstrap's kernel-init slice) and are explicitly NOT
    // userland-mountable in v1 — they own kernel-singleton state
    // (the `DeviceDispatcher`, the `ProcessTable`, the OPFS block
    // driver) that doesn't have a fresh-instance constructor on a
    // userland-arbitrary path. A future slice could grow the
    // factory into a registry once a real use case (e.g. multiple
    // tmpfs instances at different paths, or a fuse-style
    // filesystem) lands.

    /// `mount(target, fstype, flags)`: install a fresh filesystem
    /// of kind `fstype` at the absolute path `target`, OR — if
    /// `flags & MOUNT_REMOUNT` — atomically mutate an existing
    /// mount's flag bitset in place. v1 only accepts
    /// `fstype == "tmpfs"` for fresh mounts. The semantic primitive
    /// for fresh mounts lives on `Vfs::mount`; this method adds the
    /// privilege check and the path-validity invariants documented
    /// in `contracts/syscalls.md §3.5`.
    ///
    /// `MOUNT_REMOUNT` semantics (POSIX `MS_REMOUNT`-shaped):
    /// when this bit is set, `fstype` is IGNORED (POSIX preserves
    /// the original fstype on remount; the existing mount entry's
    /// filesystem trait object is the authoritative source). The
    /// `target` parameter MUST point to an existing mount; if not,
    /// `-EINVAL` ("the target is not a mount point"). The cap check
    /// uses the same `Cap::Mount` as the regular mount path — no
    /// new capability. The full path-validity gauntlet
    /// (existence/dir/empty) is SKIPPED on remount, because the
    /// target is already a live mount and the empty-dir invariant
    /// only applies at first-install time. Routing happens at the
    /// top of the method — REMOUNT is a separate code path that
    /// shares only the cap check + path normalisation. See
    /// [`Kernel::remount`].
    ///
    /// Errors:
    /// * [`KernelError::NotCapable`] — caller does not hold
    ///   [`Cap::Mount`].
    /// * [`KernelError::InvalidArgument`] — path is not absolute, is
    ///   the root `/` (fresh-mount path only — REMOUNT explicitly
    ///   permits `/` so init can re-flag the root filesystem),
    ///   fstype is anything other than `"tmpfs"` (fresh-mount path
    ///   only), OR the target directory is non-empty (a fresh mount
    ///   over a directory with existing entries would shadow them
    ///   irreversibly until umount; v1 rejects it outright). For
    ///   REMOUNT: `target` is not currently a mount point.
    /// * [`KernelError::Fs(FsError::NotFound)`] (→ ENOENT) — target
    ///   path doesn't resolve (fresh-mount path only).
    /// * [`KernelError::Fs(FsError::NotADirectory)`] (→ ENOTDIR) —
    ///   target exists but isn't a directory (fresh-mount path
    ///   only).
    /// * [`KernelError::Fs(FsError::AlreadyExists)`] (→ EBUSY at the
    ///   syscall layer) — target is already a mount point. The FS
    ///   variant pre-exists for `MountTable::insert`'s duplicate-path
    ///   guard; we re-purpose it because remounting on top of an
    ///   existing mount IS a "this thing is already there" condition.
    ///   The dispatcher maps `AlreadyExists` to EEXIST by default,
    ///   which is wrong for mount semantics — the syscall handler
    ///   in `ext.rs` translates it to EBUSY explicitly. (Fresh-mount
    ///   path only; REMOUNT WANTS the entry to exist.)
    pub fn mount(
        &mut self,
        pid: Pid,
        target: &str,
        fstype: &str,
        flags: u32,
    ) -> Result<(), KernelError> {
        if (flags & abi::ext::mount_flags::MOUNT_REMOUNT) != 0 {
            // POSIX MS_REMOUNT: source/fstype ignored. The cap
            // check + path validity live on Kernel::remount so the
            // remount path stays self-contained — no test
            // accidentally regresses by changing the order of
            // operations in the fresh-mount block below.
            return self.remount(pid, target, flags);
        }
        if !self.caps.check(pid, Cap::Mount)? {
            return Err(KernelError::NotCapable);
        }
        if !target.starts_with('/') {
            return Err(KernelError::InvalidArgument);
        }
        let normalised = crate::vfs::path::normalize(target);
        if normalised == "/" {
            return Err(KernelError::InvalidArgument);
        }
        // Already-a-mount detection runs FIRST: if `normalised`
        // already exists in the mount table, `Vfs::open(normalised)`
        // would route into the mounted fs's root (a freshly-empty
        // tmpfs) and the readdir-empty check below would silently
        // pass — the busy-mount EBUSY would then become a confusing
        // "you mounted a tmpfs over your existing tmpfs" success.
        // Walk mountpoints first to short-circuit with EBUSY.
        for (_id, mp) in self.vfs.mountpoints() {
            if mp == normalised {
                return Err(KernelError::Fs(FsError::AlreadyExists));
            }
        }
        // Confirm the target directory exists, is a directory, and
        // is empty BEFORE constructing the new filesystem instance.
        let (mount_id, ino, ty) = self.vfs.open(&normalised)?;
        if !ty.is_dir() {
            return Err(KernelError::Fs(FsError::NotADirectory));
        }
        let entries = self.vfs.readdir_ino(mount_id, ino)?;
        if !entries.is_empty() {
            return Err(KernelError::InvalidArgument);
        }
        // Fstype factory: v1 only knows "tmpfs". Other fstypes are
        // either kernel-singleton (devfs/procfs) or boot-only
        // (opfs); see the §3.5 method-doc above.
        let fs: alloc::boxed::Box<dyn crate::vfs::Filesystem> = match fstype {
            "tmpfs" => alloc::boxed::Box::new(crate::fs::tmpfs::TmpFs::new()),
            _ => return Err(KernelError::InvalidArgument),
        };
        let mount_id = self.vfs.mount(&normalised, fs).map_err(KernelError::from)?;
        // Persist the requested flag bitset on the new entry. The
        // initial insert defaults flags to 0 (see MountTable::insert);
        // a non-zero `flags` argument on a FRESH mount becomes the
        // entry's starting flag bitset, so a future REMOUNT clears
        // exactly those bits the caller installed here. The masking
        // strips MOUNT_REMOUNT itself — that bit is a control-flow
        // signal for THIS call, not a persisted property of the
        // resulting mount entry. (Without the mask a "create new
        // mount with REMOUNT bit speculatively set" call would
        // route to remount, fail with EINVAL, and never store
        // anything — but the mask is defence-in-depth in case a
        // future caller passes REMOUNT|other_bits.)
        let _ = self.vfs.set_mount_flags(
            &normalised,
            flags & !abi::ext::mount_flags::MOUNT_REMOUNT,
        );
        let _ = mount_id;
        Ok(())
    }

    /// `remount(target, flags)`: atomically change an existing
    /// mount's flag bitset in place. POSIX `MS_REMOUNT` semantics —
    /// no umount/remount, no fresh filesystem instance, no change
    /// to the mount-id, mountpoint, or backing fstype. The fstype
    /// the caller passed to the original `mount()` is ignored on
    /// remount (the existing in-table entry is authoritative).
    ///
    /// Routed to from [`Kernel::mount`] when the call's `flags`
    /// argument has the `MOUNT_REMOUNT` bit set; not normally
    /// invoked directly by the syscall handler. Public so semantic
    /// tests in `tests/sys.rs` (or future direct-callers like an
    /// init-binary helper) can exercise the path without round-
    /// tripping through `Kernel::mount`'s flags-routing branch.
    ///
    /// The persisted flag value strips `MOUNT_REMOUNT` itself —
    /// that bit is a per-call control-flow signal, not a property
    /// of the resulting mount entry. So calling
    /// `remount(/a, MOUNT_REMOUNT)` clears all OTHER flags on /a,
    /// which mirrors POSIX behaviour where the new flag set
    /// replaces (not OR-merges with) the existing one.
    ///
    /// Errors:
    /// * [`KernelError::NotCapable`] — caller does not hold
    ///   [`Cap::Mount`]. Same cap as a fresh mount.
    /// * [`KernelError::InvalidArgument`] — path is not absolute OR
    ///   the path is not currently a mount point. POSIX says
    ///   "EINVAL — the target is not a mount point" for this case.
    pub fn remount(
        &mut self,
        pid: Pid,
        target: &str,
        flags: u32,
    ) -> Result<(), KernelError> {
        if !self.caps.check(pid, Cap::Mount)? {
            return Err(KernelError::NotCapable);
        }
        if !target.starts_with('/') {
            return Err(KernelError::InvalidArgument);
        }
        let normalised = crate::vfs::path::normalize(target);
        // Persisted flags exclude the REMOUNT control bit — see the
        // doc-comment above for why. set_mount_flags returns
        // FsError::NotFound when `normalised` isn't in the mount
        // table; the §3.5 wire surface promises EINVAL for that
        // case (POSIX "target is not a mount point"), so we map
        // NotFound → InvalidArgument here rather than letting the
        // dispatcher render it as ENOENT.
        let persisted = flags & !abi::ext::mount_flags::MOUNT_REMOUNT;
        match self.vfs.set_mount_flags(&normalised, persisted) {
            Ok(_id) => Ok(()),
            Err(FsError::NotFound) => Err(KernelError::InvalidArgument),
            Err(e) => Err(KernelError::Fs(e)),
        }
    }

    /// `umount(target)`: remove the mount installed at `target`. The
    /// semantic primitive lives on `Vfs::umount`; this method adds
    /// the privilege check + path-validity invariants + the busy-fd
    /// guard.
    ///
    /// Errors:
    /// * [`KernelError::NotCapable`] — caller does not hold
    ///   [`Cap::Mount`].
    /// * [`KernelError::InvalidArgument`] — path is not absolute, OR
    ///   the path is not currently a mount point.
    /// * [`KernelError::WouldBlock`] (→ EBUSY at the syscall layer) —
    ///   any process holds an open `FdObject::Vnode` whose
    ///   `mount_id` matches the target mount. The dispatcher maps
    ///   `WouldBlock` to EAGAIN by default; the syscall handler in
    ///   `ext.rs` translates it to EBUSY for the umount semantic.
    pub fn umount(&mut self, pid: Pid, target: &str) -> Result<(), KernelError> {
        if !self.caps.check(pid, Cap::Mount)? {
            return Err(KernelError::NotCapable);
        }
        if !target.starts_with('/') {
            return Err(KernelError::InvalidArgument);
        }
        let normalised = crate::vfs::path::normalize(target);
        // Locate the mount id whose mountpoint EXACTLY matches; a
        // longest-prefix lookup would also match descendant paths
        // ("/dev/null" with /dev mounted would resolve to /dev),
        // which is wrong for umount.
        let mount_id = self
            .vfs
            .mountpoints()
            .into_iter()
            .find(|(_id, mp)| mp == &normalised)
            .map(|(id, _)| id)
            .ok_or(KernelError::InvalidArgument)?;
        // Busy-fd guard: any open Vnode fd rooted at this mount
        // pins it. We could also catch open SignalChannel / Socket
        // / CharDevice / Pipe fds — none of those carry a mount
        // id, so they don't pin a mount, and POSIX umount(2)
        // doesn't refuse on non-Vnode handles either.
        for table in self.fds.values() {
            for (_fd, entry) in table.iter() {
                if let FdObject::Vnode { mount_id: m, .. } = entry.object {
                    if m == mount_id {
                        return Err(KernelError::WouldBlock);
                    }
                }
            }
        }
        self.vfs.umount(&normalised).map(|_| ()).map_err(KernelError::from)
    }

    /// `fs_watch(pid, abs_path, mask) -> watch_fd`. Resolves
    /// `abs_path` through the VFS, registers a fresh watch on the
    /// resulting `(mount_id, ino)` pair, allocates an
    /// [`FdObject::Watch`] in `pid`'s fd table, returns the new
    /// fd number. The caller (the FS_WATCH opcode handler) is
    /// responsible for validating `mask` against
    /// [`abi::ext::WATCH_MASK_ALL`] BEFORE calling — a zero mask
    /// or a mask with unknown bits is the handler's atomic-reject
    /// concern, not this method's.
    ///
    /// Errors:
    /// * [`KernelError::NoSuchPid`] — unknown pid.
    /// * [`KernelError::Fs`]`(FsError::NotFound)` — `abs_path`
    ///   doesn't resolve. Surfaces as `-ENOENT` at the wire layer.
    /// * [`KernelError::OutOfFds`] — caller's fd table is full.
    ///   Surfaces as `-EMFILE`. The watch is rolled back from the
    ///   VFS registry on this path so a failed install doesn't
    ///   leak a watch slot the caller has no fd for.
    pub fn fs_watch(
        &mut self,
        pid: Pid,
        abs_path: &str,
        mask: u32,
    ) -> Result<u32, KernelError> {
        let watch_id = self.vfs.register_watch(abs_path, mask)?;
        let table = match self.fds.get_mut(&pid) {
            Some(t) => t,
            None => {
                let _ = self.vfs.unregister_watch(watch_id);
                return Err(KernelError::NoSuchPid);
            }
        };
        match table.alloc(FdEntry::new(FdObject::Watch { watch_id })) {
            Ok(fd) => Ok(fd),
            Err(e) => {
                // Roll back the watch registration so a fd-limit
                // failure doesn't leak a registry slot.
                let _ = self.vfs.unregister_watch(watch_id);
                Err(e.into())
            }
        }
    }

    /// Register a host-imported file under `token`. Called by the
    /// (future) TS-side bootstrap path when a user drops a file on
    /// the browser tab or picks one via the file-manager `Import…`
    /// menu. Inserts the payload into [`Self::host_files`] keyed by
    /// `token` so a subsequent [`Self::host_file_recv`] can consume
    /// it.
    ///
    /// Token-collision policy: a second `host_file_dropped` with the
    /// same `token` overwrites the prior entry. The bootstrap is the
    /// only producer of tokens, and the spec leaves collision policy
    /// to the producer. In practice the bootstrap mints monotonically
    /// increasing tokens so collisions never happen — but the kernel
    /// stays defensive (silently drop the prior bytes rather than
    /// panic) so a buggy bootstrap can't crash the kernel.
    ///
    /// No capability gate: the bootstrap is the trust root for host-
    /// file imports. If a malicious userland program ever gets the
    /// ability to call this method directly (it doesn't today — only
    /// the kernel-side bootstrap notification path will), the cap
    /// check would belong on the entry to that path, not here.
    pub fn host_file_dropped(&mut self, token: u32, file: HostFile) {
        self.host_files.insert(token, file);
    }

    /// Snapshot of all currently-pending host-file tokens. Test-
    /// helper accessor; production code routes through
    /// [`Self::host_file_recv`] which consumes a single token.
    pub fn host_file_pending_tokens(&self) -> alloc::vec::Vec<u32> {
        self.host_files.keys().copied().collect()
    }

    /// True iff `token` has an active fd-state entry — i.e. some
    /// process has called `host_file_recv(token)` and has not yet
    /// closed the resulting fd. Test-helper accessor mirroring
    /// [`Self::host_file_pending_tokens`]; production code never
    /// needs this since the fd-close arm of `release_object` is
    /// the only consumer of the entry.
    pub fn host_file_has_active_fd(&self, token: u32) -> bool {
        self.host_file_fds.contains_key(&token)
    }

    /// `host_file_recv(pid, token) -> fd`. Per
    /// `contracts/syscalls.md §3.6`: consumes the pending host-file
    /// payload registered under `token` (by a prior
    /// [`Self::host_file_dropped`] call) and installs an
    /// [`FdObject::HostFile`] in `pid`'s fd table. The new fd is
    /// readable via `fd_read` (streams the bytes) and closes via
    /// `fd_close` (drops the kernel-side bytes; the spec spells this
    /// "Closing the fd releases the browser-side `File` reference").
    ///
    /// Errors:
    /// * [`KernelError::NoSuchPid`] — unknown pid.
    /// * [`KernelError::BadFd`] — `token` does not match any pending
    ///   host-file entry. Surfaces as `-EBADF` at the wire layer.
    ///   The spec uses EBADF for two distinct conditions: an unknown
    ///   token (never produced by the bootstrap, or already consumed
    ///   by an earlier recv) AND a second recv attempt against an
    ///   already-consumed token. Both look identical from the kernel
    ///   side: the table doesn't have the token, so EBADF wins.
    /// * [`KernelError::OutOfFds`] — caller's fd table is full.
    ///   Surfaces as `-EMFILE`. The host-file payload is rolled back
    ///   into the pending table so a fd-limit failure doesn't burn
    ///   the token (the userland caller can free up an fd slot and
    ///   retry).
    ///
    /// Atomic-reject ordering: caller-pid-exists is checked BEFORE
    /// the token lookup so a recv-from-unknown-pid leaves the
    /// pending table untouched. Token lookup runs BEFORE the fd
    /// alloc; if the alloc fails, the kernel re-inserts the
    /// HostFile under the same token so userland's retry observes
    /// the same EBADF-vs-EMFILE distinction the first call would
    /// have produced if the fd table had been one slot deeper.
    ///
    /// The spec mentions an `ENOENT` arm for "a token that
    /// references an expired `File`". v1 doesn't model File
    /// lifecycle separately from token lifecycle (a token is "live"
    /// while it's in `host_files` and "consumed" once recv has
    /// removed it); a future tab-close cleanup hook would clear the
    /// table en masse, mapping to EBADF for any straggler recv. The
    /// ENOENT path becomes meaningful when the bootstrap can mark a
    /// token "expired but still nominally present" (e.g. browser
    /// freed the underlying File but the token table hasn't caught
    /// up); that's a future slice.
    pub fn host_file_recv(&mut self, pid: Pid, token: u32) -> Result<u32, KernelError> {
        // Pid existence check first — a recv from an unknown pid is
        // an ESRCH-shaped condition (KernelError::NoSuchPid), not a
        // BadFd. Probing the pid via `fds()` short-circuits before
        // the token lookup so an unknown-pid attack can't enumerate
        // pending tokens by observing a different errno.
        self.fds(pid)?;
        let Some(file) = self.host_files.remove(&token) else {
            return Err(KernelError::BadFd);
        };
        let table = self
            .fds
            .get_mut(&pid)
            .ok_or(KernelError::NoSuchPid)?;
        match table.alloc(FdEntry::new(FdObject::HostFile { token })) {
            Ok(fd) => {
                self.host_file_fds
                    .insert(token, HostFileFd::new(file));
                Ok(fd)
            }
            Err(e) => {
                // Roll back: re-insert the host file so a retry
                // observes the same token. Keeps the fd-limit
                // failure non-destructive — a userland that can
                // close an unrelated fd and retry succeeds, instead
                // of losing the imported file outright.
                self.host_files.insert(token, file);
                Err(e.into())
            }
        }
    }

    /// VFS-mutation wrapper: create a regular file at `abs_path`
    /// with `mode`, then notify any watchers on the parent
    /// directory's inode with `WATCH_CREATE` carrying the new
    /// child's inode.
    ///
    /// The wasi `path_open(O_CREAT)` and `path_unlink_file`
    /// handlers call this (and its siblings) instead of
    /// `Vfs::create` directly so the notify hook runs in lockstep
    /// with the underlying mutation. A failed mutation does NOT
    /// notify — there is no event to report.
    pub fn vfs_create(&mut self, abs_path: &str, mode: u32) -> Result<Ino, FsError> {
        let new_ino = self.vfs.create(abs_path, mode)?;
        // Resolve parent AFTER the successful create. A
        // mutation-time resolve_parent failure (which would happen
        // if the path were truly malformed) is impossible here:
        // create just succeeded so the parent resolves. Skip the
        // notify silently if it somehow doesn't — a failed parent
        // lookup post-create is a kernel invariant violation, not a
        // user-facing error.
        if let Ok((mount_id, parent_ino, _)) = self.vfs.resolve_parent(abs_path) {
            self.vfs.notify(
                mount_id,
                parent_ino,
                WatchEvent { mask: abi::ext::WATCH_CREATE, inode: new_ino as u32 },
            );
        }
        Ok(new_ino)
    }

    /// VFS-mutation wrapper: create a directory at `abs_path` with
    /// `mode`, then notify the parent's watchers with
    /// `WATCH_CREATE` and the new directory's inode.
    pub fn vfs_mkdir(&mut self, abs_path: &str, mode: u32) -> Result<Ino, FsError> {
        let new_ino = self.vfs.mkdir(abs_path, mode)?;
        if let Ok((mount_id, parent_ino, _)) = self.vfs.resolve_parent(abs_path) {
            self.vfs.notify(
                mount_id,
                parent_ino,
                WatchEvent { mask: abi::ext::WATCH_CREATE, inode: new_ino as u32 },
            );
        }
        Ok(new_ino)
    }

    /// VFS-mutation wrapper: unlink the regular file at `abs_path`,
    /// then notify the parent's watchers with `WATCH_DELETE`
    /// carrying the just-removed child's inode.
    ///
    /// Pre-resolves the child's inode BEFORE the unlink only when
    /// at least one watcher is interested in this filesystem (a
    /// fast `WatchTable::is_empty` probe) — otherwise the resolve
    /// is wasted work AND the resolve-failure errno (NotFound on a
    /// path that doesn't exist) would mask the real underlying
    /// errno (e.g. devfs's ReadOnly, surfaced as EROFS). The pre-
    /// resolve guard preserves the wasi handler's error contract
    /// while still capturing the inode for the notify path when a
    /// watcher actually exists.
    pub fn vfs_unlink(&mut self, abs_path: &str) -> Result<(), FsError> {
        let pre_inode = if !self.vfs.watches().is_empty() {
            self.vfs.resolve(abs_path).ok()
        } else {
            None
        };
        self.vfs.unlink(abs_path)?;
        if let (Some((mount_id, child_ino)), Ok((_, parent_ino, _))) =
            (pre_inode, self.vfs.resolve_parent(abs_path))
        {
            self.vfs.notify(
                mount_id,
                parent_ino,
                WatchEvent { mask: abi::ext::WATCH_DELETE, inode: child_ino as u32 },
            );
        }
        Ok(())
    }

    /// VFS-mutation wrapper: remove the empty directory at
    /// `abs_path`, then notify the parent's watchers with
    /// `WATCH_DELETE` carrying the just-removed child's inode.
    /// Same pattern as [`Self::vfs_unlink`].
    pub fn vfs_rmdir(&mut self, abs_path: &str) -> Result<(), FsError> {
        let pre_inode = if !self.vfs.watches().is_empty() {
            self.vfs.resolve(abs_path).ok()
        } else {
            None
        };
        self.vfs.rmdir(abs_path)?;
        if let (Some((mount_id, child_ino)), Ok((_, parent_ino, _))) =
            (pre_inode, self.vfs.resolve_parent(abs_path))
        {
            self.vfs.notify(
                mount_id,
                parent_ino,
                WatchEvent { mask: abi::ext::WATCH_DELETE, inode: child_ino as u32 },
            );
        }
        Ok(())
    }

    /// Notify any watchers on `(mount_id, ino)` that a write just
    /// landed on that inode with `WATCH_MODIFY`. Called from
    /// [`Self::fd_write`]'s `Vnode` arm AFTER the underlying
    /// `Vfs::write_ino` succeeds. A failed write does not notify.
    /// Public so tests can assert the hook fires on the expected
    /// inode without piping every mutation through a wrapper
    /// method.
    pub fn notify_modify(&mut self, mount_id: MountId, ino: Ino) {
        self.vfs.notify(
            mount_id,
            ino,
            WatchEvent { mask: abi::ext::WATCH_MODIFY, inode: ino as u32 },
        );
    }

    /// Borrow a watch by id. Used by tests + the FS_WATCH opcode
    /// handler's introspection paths to confirm a register/unregister
    /// round-trip.
    pub fn watch_get(&self, id: WatchId) -> Option<&crate::vfs::Watch> {
        self.vfs.watches().watches.get(&id)
    }

    /// Bump the kernel-side refcount on a pipe-ended fd object.
    /// Called when we're about to install the same object into a
    /// second fd (proc_spawn inheritance, `dup` inside a
    /// process, etc.). Non-pipe objects don't need this — they
    /// are either identifier pairs (`Vnode`) or resources with
    /// different sharing semantics (sockets, display conns).
    fn inherit_object(&mut self, object: FdObject) {
        match object {
            FdObject::PipeRead(id) => {
                if let Ok(pipe) = self.ipc.pipe_mut(PipeId(id)) {
                    pipe.dup_reader();
                }
            }
            FdObject::PipeWrite(id) => {
                if let Ok(pipe) = self.ipc.pipe_mut(PipeId(id)) {
                    pipe.dup_writer();
                }
            }
            FdObject::Vnode { .. }
            | FdObject::CharDevice(_)
            | FdObject::Socket(_)
            | FdObject::DisplayConn(_)
            | FdObject::SignalChannel
            | FdObject::Watch { .. }
            | FdObject::HostFile { .. } => {}
        }
    }
}

impl Default for Kernel {
    fn default() -> Self {
        Kernel::new()
    }
}
