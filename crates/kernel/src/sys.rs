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
use crate::ipc::{IpcError, IpcTable, PipeId, SocketId, SocketType};
use crate::proc::{
    table::{InsertError, ZombieTarget},
    ExitStatus, ProcState, Process, ProcessTable, Scheduler, SignalInbox,
};
pub use crate::proc::Signal;
use crate::vfs::{FsError, Ino, MountId, NodeType, Vfs};

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
                    self.vfs.create(path, effective_mode)?;
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
                    Ok(n) => match self
                        .fds
                        .get_mut(&pid)
                        .and_then(|t| t.get_mut(fd))
                        .ok_or(KernelError::BadFd)
                    {
                        Ok(slot) => {
                            slot.offset = slot.offset.saturating_add(n as u64);
                            Ok(n)
                        }
                        Err(e) => Err(e),
                    },
                    Err(e) => Err(KernelError::Fs(e)),
                }
            }
            FdObject::CharDevice(devnum) => {
                self.devs.write(devnum, buf).map_err(KernelError::from)
            }
            FdObject::Socket(id) => self
                .ipc
                .send_on_socket(SocketId(id), buf, Vec::new())
                .map_err(KernelError::from),
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
            | FdObject::SignalChannel => Err(KernelError::NotSupportedOnFd),
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
            FdObject::Vnode { .. }
            | FdObject::CharDevice(_)
            | FdObject::DisplayConn(_)
            | FdObject::SignalChannel => {}
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
            Signal::Term | Signal::Interrupt | Signal::Pipe | Signal::Child => {
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
            | FdObject::SignalChannel => {}
        }
    }
}

impl Default for Kernel {
    fn default() -> Self {
        Kernel::new()
    }
}
