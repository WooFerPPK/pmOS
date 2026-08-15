//! PMos extension syscalls.
//!
//! Mirrors `contracts/syscalls.md §3` exactly. Numbered from 0x1000
//! upward, grouped by subsystem with deliberate gaps between groups
//! (0x1000 IPC, 0x1100 proc, 0x1200 display, 0x1300 cap, 0x1400 mount/fs_watch,
//! 0x1500 host file transfer) so future additions in one group
//! do not shift opcodes in another.

// --- 3.1 IPC (AF_UNIX-equivalent) -----------------------------------------
pub const IPC_SOCKET: u16 = 0x1000;
pub const IPC_BIND: u16 = 0x1001;
pub const IPC_LISTEN: u16 = 0x1002;
pub const IPC_CONNECT: u16 = 0x1003;
pub const IPC_ACCEPT: u16 = 0x1004;
pub const IPC_SEND: u16 = 0x1005;
pub const IPC_RECV: u16 = 0x1006;
/// Create a pipe pair (POSIX `pipe(2)` equivalent). Installs a
/// `FdObject::PipeRead` fd and a `FdObject::PipeWrite` fd on the
/// caller, writing both fds as two u32s LE into the caller's heap
/// scratch at `heap[0..8]`. Requires `heap_len == 8`.
pub const IPC_PIPE: u16 = 0x1007;
/// Return the kernel-captured capability set of the process on the
/// other end of a connected IPC socket. This is PMos's narrow
/// `SO_PEERCRED` equivalent: possession of the connected fd is the
/// authority to inspect that endpoint, and no client-provided bytes
/// participate in the result.
pub const IPC_PEER_CAPS: u16 = 0x1008;
/// Return the kernel-captured pid of the process on the other end
/// of a connected IPC socket. Like [`IPC_PEER_CAPS`], this is an
/// fd-scoped `SO_PEERCRED` query whose result cannot be supplied or
/// changed by protocol payloads.
pub const IPC_PEER_PID: u16 = 0x1009;

/// `ipc_socket` wire types.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SocketType {
    /// The only constructible v1 local-IPC socket type.
    Stream = 0,
    /// ABI-reserved for future record-preserving datagrams. V1 returns
    /// `ENOTSUP` atomically rather than constructing a stream-shaped object.
    Dgram = 1,
}

/// `ipc_socket` flags.
pub mod ipc_flags {
    pub const NONBLOCK: u16 = 0x0001;
    pub const CLOEXEC: u16 = 0x0002;
}

/// Flag bits accepted by syscalls that have a `flags: u16` parameter
/// on the opcode surface. Mirrors the POSIX `SOCK_*` / `O_*` flag
/// families but PMos-namespaced.
pub mod accept_flags {
    /// Return -EAGAIN on empty backlog instead of parking the caller.
    /// Default (absence of this bit) is POSIX `accept(2)` — block
    /// until a client connects or the process is signalled.
    pub const NONBLOCK: u16 = 0x0001;
}

// --- 3.2 Process management (posix_spawn-style, no fork) ------------------
pub const PROC_SPAWN: u16 = 0x1100;
pub const PROC_WAIT: u16 = 0x1101;
pub const PROC_KILL: u16 = 0x1102;
pub const PROC_SELF: u16 = 0x1103;
pub const PROC_PARENT: u16 = 0x1104;
pub const PROC_CAPS_GET: u16 = 0x1105;

/// Pid type. Zero means "unset / no parent".
pub type Pid = i32;

/// `proc_spawn` manifest as it travels over the ring transport.
///
/// This is the in-memory version. Over the wire it is serialised
/// into the caller's heap scratch region; the record in the
/// request slot holds only the offset/length of the serialised
/// payload.
#[derive(Debug)]
pub struct SpawnManifest<'a> {
    pub path: &'a str,
    pub argv: &'a [&'a str],
    pub envp: &'a [(&'a str, &'a str)],
    /// fd in the caller to dup into child as stdin. `None` → inherit.
    pub stdin_fd: Option<u32>,
    pub stdout_fd: Option<u32>,
    pub stderr_fd: Option<u32>,
    /// Extra (parent_fd, child_fd) pairs dup'd into the child.
    pub extra_fds: &'a [(u32, u32)],
    /// New working directory; `None` → inherit caller's cwd.
    pub cwd: Option<&'a str>,
    /// Subset of caller's caps to pass to the child; `None` → inherit all.
    pub caps: Option<u64>,
}

/// Versioned packed representation of [`SpawnManifest`] used by
/// `PROC_SPAWN` and the additive `pmos_ext.proc_spawn_manifest` host import.
///
/// The request args contain [`MAGIC`] followed by the blob length and
/// [`VERSION`]. The heap blob starts with this fixed header, then stores path,
/// cwd, argv entries, env entries, and extra-fd pairs in that order. All
/// integers are little-endian; strings are length-delimited UTF-8 without NUL
/// terminators.
pub mod spawn_v1 {
    /// `SPN1` in little-endian form. This is far larger than the 32 KiB ring
    /// heap, so it can never be mistaken for the legacy `path_len` field.
    pub const MAGIC: u32 = u32::from_le_bytes(*b"SPN1");
    pub const VERSION: u16 = 1;
    pub const HEADER_LEN: usize = 48;

    pub const FLAG_CWD: u16 = 0x0001;
    pub const FLAG_CAPS: u16 = 0x0002;
    pub const KNOWN_FLAGS: u16 = FLAG_CWD | FLAG_CAPS;

    pub const OFF_MAGIC: usize = 0;
    pub const OFF_VERSION: usize = 4;
    pub const OFF_FLAGS: usize = 6;
    pub const OFF_TOTAL_LEN: usize = 8;
    pub const OFF_PATH_LEN: usize = 12;
    pub const OFF_CWD_LEN: usize = 14;
    pub const OFF_ARGC: usize = 16;
    pub const OFF_ENVC: usize = 18;
    pub const OFF_EXTRA_FD_COUNT: usize = 20;
    pub const OFF_RESERVED_U16: usize = 22;
    pub const OFF_STDIN_FD: usize = 24;
    pub const OFF_STDOUT_FD: usize = 28;
    pub const OFF_STDERR_FD: usize = 32;
    pub const OFF_RESERVED_U32: usize = 36;
    pub const OFF_CAPS: usize = 40;

    /// Signed fd marker meaning "inherit the corresponding parent stdio fd".
    pub const INHERIT_FD: i32 = -1;

    /// Maximum size of a dynamically loaded executable sourced from the VFS.
    pub const MAX_EXECUTABLE_BYTES: usize = 16 * 1024 * 1024;
}

/// `proc_wait` options.
pub mod wait_opts {
    pub const WNOHANG: u32 = 0x0001;
    pub const WUNTRACED: u32 = 0x0002;
}

/// Signal numbers, POSIX-style, used by `proc_kill` and `proc_raise`.
pub mod sig {
    pub const SIGHUP: u16 = 1;
    pub const SIGINT: u16 = 2;
    pub const SIGQUIT: u16 = 3;
    pub const SIGILL: u16 = 4;
    pub const SIGTRAP: u16 = 5;
    pub const SIGABRT: u16 = 6;
    pub const SIGKILL: u16 = 9;
    pub const SIGPIPE: u16 = 13;
    pub const SIGTERM: u16 = 15;
    pub const SIGCHLD: u16 = 17;
    pub const SIGSTOP: u16 = 19;
    pub const SIGCONT: u16 = 18;
}

// --- 3.3 Display server ---------------------------------------------------
pub const DISPLAY_CONNECT: u16 = 0x1200;
/// Bind the kernel-wide `/run/display` listening socket. Requires
/// `Cap::DisplayServer`. Returns a listener fd the caller can
/// `ipc_accept` from; this is how a display-server userland
/// process establishes itself as the owner of the display
/// protocol socket.
pub const DISPLAY_BIND: u16 = 0x1201;

// --- 3.4 Capability management --------------------------------------------
pub const CAP_CHECK: u16 = 0x1300;
pub const CAP_LIST: u16 = 0x1301;
pub const CAP_GRANT: u16 = 0x1302;

// --- 3.5 Mount management (privileged) ------------------------------------
pub const MOUNT: u16 = 0x1400;
pub const UMOUNT: u16 = 0x1401;

/// Mount-flag bits passed to the `mount` opcode.
///
/// The wire encoding piggybacks on [`abi::ring::Request::flags`] (a
/// per-request `u16` field documented as "reserved in v1, MUST be
/// zero"). The MOUNT handler reads `req.flags as u32` and forwards
/// them to `Kernel::mount`. This avoids touching the args window
/// (already 16 bytes full of path_ptr/path_len/fstype_ptr/fstype_len)
/// and keeps the `Request` struct layout stable. Only the lowest
/// `u16` of mount flags is reachable in v1; future flag bits beyond
/// bit 15 (e.g. NOATIME, NODEV) will require a wire revision.
pub mod mount_flags {
    /// `MS_REMOUNT`: atomically change an existing mount's flags
    /// without unmounting + remounting. The `source` and `fstype`
    /// arguments are IGNORED when this bit is set (POSIX semantics);
    /// the kernel locates the mount by `target` and mutates the
    /// in-table flags field in place. The canonical use case is
    /// remounting the root filesystem read-only at shutdown
    /// (umount-of-root is impossible, so the only path to a
    /// post-shutdown read-only state is in-place flag mutation).
    /// `target` not currently a mountpoint → -EINVAL.
    pub const MOUNT_REMOUNT: u32 = 1 << 0;
}

// --- 3.7 Filesystem watch (added in v1.1) ---------------------------------
pub const FS_WATCH: u16 = 0x1402;
/// Change the POSIX permission bits on an existing VFS node.
pub const FS_CHMOD: u16 = 0x1403;

/// `fs_watch` event record kinds.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FsWatchKind {
    Modified = 1,
    Created = 2,
    Deleted = 3,
    RenamedFrom = 4,
    RenamedTo = 5,
}

/// `fs_watch` event-mask bits used by the `mask` argument and by the
/// per-event header. Distinct from [`FsWatchKind`] (which numbers the
/// event variants 1..5 in the wire-format record): the mask is a
/// bit-OR of these constants, so `WATCH_CREATE | WATCH_MODIFY`
/// subscribes a watch to both create AND modify notifications. v1
/// implements `WATCH_CREATE`, `WATCH_DELETE`, `WATCH_MODIFY`; a successful
/// rename is represented as ordered DELETE/CREATE records on the affected
/// parent directories. Any unknown bit in the mask → `-EINVAL` at register
/// time.
pub const WATCH_CREATE: u32 = 0x0001;
pub const WATCH_DELETE: u32 = 0x0002;
pub const WATCH_MODIFY: u32 = 0x0004;

/// Bit-mask of every `WATCH_*` flag this kernel understands. Used by
/// the `fs_watch` opcode handler to reject unknown bits before
/// allocating a watch (atomic-reject — no half-installed watch on a
/// malformed mask).
pub const WATCH_MASK_ALL: u32 = WATCH_CREATE | WATCH_DELETE | WATCH_MODIFY;

/// Fixed v1 record size: `mask: u32` followed by `inode: u32`, both LE.
pub const FS_WATCH_EVENT_SIZE: usize = 8;

/// `fs_watch` flag bits.
pub mod fs_watch_flags {
    pub const RECURSIVE: u32 = 0x0001;
    pub const COALESCE_MODIFY: u32 = 0x0002;
}

// --- 3.6 Host file transfer (v1.1 import; v1.3 picker/export) --------------
pub const HOST_FILE_RECV: u16 = 0x1500;
pub const HOST_FILE_PICK: u16 = 0x1501;
pub const HOST_FILE_SEND: u16 = 0x1502;

/// Shared `/run/host-files` notification framing and resource limits.
pub mod host_file {
    pub const ENDPOINT: &str = "/run/host-files";
    pub const NOTIFICATION_MAGIC: u32 = 0x4648_4d50; // LE bytes "PMHF"
    pub const NOTIFICATION_VERSION: u16 = 1;
    pub const NOTIFICATION_HEADER_LEN: usize = 28;
    pub const MAX_NAME_BYTES: usize = 255;
    pub const MAX_MIME_BYTES: usize = 127;
    pub const MAX_IMPORT_BYTES: usize = 16 * 1024 * 1024;
    pub const MAX_IMPORT_BYTES_TOTAL: usize = 32 * 1024 * 1024;
    pub const MAX_LIVE_IMPORTS: usize = 64;
    pub const MAX_DOWNLOAD_BYTES: usize = 16 * 1024 * 1024;
    pub const MAX_DOWNLOAD_BYTES_TOTAL: usize = 32 * 1024 * 1024;
}

/// The lowest opcode value that belongs to the extension namespace.
pub const FIRST: u16 = 0x1000;
/// One past the highest extension opcode the dispatcher recognises.
pub const LAST_EXCL: u16 = 0x1503;

/// True iff `opcode` is a PMos extension opcode.
#[inline]
pub const fn is_ext(opcode: u16) -> bool {
    opcode >= FIRST && opcode < LAST_EXCL
}

#[cfg(test)]
mod tests {
    use super::SocketType;

    #[test]
    fn socket_type_discriminants_keep_stream_and_reserved_dgram_values() {
        assert_eq!(SocketType::Stream as u8, 0);
        assert_eq!(SocketType::Dgram as u8, 1);
    }
}
