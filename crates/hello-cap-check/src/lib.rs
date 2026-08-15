//! Calls `cap_check(CAP_SHELL = 3)` and writes the i32 LE return
//! value plus a trailing newline to `/dev/console`. Proves the new
//! `cap_check` PMos-ext shim is reachable from a real
//! `wasm32-wasip1` user binary and not stripped by LTO.
//!
//! cap_check is the PMos-ext yes/no-per-cap query — it returns
//! `1` if the caller holds the requested cap, `0` otherwise, and
//! `-EINVAL` if the cap id doesn't correspond to a known
//! `Cap` discriminant. Composition tests spawn this binary with
//! CAPSET_ALL, so the result is always 1.
//!
//! cap_check is the cap-by-cap counterpart to the existing
//! cap_list / proc_caps_get bitset-returning shims — useful when
//! userland wants to gate a code path on one specific cap without
//! materialising the full bitset.
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
    fn cap_check(cap_id: i32) -> i32;
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
        // Cap::Shell = 3 per abi::cap. Composition test spawns
        // this binary with CAPSET_ALL, so the answer is always 1.
        const CAP_SHELL: i32 = 3;
        let rc: i32 = cap_check(CAP_SHELL);

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
