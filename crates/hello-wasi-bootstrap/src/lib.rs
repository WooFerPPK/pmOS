//! Exercises the WASI bootstrap quartet — args/environ sizes + get,
//! fd_fdstat_get, fd_prestat_get — through the PMos user-wasm shim.
//! Proves each of the six handlers added in "kernel opcode breadth
//! #3" is reachable from real user wasm and returns what a Rust
//! `std` libc init would expect.
//!
//! ## What it does
//!
//! 1. `args_sizes_get` → expects `(0, 0)`, exits 10 on mismatch.
//! 2. `args_get`       → expects OK (argc=0 means nothing to write).
//! 3. `environ_sizes_get` → expects `(0, 0)`, exits 12 on mismatch.
//! 4. `environ_get`    → expects OK.
//! 5. `fd_fdstat_get(1)` → expects success + filetype byte 2
//!    (CHARACTER_DEVICE for stdout, which is
//!    `FdObject::CharDevice(DEV_CONSOLE)`). Exits 14 on mismatch.
//! 6. `fd_prestat_get(3)` → expects the `/` directory preopen.
//!    Exits 15 on anything else.
//!
//! On success writes `"bootstrap ok\n"` to stdout and exits 0.

#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn fd_write(fd: i32, iovs_ptr: *const Ciovec, iovs_len: i32, nwritten_ptr: *mut u32) -> i32;
    fn proc_exit(rval: i32) -> !;
    fn args_sizes_get(argc: *mut u32, buf_size: *mut u32) -> i32;
    fn args_get(argv: *mut *mut u8, argv_buf: *mut u8) -> i32;
    fn environ_sizes_get(envc: *mut u32, buf_size: *mut u32) -> i32;
    fn environ_get(env: *mut *mut u8, env_buf: *mut u8) -> i32;
    fn fd_fdstat_get(fd: i32, buf: *mut u8) -> i32;
    fn fd_prestat_get(fd: i32, buf: *mut u8) -> i32;
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
        // 1. args_sizes_get.
        let mut argc: u32 = 999;
        let mut argv_buf_size: u32 = 999;
        if args_sizes_get(&mut argc, &mut argv_buf_size) != 0 {
            proc_exit(10);
        }
        if argc != 0 || argv_buf_size != 0 {
            proc_exit(10);
        }

        // 2. args_get — nothing to pass, but the syscall must
        //    succeed with (nullable) pointers. Pass the address
        //    of zero-width scratch; the shim ignores them.
        let mut scratch: u8 = 0;
        if args_get(
            &mut scratch as *mut u8 as *mut *mut u8,
            &mut scratch as *mut u8,
        ) != 0
        {
            proc_exit(11);
        }

        // 3. environ_sizes_get.
        let mut envc: u32 = 999;
        let mut env_buf_size: u32 = 999;
        if environ_sizes_get(&mut envc, &mut env_buf_size) != 0 {
            proc_exit(12);
        }
        if envc != 0 || env_buf_size != 0 {
            proc_exit(12);
        }

        // 4. environ_get.
        if environ_get(
            &mut scratch as *mut u8 as *mut *mut u8,
            &mut scratch as *mut u8,
        ) != 0
        {
            proc_exit(13);
        }

        // 5. fd_fdstat_get(1). 24-byte fdstat_t; byte 0 is
        //    filetype. /dev/console is a CharDevice, so the
        //    expected filetype byte is 2 (WASI's
        //    CHARACTER_DEVICE).
        let mut fdstat = [0u8; 24];
        if fd_fdstat_get(1, fdstat.as_mut_ptr()) != 0 {
            proc_exit(14);
        }
        if fdstat[0] != 2 {
            proc_exit(14);
        }

        // 6. fd_prestat_get(3). PMos exposes `/` as a directory
        //    preopen: tag byte 0 and name length 1 at offset 4.
        let mut pre = [0u8; 8];
        let rc = fd_prestat_get(3, pre.as_mut_ptr());
        let name_len = u32::from_le_bytes([pre[4], pre[5], pre[6], pre[7]]);
        if rc != 0 || pre[0] != 0 || name_len != 1 {
            proc_exit(15);
        }

        // All six opcodes green — announce and exit.
        const MSG: &[u8] = b"bootstrap ok\n";
        let iov = Ciovec {
            buf: MSG.as_ptr(),
            buf_len: MSG.len() as u32,
        };
        let mut nwritten: u32 = 0;
        let rc = fd_write(1, &iov, 1, &mut nwritten);
        if rc != 0 {
            proc_exit(16);
        }
        proc_exit(0);
    }
}

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { proc_exit(101) }
}
