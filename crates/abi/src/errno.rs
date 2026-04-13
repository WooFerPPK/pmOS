//! Errno values: WASI preview 1 baseline plus PMos extensions.
//!
//! Mirrors `contracts/syscalls.md §4`. Returned from every syscall as
//! a negative `i32` on failure (`-ENOENT`, `-EBADF`, etc.). Success is
//! `0`, or a non-negative return value for calls that have one.

/// Successful return.
pub const ESUCCESS: i32 = 0;

// WASI preview 1 errno values (numeric mapping). Selected subset —
// every errno the PMos kernel actually returns is listed here. Unused
// WASI errnos (e.g. EBADMSG for message-queue implementations we do
// not ship) are omitted to keep the table short.
pub const E2BIG:            i32 = 1;  // argument list too long
pub const EACCES:           i32 = 2;  // permission denied
pub const EADDRINUSE:       i32 = 3;  // address already in use
pub const EADDRNOTAVAIL:    i32 = 4;  // address not available
pub const EAFNOSUPPORT:     i32 = 5;  // address family not supported
pub const EAGAIN:           i32 = 6;  // resource unavailable, try again
pub const EALREADY:         i32 = 7;  // connection already in progress
pub const EBADF:            i32 = 8;  // bad file descriptor
pub const EBUSY:            i32 = 10; // device or resource busy
pub const ECANCELED:        i32 = 11; // operation canceled
pub const ECONNABORTED:     i32 = 13; // connection aborted
pub const ECONNREFUSED:     i32 = 14; // connection refused
pub const ECONNRESET:       i32 = 15; // connection reset
pub const EDEADLK:          i32 = 16; // resource deadlock would occur
pub const EDESTADDRREQ:     i32 = 17; // destination address required
pub const EDOM:             i32 = 18; // argument out of domain of function
pub const EEXIST:           i32 = 20; // file exists
pub const EFAULT:           i32 = 21; // bad address
pub const EFBIG:            i32 = 22; // file too large
pub const EHOSTUNREACH:     i32 = 23; // host is unreachable
pub const EIDRM:            i32 = 24; // identifier removed
pub const EILSEQ:           i32 = 25; // illegal byte sequence
pub const EINPROGRESS:      i32 = 26; // operation in progress
pub const EINTR:            i32 = 27; // interrupted function
pub const EINVAL:           i32 = 28; // invalid argument
pub const EIO:              i32 = 29; // I/O error
pub const EISCONN:          i32 = 30; // socket is connected
pub const EISDIR:           i32 = 31; // is a directory
pub const ELOOP:            i32 = 32; // too many levels of symbolic links
pub const EMFILE:           i32 = 33; // file descriptor value too large
pub const EMLINK:           i32 = 34; // too many links
pub const EMSGSIZE:         i32 = 35; // message too large
pub const ENAMETOOLONG:     i32 = 37; // filename too long
pub const ENETDOWN:         i32 = 38; // network is down
pub const ENETRESET:        i32 = 39; // connection aborted by network
pub const ENETUNREACH:      i32 = 40; // network unreachable
pub const ENFILE:           i32 = 41; // too many files open in system
pub const ENOBUFS:          i32 = 42; // no buffer space available
pub const ENODEV:           i32 = 43; // no such device
pub const ENOENT:           i32 = 44; // no such file or directory
pub const ENOEXEC:          i32 = 45; // executable file format error
pub const ENOLCK:           i32 = 46; // no locks available
pub const ENOMEM:           i32 = 48; // not enough space
pub const ENOPROTOOPT:      i32 = 50; // protocol not available
pub const ENOSPC:           i32 = 51; // no space left on device
pub const ENOSYS:           i32 = 52; // function not supported
pub const ENOTCONN:         i32 = 53; // the socket is not connected
pub const ENOTDIR:          i32 = 54; // not a directory
pub const ENOTEMPTY:        i32 = 55; // directory not empty
pub const ENOTRECOVERABLE:  i32 = 56; // state not recoverable
pub const ENOTSOCK:         i32 = 57; // not a socket
pub const ENOTSUP:          i32 = 58; // not supported
pub const ENOTTY:           i32 = 59; // inappropriate I/O control
pub const ENXIO:            i32 = 60; // no such device or address
pub const EOVERFLOW:        i32 = 61; // value too large to store in data type
pub const EOWNERDEAD:       i32 = 62; // previous owner died
pub const EPERM:            i32 = 63; // operation not permitted
pub const EPIPE:            i32 = 64; // broken pipe
pub const EPROTO:           i32 = 65; // protocol error
pub const EPROTONOSUPPORT:  i32 = 66; // protocol not supported
pub const EPROTOTYPE:       i32 = 67; // protocol wrong type for socket
pub const ERANGE:           i32 = 68; // result too large
pub const EROFS:            i32 = 69; // read-only filesystem
pub const ESPIPE:           i32 = 70; // invalid seek
pub const ESRCH:            i32 = 71; // no such process
pub const ETIMEDOUT:        i32 = 73; // connection timed out
pub const ETXTBSY:          i32 = 74; // text file busy
pub const EXDEV:            i32 = 75; // cross-device link

// PMos extensions (contracts/syscalls.md §4).
/// Caller's capability set does not permit this operation.
pub const ENOTCAPABLE: i32 = 76;
/// Spawned process uses an incompatible ABI version.
pub const ENOABIVER:   i32 = 77;

/// Convert a positive errno into its negative syscall-return form.
#[inline]
pub const fn err(e: i32) -> i32 {
    -e
}
