//! `display-server-lite` — a no_std wasm32-wasip1 binary that
//! composes the display-server-plus-framebuffer pipeline into
//! one self-contained proof-of-pipeline.
//!
//! The binary plays server, client, and framebuffer-writer in
//! a single `_start` pass. That simplifies the test harness
//! (one binary, one drain pass) while still exercising every
//! cap-gated path the real display server will eventually
//! traverse:
//!
//!   1. `display_bind()` as a DISPLAY_SERVER-cap process →
//!      listener fd on `/run/display`.
//!   2. `display_connect()` as a DISPLAY_CLIENT-cap process
//!      → client-side fd connected to the listener. (The
//!      binary holds CAPSET_ALL so it satisfies both cap
//!      checks.)
//!   3. `ipc_accept(listener)` → server-side fd paired with
//!      the client.
//!   4. `fd_write(client, pixels)` — client sends pixel bytes
//!      through the display protocol socket.
//!   5. `fd_read(server, buf)` — server reads the pixel bytes
//!      it just received over the protocol.
//!   6. `path_open("/dev/fb0")` — server opens the framebuffer
//!      device (requires DISPLAY_SERVER cap, which the binary
//!      holds).
//!   7. `fd_write(fb_fd, received_bytes)` — server forwards
//!      the exact bytes it got from the client onto the
//!      framebuffer.
//!   8. `proc_exit(0)`.
//!
//! The acceptance test observes `onFramebufferWrite` receiving
//! the four RGBA pixels — red, green, blue, white — that the
//! "client" originally sent. This proves the end-to-end
//! pipeline: user wasm → display protocol socket → user wasm
//! → `/dev/fb0` → kernel driver_call → TS host callback.
//!
//! ## What this is not
//!
//! A real display server: it doesn't parse a protocol, it
//! doesn't maintain surface state, it doesn't composite. Every
//! one of those concerns lives in the existing
//! `crates/display-server/` crate which is still `std`-
//! dependent and needs its own migration slice. This binary
//! is the narrowest possible demonstration that the PLUMBING
//! works end-to-end — the display server can bind the
//! privileged path, a client can connect, bytes flow in both
//! directions through the kernel's IPC state machine, and the
//! server can forward bytes to the framebuffer where the host
//! callback observes them.
//!
//! ## Exit codes
//!
//! * 0  = success
//! * 10 = `display_bind` failed (missing DISPLAY_SERVER cap,
//!   address already in use, internal error)
//! * 11 = `display_connect` failed (missing DISPLAY_CLIENT
//!   cap, connection refused, backlog full)
//! * 12 = `ipc_accept` failed (no pending client, bad fd,
//!   listener not in Listening state)
//! * 13 = `fd_write` on the client fd failed (short write,
//!   closed peer, etc.)
//! * 14 = `fd_read` on the server fd failed or short-read
//! * 15 = `path_open("/dev/fb0")` failed (missing cap, no
//!   such device)
//! * 16 = `fd_write` on the framebuffer fd failed
//! * 101 = panic

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
    fn fd_write(fd: i32, iovs_ptr: *const Ciovec, iovs_len: i32, nwritten_ptr: *mut u32) -> i32;
    fn fd_read(fd: i32, iovs_ptr: *const Iovec, iovs_len: i32, nread_ptr: *mut u32) -> i32;
    fn proc_exit(rval: i32) -> !;
}

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "pmos_ext")]
extern "C" {
    fn display_bind() -> i32;
    fn display_connect() -> i32;
    fn ipc_accept(listener_fd: i32) -> i32;
}

#[cfg(target_arch = "wasm32")]
#[repr(C)]
struct Ciovec {
    buf: *const u8,
    buf_len: u32,
}

#[cfg(target_arch = "wasm32")]
#[repr(C)]
struct Iovec {
    buf: *mut u8,
    buf_len: u32,
}

/// Four RGBA pixels: red, green, blue, white. The test asserts
/// these arrive on `onFramebufferWrite` intact.
#[cfg(target_arch = "wasm32")]
const PIXELS: [u8; 16] = [
    0xff, 0x00, 0x00, 0xff, // red
    0x00, 0xff, 0x00, 0xff, // green
    0x00, 0x00, 0xff, 0xff, // blue
    0xff, 0xff, 0xff, 0xff, // white
];

#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub extern "C" fn _start() {
    unsafe {
        // 1. Server: bind /run/display.
        let listener = display_bind();
        if listener < 0 {
            proc_exit(10);
        }

        // 2. Client: connect to /run/display.
        let client = display_connect();
        if client < 0 {
            proc_exit(11);
        }

        // 3. Server: accept the pending connection.
        let server = ipc_accept(listener);
        if server < 0 {
            proc_exit(12);
        }

        // 4. Client writes pixels into the protocol socket.
        let write_iov = Ciovec {
            buf: PIXELS.as_ptr(),
            buf_len: PIXELS.len() as u32,
        };
        let mut nwritten: u32 = 0;
        let rc = fd_write(client, &write_iov, 1, &mut nwritten);
        if rc != 0 || nwritten != PIXELS.len() as u32 {
            proc_exit(13);
        }

        // 5. Server reads them back out.
        let mut recv_buf = [0u8; 32];
        let read_iov = Iovec {
            buf: recv_buf.as_mut_ptr(),
            buf_len: recv_buf.len() as u32,
        };
        let mut nread: u32 = 0;
        let rc = fd_read(server, &read_iov, 1, &mut nread);
        if rc != 0 || nread as usize != PIXELS.len() {
            proc_exit(14);
        }

        // 6. Server opens /dev/fb0.
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
            proc_exit(15);
        }

        // 7. Server forwards the received pixels to /dev/fb0.
        //    Reading straight from `recv_buf` rather than
        //    `PIXELS` here is deliberate — asserting the
        //    bytes actually travelled through the IPC socket
        //    rather than the binary accidentally short-
        //    circuiting via the constant.
        let fb_iov = Ciovec {
            buf: recv_buf.as_ptr(),
            buf_len: nread,
        };
        let mut fb_written: u32 = 0;
        let rc = fd_write(fb_fd as i32, &fb_iov, 1, &mut fb_written);
        if rc != 0 || fb_written != nread {
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
