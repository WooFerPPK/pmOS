//! Per-process file descriptor table.
//!
//! Mirrors `data-model.md §2`. Each `Process` owns exactly one
//! `FdTable`. The kernel's syscall dispatch takes a mutable
//! borrow of the calling process's table when servicing any
//! `fd_*` or `ipc_*` syscall.
//!
//! Invariants this module enforces:
//!
//! * **fd 0/1/2 are reserved** for stdin/stdout/stderr. They are
//!   valid fd numbers and may refer to any `FdObject`; the
//!   process-spawn path (T074) populates them from the spawn
//!   manifest. They are otherwise unremarkable — close()ing them
//!   frees the slot like any other fd.
//! * **Lowest-free allocation**: `alloc()` returns the smallest
//!   unused fd number, POSIX-style. This is what userland code
//!   compiled for other POSIX-ish systems expects.
//! * **O_CLOEXEC semantics**: entries with `FdFlags::CLOEXEC` are
//!   dropped from child fd tables by `proc_spawn` (T074).
//! * **dup / renumber / inheritance**: implemented via the same
//!   public method the spawn path uses, `install_at`.

use alloc::vec::Vec;

use crate::vfs::{Ino, MountId, WatchId};

/// Maximum fd number a process may hold in v1. A process that
/// tries to allocate past this returns `EMFILE`.
pub const FD_SOFT_LIMIT: usize = 1024;

/// Per-fd flag bits (O_CLOEXEC et al). Matches
/// `abi::wasi::fdflags` semantics; we redefine here to keep the
/// kernel decoupled from the WASI numeric encoding.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct FdFlags(u32);

impl FdFlags {
    pub const EMPTY:    FdFlags = FdFlags(0);
    pub const CLOEXEC:  FdFlags = FdFlags(0x0001);
    pub const NONBLOCK: FdFlags = FdFlags(0x0002);
    pub const APPEND:   FdFlags = FdFlags(0x0004);

    #[inline]
    pub const fn contains(self, other: FdFlags) -> bool {
        self.0 & other.0 == other.0
    }

    #[inline]
    pub fn insert(&mut self, other: FdFlags) {
        self.0 |= other.0;
    }

    #[inline]
    pub fn remove(&mut self, other: FdFlags) {
        self.0 &= !other.0;
    }

    #[inline]
    pub const fn bits(self) -> u32 {
        self.0
    }

    #[inline]
    pub const fn from_bits(bits: u32) -> FdFlags {
        FdFlags(bits)
    }

    /// Translate a WASI `fdflags` u32 (from `abi::wasi::fdflags`,
    /// APPEND=0x01, DSYNC=0x02, NONBLOCK=0x04, RSYNC=0x08,
    /// SYNC=0x10) into PMos internal FdFlags bits. Used by every
    /// WASI opcode that accepts fdflags from userland — `path_open`
    /// and `fd_fdstat_set_flags` today.
    ///
    /// v1 recognises only APPEND + NONBLOCK meaningfully;
    /// DSYNC / RSYNC / SYNC are accepted and discarded because
    /// tmpfs writes are already synchronous into in-memory state,
    /// so there's nothing to flush. CLOEXEC is NOT represented in
    /// the WASI fdflags encoding — the F_SETFD-level flag is only
    /// set via proc_spawn inheritance policy, not via path_open or
    /// fd_fdstat_set_flags — so this helper never sets it.
    #[inline]
    pub const fn from_wasi_bits(wasi_bits: u32) -> FdFlags {
        let mut bits: u32 = 0;
        if wasi_bits & (abi::wasi::fdflags::APPEND as u32) != 0 {
            bits |= FdFlags::APPEND.0;
        }
        if wasi_bits & (abi::wasi::fdflags::NONBLOCK as u32) != 0 {
            bits |= FdFlags::NONBLOCK.0;
        }
        FdFlags(bits)
    }
}

/// Logical types of objects an fd can reference.
///
/// Each variant carries an opaque identifier that points at state
/// owned by another kernel subsystem:
///
/// * `Vnode` — `(mount_id, ino)` pair naming a regular-file /
///   directory / symlink / fifo / socket node in the VFS. The
///   kernel routes `fd_read`/`fd_write` on this variant through
///   the owning filesystem.
/// * `CharDevice` — devnum into `DeviceDispatcher` (T067..T070).
///   Used for any fd that open()ed a `NodeType::CharDevice` node
///   such as `/dev/null`, `/dev/console`, or `/dev/fb0`.
/// * `PipeRead` / `PipeWrite` — pipe id from the IPC subsystem (T062).
/// * `Socket` — socket id from the IPC subsystem (T063).
/// * `DisplayConn` — connection id returned by `display_connect`
///   (T072 / T100).
/// * `SignalChannel` — the per-process signal inbox IPC fd that
///   every process inherits at spawn.
/// * `Watch` — a filesystem-watch fd returned by the `fs_watch`
///   extension opcode. `fd_read` drains queued [`WatchEvent`]s as
///   8-byte fixed-size records; `fd_close` unregisters the watch
///   from the VFS notifier.
///
/// The kernel's syscall dispatcher pattern-matches on this enum
/// to route `fd_read`/`fd_write`/`ipc_send`/etc. to the right
/// subsystem.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FdObject {
    Vnode { mount_id: MountId, ino: Ino },
    CharDevice(u32),
    PipeRead(u32),
    PipeWrite(u32),
    Socket(u32),
    DisplayConn(u32),
    SignalChannel,
    Watch { watch_id: WatchId },
}

/// A single fd-table entry.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FdEntry {
    pub object: FdObject,
    pub flags: FdFlags,
    pub offset: u64,
}

impl FdEntry {
    pub const fn new(object: FdObject) -> Self {
        FdEntry {
            object,
            flags: FdFlags::EMPTY,
            offset: 0,
        }
    }

    pub const fn with_flags(object: FdObject, flags: FdFlags) -> Self {
        FdEntry {
            object,
            flags,
            offset: 0,
        }
    }
}

/// Per-process fd table.
///
/// Sparse: `entries[fd]` is `Some` iff fd is open. Growing is
/// cheap because `Vec` only allocates when needed. Closing an fd
/// sets the slot to `None` without shrinking the vector.
pub struct FdTable {
    entries: Vec<Option<FdEntry>>,
    soft_limit: usize,
}

impl FdTable {
    /// Empty table with the default soft limit.
    pub fn new() -> Self {
        FdTable {
            entries: Vec::new(),
            soft_limit: FD_SOFT_LIMIT,
        }
    }

    /// Empty table with a custom soft limit. Used by tests.
    pub fn with_limit(soft_limit: usize) -> Self {
        FdTable {
            entries: Vec::new(),
            soft_limit,
        }
    }

    /// Number of currently-open fds.
    pub fn open_count(&self) -> usize {
        self.entries.iter().filter(|e| e.is_some()).count()
    }

    /// Is this fd number currently open?
    pub fn is_open(&self, fd: u32) -> bool {
        self.get(fd).is_some()
    }

    /// Borrow the entry at `fd`, if any.
    pub fn get(&self, fd: u32) -> Option<&FdEntry> {
        self.entries.get(fd as usize).and_then(|e| e.as_ref())
    }

    /// Mutably borrow the entry at `fd`, if any.
    pub fn get_mut(&mut self, fd: u32) -> Option<&mut FdEntry> {
        self.entries.get_mut(fd as usize).and_then(|e| e.as_mut())
    }

    /// Allocate the lowest free fd number and install `entry` there.
    /// Returns `Err(FdError::OutOfFds)` on soft-limit exhaustion.
    pub fn alloc(&mut self, entry: FdEntry) -> Result<u32, FdError> {
        // Linear scan for the lowest free slot. The soft-limit
        // (~1024) caps this at a small number; trees of free
        // slots are overkill.
        for (i, slot) in self.entries.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(entry);
                return Ok(i as u32);
            }
        }
        if self.entries.len() >= self.soft_limit {
            return Err(FdError::OutOfFds);
        }
        let fd = self.entries.len() as u32;
        self.entries.push(Some(entry));
        Ok(fd)
    }

    /// Install `entry` at a specific fd number, replacing any
    /// existing entry (which is closed in the process). Used by
    /// `proc_spawn` to populate stdin/stdout/stderr from the
    /// spawn manifest and by `fd_renumber` (WASI's dup2 spelling).
    /// Returns the closed entry, if there was one, so the caller
    /// can release any object-side resources.
    pub fn install_at(&mut self, fd: u32, entry: FdEntry) -> Result<Option<FdEntry>, FdError> {
        let idx = fd as usize;
        if idx >= self.soft_limit {
            return Err(FdError::OutOfFds);
        }
        while self.entries.len() <= idx {
            self.entries.push(None);
        }
        let closed = self.entries[idx].take();
        self.entries[idx] = Some(entry);
        Ok(closed)
    }

    /// Close an fd. Returns the removed entry so the caller can
    /// release any object-side resources (pipe endpoint, socket,
    /// display connection). `Err(EBADF)` if the fd is not open.
    pub fn close(&mut self, fd: u32) -> Result<FdEntry, FdError> {
        let idx = fd as usize;
        let slot = self.entries.get_mut(idx).ok_or(FdError::BadFd)?;
        slot.take().ok_or(FdError::BadFd)
    }

    /// Atomically renumber `from` to `to` (WASI's dup2-spelling).
    ///
    /// * `from == to` and `from` is open: no-op, returns `Ok(None)`.
    ///   The fd stays open with its entry unchanged.
    /// * `from == to` and `from` is NOT open: returns
    ///   `Err(FdError::BadFd)`. (POSIX's dup2(bad, bad) = EBADF.)
    /// * `from != to` and `from` is open: `from` is closed, `to`
    ///   is replaced with `from`'s entry. Returns the entry that
    ///   was at `to` (if any) so the caller can release any object-
    ///   side resources it held (pipe / socket refs, etc.).
    /// * `from != to` and `from` is NOT open: returns
    ///   `Err(FdError::BadFd)`. `to` is left unchanged.
    ///
    /// The move preserves `offset` and `flags` on the `from` entry
    /// verbatim — no transformations, no CLOEXEC clearing. Userland
    /// asked for this specific number; it gets exactly what `from`
    /// had.
    pub fn renumber(
        &mut self,
        from: u32,
        to: u32,
    ) -> Result<Option<FdEntry>, FdError> {
        if from == to {
            // Validate from is open; otherwise EBADF.
            self.get(from).ok_or(FdError::BadFd)?;
            return Ok(None);
        }
        let from_entry = self.close(from)?;
        let prior = self.install_at(to, from_entry)?;
        Ok(prior)
    }

    /// Duplicate an existing fd into a new lowest-free slot.
    /// Clears `CLOEXEC` on the new entry (POSIX `dup` behaviour).
    pub fn dup(&mut self, fd: u32) -> Result<u32, FdError> {
        let src = *self.get(fd).ok_or(FdError::BadFd)?;
        let mut new_entry = src;
        new_entry.flags.remove(FdFlags::CLOEXEC);
        self.alloc(new_entry)
    }

    /// Iterate over (fd, entry) pairs for every open fd.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &FdEntry)> + '_ {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| slot.as_ref().map(|e| (i as u32, e)))
    }

    /// Number of entries drop()'d by CLOEXEC filtering. Returns
    /// the closed entries so the caller can release resources.
    ///
    /// Used by `proc_spawn` when building the child's fd table:
    /// entries flagged CLOEXEC in the parent are NOT inherited,
    /// and this method efficiently drops them.
    pub fn drop_cloexec(&mut self) -> Vec<(u32, FdEntry)> {
        let mut dropped = Vec::new();
        for (i, slot) in self.entries.iter_mut().enumerate() {
            if let Some(entry) = slot.as_ref() {
                if entry.flags.contains(FdFlags::CLOEXEC) {
                    if let Some(e) = slot.take() {
                        dropped.push((i as u32, e));
                    }
                }
            }
        }
        dropped
    }

    /// Empty the entire fd table and return all entries that were
    /// open. Used by the kernel's process-reap path (T075) to
    /// ensure every object-side resource is released when a
    /// process exits.
    pub fn drain_all(&mut self) -> Vec<(u32, FdEntry)> {
        let mut out = Vec::new();
        for (i, slot) in self.entries.iter_mut().enumerate() {
            if let Some(e) = slot.take() {
                out.push((i as u32, e));
            }
        }
        out
    }

    /// Get the current read/write offset for a seekable fd.
    pub fn offset(&self, fd: u32) -> Result<u64, FdError> {
        Ok(self.get(fd).ok_or(FdError::BadFd)?.offset)
    }

    /// Set the read/write offset for a seekable fd.
    pub fn set_offset(&mut self, fd: u32, offset: u64) -> Result<(), FdError> {
        self.get_mut(fd).ok_or(FdError::BadFd)?.offset = offset;
        Ok(())
    }
}

impl Default for FdTable {
    fn default() -> Self {
        FdTable::new()
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FdError {
    /// Fd number does not refer to an open fd.
    BadFd,
    /// Allocating a new fd would exceed the process's soft limit.
    OutOfFds,
}
