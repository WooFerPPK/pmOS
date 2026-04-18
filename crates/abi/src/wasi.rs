//! WASI preview 1 opcode identifiers.
//!
//! Mirrors `contracts/syscalls.md §2`. These opcodes occupy the
//! low half of the opcode space (0x0000..0x0FFF). Extension
//! opcodes start at 0x1000 in `crate::ext`.
//!
//! PMos assigns its own numeric values; we do not promise
//! binary compatibility with any specific WASI runtime's wire
//! layout. Rust `wasm32-wasip1` userland crates link against
//! the WASI *function names* provided by the kernel, not these
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

/// Lookup flags used with `path_open` (dirflags arg) and
/// `path_filestat_get` (lookup_flags arg). WASI defines a single
/// bit — SYMLINK_FOLLOW — governing whether the final component
/// of a path is dereferenced when it is itself a symlink.
/// Intermediate components always follow symlinks regardless of
/// this bit.
pub mod lookupflags {
    /// Follow the final symlink (stat-like). When clear, the
    /// final symlink is NOT dereferenced (lstat-like; open on a
    /// symlink yields the symlink's own vnode).
    pub const SYMLINK_FOLLOW: u32 = 0x0001;
}

/// Sdflags used with `sock_shutdown`. WASI models shutdown as a
/// pair of independent bits (read-side and write-side) so userland
/// can half-close one direction while leaving the other open.
///
/// v1's IpcTable has no half-close primitive — `close_socket`
/// tears down both directions at once — so the dispatcher accepts
/// only `RD | WR` (full close, mapped to `close_socket`) and
/// rejects the half-close combinations with `ENOTSUP`. A zero
/// `how` value (neither bit set) rejects with `EINVAL` since
/// shutting down nothing is meaningless. Any bits beyond RD | WR
/// also reject with `EINVAL` — WASI reserves those but v1 is
/// strict about input validation.
pub mod sdflags {
    pub const RD: u8 = 0x1;
    pub const WR: u8 = 0x2;
}

/// Seek whence for `fd_seek`.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Whence {
    Set = 0,
    Cur = 1,
    End = 2,
}

/// `fstflags` bitfield shared by `fd_filestat_set_times` and
/// `path_filestat_set_times`. Each pair of `_NOW`/explicit bits is
/// mutually exclusive — setting both for the same field (`SET_ATIM |
/// SET_ATIM_NOW`, `SET_MTIM | SET_MTIM_NOW`) is `EINVAL`. With zero
/// bits set the call is a no-op success.
pub mod fstflags {
    pub const SET_ATIM:     u16 = 0x1;
    pub const SET_ATIM_NOW: u16 = 0x2;
    pub const SET_MTIM:     u16 = 0x4;
    pub const SET_MTIM_NOW: u16 = 0x8;
}

/// `filestat_t` wire layout shared by `FD_FILESTAT_GET` and
/// `PATH_FILESTAT_GET`. Matches WASI preview 1's C ABI: 64 bytes,
/// little-endian, with the 1-byte `filetype` padded out to 8 bytes
/// so `nlink` onwards stays u64-aligned.
pub mod filestat {
    pub const SIZE:         usize = 64;
    pub const OFF_DEV:      usize = 0;
    pub const OFF_INO:      usize = 8;
    pub const OFF_FILETYPE: usize = 16;
    pub const OFF_NLINK:    usize = 24;
    pub const OFF_SIZE:     usize = 32;
    pub const OFF_ATIM:     usize = 40;
    pub const OFF_MTIM:     usize = 48;
    pub const OFF_CTIM:     usize = 56;
}

/// Wire layouts + flag tables for `POLL_ONEOFF` (opcode 0x0050).
///
/// Mirrors WASI preview 1's `subscription_t` + `event_t` C ABI:
///
/// `subscription_t` is 48 bytes (each caller-supplied subscription):
///
///   offset  0: userdata     u64  (opaque — echoed verbatim into the
///                                 matching `event.userdata` so the
///                                 caller can correlate subscriptions
///                                 and events without tracking index
///                                 positions)
///   offset  8: tag          u8   (`eventtype::*`: CLOCK / FD_READ / FD_WRITE)
///   offset  9: padding (7 bytes; must be zero)
///   offset 16: payload — variant by tag (see below)
///
/// When `tag == eventtype::CLOCK`, the payload is a
/// `subscription_clock_t` at offset 16..48:
///
///   offset 16: clock_id     u32  (`CLOCKID_MONOTONIC` / `CLOCKID_REALTIME`;
///                                 cputime ids emit a per-subscription
///                                 ENOTSUP event, invalid ids emit EINVAL)
///   offset 20: padding (4 bytes)
///   offset 24: timeout      u64  (absolute ns since an epoch if
///                                 `subclockflags::ABSTIME` is set,
///                                 relative ns otherwise)
///   offset 32: precision    u64  (advisory; v1 ignores — the Platform
///                                 clock is nanosecond-granular)
///   offset 40: flags        u16  (`subclockflags::*`)
///   offset 42: padding (6 bytes)
///
/// When `tag == eventtype::FD_READ` or `tag == eventtype::FD_WRITE`,
/// the payload is a `subscription_fd_readwrite_t` at offset 16..48:
///
///   offset 16: fd           u32
///   offset 20: padding (28 bytes — the fd-readwrite payload only
///              decodes four bytes; the rest keeps the outer struct
///              at 48 bytes for a fixed stride)
///
/// `event_t` is 32 bytes (each kernel-emitted event):
///
///   offset  0: userdata     u64  (echoed from the triggering subscription)
///   offset  8: error        u16  (per-subscription errno — 0 on success,
///                                 EBADF / EINVAL / ENOTSUP for error
///                                 cases that are meaningful per-entry
///                                 rather than aborting the whole syscall)
///   offset 10: type         u8   (`eventtype::*` — echo of the subscription
///                                 tag so the caller can quickly filter by
///                                 kind without re-decoding the userdata
///                                 → subscription pairing)
///   offset 11: padding (5 bytes)
///   offset 16: fd_readwrite.nbytes u64  (meaningful for FD_READ /
///                                        FD_WRITE — estimated bytes
///                                        ready to read / space to write;
///                                        zero for CLOCK events)
///   offset 24: fd_readwrite.flags  u16  (`eventrwflags::*` — only
///                                        FD_READWRITE_HANGUP fires in v1)
///   offset 26: padding (6 bytes)
pub mod poll {
    pub const SUBSCRIPTION_SIZE: usize = 48;
    pub const EVENT_SIZE: usize = 32;

    pub const SUB_OFF_USERDATA:   usize = 0;
    pub const SUB_OFF_TAG:        usize = 8;
    pub const SUB_OFF_PAYLOAD:    usize = 16;

    pub const SUB_CLOCK_OFF_ID:        usize = 16;
    pub const SUB_CLOCK_OFF_TIMEOUT:   usize = 24;
    pub const SUB_CLOCK_OFF_PRECISION: usize = 32;
    pub const SUB_CLOCK_OFF_FLAGS:     usize = 40;

    pub const SUB_FDRW_OFF_FD: usize = 16;

    pub const EVENT_OFF_USERDATA:     usize = 0;
    pub const EVENT_OFF_ERROR:        usize = 8;
    pub const EVENT_OFF_TYPE:         usize = 10;
    pub const EVENT_OFF_RW_NBYTES:    usize = 16;
    pub const EVENT_OFF_RW_FLAGS:     usize = 24;
}

/// Wire layout of a single `dirent_t` record produced by
/// `FD_READDIR` (opcode 0x002F). Mirrors WASI preview 1's C ABI:
///
///   offset  0: d_next   u64  (resumption cookie — caller passes
///                              this value as the next call's
///                              cookie to continue the listing
///                              AFTER this entry)
///   offset  8: d_ino    u64
///   offset 16: d_namlen u32
///   offset 20: d_type   u8   (`filetype::*`)
///   offset 21: padding  3B   (pads to 24-byte alignment; the name
///                              bytes follow immediately, with no
///                              trailing null)
///
/// After the 24-byte header, `d_namlen` bytes of UTF-8 name follow
/// in-line. The next entry's header starts immediately after the
/// name — no inter-entry padding. A caller-provided buffer that
/// fills mid-entry receives a truncated final entry; the kernel
/// signals "more entries may exist" by returning value == buf_len.
pub mod dirent {
    pub const HEADER_SIZE:    usize = 24;
    pub const OFF_D_NEXT:     usize = 0;
    pub const OFF_D_INO:      usize = 8;
    pub const OFF_D_NAMLEN:   usize = 16;
    pub const OFF_D_TYPE:     usize = 20;
}

/// WASI `eventtype_t` — identifies a subscription / event variant.
pub mod eventtype {
    pub const CLOCK:    u8 = 0;
    pub const FD_READ:  u8 = 1;
    pub const FD_WRITE: u8 = 2;
}

/// WASI `subclockflags_t` — applies to CLOCK subscriptions.
pub mod subclockflags {
    /// When set, `timeout` is an absolute time; when clear it is
    /// relative to the instant the subscription is posted.
    pub const ABSTIME: u16 = 0x1;
}

/// WASI `eventrwflags_t` — applies to FD_READ / FD_WRITE events.
pub mod eventrwflags {
    /// The peer end of the fd has hung up (reader for a write fd,
    /// writer for a read fd). Semantically "readable at EOF" for
    /// FD_READ, "writing will produce SIGPIPE" for FD_WRITE.
    pub const FD_READWRITE_HANGUP: u16 = 0x1;
}

/// WASI preview 1 filetype byte (first byte of `filestat_t` /
/// `fdstat_t`). Mirrors `__wasi_filetype_t` from the C header.
pub mod filetype {
    pub const UNKNOWN:          u8 = 0;
    pub const BLOCK_DEVICE:     u8 = 1;
    pub const CHARACTER_DEVICE: u8 = 2;
    pub const DIRECTORY:        u8 = 3;
    pub const REGULAR_FILE:     u8 = 4;
    pub const SOCKET_DGRAM:     u8 = 5;
    pub const SOCKET_STREAM:    u8 = 6;
    pub const SYMBOLIC_LINK:    u8 = 7;
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
