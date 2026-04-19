//! Calls `proc_kill(proc_self(), 0)` — the POSIX `kill(getpid(), 0)`
//! existence + permission probe (cab9dc5) targeting the caller
//! itself — and writes the i32 LE return code plus a trailing
//! newline to `/dev/console`. Proves the **success** arm of the
//! signum-0 path through a real `wasm32-wasip1` user binary,
//! complementing hello-kill-probe (ESRCH arm via probing an
//! unallocated pid).
//!
//! Self-targeting always succeeds because the kernel's
//! `proc_check_signal` permits any sender to signal itself
//! regardless of caps (mirrors POSIX, where `kill(getpid(), sig)`
//! is the standard way to deliver a signal to oneself). Pre-cab9dc5
//! this would have returned `-EINVAL = -28` (the dispatcher's
//! signum match had no arm for 0). Post-cab9dc5 it returns `0`.
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
        let pid: i32 = proc_self();
        let rc: i32 = proc_kill(pid, 0);

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
