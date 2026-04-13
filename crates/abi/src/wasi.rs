//! WASI preview 1 opcode identifiers.
//!
//! Mirrors `contracts/syscalls.md §2`. These opcodes occupy the
//! low half of the opcode space (0x0000..0x0FFF). Extension
//! opcodes start at 0x1000 in `crate::ext`.
//!
//! PMos assigns its own numeric values; we do not promise
//! binary compatibility with any specific WASI runtime's wire
//! layout. Rust `wasm32-wasi` userland crates link against the
//! WASI *function names* provided by the kernel, not these
//! numeric opcodes directly — the opcodes are the internal
//! dispatch identity used by the SAB ring transport.
//!
//! The numbering below is dense (no gaps) and grouped by
//! category for readability.

// --- args / environ -------------------------------------------------------
pub const ARGS_GET:            u16 = 0x0001;
pub const ARGS_SIZES_GET:      u16 = 0x0002;
pub const ENVIRON_GET:         u16 = 0x0003;
pub const ENVIRON_SIZES_GET:   u16 = 0x0004;

// --- clocks ---------------------------------------------------------------
pub const CLOCK_RES_GET:       u16 = 0x0010;
pub const CLOCK_TIME_GET:      u16 = 0x0011;

/// WASI clock IDs.
pub const CLOCKID_REALTIME:           u32 = 0;
pub const CLOCKID_MONOTONIC:          u32 = 1;
pub const CLOCKID_PROCESS_CPUTIME_ID: u32 = 2;
pub const CLOCKID_THREAD_CPUTIME_ID:  u32 = 3;

// --- file / fd ------------------------------------------------------------
pub const FD_ADVISE:                u16 = 0x0020;
pub const FD_ALLOCATE:              u16 = 0x0021;
pub const FD_CLOSE:                 u16 = 0x0022;
pub const FD_DATASYNC:              u16 = 0x0023;
pub const FD_FDSTAT_GET:            u16 = 0x0024;
pub const FD_FDSTAT_SET_FLAGS:      u16 = 0x0025;
pub const FD_FDSTAT_SET_RIGHTS:     u16 = 0x0026;
pub const FD_FILESTAT_GET:          u16 = 0x0027;
pub const FD_FILESTAT_SET_SIZE:     u16 = 0x0028;
pub const FD_FILESTAT_SET_TIMES:    u16 = 0x0029;
pub const FD_PREAD:                 u16 = 0x002A;
pub const FD_PRESTAT_GET:           u16 = 0x002B;
pub const FD_PRESTAT_DIR_NAME:      u16 = 0x002C;
pub const FD_PWRITE:                u16 = 0x002D;
pub const FD_READ:                  u16 = 0x002E;
pub const FD_READDIR:               u16 = 0x002F;
pub const FD_RENUMBER:              u16 = 0x0030;
pub const FD_SEEK:                  u16 = 0x0031;
pub const FD_SYNC:                  u16 = 0x0032;
pub const FD_TELL:                  u16 = 0x0033;
pub const FD_WRITE:                 u16 = 0x0034;

// --- path -----------------------------------------------------------------
pub const PATH_CREATE_DIRECTORY:    u16 = 0x0040;
pub const PATH_FILESTAT_GET:        u16 = 0x0041;
pub const PATH_FILESTAT_SET_TIMES:  u16 = 0x0042;
pub const PATH_LINK:                u16 = 0x0043;
pub const PATH_OPEN:                u16 = 0x0044;
pub const PATH_READLINK:            u16 = 0x0045;
pub const PATH_REMOVE_DIRECTORY:    u16 = 0x0046;
pub const PATH_RENAME:              u16 = 0x0047;
pub const PATH_SYMLINK:             u16 = 0x0048;
pub const PATH_UNLINK_FILE:         u16 = 0x0049;

// --- poll / random / sched ------------------------------------------------
pub const POLL_ONEOFF:         u16 = 0x0050;
pub const RANDOM_GET:          u16 = 0x0051;
pub const SCHED_YIELD:         u16 = 0x0052;

// --- proc -----------------------------------------------------------------
pub const PROC_EXIT:           u16 = 0x0060;
pub const PROC_RAISE:          u16 = 0x0061;

// --- socket (WASI preview 1 subset) ---------------------------------------
pub const SOCK_ACCEPT:         u16 = 0x0070;
pub const SOCK_RECV:           u16 = 0x0071;
pub const SOCK_SEND:           u16 = 0x0072;
pub const SOCK_SHUTDOWN:       u16 = 0x0073;

/// File open flags the kernel's `path_open` understands.
pub mod oflags {
    pub const CREAT:     u16 = 0x0001;
    pub const DIRECTORY: u16 = 0x0002;
    pub const EXCL:      u16 = 0x0004;
    pub const TRUNC:     u16 = 0x0008;
}

/// Fdflags used with `fd_fdstat_set_flags`.
pub mod fdflags {
    pub const APPEND:   u16 = 0x0001;
    pub const DSYNC:    u16 = 0x0002;
    pub const NONBLOCK: u16 = 0x0004;
    pub const RSYNC:    u16 = 0x0008;
    pub const SYNC:     u16 = 0x0010;
}

/// Seek whence for `fd_seek`.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Whence {
    Set = 0,
    Cur = 1,
    End = 2,
}

/// The lowest opcode value that belongs to the WASI preview 1 namespace.
pub const FIRST: u16 = 0x0001;
/// One past the highest WASI opcode the dispatcher recognises. Extension
/// opcodes start at `ext::FIRST` (0x1000).
pub const LAST_EXCL: u16 = 0x0080;

/// True iff `opcode` is in the WASI preview 1 range.
#[inline]
pub const fn is_wasi(opcode: u16) -> bool {
    opcode >= FIRST && opcode < LAST_EXCL
}
