//! Calls WASI `fd_close(99)` — a deliberately unopened fd — and
//! writes the i32 LE return code (always `-EBADF = -8`) plus a
//! trailing newline to `/dev/console`. Proves the new WASI
//! `fd_close` shim is reachable from a real `wasm32-wasip1` user
//! binary and not stripped by LTO.
//!
//! Why fd 99: PMos fd tables for spawned children start with fd
//! 0/1/2 = console (per the spawn auto-install loop) and fd 3 =
//! SignalChannel (per 9fbe708's auto-install). fd 99 is well
//! beyond any allocation, so the kernel's `fd_close` returns
//! `KernelError::NoSuchFd -> -EBADF` immediately. Closing a real
//! fd would also need a path_open round-trip; an unopened-fd test
//! is the smallest possible exercise of the close path.
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
    fn fd_close(fd: i32) -> i32;
    fn fd_write(
        fd: i32,
        iovs_ptr: *const Ciovec,
        iovs_len: i32,
        nwritten_ptr: *mut u32,
    ) -> i32;
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
        // WASI errno convention: 0 on success, positive errno on
        // failure (8 = EBADF for an unopened fd). The shim returns
        // exactly that — no negation. Bytes [0x08, 0x00, 0x00,
        // 0x00, 0x0a] for the expected -EBADF case.
        let rc: i32 = fd_close(99);

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
