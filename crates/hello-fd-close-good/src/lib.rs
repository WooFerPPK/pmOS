//! Calls WASI `fd_close(2)` — closing the auto-installed
//! `/dev/console` stderr fd — and writes the i32 LE return code
//! (always `0`) plus a trailing newline to fd 1 (stdout, still
//! open). Then re-closes fd 2 to verify the slot was released
//! (the second close is intentionally not asserted in console
//! output since stdout is what carries the test signal — but the
//! second-close failure is what the binary exits 14 on if the
//! kernel didn't actually free the slot).
//!
//! Companion to hello-fd-close-bad (proves the EBADF arm) — this
//! binary proves the **success** arm of f03cf74's fd_close shim:
//! a real fd in the table can be released cleanly.
//!
//! fd 2 is auto-installed by proc_spawn (kernel-side) alongside
//! fd 0 / 1 = console, fd 3 = the `/` preopen, and fd 4 = the signal
//! channel. Closing fd 2 releases the slot without affecting them.
//!
//! Exit codes:
//!
//! * 0   = success — wrote the 5-byte record cleanly
//! * 13  = fd_write to stdout failed or short-wrote
//! * 14  = re-close of the just-closed fd 2 didn't surface
//!   EBADF as expected (kernel might have left the slot
//!   half-released — invariant violation)
//! * 101 = panic

#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn fd_close(fd: i32) -> i32;
    fn fd_write(fd: i32, iovs_ptr: *const Ciovec, iovs_len: i32, nwritten_ptr: *mut u32) -> i32;
    fn proc_exit(rval: i32) -> !;
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
        // First close: fd 2 = /dev/console stderr, auto-installed
        // by proc_spawn. Should succeed cleanly with rc=0.
        let rc: i32 = fd_close(2);

        // Re-close: fd 2 is now released; the second call must
        // surface EBADF (8). If the slot wasn't actually freed
        // the kernel will return 0 here — we exit 14 to flag the
        // invariant violation.
        let recheck: i32 = fd_close(2);
        const EBADF: i32 = 8;
        if recheck != EBADF {
            proc_exit(14);
        }

        // Write the first close's rc as i32 LE plus a trailing
        // newline to fd 1 (still open since we only closed fd 2).
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
