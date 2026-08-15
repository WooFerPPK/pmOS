//! Calls WASI `sched_yield()` four times in a loop, asserts each
//! return value is 0, and writes the loop iteration count (always
//! 4 — the binary exits 12 on any earlier yield failure) as an
//! i32 LE plus a trailing newline to `/dev/console`. Proves the
//! new WASI sched_yield shim from 5dd1714 is reachable from a
//! real `wasm32-wasip1` user binary and not stripped by LTO.
//!
//! PMos's scheduler is a single-threaded round-robin that runs
//! each dispatch to completion, so yield has no behavioural
//! effect — every syscall already "yields" in the sense that the
//! kernel can pick the next runnable process on the next dispatch
//! loop iteration. This binary's load-bearing assertion is purely
//! "the shim is callable and returns 0", but the loop does it
//! four times so a regression that broke the shim on the second
//! call would still be observable.
//!
//! Exit codes:
//!
//! * 0   = success — wrote the 5-byte record cleanly
//! * 12  = sched_yield returned a non-zero value (the kernel
//!   handler always returns 0, so any non-zero signals
//!   shim or dispatcher breakage)
//! * 13  = fd_write to stdout failed or short-wrote
//! * 101 = panic

#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn fd_write(fd: i32, iovs_ptr: *const Ciovec, iovs_len: i32, nwritten_ptr: *mut u32) -> i32;
    fn proc_exit(rval: i32) -> !;
    fn sched_yield() -> i32;
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
        let mut iterations: i32 = 0;
        for _ in 0..4 {
            let rc = sched_yield();
            if rc != 0 {
                proc_exit(12);
            }
            iterations += 1;
        }

        let mut write_buf = [0u8; 5];
        let count_bytes = iterations.to_le_bytes();
        write_buf[0] = count_bytes[0];
        write_buf[1] = count_bytes[1];
        write_buf[2] = count_bytes[2];
        write_buf[3] = count_bytes[3];
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
