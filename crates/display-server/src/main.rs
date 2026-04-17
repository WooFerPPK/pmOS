//! PMos display server binary — minimum viable std binary.
//!
//! First `std` binary in the workspace to do IPC over the M1
//! multi-process substrate. Self-connects (plays server, client,
//! AND framebuffer-writer in one boot pass, mirroring
//! `display-server-lite`'s single-binary composition pattern)
//! to prove a real long-running std binary can traverse the full
//! display path through a real user Worker + real SAB + real
//! `Atomics.wait`/`notify` wake protocol.
//!
//! Flow:
//!   1. `display_bind()`                    — claim `/run/display`.
//!   2. `display_connect()`                  — open a client fd.
//!   3. `ipc_accept(listener)`               — server fd paired with client.
//!   4. `fd_write(client, PIXELS)`           — 16 bytes RGBA cross the socket.
//!   5. `fd_read(server, buf)`               — server reads same bytes back.
//!   6. `path_open("/dev/fb0")`              — open the framebuffer.
//!   7. `fd_write(fb_fd, buf)`               — relay bytes to `/dev/fb0`.
//!   8. fall off the end of `main`           — std emits `__wasi_proc_exit(0)`.
//!
//! A future slice swaps this single-shot self-connect pattern for a
//! persistent accept loop paired with a second binary as the client.
//! That's blocked on `ipc_accept` blocking semantics (today it
//! returns `EAGAIN` on an empty backlog) and on `proc_wait` for
//! init-side supervision.
//!
//! Exit codes (match `display-server-lite`'s numbering so a
//! regression in a shared step surfaces with the same code on both
//! binaries):
//!
//!   * 0  = success
//!   * 10 = `display_bind` failed
//!   * 11 = `display_connect` failed
//!   * 12 = `ipc_accept` failed
//!   * 13 = client-side `fd_write` failed or short-wrote
//!   * 14 = server-side `fd_read` failed or short-read
//!   * 15 = `path_open("/dev/fb0")` failed
//!   * 16 = framebuffer `fd_write` failed or short-wrote

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
    fn fd_read(
        fd: i32,
        iovs_ptr: *const Iovec,
        iovs_len: i32,
        nread_ptr: *mut u32,
    ) -> i32;
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

/// Four RGBA pixels: red, green, blue, white — the same payload
/// `display-server-lite`'s composition test pins on the framebuffer.
const PIXELS: [u8; 16] = [
    0xff, 0x00, 0x00, 0xff, // red
    0x00, 0xff, 0x00, 0xff, // green
    0x00, 0x00, 0xff, 0xff, // blue
    0xff, 0xff, 0xff, 0xff, // white
];

#[cfg(target_arch = "wasm32")]
fn main() {
    println!("display-server starting");

    unsafe {
        let listener = display_bind();
        if listener < 0 {
            std::process::exit(10);
        }

        let client = display_connect();
        if client < 0 {
            std::process::exit(11);
        }

        let server = ipc_accept(listener);
        if server < 0 {
            std::process::exit(12);
        }

        let write_iov = Ciovec {
            buf: PIXELS.as_ptr(),
            buf_len: PIXELS.len() as u32,
        };
        let mut nwritten: u32 = 0;
        let rc = fd_write(client, &write_iov, 1, &mut nwritten);
        if rc != 0 || nwritten != PIXELS.len() as u32 {
            std::process::exit(13);
        }

        let mut recv_buf = [0u8; 32];
        let read_iov = Iovec {
            buf: recv_buf.as_mut_ptr(),
            buf_len: recv_buf.len() as u32,
        };
        let mut nread: u32 = 0;
        let rc = fd_read(server, &read_iov, 1, &mut nread);
        if rc != 0 || nread as usize != PIXELS.len() {
            std::process::exit(14);
        }

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
            std::process::exit(15);
        }

        // Writing from `recv_buf` (not the `PIXELS` constant) is
        // deliberate: it pins the IPC round-trip as load-bearing,
        // so a regression that broke `fd_read` but left everything
        // else intact surfaces as wrong framebuffer bytes rather
        // than a green test.
        let fb_iov = Ciovec {
            buf: recv_buf.as_ptr(),
            buf_len: nread,
        };
        let mut fb_written: u32 = 0;
        let rc = fd_write(fb_fd as i32, &fb_iov, 1, &mut fb_written);
        if rc != 0 || fb_written != nread {
            std::process::exit(16);
        }
    }

    println!("display-server fb blit ok");
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    // Native stub so `cargo test --workspace` + `cargo build
    // --workspace` link the bin target. The WASM target is the
    // only one the slice exercises; everything above is behind
    // `#[cfg(target_arch = "wasm32")]`.
}
