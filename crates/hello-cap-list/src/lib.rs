//! Calls `cap_list(&caps_out)` and writes the returned u64 LE
//! CapSet plus a trailing newline to `/dev/console`. Proves the
//! new `cap_list` PMos-ext shim is reachable from a real
//! `wasm32-wasip1` user binary and not stripped by LTO.
//!
//! cap_list is the no-args "give me my own caps" primitive — the
//! simpler companion to proc_caps_get(self). Functionally they
//! return identical values because the kernel's handle_proc_caps_get
//! short-circuits to handle_cap_list when target == sender, but
//! cap_list saves the proc_self round-trip and avoids passing a
//! pid in.
//!
//! Composition test spawns this binary with CAPSET_ALL, so the
//! returned u64 = 0xffff_ffff_ffff_ffff (all 64 bits set). The
//! 8-byte u64 LE record plus a trailing newline = 9 bytes total.
//!
//! Exit codes:
//!
//!   * 0   = success — wrote the 9-byte record cleanly
//!   * 11  = cap_list returned a non-zero errno
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
    fn cap_list(caps_out_ptr: *mut u64) -> i32;
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
        let mut caps: u64 = 0;
        let rc: i32 = cap_list(&mut caps as *mut u64);
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
