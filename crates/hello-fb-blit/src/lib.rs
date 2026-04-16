//! Writes driver-framed OP_SET_MODE + OP_BLIT payloads to /dev/fb0 so
//! the TS-side `FramebufferDriver` decodes them into typed
//! `fb:set-mode` / `fb:blit` messages. Proves the
//! KernelWasmHost.framebufferDriver + onFramebufferMessage wiring:
//! user wasm → kernel driver_call → TS host → FramebufferDriver.call
//! → DriverHost.postToMain → caller's onFramebufferMessage callback.
//!
//! The framing is the convention the TS-side `FramebufferDriver` in
//! `web/src/drivers/fb.ts` reads: byte 0 is the driver op
//! (`OP_SET_MODE = 0x01`, `OP_BLIT = 0x02`), the rest is the driver's
//! op-specific payload. The kernel's `/dev/fb0` dispatcher is
//! transparent — it just forwards the bytes — so user binaries that
//! want structured blits write the whole `[op, ...payload]` in one
//! `fd_write` call.
//!
//! Payloads:
//!
//!   * OP_SET_MODE (9 bytes total)
//!     byte 0    : 0x01
//!     bytes 1-4 : width  u32 LE
//!     bytes 5-8 : height u32 LE
//!
//!   * OP_BLIT (9 + width * height * 4 bytes)
//!     byte 0         : 0x02
//!     bytes 1-4      : width  u32 LE
//!     bytes 5-8      : height u32 LE
//!     bytes 9..      : RGBA8 pixels, row-major
//!
//! This binary sends a 2x2 OP_SET_MODE followed by a 2x2 OP_BLIT with
//! the four RGBA pixels (red, green, blue, white). Exit codes:
//!
//!   * 0  = every step succeeded
//!   * 10 = path_open("/dev/fb0") failed
//!   * 11 = SET_MODE fd_write failed or short-wrote
//!   * 12 = BLIT fd_write failed or short-wrote
//!   * 101 = panic

#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn path_open(
        dirfd: i32,
        dirflags: i32,
        path_ptr: *const u8,
        path_len: i32,
        oflags: i32,
        fs_rights_base: i64,
        fs_rights_inheriting: i64,
        fdflags: i32,
        fd_out_ptr: *mut u32,
    ) -> i32;
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
const OP_SET_MODE: u8 = 0x01;
#[cfg(target_arch = "wasm32")]
const OP_BLIT: u8 = 0x02;

/// 2x2 = 4 RGBA pixels: red, green, blue, white.
#[cfg(target_arch = "wasm32")]
const PIXELS: [u8; 16] = [
    0xff, 0x00, 0x00, 0xff, // red
    0x00, 0xff, 0x00, 0xff, // green
    0x00, 0x00, 0xff, 0xff, // blue
    0xff, 0xff, 0xff, 0xff, // white
];

#[cfg(target_arch = "wasm32")]
fn u32_le(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}

#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub extern "C" fn _start() {
    unsafe {
        const FB_PATH: &[u8] = b"/dev/fb0";
        let mut fb_fd: u32 = 0;
        let rc = path_open(
            0,
            0,
            FB_PATH.as_ptr(),
            FB_PATH.len() as i32,
            0,
            0,
            0,
            0,
            &mut fb_fd,
        );
        if rc != 0 {
            proc_exit(10);
        }

        // OP_SET_MODE with 2x2 dimensions. 9 bytes total.
        let w_bytes = u32_le(2);
        let h_bytes = u32_le(2);
        let set_mode: [u8; 9] = [
            OP_SET_MODE,
            w_bytes[0], w_bytes[1], w_bytes[2], w_bytes[3],
            h_bytes[0], h_bytes[1], h_bytes[2], h_bytes[3],
        ];
        let iov = Ciovec {
            buf: set_mode.as_ptr(),
            buf_len: set_mode.len() as u32,
        };
        let mut nwritten: u32 = 0;
        let rc = fd_write(fb_fd as i32, &iov, 1, &mut nwritten);
        if rc != 0 || nwritten != set_mode.len() as u32 {
            proc_exit(11);
        }

        // OP_BLIT with the 2x2 RGBA payload. 9 + 16 = 25 bytes.
        let mut blit = [0u8; 25];
        blit[0] = OP_BLIT;
        blit[1..5].copy_from_slice(&w_bytes);
        blit[5..9].copy_from_slice(&h_bytes);
        blit[9..25].copy_from_slice(&PIXELS);
        let iov = Ciovec {
            buf: blit.as_ptr(),
            buf_len: blit.len() as u32,
        };
        let rc = fd_write(fb_fd as i32, &iov, 1, &mut nwritten);
        if rc != 0 || nwritten != blit.len() as u32 {
            proc_exit(12);
        }

        proc_exit(0);
    }
}

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { proc_exit(101) }
}
