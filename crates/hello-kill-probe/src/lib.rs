//! Calls `proc_kill(9999, 0)` — the POSIX `kill(pid, 0)` existence
//! probe (cab9dc5) against a pid that's deliberately unallocated —
//! and writes the i32 LE return code (always `-ESRCH = -71`) plus a
//! trailing newline to `/dev/console`. Proves the signum-0 arm of
//! `PROC_KILL` is reachable through a real `wasm32-wasip1` binary
//! and not stripped by LTO.
//!
//! Why pid 9999: a self-probe would need a `proc_self` shim
//! (currently unimplemented in `pmos_ext`); a parent-probe would
//! need to know the parent's pid up front. Probing an unallocated
//! pid only requires the (already-shipped) `proc_kill` shim and
//! exercises the new arm meaningfully — pre-cab9dc5 returned
//! `-EINVAL = -28`, post-cab9dc5 returns `-ESRCH = -71`. The
//! distinct errno makes the test assertion sharp.
//!
//! The 4-byte i32 LE record plus a trailing newline byte is what
//! the binary writes to stdout — the `/dev/console` driver is
//! line-buffered and only flushes complete lines to
//! `onConsoleWrite`, so without the newline the bytes would stay
//! in the kernel's buffer and never reach the host callback.
//!
//! Exit codes:
//!
//!   * 0   = success — wrote the 5-byte record cleanly
//!   * 13  = fd_write to stdout failed or short-wrote
//!   * 101 = panic

#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn fd_write(fd: i32, iovs_ptr: *const Ciovec, iovs_len: i32, nwritten_ptr: *mut u32) -> i32;
    fn proc_exit(rval: i32) -> !;
}

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "pmos_ext")]
extern "C" {
    fn proc_kill(target_pid: i32, signum: i32) -> i32;
}

#[cfg(target_arch = "wasm32")]
#[repr(C)]
struct Ciovec {
    buf: *const u8,
    buf_len: u32,
}

#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub extern "C" fn _start() {
    unsafe {
        // Probe a pid that's deliberately not allocated. The kernel's
        // POSIX-correct response is `-ESRCH = -71` (the same errno a
        // real signal delivery to a missing pid would return) — proves
        // the existence check fires for signum 0 the same way it does
        // for non-zero signums. Pre-cab9dc5 this would have returned
        // `-EINVAL = -28` (the dispatcher's signum match had no arm
        // for 0).
        let rc: i32 = proc_kill(9999, 0);

        // Pack the i32 LE return value plus a trailing newline into
        // a 5-byte buffer for one fd_write. The newline forces the
        // line-buffered console to flush through onConsoleWrite.
        let mut write_buf = [0u8; 5];
        let rc_bytes = rc.to_le_bytes();
        write_buf[0] = rc_bytes[0];
        write_buf[1] = rc_bytes[1];
        write_buf[2] = rc_bytes[2];
        write_buf[3] = rc_bytes[3];
        write_buf[4] = b'\n';

        let write_iov = Ciovec {
            buf: write_buf.as_ptr(),
            buf_len: write_buf.len() as u32,
        };
        let mut nwritten: u32 = 0;
        let wc = fd_write(1, &write_iov, 1, &mut nwritten);
        if wc != 0 || nwritten != write_buf.len() as u32 {
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
