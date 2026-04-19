//! Calls `proc_self()` (POSIX `getpid(2)` analogue) and writes the
//! returned i32 LE pid plus a trailing newline to `/dev/console`.
//! Proves the new `proc_self` PMos-ext shim is reachable from a real
//! `wasm32-wasip1` user binary and not stripped by LTO.
//!
//! This is the smallest possible end-to-end exercise of an opcode
//! that takes no args and returns a single value (PROC_SELF =
//! 0x1103). The companion to hello-kill-probe (PROC_KILL signum 0)
//! and hello-sigchld (FD_READ on SignalChannel) — same cdylib +
//! one-shot _start shape.
//!
//! Composition tests assert that the i32 LE bytes match the pid
//! the kernel allocated for the spawned child, which the test
//! captures via the `onSpawnProcess` callback.
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
    fn proc_self() -> i32;
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
        let pid: i32 = proc_self();

        let mut write_buf = [0u8; 5];
        let pid_bytes = pid.to_le_bytes();
        write_buf[0] = pid_bytes[0];
        write_buf[1] = pid_bytes[1];
        write_buf[2] = pid_bytes[2];
        write_buf[3] = pid_bytes[3];
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
