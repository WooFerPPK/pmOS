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
use crate::ipc::{IpcTable, PipeId};
use crate::proc::{
    table::{InsertError, ZombieTarget},
    ExitStatus, ProcState, Process, ProcessTable, Scheduler, SignalInbox,
};
pub use crate::proc::Signal;
use crate::vfs::{FsError, NodeType, Vfs};

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

    /// Reap a process's kernel-side state once it has exited. This
    /// is the single place where per-process resources are
    /// released; it drains the fd table first so every
    /// object-side resource (pipe reader, socket, display conn)
    /// is freed before the process table entry goes away.
    ///
    /// Returns the exit status recorded on the zombie, or
    /// `NoSuchPid` if the pid is unknown or not a zombie.
    pub fn reap(&mut self, pid: Pid) -> Result<ExitStatus, KernelError> {
        let Some(mut table) = self.fds.remove(&pid) else {
            return Err(KernelError::NoSuchPid);
        };
        let dropped = table.drain_all();
        for (_fd, entry) in dropped {
            self.release_object(entry.object);
        }
        self.caps.remove(pid);
        self.signal_inboxes.remove(&pid);
        self.procs.reap(pid).ok_or(KernelError::NoSuchPid)
    }

    // --- path_open / fd_read / fd_write / fd_close ----------------

    /// `path_open(pid, path, flags) -> fd`.
    ///
    /// Resolves `path` through the VFS, checks per-device
    /// capabilities if the target is a character device, installs
    /// a matching [`FdObject`] in the calling process's fd table,
    /// and returns the fresh fd number.
    pub fn path_open(
        &mut self,
        pid: Pid,
        path: &str,
        flags: FdFlags,
    ) -> Result<u32, KernelError> {
        let caps = self.caps.list(pid)?;
        let (mount_id, ino, ty) = self.vfs.open(path)?;
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
            FdObject::PipeRead(_) | FdObject::Socket(_) => Err(KernelError::NotSupportedOnFd),
            FdObject::PipeWrite(_)
            | FdObject::DisplayConn(_)
            | FdObject::SignalChannel => Err(KernelError::NotSupportedOnFd),
        }
    }

    /// `fd_write(pid, fd, buf) -> bytes_written`.
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
        match entry.object {
            FdObject::Vnode { mount_id, ino } => {
                let n = self.vfs.write_ino(mount_id, ino, entry.offset, buf)?;
                let slot = self
                    .fds
                    .get_mut(&pid)
                    .and_then(|t| t.get_mut(fd))
                    .ok_or(KernelError::BadFd)?;
                slot.offset = slot.offset.saturating_add(n as u64);
                Ok(n)
            }
            FdObject::CharDevice(devnum) => Ok(self.devs.write(devnum, buf)?),
            FdObject::PipeWrite(_) | FdObject::Socket(_) => Err(KernelError::NotSupportedOnFd),
            FdObject::PipeRead(_)
            | FdObject::DisplayConn(_)
            | FdObject::SignalChannel => Err(KernelError::NotSupportedOnFd),
        }
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
    /// move it to `Zombie`. Does NOT reap — that's a separate
    /// call made by the parent's `proc_wait`.
    pub fn proc_exit(&mut self, pid: Pid, status: ExitStatus) -> Result<(), KernelError> {
        self.sched.remove(pid);
        self.procs
            .exit(pid, status)
            .map_err(|_| KernelError::NoSuchPid)
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
        if target.state == ProcState::Dead {
            return Err(KernelError::NoSuchPid);
        }

        // Cap check.
        if !is_parent && !sender_caps.contains(Cap::ProcKillAny) {
            return Err(KernelError::NotCapable);
        }

        // Deliver. SIGKILL terminates synchronously; catchable
        // signals (Term, Interrupt) are queued on the target's
        // per-process SignalInbox for `drain_signals` to pick
        // up. Coalescing is handled by `SignalInbox::post`:
        // repeated Terms against a target that already has Term
        // pending do not grow the queue.
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
            Signal::Term | Signal::Interrupt => {
                if let Some(inbox) = self.signal_inboxes.get_mut(&target_pid) {
                    inbox.post(signal);
                }
            }
        }
        Ok(())
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
