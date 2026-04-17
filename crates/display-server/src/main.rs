//! PMos display server binary — persistent-accept-loop std binary.
//!
//! Long-running server over the M1 multi-process substrate. Binds
//! `/run/display`, then parks on an `ipc_accept` poll loop waiting
//! for a real client process (not a self-connect). First client's
//! pixel payload is relayed to `/dev/fb0`; the binary then exits
//! cleanly so the dispatch loop can reap it alongside init's other
//! children. The companion client this slice ships is
//! `/bin/display-client-demo` (see `crates/display-client-demo/`);
//! init spawns both so display-server has a real peer to accept
//! from.
//!
//! Flow:
//!   1. `display_bind()`                        — claim `/run/display`.
//!   2. `ipc_accept(listener)` (EAGAIN poll)   — wait for a client.
//!   3. `fd_read(server, buf)` (EAGAIN poll)   — read pixel payload.
//!   4. `path_open("/dev/fb0")`                  — open framebuffer.
//!   5. `fd_write(fb_fd, buf)`                   — relay to `/dev/fb0`.
//!   6. fall off the end of `main`               — std emits `__wasi_proc_exit(0)`.
//!
//! The accept poll loop is bounded so the vitest in-process
//! composition helper (`runAllSpawns`) doesn't hang when no
//! real-Worker client ever connects. Under production Playwright
//! a `/bin/display-client-demo` sibling lands a connection within
//! the first few dispatch passes. Same shape as
//! `hello-input-echo`'s EAGAIN poll on `fd_read` — precedent for
//! bounded polling exists in the workspace.
//!
//! Exit codes:
//!
//!   * 0  = success
//!   * 10 = `display_bind` failed
//!   * 12 = `ipc_accept` returned a non-EAGAIN error
//!   * 14 = `fd_read` returned a non-EAGAIN error or read 0 bytes
//!   * 15 = `path_open("/dev/fb0")` failed
//!   * 16 = framebuffer `fd_write` failed or short-wrote
//!   * 17 = `ipc_accept` poll loop exhausted (no client arrived)
//!   * 18 = `fd_read` poll loop exhausted (client never wrote)

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

#[cfg(target_arch = "wasm32")]
fn main() {
    println!("display-server starting");

    // EAGAIN is positive `abi::errno::EAGAIN = 6`. WASI syscalls
    // (`fd_read`, `fd_write`, `path_open`) surface errno directly
    // as positive on error; PMos extension syscalls (`ipc_accept`,
    // `display_bind`) negate into `-errno`. Both conventions
    // agree on the numeric value.
    const EAGAIN: i32 = 6;
    // Safety valve: bounded iteration count so the vitest
    // in-process composition test (`runAllSpawns`, strictly
    // sequential) doesn't spin forever when no sibling client
    // process exists. Real Playwright (concurrent Workers) lands
    // a connection within the first handful of passes.
    const MAX_POLLS: u32 = 10_000;

    unsafe {
        let listener = display_bind();
        if listener < 0 {
            std::process::exit(10);
        }

        let mut server: i32 = -1;
        for _ in 0..MAX_POLLS {
            let rc = ipc_accept(listener);
            if rc >= 0 {
                server = rc;
                break;
            }
            if rc == -EAGAIN {
                continue;
            }
            std::process::exit(12);
        }
        if server < 0 {
            std::process::exit(17);
        }

        let mut recv_buf = [0u8; 32];
        let read_iov = Iovec {
            buf: recv_buf.as_mut_ptr(),
            buf_len: recv_buf.len() as u32,
        };
        let mut nread: u32 = 0;
        let mut got_bytes = false;
        for _ in 0..MAX_POLLS {
            let rc = fd_read(server, &read_iov, 1, &mut nread);
            if rc == 0 && nread > 0 {
                got_bytes = true;
                break;
            }
            if rc == 0 || rc == EAGAIN {
                nread = 0;
                continue;
            }
            std::process::exit(14);
        }
        if !got_bytes {
            std::process::exit(18);
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

        // Writing from `recv_buf` (not a local constant) is
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
