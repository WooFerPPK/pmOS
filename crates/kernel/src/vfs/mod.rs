//! Virtual filesystem.
//!
//! Mirrors `data-model.md §3`. The VFS is the indirection layer
//! between user-process `fd_*` / `path_*` syscalls and the
//! concrete filesystems (tmpfs, devfs, procfs, opfs).
//!
//! ## Design
//!
//! * **Inodes** (`Ino`) are per-filesystem integer identifiers.
//!   Each [`Filesystem`] owns its own ino namespace.
//! * **Mount points** are absolute VFS paths; each mount owns one
//!   [`Filesystem`] trait object. Path resolution picks the
//!   longest matching mount prefix and strips it before handing
//!   the remainder to the filesystem's `lookup` chain.
//! * **No vnode cache** in v1: lookup walks recompute each time.
//!   This is acceptable because tmpfs/devfs/procfs all answer
//!   lookups in O(1) or O(log n) against small in-memory maps,
//!   and the OPFS filesystem (T056..T061) adds its own block-level
//!   cache layer below the VFS. When hot-path measurements in
//!   T220 show the absence of a vnode cache is blowing an input
//!   budget, we add one — no kernel API changes needed.
//! * **Paths** are plain `String` / `&str` slash-separated
//!   absolute paths, normalised by [`path::normalize`] before
//!   resolution.
//!
//! ## Subsystem coupling
//!
//! The VFS is deliberately **not** aware of the process table,
//! the capability system, or IPC. Filesystems that need process
//! information (procfs) receive it via an injected context
//! parameter on their methods, not via a static global. This
//! keeps the VFS testable in isolation — the tests in
//! `kernel/tests/vfs.rs` construct a `Vfs` with zero non-VFS
//! dependencies.

use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

pub mod mount;
pub mod path;
pub mod watch;

pub use mount::{Mount, MountId, MountTable};
pub use watch::{
    Watch, WatchEvent, WatchId, WatchTable, MAX_WATCHES, MAX_WATCHES_PER_PID,
    MAX_WATCHES_PER_TARGET, WATCH_EVENT_QUEUE_CAP,
};

/// Per-filesystem inode identifier. Meaning depends on the
/// concrete [`Filesystem`]; the VFS treats it as opaque.
pub type Ino = u64;

/// Permission / mode bits, POSIX-compatible numeric encoding.
/// The top bits encode file type (duplicated by [`NodeType`] for
/// convenience); the bottom nine bits are rwxrwxrwx.
pub type Mode = u32;

/// Nanoseconds since the epoch (for mtime / atime / ctime).
/// Tests run with zero timestamps; the real kernel fills these
/// in from `Platform::now_ns()`.
pub type NanosSinceEpoch = u64;

/// File system node type.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NodeType {
    RegularFile,
    Directory,
    /// A character device. The `u32` is a driver-defined device
    /// number routed via `crate::dev::dispatch` at read/write time.
    CharDevice(u32),
    Socket,
    SymLink,
    /// Named pipe (FIFO).
    Fifo,
}

impl NodeType {
    #[inline]
    pub const fn is_dir(self) -> bool {
        matches!(self, NodeType::Directory)
    }

    #[inline]
    pub const fn is_regular(self) -> bool {
        matches!(self, NodeType::RegularFile)
    }

    #[inline]
    pub const fn is_symlink(self) -> bool {
        matches!(self, NodeType::SymLink)
    }
}

/// A single stat-like snapshot of a vnode.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FileStat {
    pub ino: Ino,
    pub ty: NodeType,
    pub mode: Mode,
    pub nlink: u32,
    pub size: u64,
    pub atime_ns: NanosSinceEpoch,
    pub mtime_ns: NanosSinceEpoch,
    pub ctime_ns: NanosSinceEpoch,
}

impl FileStat {
    /// A zero-timestamp stat used by tests and by filesystems
    /// that don't maintain access/modification tracking yet.
    pub const fn zeroed(ino: Ino, ty: NodeType, mode: Mode) -> Self {
        FileStat {
            ino,
            ty,
            mode,
            nlink: 1,
            size: 0,
            atime_ns: 0,
            mtime_ns: 0,
            ctime_ns: 0,
        }
    }
}

/// One entry from a `readdir` listing. Filesystems yield one of
/// these per child via the `callback` passed to
/// [`Filesystem::readdir`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub ino: Ino,
    pub ty: NodeType,
}

/// Structured storage counters for a mounted filesystem.
///
/// `quota_bytes` is the total addressable capacity backing the
/// filesystem; `used_bytes` is the currently allocated portion of
/// that capacity; `file_count` counts allocated inodes/nodes. Not
/// every filesystem has meaningful quota data, so the trait method
/// returning this type is optional.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct StorageUsage {
    pub quota_bytes: u64,
    pub used_bytes: u64,
    pub file_count: u64,
}

/// Errors a filesystem operation can return.
///
/// Each variant maps to a specific WASI errno. The mapping is
/// owned by the kernel's syscall dispatcher (T071) — filesystems
/// return this enum and the dispatcher translates to the
/// numeric errno.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FsError {
    /// Name does not exist in the parent directory.
    NotFound,
    /// A file with this name already exists.
    AlreadyExists,
    /// Expected a directory but found a non-directory.
    NotADirectory,
    /// Operation requires a non-directory but found a directory.
    IsADirectory,
    /// rmdir on a non-empty directory.
    NotEmpty,
    /// This filesystem doesn't implement this operation.
    NotSupported,
    /// Out of space (tmpfs capacity hit, OPFS quota exceeded, ...).
    NoSpace,
    /// Generic I/O failure.
    Io,
    /// Invalid argument (bad path, bad offset, etc.).
    InvalidArgument,
    /// The operation is permitted by the filesystem but not by the
    /// caller's capability set. Path resolution and permission
    /// enforcement live in the VFS layer, not in the concrete
    /// filesystem, but some filesystems (procfs) self-enforce.
    PermissionDenied,
    /// Read-only filesystem.
    ReadOnly,
    /// Path resolution encountered a symlink loop or chain longer
    /// than [`Vfs::SYMLOOP_MAX`]. Maps to ELOOP at the syscall layer.
    SymLoop,
}

/// The interface every concrete filesystem implements.
///
/// Every method takes `&mut self` because real filesystems (tmpfs,
/// OPFS) mutate their internal caches on every call. Filesystems
/// that truly don't mutate can still take `&mut self` cheaply.
///
/// **Bound on `Send` only, not `Sync`.** The kernel is strictly
/// single-threaded (it runs in one Web Worker) and filesystems
/// are owned by the kernel. `Sync` would forbid
/// `Box<dyn BlockDevice>` (correctly not `Sync`) inside an
/// `OpfsFs`. `Send` is kept so tests can move a boxed filesystem
/// across thread boundaries.
pub trait Filesystem: Send {
    /// Whether this filesystem exposes stable mutation targets for
    /// `fs_watch`. Synthetic filesystems override this to reject unwakeable
    /// registrations explicitly.
    fn supports_watches(&self) -> bool {
        true
    }

    /// Return the root inode number of this filesystem.
    fn root(&self) -> Ino;

    /// Look up `name` in the directory at `dir`. Returns the
    /// child's ino or `Err(NotFound)`.
    fn lookup(&mut self, dir: Ino, name: &str) -> Result<Ino, FsError>;

    /// Read at most `buf.len()` bytes from the regular file at
    /// `ino`, starting at byte `offset`. Returns the number of
    /// bytes actually read (0 means EOF).
    fn read(&mut self, ino: Ino, offset: u64, buf: &mut [u8]) -> Result<usize, FsError>;

    /// Write `buf` to the regular file at `ino`, starting at
    /// byte `offset`. Returns the number of bytes written.
    fn write(&mut self, ino: Ino, offset: u64, buf: &[u8]) -> Result<usize, FsError>;

    /// Push each entry of the directory at `dir` to `out`.
    fn readdir(&mut self, dir: Ino, out: &mut Vec<DirEntry>) -> Result<(), FsError>;

    /// Create a regular file in `dir` with `name`. Returns the
    /// new ino.
    fn create(&mut self, dir: Ino, name: &str, mode: Mode) -> Result<Ino, FsError>;

    /// Create an empty directory in `dir`. Returns the new ino.
    fn mkdir(&mut self, dir: Ino, name: &str, mode: Mode) -> Result<Ino, FsError>;

    /// Unlink a regular file or symlink.
    fn unlink(&mut self, dir: Ino, name: &str) -> Result<(), FsError>;

    /// Remove an empty directory.
    fn rmdir(&mut self, dir: Ino, name: &str) -> Result<(), FsError>;

    /// Rename within the same filesystem. Cross-mount rename is
    /// handled at the VFS layer.
    fn rename(
        &mut self,
        from_dir: Ino,
        from_name: &str,
        to_dir: Ino,
        to_name: &str,
    ) -> Result<(), FsError>;

    /// Snapshot a vnode's metadata.
    fn stat(&mut self, ino: Ino) -> Result<FileStat, FsError>;

    /// Truncate a regular file to `new_size` bytes.
    fn truncate(&mut self, ino: Ino, new_size: u64) -> Result<(), FsError>;

    /// Replace the low nine POSIX permission bits on `ino`.
    /// Read-only and synthetic filesystems inherit this default.
    fn set_mode(&mut self, _ino: Ino, _mode: Mode) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }

    /// Set the `atime_ns` and / or `mtime_ns` on `ino`. Either
    /// argument being `None` means "leave this field unchanged".
    /// A successful call also bumps `ctime_ns` to now() — setting
    /// the times is itself a metadata change.
    ///
    /// Default: returns `FsError::ReadOnly`. Filesystems that can
    /// honour the set override this. Splitting the WASI
    /// `fstflags` decode in the dispatcher means this trait method
    /// takes already-materialised Option values — the filesystem
    /// never sees the SET_ATIM_NOW / SET_MTIM_NOW bits.
    fn set_times(
        &mut self,
        _ino: Ino,
        _atime_ns: Option<NanosSinceEpoch>,
        _mtime_ns: Option<NanosSinceEpoch>,
    ) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }

    /// Create a hardlink `to_name` in `to_dir` pointing at the same
    /// inode that `from_name` in `from_dir` already points at.
    /// Cross-mount linking is rejected at the VFS layer before this
    /// method is called, so all four arguments reference the same
    /// filesystem.
    ///
    /// Default: returns `FsError::ReadOnly`. tmpfs overrides with the
    /// real `nlink++` + dir-entry-insert path; devfs / procfs / opfs
    /// inherit the default (→ `EROFS`), matching
    /// [`Filesystem::set_times`]'s default.
    fn link(
        &mut self,
        _from_dir: Ino,
        _from_name: &str,
        _to_dir: Ino,
        _to_name: &str,
    ) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }

    /// Create a symlink in `dir` named `name` whose content is the
    /// arbitrary UTF-8 `target` string. Returns the new ino.
    ///
    /// The target need not exist — dangling symlinks are first-class
    /// in POSIX/WASI, and v1's `Vfs::resolve` doesn't follow
    /// symlinks anyway so the distinction never matters on a
    /// resolution path.
    ///
    /// Default: returns `FsError::NotSupported` (→ `ENOTSUP`).
    /// Filesystems that support symlinks (tmpfs) override; devfs /
    /// procfs / opfs inherit the default. The default differs from
    /// `link`'s `ReadOnly` default because symlink semantics are a
    /// capability (does this filesystem know what a symlink is?)
    /// rather than a write permission, and devfs / procfs never gain
    /// symlinks in any conceivable future.
    fn symlink(&mut self, _dir: Ino, _name: &str, _target: &str) -> Result<Ino, FsError> {
        Err(FsError::NotSupported)
    }

    /// Copy the symlink target at `ino` into `out`, returning the
    /// number of bytes actually written. If `out` is shorter than the
    /// target, the output is silently truncated (POSIX readlink(2)
    /// semantics — the caller uses the returned byte count).
    ///
    /// Default: returns `FsError::NotSupported` (→ `ENOTSUP`).
    /// Filesystems that support symlinks (tmpfs) override with a
    /// per-variant match: a SymLink node yields its target bytes; a
    /// non-SymLink node returns `FsError::InvalidArgument` (→
    /// `EINVAL`).
    fn readlink(&mut self, _ino: Ino, _out: &mut [u8]) -> Result<usize, FsError> {
        Err(FsError::NotSupported)
    }

    /// Flush any buffered writes. Default no-op.
    fn sync(&mut self) -> Result<(), FsError> {
        Ok(())
    }

    /// Return structured storage counters for this filesystem, if
    /// it has a quota-backed storage device. tmpfs/devfs/procfs
    /// inherit `None`; OPFS overrides with its superblock-derived
    /// counters.
    fn storage_usage(&self) -> Option<StorageUsage> {
        None
    }

    /// A short human name used in `/proc/mounts`-style output.
    /// Examples: "tmpfs", "devfs", "procfs", "opfs".
    fn kind_name(&self) -> &'static str;
}

/// The kernel-wide virtual filesystem.
pub struct Vfs {
    mounts: MountTable,
    watches: WatchTable,
    dirty_mounts: BTreeSet<MountId>,
}

impl Vfs {
    pub fn new() -> Self {
        Vfs {
            mounts: MountTable::new(),
            watches: WatchTable::new(),
            dirty_mounts: BTreeSet::new(),
        }
    }

    /// Borrow the watch table immutably. Used by the syscall layer
    /// to inspect the registry from `fd_read` / `fd_close` paths
    /// that need a `&mut Watch`.
    pub fn watches(&self) -> &WatchTable {
        &self.watches
    }

    /// Mutably borrow the watch table. The kernel uses this to
    /// drain a watch's event queue into a userland heap_out window
    /// from the `fd_read` Watch arm.
    pub fn watches_mut(&mut self) -> &mut WatchTable {
        &mut self.watches
    }

    /// Resolve `abs_path` and install a fresh watch on the resulting
    /// `(mount_id, ino)` pair. Caller is responsible for validating
    /// `mask` against [`abi::ext::WATCH_MASK_ALL`] and for rejecting
    /// non-absolute paths BEFORE calling this method — the VFS
    /// surface is intentionally minimal so the syscall handler owns
    /// the wire-level validation invariants.
    pub fn register_watch(&mut self, abs_path: &str, mask: u32) -> Result<WatchId, FsError> {
        let (mount_id, ino) = self.resolve(abs_path)?;
        if !self
            .mounts
            .fs(mount_id)
            .ok_or(FsError::NotFound)?
            .supports_watches()
        {
            return Err(FsError::NotSupported);
        }
        self.watches
            .register(mount_id, ino, mask)
            .map_err(|_| FsError::NoSpace)
    }

    /// Remove a watch by id. Returns `true` iff the id named a
    /// previously-registered watch. The `fd_close` Watch arm uses
    /// the bool only for debug-assertion purposes — a stale id (a
    /// watch already unregistered by an earlier close) is not an
    /// error at the caller layer.
    pub fn unregister_watch(&mut self, id: WatchId) -> bool {
        self.watches.unregister(id)
    }

    /// Push a single event onto every watch subscribed to
    /// `(mount_id, inode)`. Watches whose mask doesn't include the
    /// event's bit drop it silently. Called from the kernel's
    /// mutation-wrapping methods (`Kernel::vfs_create`,
    /// `Kernel::vfs_unlink`, etc.) AFTER the underlying VFS call
    /// succeeds so a failed mutation never queues a phantom event.
    pub fn notify(&mut self, mount_id: MountId, inode: Ino, event: WatchEvent) {
        self.watches.notify(mount_id, inode, event);
    }

    /// Install a mount at an absolute path. Every subsequent
    /// resolve() that lands on or below this path is routed to
    /// the mounted filesystem. The path is normalised before
    /// insertion — `mount("/dev/", ...)` and `mount("/dev", ...)`
    /// are equivalent.
    pub fn mount(&mut self, mountpoint: &str, fs: Box<dyn Filesystem>) -> Result<MountId, FsError> {
        let normalised = path::normalize(mountpoint);
        self.mounts.insert(normalised, fs)
    }

    /// Unmount. Returns the filesystem trait object so the
    /// caller can perform any last sync.
    pub fn umount(&mut self, mountpoint: &str) -> Result<Box<dyn Filesystem>, FsError> {
        let normalised = path::normalize(mountpoint);
        let mount_id = self.mounts.id_of(&normalised).ok_or(FsError::NotFound)?;
        let fs = self.mounts.remove(&normalised)?;
        self.dirty_mounts.remove(&mount_id);
        Ok(fs)
    }

    /// Number of currently-installed mounts.
    pub fn mount_count(&self) -> usize {
        self.mounts.len()
    }

    /// Number of mounts with successful mutations that have not
    /// yet been flushed through [`Filesystem::sync`]. Exposed for
    /// diagnostics and native isolation tests.
    pub fn dirty_mount_count(&self) -> usize {
        self.dirty_mounts.len()
    }

    #[inline]
    fn mark_dirty(&mut self, mount_id: MountId) {
        self.dirty_mounts.insert(mount_id);
    }

    /// Flush every dirty mount and clear its dirty bit only after
    /// that filesystem reports success.
    pub fn sync_dirty(&mut self) -> Result<(), FsError> {
        let mounts: Vec<MountId> = self.dirty_mounts.iter().copied().collect();
        for mount_id in mounts {
            self.sync_mount(mount_id)?;
        }
        Ok(())
    }

    /// Flush a specific mount, clearing its dirty bit after a
    /// successful filesystem sync.
    pub fn sync_mount(&mut self, mount_id: MountId) -> Result<(), FsError> {
        let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
        fs.sync()?;
        self.dirty_mounts.remove(&mount_id);
        Ok(())
    }

    /// Mutate an existing mount's flag bitset in place. Pass-through
    /// to [`MountTable::set_flags`]; the path is normalised first so
    /// callers can pass `/dev/` or `/dev` interchangeably (mirrors
    /// the [`Vfs::mount`] / [`Vfs::umount`] normalisation step). Used
    /// by `Kernel::mount` to implement the `MOUNT_REMOUNT` flag.
    pub fn set_mount_flags(&mut self, mountpoint: &str, flags: u32) -> Result<MountId, FsError> {
        let normalised = path::normalize(mountpoint);
        self.mounts.set_flags(&normalised, flags)
    }

    /// Read a mount's current flag bitset by mountpoint. Returns
    /// `None` if the path is not a mount point. Pass-through to
    /// [`MountTable::flags_of`] with normalisation.
    pub fn mount_flags(&self, mountpoint: &str) -> Option<u32> {
        let normalised = path::normalize(mountpoint);
        self.mounts.flags_of(&normalised)
    }

    /// Return structured storage counters for an exact mountpoint.
    /// Filesystems without quota-backed storage return `Ok(None)`.
    /// Read-only — the trait method `Filesystem::storage_usage`
    /// takes `&self`, so the wasm entry seam can copy the value
    /// into its independent procfs projection before syscall
    /// dispatch begins.
    pub fn storage_usage(&self, mountpoint: &str) -> Result<Option<StorageUsage>, FsError> {
        let normalised = path::normalize(mountpoint);
        let mount_id = self.mounts.id_of(&normalised).ok_or(FsError::NotFound)?;
        let fs = self.mounts.fs(mount_id).ok_or(FsError::NotFound)?;
        Ok(fs.storage_usage())
    }

    /// Snapshot every installed mount's `(id, mountpoint)`. Returned
    /// as owned `String`s so the caller doesn't need to keep an
    /// immutable borrow of `Vfs` alive while iterating — useful from
    /// `Kernel::mount` / `Kernel::umount`, both of which need
    /// further `&mut self.vfs` calls (e.g. `vfs.mount(..)`,
    /// `vfs.readdir_ino(..)`) after the mount-table walk.
    pub fn mountpoints(&self) -> Vec<(MountId, String)> {
        self.mounts
            .iter()
            .map(|(id, mp)| (id, String::from(mp)))
            .collect()
    }

    /// Look up the normalised mountpoint path for a mount id, or
    /// `None` if no such mount is installed. Thin pass-through to
    /// [`MountTable::mountpoint_of`] — used by procfs's live
    /// `/proc/<pid>/fd/<n>` symlink projection to render a
    /// `Vnode { mount_id, ino }` as a `<mountpoint>#<ino>` target.
    pub fn mountpoint_of(&self, id: MountId) -> Option<&str> {
        self.mounts.mountpoint_of(id)
    }

    /// Maximum number of symlink dereferences on a single resolve
    /// before `FsError::SymLoop` is returned. POSIX requires at
    /// least SYMLOOP_MAX=8; v1 uses 40 to match Linux's
    /// MAXSYMLINKS default so an unusual but legitimate deep chain
    /// doesn't fail.
    pub const SYMLOOP_MAX: u32 = 40;

    /// Resolve an absolute path to `(mount_id, ino)`, following
    /// symlinks for both intermediate and final components.
    /// Bounded by [`Vfs::SYMLOOP_MAX`]; exceeds the budget →
    /// `FsError::SymLoop` (→ `ELOOP` at the syscall layer).
    pub fn resolve(&mut self, abs_path: &str) -> Result<(MountId, Ino), FsError> {
        self.resolve_inner(abs_path, true, Self::SYMLOOP_MAX, None)
    }

    /// Resolve an absolute path while requiring every symlink expansion to
    /// remain beneath `root`. Both arguments are normalised before the check,
    /// and an absolute or relative symlink that escapes the root fails with
    /// [`FsError::PermissionDenied`].
    ///
    /// This is intentionally narrower than a lexical prefix check: executable
    /// loading uses it to ensure `/opt/link -> /home/program.wasm` cannot turn
    /// writable data outside the package namespace into executable code.
    pub fn resolve_beneath(
        &mut self,
        abs_path: &str,
        root: &str,
    ) -> Result<(MountId, Ino), FsError> {
        let root = path::normalize(root);
        self.resolve_inner(abs_path, true, Self::SYMLOOP_MAX, Some(&root))
    }

    /// Resolve an absolute path to `(mount_id, ino)` without
    /// following the FINAL symlink component (POSIX lstat /
    /// `O_NOFOLLOW` semantics). Intermediate symlinks are still
    /// dereferenced — only the last component is left at its own
    /// ino when it happens to be a symlink. Used by `stat`,
    /// `readlink`, and `path_open(SYMLINK_FOLLOW=0)` so the final
    /// symlink stays addressable through its own ino without
    /// requiring callers to hand-walk their paths.
    pub fn resolve_nofollow(&mut self, abs_path: &str) -> Result<(MountId, Ino), FsError> {
        self.resolve_inner(abs_path, false, Self::SYMLOOP_MAX, None)
    }

    /// Follow symlinks through mounts other than `stop_mountpoint` and return
    /// the canonical path at the first point resolution would enter that
    /// mount. The stop mount itself is never consulted.
    pub fn path_entering_mount(
        &mut self,
        abs_path: &str,
        stop_mountpoint: &str,
        follow_last: bool,
    ) -> Result<Option<String>, FsError> {
        self.path_entering_mount_inner(abs_path, stop_mountpoint, follow_last, Self::SYMLOOP_MAX)
    }

    fn path_entering_mount_inner(
        &mut self,
        abs_path: &str,
        stop_mountpoint: &str,
        follow_last: bool,
        follows_left: u32,
    ) -> Result<Option<String>, FsError> {
        let canonical = path::normalize(abs_path);
        let (mount_id, relative) = self
            .mounts
            .longest_prefix(&canonical)
            .ok_or(FsError::NotFound)?;
        if self.mounts.mountpoint_of(mount_id) == Some(stop_mountpoint) {
            return Ok(Some(canonical));
        }

        let components: Vec<&str> = path::components(&relative).collect();
        let mut ino = self
            .mounts
            .fs_mut(mount_id)
            .ok_or(FsError::NotFound)?
            .root();
        for (index, component) in components.iter().enumerate() {
            let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
            ino = fs.lookup(ino, component)?;
            if fs.stat(ino)?.ty != NodeType::SymLink {
                continue;
            }
            if index + 1 == components.len() && !follow_last {
                continue;
            }
            if follows_left == 0 {
                return Err(FsError::SymLoop);
            }
            let mut target_buf = [0u8; 4096];
            let target_len = fs.readlink(ino, &mut target_buf)?;
            let target = core::str::from_utf8(&target_buf[..target_len])
                .map_err(|_| FsError::InvalidArgument)?;

            let mut redirected = String::new();
            if target.starts_with('/') {
                redirected.push_str(target);
            } else {
                let mountpoint = self
                    .mounts
                    .mountpoint_of(mount_id)
                    .ok_or(FsError::NotFound)?;
                redirected.push_str(mountpoint);
                for prefix_component in &components[..index] {
                    if !redirected.ends_with('/') {
                        redirected.push('/');
                    }
                    redirected.push_str(prefix_component);
                }
                if !redirected.ends_with('/') {
                    redirected.push('/');
                }
                redirected.push_str(target);
            }
            for tail in &components[index + 1..] {
                if !redirected.ends_with('/') {
                    redirected.push('/');
                }
                redirected.push_str(tail);
            }
            return self.path_entering_mount_inner(
                &redirected,
                stop_mountpoint,
                follow_last,
                follows_left - 1,
            );
        }
        Ok(None)
    }

    /// Resolve `relative_path` from an already-open directory vnode.
    ///
    /// WASI path syscalls carry a directory fd rather than a process cwd.
    /// Once the fd has been translated to `(mount_id, dir_ino)`, this is the
    /// inode-relative counterpart to [`Vfs::resolve`]. `..` is rejected: v1
    /// filesystems do not retain parent-inode links, so accepting it would
    /// either escape the fd's directory or silently resolve against `/`.
    pub fn resolve_at(
        &mut self,
        mount_id: MountId,
        dir_ino: Ino,
        relative_path: &str,
    ) -> Result<(MountId, Ino), FsError> {
        self.resolve_at_inner(mount_id, dir_ino, relative_path, true, Self::SYMLOOP_MAX)
    }

    /// [`Vfs::resolve_at`] without following the final symlink component.
    pub fn resolve_at_nofollow(
        &mut self,
        mount_id: MountId,
        dir_ino: Ino,
        relative_path: &str,
    ) -> Result<(MountId, Ino), FsError> {
        self.resolve_at_inner(mount_id, dir_ino, relative_path, false, Self::SYMLOOP_MAX)
    }

    /// Inode-relative counterpart to [`Vfs::path_entering_mount`]. Relative
    /// symlinks remain rooted at their containing directory; absolute targets
    /// re-enter ordinary namespace resolution.
    pub fn path_entering_mount_at(
        &mut self,
        mount_id: MountId,
        dir_ino: Ino,
        relative_path: &str,
        stop_mountpoint: &str,
        follow_last: bool,
    ) -> Result<Option<String>, FsError> {
        self.path_entering_mount_at_inner(
            mount_id,
            dir_ino,
            relative_path,
            stop_mountpoint,
            follow_last,
            Self::SYMLOOP_MAX,
        )
    }

    fn path_entering_mount_at_inner(
        &mut self,
        mount_id: MountId,
        dir_ino: Ino,
        relative_path: &str,
        stop_mountpoint: &str,
        follow_last: bool,
        follows_left: u32,
    ) -> Result<Option<String>, FsError> {
        if let Some(absolute) = self.mount_root_relative_path(mount_id, dir_ino, relative_path)? {
            return self.path_entering_mount_inner(
                &absolute,
                stop_mountpoint,
                follow_last,
                follows_left,
            );
        }
        let components = Self::checked_relative_components(relative_path)?;
        let mut ino = dir_ino;
        for (index, component) in components.iter().enumerate() {
            let parent_ino = ino;
            let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
            ino = fs.lookup(parent_ino, component)?;
            if fs.stat(ino)?.ty != NodeType::SymLink {
                continue;
            }
            if index + 1 == components.len() && !follow_last {
                continue;
            }
            if follows_left == 0 {
                return Err(FsError::SymLoop);
            }
            let mut target_buf = [0u8; 4096];
            let target_len = fs.readlink(ino, &mut target_buf)?;
            let target = core::str::from_utf8(&target_buf[..target_len])
                .map_err(|_| FsError::InvalidArgument)?;
            let mut redirected = String::from(target);
            for tail in &components[index + 1..] {
                if !redirected.ends_with('/') {
                    redirected.push('/');
                }
                redirected.push_str(tail);
            }
            if target.starts_with('/') {
                return self.path_entering_mount_inner(
                    &redirected,
                    stop_mountpoint,
                    follow_last,
                    follows_left - 1,
                );
            }
            return self.path_entering_mount_at_inner(
                mount_id,
                parent_ino,
                &redirected,
                stop_mountpoint,
                follow_last,
                follows_left - 1,
            );
        }
        Ok(None)
    }

    fn checked_relative_components(relative_path: &str) -> Result<Vec<&str>, FsError> {
        if relative_path.is_empty() || relative_path.starts_with('/') {
            return Err(FsError::InvalidArgument);
        }
        let mut components = Vec::new();
        for component in relative_path.split('/') {
            match component {
                "" | "." => {}
                ".." => return Err(FsError::InvalidArgument),
                name => components.push(name),
            }
        }
        Ok(components)
    }

    /// When a dirfd names a filesystem root, recover its namespace path so
    /// ordinary absolute VFS resolution can still cross a nested mount (the
    /// `/` preopen reaching `/dev` and `/proc` is the important v1 case).
    fn mount_root_relative_path(
        &mut self,
        mount_id: MountId,
        dir_ino: Ino,
        relative_path: &str,
    ) -> Result<Option<String>, FsError> {
        let _ = Self::checked_relative_components(relative_path)?;
        let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
        if fs.root() != dir_ino {
            return Ok(None);
        }
        let mountpoint = self
            .mounts
            .mountpoint_of(mount_id)
            .ok_or(FsError::NotFound)?;
        let mut absolute = String::from(mountpoint);
        if relative_path != "." {
            if !absolute.ends_with('/') {
                absolute.push('/');
            }
            absolute.push_str(relative_path);
        }
        Ok(Some(absolute))
    }

    fn resolve_at_inner(
        &mut self,
        mount_id: MountId,
        dir_ino: Ino,
        relative_path: &str,
        follow_last: bool,
        follows_left: u32,
    ) -> Result<(MountId, Ino), FsError> {
        if let Some(absolute) = self.mount_root_relative_path(mount_id, dir_ino, relative_path)? {
            return self.resolve_inner(&absolute, follow_last, follows_left, None);
        }

        let components = Self::checked_relative_components(relative_path)?;
        let base_stat = {
            let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
            fs.stat(dir_ino)?
        };
        if base_stat.ty != NodeType::Directory {
            return Err(FsError::NotADirectory);
        }

        let mut ino = dir_ino;
        for (index, component) in components.iter().enumerate() {
            let parent_ino = ino;
            ino = {
                let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
                fs.lookup(parent_ino, component)?
            };
            let is_last = index + 1 == components.len();
            if is_last && !follow_last {
                continue;
            }
            let stat = {
                let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
                fs.stat(ino)?
            };
            if stat.ty != NodeType::SymLink {
                continue;
            }
            if follows_left == 0 {
                return Err(FsError::SymLoop);
            }

            let mut target_buf = [0u8; 4096];
            let target_len = {
                let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
                fs.readlink(ino, &mut target_buf)?
            };
            let target = core::str::from_utf8(&target_buf[..target_len])
                .map_err(|_| FsError::InvalidArgument)?;
            let mut redirected = String::from(target);
            for tail in &components[index + 1..] {
                if !redirected.ends_with('/') {
                    redirected.push('/');
                }
                redirected.push_str(tail);
            }
            if target.starts_with('/') {
                return self.resolve_inner(&redirected, follow_last, follows_left - 1, None);
            }
            return self.resolve_at_inner(
                mount_id,
                parent_ino,
                &redirected,
                follow_last,
                follows_left - 1,
            );
        }
        Ok((mount_id, ino))
    }

    fn resolve_inner(
        &mut self,
        abs_path: &str,
        follow_last: bool,
        follows_left: u32,
        confined_root: Option<&str>,
    ) -> Result<(MountId, Ino), FsError> {
        let canon = path::normalize(abs_path);
        if let Some(root) = confined_root {
            let beneath = root == "/"
                || canon == root
                || canon
                    .strip_prefix(root)
                    .is_some_and(|suffix| suffix.starts_with('/'));
            if !beneath {
                return Err(FsError::PermissionDenied);
            }
        }
        let (mount_id, rel) = self
            .mounts
            .longest_prefix(&canon)
            .ok_or(FsError::NotFound)?;
        let components: Vec<&str> = path::components(&rel).collect();
        let n = components.len();

        let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
        let mut ino = fs.root();
        if n == 0 {
            return Ok((mount_id, ino));
        }

        for (i, comp) in components.iter().enumerate() {
            let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
            ino = fs.lookup(ino, comp)?;
            let is_last = i + 1 == n;
            let should_follow = !is_last || follow_last;
            if !should_follow {
                continue;
            }
            let st = fs.stat(ino)?;
            if st.ty != NodeType::SymLink {
                continue;
            }
            if follows_left == 0 {
                return Err(FsError::SymLoop);
            }
            // Fetch the target bytes. POSIX PATH_MAX is 4096;
            // any target longer than that is `ENAMETOOLONG` in
            // principle, but v1's filesystems already cap the
            // target string on symlink() and readlink truncates
            // silently, so a simple fixed buffer is fine.
            let mut buf = [0u8; 4096];
            let target_len = fs.readlink(ino, &mut buf)?;
            let target =
                core::str::from_utf8(&buf[..target_len]).map_err(|_| FsError::InvalidArgument)?;

            // Rebuild the path to re-resolve. For an absolute
            // target, `new_path = target + remainder`. For a
            // relative target, the symlink's *parent directory*
            // (in the VFS path namespace, i.e. mount prefix +
            // components up to but not including the symlink)
            // is the base.
            let mut new_path = String::new();
            if target.starts_with('/') {
                new_path.push_str(target);
            } else {
                let mount_prefix = self
                    .mounts
                    .mountpoint_of(mount_id)
                    .ok_or(FsError::NotFound)?;
                new_path.push_str(mount_prefix);
                for prefix_comp in &components[..i] {
                    if !new_path.ends_with('/') {
                        new_path.push('/');
                    }
                    new_path.push_str(prefix_comp);
                }
                if !new_path.ends_with('/') {
                    new_path.push('/');
                }
                new_path.push_str(target);
            }
            for tail in &components[i + 1..] {
                if !new_path.ends_with('/') {
                    new_path.push('/');
                }
                new_path.push_str(tail);
            }
            return self.resolve_inner(&new_path, follow_last, follows_left - 1, confined_root);
        }
        Ok((mount_id, ino))
    }

    /// Resolve a path to its parent directory and the final
    /// component name. Used by create/mkdir/unlink/rmdir to
    /// find the target directory before performing the op.
    pub fn resolve_parent(&mut self, abs_path: &str) -> Result<(MountId, Ino, String), FsError> {
        let canon = path::normalize(abs_path);
        let (parent_path, name) = path::split_last(&canon).ok_or(FsError::InvalidArgument)?;
        if name.is_empty() {
            return Err(FsError::InvalidArgument);
        }
        let (mount_id, parent_ino) = self.resolve(&parent_path)?;
        Ok((mount_id, parent_ino, name))
    }

    /// Resolve a relative path's parent from an open directory vnode.
    pub fn resolve_parent_at(
        &mut self,
        mount_id: MountId,
        dir_ino: Ino,
        relative_path: &str,
    ) -> Result<(MountId, Ino, String), FsError> {
        if let Some(absolute) = self.mount_root_relative_path(mount_id, dir_ino, relative_path)? {
            return self.resolve_parent(&absolute);
        }
        let components = Self::checked_relative_components(relative_path)?;
        let (name, parents) = components.split_last().ok_or(FsError::InvalidArgument)?;
        if parents.is_empty() {
            let stat = {
                let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
                fs.stat(dir_ino)?
            };
            if stat.ty != NodeType::Directory {
                return Err(FsError::NotADirectory);
            }
            return Ok((mount_id, dir_ino, String::from(*name)));
        }
        let mut parent_path = String::new();
        for component in parents {
            if !parent_path.is_empty() {
                parent_path.push('/');
            }
            parent_path.push_str(component);
        }
        let (resolved_mount, parent_ino) = self.resolve_at(mount_id, dir_ino, &parent_path)?;
        Ok((resolved_mount, parent_ino, String::from(*name)))
    }

    /// Open an absolute path. Returns `(mount_id, ino, ty)` on
    /// success — enough for the syscall layer to decide which
    /// `FdObject` variant to install in the calling process's
    /// fd table.
    ///
    /// Paths resolving to character devices return
    /// `NodeType::CharDevice(devnum)`; the caller should install
    /// a `FdObject::CharDevice(devnum)` in the fd table so
    /// subsequent read/write routes through `DeviceDispatcher`
    /// rather than the filesystem. Intermediate + final symlinks
    /// are dereferenced via [`Vfs::resolve`].
    pub fn open(&mut self, abs_path: &str) -> Result<(MountId, Ino, NodeType), FsError> {
        let (mount_id, ino) = self.resolve(abs_path)?;
        let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
        let st = fs.stat(ino)?;
        Ok((mount_id, ino, st.ty))
    }

    /// Open an absolute path without dereferencing the final
    /// symlink component (POSIX `O_NOFOLLOW` / WASI
    /// `LOOKUP_SYMLINK_FOLLOW=0` semantics). Mirrors
    /// [`Vfs::open`] but routes through [`Vfs::resolve_nofollow`]
    /// so a path whose final component is itself a symlink yields
    /// the symlink's own vnode rather than the target's.
    pub fn open_nofollow(&mut self, abs_path: &str) -> Result<(MountId, Ino, NodeType), FsError> {
        let (mount_id, ino) = self.resolve_nofollow(abs_path)?;
        let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
        let st = fs.stat(ino)?;
        Ok((mount_id, ino, st.ty))
    }

    /// Open a relative path from an already-open directory vnode.
    pub fn open_at(
        &mut self,
        mount_id: MountId,
        dir_ino: Ino,
        relative_path: &str,
    ) -> Result<(MountId, Ino, NodeType), FsError> {
        let (resolved_mount, ino) = self.resolve_at(mount_id, dir_ino, relative_path)?;
        let fs = self
            .mounts
            .fs_mut(resolved_mount)
            .ok_or(FsError::NotFound)?;
        let stat = fs.stat(ino)?;
        Ok((resolved_mount, ino, stat.ty))
    }

    /// [`Vfs::open_at`] without dereferencing the final symlink.
    pub fn open_at_nofollow(
        &mut self,
        mount_id: MountId,
        dir_ino: Ino,
        relative_path: &str,
    ) -> Result<(MountId, Ino, NodeType), FsError> {
        let (resolved_mount, ino) = self.resolve_at_nofollow(mount_id, dir_ino, relative_path)?;
        let fs = self
            .mounts
            .fs_mut(resolved_mount)
            .ok_or(FsError::NotFound)?;
        let stat = fs.stat(ino)?;
        Ok((resolved_mount, ino, stat.ty))
    }

    /// Read from a regular file at an absolute path.
    pub fn read(&mut self, abs_path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        let (mount_id, ino) = self.resolve(abs_path)?;
        self.read_ino(mount_id, ino, offset, buf)
    }

    /// Write to a regular file at an absolute path.
    pub fn write(&mut self, abs_path: &str, offset: u64, buf: &[u8]) -> Result<usize, FsError> {
        let (mount_id, ino) = self.resolve(abs_path)?;
        self.write_ino(mount_id, ino, offset, buf)
    }

    /// Read from `(mount_id, ino)` directly. Used by the syscall
    /// layer: an fd already carries its mount and inode, so no
    /// path resolution is necessary per `fd_read`.
    pub fn read_ino(
        &mut self,
        mount_id: MountId,
        ino: Ino,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
        fs.read(ino, offset, buf)
    }

    /// Write to `(mount_id, ino)` directly. See [`Vfs::read_ino`].
    pub fn write_ino(
        &mut self,
        mount_id: MountId,
        ino: Ino,
        offset: u64,
        buf: &[u8],
    ) -> Result<usize, FsError> {
        let n = {
            let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
            fs.write(ino, offset, buf)?
        };
        if n > 0 {
            self.mark_dirty(mount_id);
        }
        Ok(n)
    }

    /// List the entries of the directory at an absolute path.
    pub fn readdir(&mut self, abs_path: &str) -> Result<Vec<DirEntry>, FsError> {
        let (mount_id, ino) = self.resolve(abs_path)?;
        self.readdir_ino(mount_id, ino)
    }

    /// List the entries of the directory at `(mount_id, ino)`.
    /// Mirrors [`Vfs::stat_ino`] / [`Vfs::read_ino`]: the syscall
    /// layer already carries the pair on a `Vnode` fd, so
    /// `fd_readdir` skips path resolution by calling this instead
    /// of [`Vfs::readdir`]. Returns the directory's entries
    /// verbatim (no `.` / `..` injection — WASI doesn't require
    /// them and v1 filesystems don't track parent inodes).
    pub fn readdir_ino(&mut self, mount_id: MountId, ino: Ino) -> Result<Vec<DirEntry>, FsError> {
        let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
        let mut out = Vec::new();
        fs.readdir(ino, &mut out)?;
        Ok(out)
    }

    /// Create a regular file at an absolute path.
    pub fn create(&mut self, abs_path: &str, mode: Mode) -> Result<Ino, FsError> {
        let (mount_id, parent_ino, name) = self.resolve_parent(abs_path)?;
        let ino = {
            let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
            fs.create(parent_ino, &name, mode)?
        };
        self.mark_dirty(mount_id);
        Ok(ino)
    }

    /// Create a regular file relative to an open directory vnode.
    pub fn create_at(
        &mut self,
        mount_id: MountId,
        dir_ino: Ino,
        relative_path: &str,
        mode: Mode,
    ) -> Result<Ino, FsError> {
        let (target_mount, parent_ino, name) =
            self.resolve_parent_at(mount_id, dir_ino, relative_path)?;
        let ino = {
            let fs = self.mounts.fs_mut(target_mount).ok_or(FsError::NotFound)?;
            fs.create(parent_ino, &name, mode)?
        };
        self.mark_dirty(target_mount);
        Ok(ino)
    }

    /// Create a directory at an absolute path.
    pub fn mkdir(&mut self, abs_path: &str, mode: Mode) -> Result<Ino, FsError> {
        let (mount_id, parent_ino, name) = self.resolve_parent(abs_path)?;
        let ino = {
            let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
            fs.mkdir(parent_ino, &name, mode)?
        };
        self.mark_dirty(mount_id);
        Ok(ino)
    }

    /// Unlink (remove) a regular file at an absolute path.
    pub fn unlink(&mut self, abs_path: &str) -> Result<(), FsError> {
        let (mount_id, parent_ino, name) = self.resolve_parent(abs_path)?;
        {
            let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
            fs.unlink(parent_ino, &name)?;
        }
        self.mark_dirty(mount_id);
        Ok(())
    }

    /// Unlink a regular file relative to an open directory vnode.
    pub fn unlink_at(
        &mut self,
        mount_id: MountId,
        dir_ino: Ino,
        relative_path: &str,
    ) -> Result<(), FsError> {
        let (target_mount, parent_ino, name) =
            self.resolve_parent_at(mount_id, dir_ino, relative_path)?;
        {
            let fs = self.mounts.fs_mut(target_mount).ok_or(FsError::NotFound)?;
            fs.unlink(parent_ino, &name)?;
        }
        self.mark_dirty(target_mount);
        Ok(())
    }

    /// Remove an empty directory at an absolute path.
    pub fn rmdir(&mut self, abs_path: &str) -> Result<(), FsError> {
        let (mount_id, parent_ino, name) = self.resolve_parent(abs_path)?;
        {
            let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
            fs.rmdir(parent_ino, &name)?;
        }
        self.mark_dirty(mount_id);
        Ok(())
    }

    /// Remove an empty directory relative to an open directory vnode.
    pub fn rmdir_at(
        &mut self,
        mount_id: MountId,
        dir_ino: Ino,
        relative_path: &str,
    ) -> Result<(), FsError> {
        let (target_mount, parent_ino, name) =
            self.resolve_parent_at(mount_id, dir_ino, relative_path)?;
        {
            let fs = self.mounts.fs_mut(target_mount).ok_or(FsError::NotFound)?;
            fs.rmdir(parent_ino, &name)?;
        }
        self.mark_dirty(target_mount);
        Ok(())
    }

    /// Rename within the same filesystem. Cross-mount renames
    /// are rejected (caller must use create+write+unlink).
    pub fn rename(&mut self, from: &str, to: &str) -> Result<(), FsError> {
        let (from_mount, from_parent, from_name) = self.resolve_parent(from)?;
        let (to_mount, to_parent, to_name) = self.resolve_parent(to)?;
        if from_mount != to_mount {
            return Err(FsError::NotSupported);
        }
        {
            let fs = self.mounts.fs_mut(from_mount).ok_or(FsError::NotFound)?;
            fs.rename(from_parent, &from_name, to_parent, &to_name)?;
        }
        self.mark_dirty(from_mount);
        Ok(())
    }

    /// Create a hardlink at `to` pointing at the same inode already
    /// named by `from`. Cross-mount links are rejected with
    /// `NotSupported` (→ ENOTSUP) — a hardlink can't span filesystems
    /// because inode numbers are per-mount. Within a single mount
    /// this dispatches to [`Filesystem::link`] on the owning mount;
    /// tmpfs overrides with the real `nlink++` path, devfs / procfs
    /// inherit the default (`ReadOnly` → EROFS).
    pub fn link(&mut self, from: &str, to: &str) -> Result<(), FsError> {
        let (from_mount, from_parent, from_name) = self.resolve_parent(from)?;
        let (to_mount, to_parent, to_name) = self.resolve_parent(to)?;
        if from_mount != to_mount {
            return Err(FsError::NotSupported);
        }
        {
            let fs = self.mounts.fs_mut(from_mount).ok_or(FsError::NotFound)?;
            fs.link(from_parent, &from_name, to_parent, &to_name)?;
        }
        self.mark_dirty(from_mount);
        Ok(())
    }

    /// Create a symlink at `link_path` that holds the arbitrary UTF-8
    /// `target` string. resolve_parent on `link_path` locates the
    /// directory + final component, then dispatches to
    /// [`Filesystem::symlink`] on the owning mount. tmpfs overrides
    /// with the real `TmpNode::SymLink` allocation; devfs / procfs /
    /// opfs inherit the default (`NotSupported` → ENOTSUP).
    pub fn symlink(&mut self, target: &str, link_path: &str) -> Result<Ino, FsError> {
        let (mount_id, parent_ino, name) = self.resolve_parent(link_path)?;
        let ino = {
            let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
            fs.symlink(parent_ino, &name, target)?
        };
        self.mark_dirty(mount_id);
        Ok(ino)
    }

    /// Copy the symlink target at `abs_path` into `out`. Uses
    /// [`Vfs::resolve_nofollow`] so the final component — expected
    /// to be a symlink — is not dereferenced before the readlink
    /// call. Dispatches to [`Filesystem::readlink`] on the owning
    /// mount; tmpfs returns the target string; devfs / procfs /
    /// opfs inherit the default (`NotSupported` → ENOTSUP). A
    /// non-symlink target returns `InvalidArgument` → EINVAL.
    pub fn readlink(&mut self, abs_path: &str, out: &mut [u8]) -> Result<usize, FsError> {
        let (mount_id, ino) = self.resolve_nofollow(abs_path)?;
        let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
        fs.readlink(ino, out)
    }

    /// Stat an absolute path. Uses [`Vfs::resolve_nofollow`] so a
    /// path whose final component is itself a symlink reports the
    /// symlink's own metadata (ty = SymLink), matching POSIX lstat
    /// semantics. Callers that want POSIX stat semantics — follow
    /// the final symlink — should resolve the path themselves via
    /// [`Vfs::resolve`] and call [`Vfs::stat_ino`] on the
    /// resulting `(mount, ino)` pair.
    pub fn stat(&mut self, abs_path: &str) -> Result<FileStat, FsError> {
        let (mount_id, ino) = self.resolve_nofollow(abs_path)?;
        let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
        fs.stat(ino)
    }

    /// Stat `(mount_id, ino)` directly. Mirrors [`Vfs::read_ino`] /
    /// [`Vfs::write_ino`]: the syscall layer already carries the
    /// pair on a `Vnode` fd, so `fd_filestat_get` skips path
    /// resolution by calling this instead of [`Vfs::stat`].
    pub fn stat_ino(&mut self, mount_id: MountId, ino: Ino) -> Result<FileStat, FsError> {
        let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
        fs.stat(ino)
    }

    /// Truncate a regular file to `new_size` bytes.
    pub fn truncate(&mut self, abs_path: &str, new_size: u64) -> Result<(), FsError> {
        let (mount_id, ino) = self.resolve(abs_path)?;
        self.truncate_ino(mount_id, ino, new_size).map(|_| ())
    }

    /// Truncate `(mount_id, ino)` directly. Mirrors [`Vfs::stat_ino`] /
    /// [`Vfs::set_times_ino`]: the syscall layer already carries the
    /// pair on a `Vnode` fd, so `fd_filestat_set_size` skips path
    /// resolution by calling this instead of [`Vfs::truncate`].
    pub fn truncate_ino(
        &mut self,
        mount_id: MountId,
        ino: Ino,
        new_size: u64,
    ) -> Result<bool, FsError> {
        let changed = {
            let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
            let stat = fs.stat(ino)?;
            fs.truncate(ino, new_size)?;
            stat.ty == NodeType::RegularFile && stat.size != new_size
        };
        if changed {
            self.mark_dirty(mount_id);
        }
        Ok(changed)
    }

    /// Replace an existing path's low nine POSIX permission bits.
    pub fn set_mode(&mut self, abs_path: &str, mode: Mode) -> Result<(), FsError> {
        let (mount_id, ino) = self.resolve(abs_path)?;
        {
            let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
            fs.set_mode(ino, mode)?;
        }
        self.mark_dirty(mount_id);
        Ok(())
    }

    /// Set atim and/or mtim on the vnode at `abs_path`. `None`
    /// means "leave this field unchanged". Pure passthrough to
    /// [`Filesystem::set_times`]; a zero-effect call (both
    /// `None`) still round-trips through the resolve step so a
    /// userland caller using the syscall as a permission probe
    /// gets an `ENOENT` for a missing path + a success for an
    /// existing one, rather than a spurious `ReadOnly` short-cut.
    pub fn set_times(
        &mut self,
        abs_path: &str,
        atime_ns: Option<NanosSinceEpoch>,
        mtime_ns: Option<NanosSinceEpoch>,
    ) -> Result<(), FsError> {
        let (mount_id, ino) = self.resolve(abs_path)?;
        if atime_ns.is_none() && mtime_ns.is_none() {
            return Ok(());
        }
        self.set_times_ino(mount_id, ino, atime_ns, mtime_ns)
    }

    /// Set atim and/or mtim on `(mount_id, ino)` directly. Mirrors
    /// [`Vfs::stat_ino`]: the syscall layer already carries the
    /// pair on a `Vnode` fd, so `fd_filestat_set_times` skips path
    /// resolution by calling this instead of [`Vfs::set_times`].
    /// A zero-effect call (both `None`) returns `Ok(())` without
    /// touching the filesystem — there's no permission-probe value
    /// here because the caller already holds a valid fd, which
    /// means path-resolve + rights were already checked at
    /// path_open time.
    pub fn set_times_ino(
        &mut self,
        mount_id: MountId,
        ino: Ino,
        atime_ns: Option<NanosSinceEpoch>,
        mtime_ns: Option<NanosSinceEpoch>,
    ) -> Result<(), FsError> {
        if atime_ns.is_none() && mtime_ns.is_none() {
            return Ok(());
        }
        {
            let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
            fs.set_times(ino, atime_ns, mtime_ns)?;
        }
        self.mark_dirty(mount_id);
        Ok(())
    }
}

impl Default for Vfs {
    fn default() -> Self {
        Vfs::new()
    }
}
