//! Calls `proc_caps_get(proc_self(), &caps_out)` and writes the
//! returned u64 LE CapSet plus a trailing newline to `/dev/console`.
//! Proves that two PMos-ext shims (proc_self from c7d5c9b and the
//! pre-existing proc_caps_get) compose cleanly through the
//! dispatcher and that the heap-out u64 path is reachable from a
//! real `wasm32-wasip1` user binary.
//!
//! Self-querying always succeeds because the kernel's
//! `handle_proc_caps_get` short-circuits the cap check when
//! `target == sender` — no Cap::ProcInspect requirement for self.
//!
//! The 8-byte u64 LE record plus a trailing newline byte is what
//! the binary writes to stdout — the `/dev/console` driver is
//! line-buffered and only flushes complete lines to
//! `onConsoleWrite`, so without the newline the bytes would stay
//! in the kernel's buffer and never reach the host callback.
//!
//! Exit codes:
//!
//!   * 0   = success — wrote the 9-byte record cleanly
//!   * 11  = proc_caps_get returned a non-zero errno
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
    fn proc_caps_get(target_pid: i32, caps_out_ptr: i32) -> i32;
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
        let mut caps: u64 = 0;
        let caps_ptr = &mut caps as *mut u64 as i32;
        let rc: i32 = proc_caps_get(pid, caps_ptr);
        if rc != 0 {
            proc_exit(11);
        }

        let mut write_buf = [0u8; 9];
        let caps_bytes = caps.to_le_bytes();
        write_buf[..8].copy_from_slice(&caps_bytes);
        write_buf[8] = b'\n';

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
