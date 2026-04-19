//! Calls `proc_wait(-1, 0, 0)` — POSIX `wait(NULL)` analogue
//! targeting any child — on a process with no children, and writes
//! the i32 LE return code (always `-ECHILD = -9`) plus a trailing
//! newline to `/dev/console`. Proves the `proc_wait` PMos-ext shim
//! (98a3341) is reachable from a real `wasm32-wasip1` user binary
//! and not stripped by LTO.
//!
//! Why no children: this binary is itself a child spawned by init
//! in the composition test, so its own children list is empty —
//! triggering the kernel's `WaitOutcome::NoChildren -> ECHILD`
//! arm at `crates/kernel/src/syscall/ext.rs:563`. This is the
//! cleanest single-shot test of the proc_wait error path through
//! a real wasm binary: no companion processes needed, no
//! sequencing concerns.
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
    fn fd_write(
        fd: i32,
        iovs_ptr: *const Ciovec,
        iovs_len: i32,
        nwritten_ptr: *mut u32,
    ) -> i32;
    fn proc_exit(rval: i32) -> !;
}

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "pmos_ext")]
extern "C" {
    fn proc_wait(target_pid: i32, options: i32, status_out_ptr: i32) -> i32;
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
        // -1 = WaitTarget::Any. options = 0 (no WNOHANG, but the
        // v1 dispatcher is non-blocking regardless). status_out_ptr
        // = 0 because we expect failure and won't have a status to
        // write.
        let rc: i32 = proc_wait(-1, 0, 0);

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
