//! Self-raises SIGTERM via WASI `proc_raise(15)` — the POSIX
//! `raise(sig)` equivalent — then drains the queued signum from
//! fd 4 (the auto-installed `FdObject::SignalChannel`) and echoes
//! the 2-byte u16 LE record plus a trailing newline to
//! `/dev/console`. Proves the WASI `proc_raise` path (cbe8959's
//! self-signal arm) through a real `wasm32-wasip1` user binary —
//! a sister end-to-end check to hello-sigchld, which exercises the
//! kernel-generated parent-kill path.
//!
//! Origin difference from hello-sigchld: hello-sigchld observes
//! SIGTERM queued by its parent's `PROC_KILL`; hello-raise observes
//! SIGTERM queued by its own `proc_raise` call. The fd 4 drain
//! path is identical in both cases — the asymmetry is purely in
//! how the signal lands in the inbox.
//!
//! fd 4 is installed automatically by proc_spawn as the
//! per-process signal channel — no explicit path_open is needed.
//! Because proc_raise is synchronous (the signal is queued on the
//! caller's inbox before the shim returns), no EAGAIN polling is
//! required: the fd_read that follows always finds the pending
//! signal on the first dispatch.
//!
//! Exit codes:
//!
//! * 0   = success — drained 2 bytes, echoed + newline
//! * 10  = proc_raise returned non-zero errno
//! * 11  = fd_read returned non-zero errno
//! * 12  = fd_read returned 0 bytes (unexpected — inbox always
//!   holds 2-byte records after a successful raise)
//! * 13  = fd_write to stdout failed or short-wrote
//! * 101 = panic

#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn fd_read(fd: i32, iovs_ptr: *const Iovec, iovs_len: i32, nread_ptr: *mut u32) -> i32;
    fn fd_write(fd: i32, iovs_ptr: *const Ciovec, iovs_len: i32, nwritten_ptr: *mut u32) -> i32;
    fn proc_exit(rval: i32) -> !;
    fn proc_raise(signum: i32) -> i32;
}

#[cfg(target_arch = "wasm32")]
#[repr(C)]
struct Ciovec {
    buf: *const u8,
    buf_len: u32,
}

#[cfg(target_arch = "wasm32")]
#[repr(C)]
struct Iovec {
    buf: *mut u8,
    buf_len: u32,
}

#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub extern "C" fn _start() {
    unsafe {
        // SIGTERM = 15. Queues on the caller's own SignalInbox.
        let rc = proc_raise(15);
        if rc != 0 {
            proc_exit(10);
        }

        let mut buf = [0u8; 4];
        let read_iov = Iovec {
            buf: buf.as_mut_ptr(),
            buf_len: buf.len() as u32,
        };
        let mut nread: u32 = 0;
        let read_rc = fd_read(4, &read_iov, 1, &mut nread);
        if read_rc != 0 {
            proc_exit(11);
        }
        if nread == 0 {
            proc_exit(12);
        }

        // Echo the 2-byte u16 LE signum record back to stdout, plus
        // a trailing newline so the line-buffered console flushes
        // the record to `onConsoleWrite`.
        let mut write_buf = [0u8; 5];
        let copy_len = nread as usize;
        write_buf[..copy_len].copy_from_slice(&buf[..copy_len]);
        write_buf[copy_len] = b'\n';
        let total_len = (copy_len + 1) as u32;
        let write_iov = Ciovec {
            buf: write_buf.as_ptr(),
            buf_len: total_len,
        };
        let mut nwritten: u32 = 0;
        let wc = fd_write(1, &write_iov, 1, &mut nwritten);
        if wc != 0 || nwritten != total_len {
            proc_exit(13);
        }

        proc_exit(0);
    }
}

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { proc_exit(101) }
}
