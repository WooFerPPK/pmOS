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
        self.ipc.connect_socket(socket_id, DISPLAY_SOCKET_PATH)?;
        let table = self.fds.get_mut(&pid).ok_or(KernelError::NoSuchPid)?;
        let fd = table.alloc(FdEntry::new(FdObject::Socket(socket_id.0)))?;
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
        self.ipc.connect_socket(socket_id, path)?;
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
                let _ = self.ipc.close_socket(crate::ipc::SocketId(id));
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
        self.release_fd_table_resources(pid);
        self.procs
            .exit(pid, status)
            .map_err(|_| KernelError::NoSuchPid)
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
                self.procs
                    .exit(target_pid, ExitStatus::Signaled(signal.number()))
                    .map_err(|_| KernelError::NoSuchPid)?;
            }
            Signal::Term | Signal::Interrupt | Signal::Pipe => {
                if let Some(inbox) = self.signal_inboxes.get_mut(&target_pid) {
                    inbox.post(signal);
                }
            }
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
