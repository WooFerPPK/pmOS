//! Calls WASI `random_get(buf, 8)` twice into adjacent stack
//! buffers, checks that the two reads produced different bytes
//! (any matching pair fails fast — the chance of a collision in
//! 8 random bytes is 1 in 2^64), and writes both 8-byte records
//! plus a trailing newline (17 bytes total) to `/dev/console`.
//!
//! Proves the new WASI random_get shim is reachable from a real
//! `wasm32-wasip1` user binary AND that the JS host's random
//! source is actually returning entropy (not a stuck zero
//! buffer). Two distinct reads is a strong-enough invariant for
//! a no-cryptographic-strength sanity check; the production host
//! delegates to `crypto.getRandomValues`.
//!
//! Required by Rust's std startup whenever HashMap is used — the
//! SipHash key gets seeded from random_get during the first
//! HashMap allocation. This binary doesn't use HashMap, but the
//! shim and end-to-end path it exercises is the same one that
//! future std binaries with HashMap will hit.
//!
//! Exit codes:
//!
//! * 0   = success — wrote the 17-byte record cleanly
//! * 10  = first random_get returned non-zero errno
//! * 11  = second random_get returned non-zero errno
//! * 12  = both reads produced identical bytes (1 in 2^64
//!   chance — almost certainly the random source is
//!   broken)
//! * 13  = fd_write to stdout failed or short-wrote
//! * 101 = panic

#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn fd_write(fd: i32, iovs_ptr: *const Ciovec, iovs_len: i32, nwritten_ptr: *mut u32) -> i32;
    fn proc_exit(rval: i32) -> !;
    fn random_get(buf_ptr: *mut u8, buf_len: u32) -> i32;
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
        let mut buf_a = [0u8; 8];
        let mut buf_b = [0u8; 8];

        let rc_a = random_get(buf_a.as_mut_ptr(), buf_a.len() as u32);
        if rc_a != 0 {
            proc_exit(10);
        }
        let rc_b = random_get(buf_b.as_mut_ptr(), buf_b.len() as u32);
        if rc_b != 0 {
            proc_exit(11);
        }

        // Reject identical reads — astronomically unlikely with a
        // real entropy source. If this fires the random_get path is
        // returning constant data.
        if buf_a == buf_b {
            proc_exit(12);
        }

        // Pack buf_a ++ buf_b ++ '\n' into a 17-byte buffer for
        // one fd_write. The newline forces the line-buffered
        // console to flush through onConsoleWrite.
        let mut write_buf = [0u8; 17];
        write_buf[0..8].copy_from_slice(&buf_a);
        write_buf[8..16].copy_from_slice(&buf_b);
        write_buf[16] = b'\n';

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
