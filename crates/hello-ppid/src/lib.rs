//! Calls `proc_parent()` (POSIX `getppid(2)` analogue) and writes
//! the returned i32 LE ppid plus a trailing newline to
//! `/dev/console`. Proves the new `proc_parent` PMos-ext shim is
//! reachable from a real `wasm32-wasip1` user binary and not
//! stripped by LTO.
//!
//! Sister to hello-pid: PROC_SELF returns the caller's own pid,
//! PROC_PARENT returns its parent's pid (or 0 if the parent has
//! been reaped or the caller is init). Composition tests assert
//! the i32 LE bytes match the pid of the spawning process.
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
    fn proc_parent() -> i32;
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
        let ppid: i32 = proc_parent();

        let mut write_buf = [0u8; 5];
        let ppid_bytes = ppid.to_le_bytes();
        write_buf[0] = ppid_bytes[0];
        write_buf[1] = ppid_bytes[1];
        write_buf[2] = ppid_bytes[2];
        write_buf[3] = ppid_bytes[3];
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
