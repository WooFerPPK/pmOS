//! PMos extension syscalls.
//!
//! Mirrors `contracts/syscalls.md §3` exactly. Numbered from 0x1000
//! upward, grouped by subsystem with deliberate gaps between groups
//! (0x1000 IPC, 0x1100 proc, 0x1200 display, 0x1300 cap, 0x1400 mount
//! + fs_watch, 0x1500 host import) so future additions in one group
//! do not shift opcodes in another.

// --- 3.1 IPC (AF_UNIX-equivalent) -----------------------------------------
pub const IPC_SOCKET:   u16 = 0x1000;
pub const IPC_BIND:     u16 = 0x1001;
pub const IPC_LISTEN:   u16 = 0x1002;
pub const IPC_CONNECT:  u16 = 0x1003;
pub const IPC_ACCEPT:   u16 = 0x1004;
pub const IPC_SEND:     u16 = 0x1005;
pub const IPC_RECV:     u16 = 0x1006;
/// Create a pipe pair (POSIX `pipe(2)` equivalent). Installs a
/// `FdObject::PipeRead` fd and a `FdObject::PipeWrite` fd on the
/// caller, writing both fds as two u32s LE into the caller's heap
/// scratch at `heap[0..8]`. Requires `heap_len == 8`.
pub const IPC_PIPE:     u16 = 0x1007;

/// `ipc_socket` types.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SocketType {
    Stream = 0,
    Dgram  = 1,
}

/// `ipc_socket` flags.
pub mod ipc_flags {
    pub const NONBLOCK: u16 = 0x0001;
    pub const CLOEXEC:  u16 = 0x0002;
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
pub const PROC_SPAWN:     u16 = 0x1100;
pub const PROC_WAIT:      u16 = 0x1101;
pub const PROC_KILL:      u16 = 0x1102;
pub const PROC_SELF:      u16 = 0x1103;
pub const PROC_PARENT:    u16 = 0x1104;
pub const PROC_CAPS_GET:  u16 = 0x1105;

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
    pub stdin_fd:  Option<u32>,
    pub stdout_fd: Option<u32>,
    pub stderr_fd: Option<u32>,
    /// Extra (parent_fd, child_fd) pairs dup'd into the child.
    pub extra_fds: &'a [(u32, u32)],
    /// New working directory; `None` → inherit caller's cwd.
    pub cwd: Option<&'a str>,
    /// Subset of caller's caps to pass to the child; `None` → inherit all.
    pub caps: Option<u64>,
}

/// `proc_wait` options.
pub mod wait_opts {
    pub const WNOHANG:   u32 = 0x0001;
    pub const WUNTRACED: u32 = 0x0002;
}

/// Signal numbers, POSIX-style, used by `proc_kill` and `proc_raise`.
pub mod sig {
    pub const SIGHUP:  u16 = 1;
    pub const SIGINT:  u16 = 2;
    pub const SIGQUIT: u16 = 3;
    pub const SIGILL:  u16 = 4;
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
pub const CAP_LIST:  u16 = 0x1301;
pub const CAP_GRANT: u16 = 0x1302;

// --- 3.5 Mount management (privileged) ------------------------------------
pub const MOUNT:  u16 = 0x1400;
pub const UMOUNT: u16 = 0x1401;

// --- 3.7 Filesystem watch (added in v1.1) ---------------------------------
pub const FS_WATCH: u16 = 0x1402;

/// `fs_watch` event record kinds.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FsWatchKind {
    Modified    = 1,
    Created     = 2,
    Deleted     = 3,
    RenamedFrom = 4,
    RenamedTo   = 5,
}

/// `fs_watch` flag bits.
pub mod fs_watch_flags {
    pub const RECURSIVE:       u32 = 0x0001;
    pub const COALESCE_MODIFY: u32 = 0x0002;
}

// --- 3.6 Host import (added in v1.1) --------------------------------------
pub const HOST_FILE_RECV: u16 = 0x1500;

/// The lowest opcode value that belongs to the extension namespace.
pub const FIRST: u16 = 0x1000;
/// One past the highest extension opcode the dispatcher recognises.
pub const LAST_EXCL: u16 = 0x1501;

/// True iff `opcode` is a PMos extension opcode.
#[inline]
pub const fn is_ext(opcode: u16) -> bool {
    opcode >= FIRST && opcode < LAST_EXCL
}
