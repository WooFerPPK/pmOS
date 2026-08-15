//! Top-level syscall dispatcher: decode a request, route to the
//! right handler, encode a response.
//!
//! The [`Dispatcher`] is stateless by design. It's rebuilt on every
//! loop iteration (in the real Worker runtime, once T091 lands) or
//! on every test call. All state lives in the borrowed
//! [`crate::sys::Kernel`] — the dispatcher just plumbs the ring
//! buffer transport into that state.
//!
//! Opcode routing splits the 16-bit opcode space into two ranges:
//! WASI preview 1 (0x0001..0x0080, covered by
//! [`super::wasi::dispatch_wasi`]) and PMos extensions
//! (0x1000..0x1501, covered by [`super::ext::dispatch_ext`]).
//! Anything outside both ranges is `ENOSYS`, same as a known-but-
//! unimplemented opcode inside either range. Userland cannot tell
//! the two apart — and shouldn't need to.

use abi::errno::{
    self, EACCES, EAGAIN, EBADF, ECONNREFUSED, EEXIST, EINVAL, EIO, EISDIR, EMFILE, ENODEV, ENOENT,
    ENOSPC, ENOSYS, ENOTDIR, ENOTEMPTY, ENOTSUP, EROFS, ESRCH,
};
use abi::ext::{is_ext, Pid};
use abi::ring::{Request, Response};
use abi::wasi::is_wasi;
use ring::Sab;

use crate::dev::DevError;
use crate::sys::{Kernel, KernelError};
use crate::vfs::FsError;

/// Outcome of a `dispatch()` call. Distinguishes "handler produced a
/// response" (today's universal path) from "handler parked the
/// caller" (new in slice 2a for `IPC_ACCEPT` with flags=0).
///
/// The caller (`Dispatcher::service_one`) pushes the response on
/// `Done`; on `Parked`, the caller's user Worker stays on
/// `Atomics.wait` until a future dispatch pass drains a wake for it
/// via `kernel_take_next_wake_for_pid`.
#[derive(Debug)]
pub enum ServiceOutcome {
    Done(Response),
    Parked,
}

/// Per-process syscall dispatcher. Holds the bits every handler
/// needs to know about — the kernel and the pid whose ring is
/// being serviced — and nothing else. Rebuilt on every service
/// loop iteration; callers do not hold it across awaits.
pub struct Dispatcher<'k> {
    pub kernel: &'k mut Kernel,
    pub pid: Pid,
}

impl<'k> Dispatcher<'k> {
    /// Construct a dispatcher for `pid` against `kernel`. No work
    /// is done until [`Self::service_one`] is called.
    pub fn new(kernel: &'k mut Kernel, pid: Pid) -> Self {
        Dispatcher { kernel, pid }
    }

    /// Pop one pending request from `sab`, service it, push the
    /// response back. Returns `true` if a request was serviced,
    /// `false` if the request ring was empty.
    ///
    /// The heap scratch region is passed in separately (as a
    /// plain `&mut [u8]`) so the dispatcher can be driven against
    /// a test-only `Vec<u8>` — where the ring and the heap are
    /// separate allocations — as well as a real SAB, where the
    /// heap lives at `abi::ring::OFF_HEAP_SCRATCH` inside the same
    /// shared memory. The ring crate owns the transport; this
    /// crate owns the interpretation.
    ///
    /// A response ring overflow is a test-harness bug in v1: the
    /// real runtime pairs every request with one response and
    /// userland always drains responses before pushing new
    /// requests. The `debug_assert!` catches test-side mistakes
    /// without compiling an overflow branch into release builds.
    pub fn service_one(&mut self, sab: &Sab, heap: &mut [u8]) -> bool {
        let Some(req) = sab.try_pop_request() else {
            return false;
        };
        match dispatch(self.kernel, self.pid, &req, heap) {
            ServiceOutcome::Done(resp) => {
                let ok = sab.try_push_response(&resp);
                debug_assert!(ok, "syscall dispatcher: response ring overflow");
            }
            ServiceOutcome::Parked => {
                // Caller blocked; no response push. A future dispatch
                // pass that unblocks them (via a peer's ipc_connect
                // → `kernel.pending_wakes` append) will push the
                // response through `kernel_take_next_wake_for_pid`.
            }
        }
        true
    }
}

/// Dispatch a single decoded [`Request`] against `kernel` on behalf
/// of `pid`. Always returns a [`Response`] with the same
/// `request_id`; unknown opcodes turn into `ENOSYS`.
///
/// This is the entry point isolation tests call directly — most
/// tests do not care about the ring transport and just want to
/// assert that a given request shape produces a given response
/// shape. [`Dispatcher::service_one`] wraps this with the ring
/// pop/push pair.
pub fn dispatch(kernel: &mut Kernel, pid: Pid, req: &Request, heap: &mut [u8]) -> ServiceOutcome {
    let outcome = if is_wasi(req.opcode) {
        super::wasi::dispatch_wasi(kernel, pid, req, heap)
    } else if is_ext(req.opcode) {
        super::ext::dispatch_ext(kernel, pid, req, heap)
    } else {
        ServiceOutcome::Done(Response::err(req.request_id, ENOSYS))
    };
    // Re-scan only after calls that can mutate poll-visible state. Host-side
    // input/file completions have their own explicit hooks, and the Worker
    // performs a final double-check before parking. This avoids charging
    // read-only clock/stat/capability syscalls for a global readiness scan.
    if syscall_may_change_poll_readiness(req.opcode) {
        kernel.service_poll_waiters();
    }
    outcome
}

/// Whether servicing `opcode` can change readiness observed by a process
/// other than the caller (or must close poll registration's no-lost-wake
/// window). Keep this deliberately conservative: a false positive costs one
/// bounded scan, while a false negative can strand a parked process until the
/// next host event or timer deadline.
fn syscall_may_change_poll_readiness(opcode: u16) -> bool {
    use abi::ext;
    use abi::wasi;

    matches!(
        opcode,
        wasi::FD_ALLOCATE
            | wasi::FD_CLOSE
            | wasi::FD_FILESTAT_SET_SIZE
            | wasi::FD_FILESTAT_SET_TIMES
            | wasi::FD_PWRITE
            | wasi::FD_READ
            | wasi::FD_RENUMBER
            | wasi::FD_WRITE
            | wasi::PATH_CREATE_DIRECTORY
            | wasi::PATH_FILESTAT_SET_TIMES
            | wasi::PATH_LINK
            | wasi::PATH_OPEN
            | wasi::PATH_REMOVE_DIRECTORY
            | wasi::PATH_RENAME
            | wasi::PATH_SYMLINK
            | wasi::PATH_UNLINK_FILE
            | wasi::POLL_ONEOFF
            | wasi::PROC_EXIT
            | wasi::PROC_RAISE
            | wasi::SOCK_ACCEPT
            | wasi::SOCK_RECV
            | wasi::SOCK_SEND
            | wasi::SOCK_SHUTDOWN
            | ext::IPC_BIND
            | ext::IPC_LISTEN
            | ext::IPC_CONNECT
            | ext::IPC_ACCEPT
            | ext::IPC_SEND
            | ext::IPC_RECV
            | ext::PROC_KILL
            | ext::DISPLAY_CONNECT
            | ext::DISPLAY_BIND
            | ext::MOUNT
            | ext::UMOUNT
            | ext::FS_CHMOD
            | ext::HOST_FILE_RECV
            | ext::HOST_FILE_SEND
    )
}

// ---- shared decoding helpers -------------------------------------------

/// Read a `u32` little-endian from `req.args` at `offset`. Every
/// handler that takes a `u32`/`i32` scalar uses this — keeping the
/// byte surgery in one place lets the handler bodies be about
/// policy, not arithmetic.
///
/// Panics (debug only) if `offset + 4 > 16` so a handler that
/// reaches past the inline arg window is caught at test time.
#[inline]
pub(super) fn args_u32(req: &Request, offset: usize) -> u32 {
    debug_assert!(offset + 4 <= 16);
    u32::from_le_bytes([
        req.args[offset],
        req.args[offset + 1],
        req.args[offset + 2],
        req.args[offset + 3],
    ])
}

/// Read a `u16` little-endian from `req.args` at `offset`. Mirror of
/// [`args_u32`] / [`args_u64`]. Used by handlers that pack a 16-bit
/// flag word into the inline args window (notably `IPC_ACCEPT`,
/// which carries an `accept_flags` u16 at `args[4..6]`).
///
/// Panics (debug only) if `offset + 2 > 16`.
#[inline]
pub(super) fn args_u16(req: &Request, offset: usize) -> u16 {
    debug_assert!(offset + 2 <= 16);
    u16::from_le_bytes([req.args[offset], req.args[offset + 1]])
}

/// Read a `u64` little-endian from `req.args` at `offset`. Used by
/// handlers that pack a 64-bit value into the inline args window
/// (notably `PROC_SPAWN`, which carries a `CapSet` bitset).
///
/// Panics (debug only) if `offset + 8 > 16`.
#[inline]
pub(super) fn args_u64(req: &Request, offset: usize) -> u64 {
    debug_assert!(offset + 8 <= 16);
    u64::from_le_bytes([
        req.args[offset],
        req.args[offset + 1],
        req.args[offset + 2],
        req.args[offset + 3],
        req.args[offset + 4],
        req.args[offset + 5],
        req.args[offset + 6],
        req.args[offset + 7],
    ])
}

/// Borrow the heap slice the request refers to via
/// `heap_ptr` / `heap_len`. Returns `None` if the requested range
/// is out of bounds, which the caller turns into `EINVAL`.
///
/// Used by handlers that *read* a payload from the scratch
/// region (write bytes for `FD_WRITE`, the path string for
/// `PATH_OPEN`).
#[inline]
pub(super) fn heap_in<'a>(req: &Request, heap: &'a [u8]) -> Option<&'a [u8]> {
    let start = req.heap_ptr as usize;
    let len = req.heap_len as usize;
    let end = start.checked_add(len)?;
    heap.get(start..end)
}

/// Mutably borrow a slice of `heap` starting at `req.heap_ptr` with
/// capacity `max_len`. Used by handlers that *write* into the
/// scratch region so the producer can pick up the payload after
/// the response lands (the bytes read for `FD_READ`, the random
/// bytes for `RANDOM_GET`, etc.).
#[inline]
pub(super) fn heap_out_mut<'a>(
    req: &Request,
    heap: &'a mut [u8],
    max_len: usize,
) -> Option<&'a mut [u8]> {
    let start = req.heap_ptr as usize;
    let end = start.checked_add(max_len)?;
    heap.get_mut(start..end)
}

// ---- KernelError → errno mapping --------------------------------------

/// Map a [`KernelError`] into the errno value a syscall response
/// carries in its `status` field. The return is the *positive*
/// errno constant from [`abi::errno`]; [`Response::err`] is what
/// negates it on the way into the ring.
///
/// The match is exhaustive so that adding a new `KernelError`
/// variant forces a conscious decision at the dispatcher layer
/// — drift between the kernel's internal error vocabulary and
/// the on-wire errno set is the exact kind of bug this function
/// exists to catch.
pub fn kerr_to_errno(err: KernelError) -> i32 {
    match err {
        KernelError::NoSuchPid => ESRCH,
        KernelError::BadFd => EBADF,
        KernelError::OutOfFds => EMFILE,
        KernelError::ProcessLimit => EAGAIN,
        KernelError::NotSupportedOnFd => EINVAL,
        KernelError::UnsupportedAncillary => ENOTSUP,
        KernelError::UnsupportedSocketType => ENOTSUP,
        KernelError::InvalidArgument => EINVAL,
        KernelError::FileTooLarge => errno::EFBIG,
        KernelError::NotCapable => errno::ENOTCAPABLE,
        KernelError::WouldBlock => EAGAIN,
        KernelError::ConnectionRefused => ECONNREFUSED,
        KernelError::AddressInUse => errno::EADDRINUSE,
        KernelError::ResourceLimit => errno::ENOSPC,
        KernelError::PipeBroken => errno::EPIPE,
        KernelError::Fs(fs_err) => fs_err_to_errno(fs_err),
        KernelError::Dev(dev_err) => dev_err_to_errno(dev_err),
    }
}

fn fs_err_to_errno(err: FsError) -> i32 {
    match err {
        FsError::NotFound => ENOENT,
        FsError::AlreadyExists => EEXIST,
        FsError::NotADirectory => ENOTDIR,
        FsError::IsADirectory => EISDIR,
        FsError::NotEmpty => ENOTEMPTY,
        FsError::NotSupported => ENOTSUP,
        FsError::NoSpace => ENOSPC,
        FsError::Io => EIO,
        FsError::InvalidArgument => EINVAL,
        FsError::PermissionDenied => EACCES,
        FsError::ReadOnly => EROFS,
        FsError::SymLoop => errno::ELOOP,
    }
}

fn dev_err_to_errno(err: DevError) -> i32 {
    match err {
        DevError::UnknownDevice => ENODEV,
        DevError::NotCapable => errno::ENOTCAPABLE,
        DevError::NotSupported => ENOTSUP,
        DevError::DriverFailed => EIO,
        DevError::WouldBlock => EAGAIN,
    }
}
