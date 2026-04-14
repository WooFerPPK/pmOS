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
use alloc::string::String;
use alloc::vec::Vec;

pub mod mount;
pub mod path;

pub use mount::{Mount, MountId, MountTable};

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

    /// Flush any buffered writes. Default no-op.
    fn sync(&mut self) -> Result<(), FsError> {
        Ok(())
    }

    /// A short human name used in `/proc/mounts`-style output.
    /// Examples: "tmpfs", "devfs", "procfs", "opfs".
    fn kind_name(&self) -> &'static str;
}

/// The kernel-wide virtual filesystem.
pub struct Vfs {
    mounts: MountTable,
}

impl Vfs {
    pub fn new() -> Self {
        Vfs {
            mounts: MountTable::new(),
        }
    }

    /// Install a mount at an absolute path. Every subsequent
    /// resolve() that lands on or below this path is routed to
    /// the mounted filesystem. The path is normalised before
    /// insertion — `mount("/dev/", ...)` and `mount("/dev", ...)`
    /// are equivalent.
    pub fn mount(
        &mut self,
        mountpoint: &str,
        fs: Box<dyn Filesystem>,
    ) -> Result<MountId, FsError> {
        let normalised = path::normalize(mountpoint);
        self.mounts.insert(normalised, fs)
    }

    /// Unmount. Returns the filesystem trait object so the
    /// caller can perform any last sync.
    pub fn umount(&mut self, mountpoint: &str) -> Result<Box<dyn Filesystem>, FsError> {
        let normalised = path::normalize(mountpoint);
        self.mounts.remove(&normalised)
    }

    /// Number of currently-installed mounts.
    pub fn mount_count(&self) -> usize {
        self.mounts.len()
    }

    /// Resolve an absolute path to `(mount_id, ino)`. Follows
    /// component lookups through the owning filesystem. Does
    /// not follow symlinks in v1.
    pub fn resolve(&mut self, abs_path: &str) -> Result<(MountId, Ino), FsError> {
        let canon = path::normalize(abs_path);
        let (mount_id, rel) = self
            .mounts
            .longest_prefix(&canon)
            .ok_or(FsError::NotFound)?;
        let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
        let mut ino = fs.root();
        for component in path::components(&rel) {
            ino = fs.lookup(ino, component)?;
        }
        Ok((mount_id, ino))
    }

    /// Resolve a path to its parent directory and the final
    /// component name. Used by create/mkdir/unlink/rmdir to
    /// find the target directory before performing the op.
    pub fn resolve_parent<'a>(
        &mut self,
        abs_path: &'a str,
    ) -> Result<(MountId, Ino, String), FsError> {
        let canon = path::normalize(abs_path);
        let (parent_path, name) = path::split_last(&canon).ok_or(FsError::InvalidArgument)?;
        if name.is_empty() {
            return Err(FsError::InvalidArgument);
        }
        let (mount_id, parent_ino) = self.resolve(&parent_path)?;
        Ok((mount_id, parent_ino, name))
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
    /// rather than the filesystem.
    pub fn open(&mut self, abs_path: &str) -> Result<(MountId, Ino, NodeType), FsError> {
        let (mount_id, ino) = self.resolve(abs_path)?;
        let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
        let st = fs.stat(ino)?;
        Ok((mount_id, ino, st.ty))
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
        let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
        fs.write(ino, offset, buf)
    }

    /// List the entries of the directory at an absolute path.
    pub fn readdir(&mut self, abs_path: &str) -> Result<Vec<DirEntry>, FsError> {
        let (mount_id, ino) = self.resolve(abs_path)?;
        let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
        let mut out = Vec::new();
        fs.readdir(ino, &mut out)?;
        Ok(out)
    }

    /// Create a regular file at an absolute path.
    pub fn create(&mut self, abs_path: &str, mode: Mode) -> Result<Ino, FsError> {
        let (mount_id, parent_ino, name) = self.resolve_parent(abs_path)?;
        let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
        fs.create(parent_ino, &name, mode)
    }

    /// Create a directory at an absolute path.
    pub fn mkdir(&mut self, abs_path: &str, mode: Mode) -> Result<Ino, FsError> {
        let (mount_id, parent_ino, name) = self.resolve_parent(abs_path)?;
        let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
        fs.mkdir(parent_ino, &name, mode)
    }

    /// Unlink (remove) a regular file at an absolute path.
    pub fn unlink(&mut self, abs_path: &str) -> Result<(), FsError> {
        let (mount_id, parent_ino, name) = self.resolve_parent(abs_path)?;
        let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
        fs.unlink(parent_ino, &name)
    }

    /// Remove an empty directory at an absolute path.
    pub fn rmdir(&mut self, abs_path: &str) -> Result<(), FsError> {
        let (mount_id, parent_ino, name) = self.resolve_parent(abs_path)?;
        let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
        fs.rmdir(parent_ino, &name)
    }

    /// Rename within the same filesystem. Cross-mount renames
    /// are rejected (caller must use create+write+unlink).
    pub fn rename(&mut self, from: &str, to: &str) -> Result<(), FsError> {
        let (from_mount, from_parent, from_name) = self.resolve_parent(from)?;
        let (to_mount, to_parent, to_name) = self.resolve_parent(to)?;
        if from_mount != to_mount {
            return Err(FsError::NotSupported);
        }
        let fs = self.mounts.fs_mut(from_mount).ok_or(FsError::NotFound)?;
        fs.rename(from_parent, &from_name, to_parent, &to_name)
    }

    /// Stat an absolute path.
    pub fn stat(&mut self, abs_path: &str) -> Result<FileStat, FsError> {
        let (mount_id, ino) = self.resolve(abs_path)?;
        let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
        fs.stat(ino)
    }

    /// Truncate a regular file to `new_size` bytes.
    pub fn truncate(&mut self, abs_path: &str, new_size: u64) -> Result<(), FsError> {
        let (mount_id, ino) = self.resolve(abs_path)?;
        let fs = self.mounts.fs_mut(mount_id).ok_or(FsError::NotFound)?;
        fs.truncate(ino, new_size)
    }
}

impl Default for Vfs {
    fn default() -> Self {
        Vfs::new()
    }
}
