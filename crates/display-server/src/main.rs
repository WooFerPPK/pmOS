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
//!   1. `display_bind()`                     — claim `/run/display`.
//!   2. `path_open("/dev/fb0")`               — open framebuffer once,
//!                                               reused for every served
//!                                               client.
//!   3. Outer loop (at most `MAX_CLIENTS = 4` iterations):
//!      a. `ipc_accept(listener)` (blocking — slice 2a's kernel
//!          park/wake; flags=0 is the blocking default).
//!         * returns `server fd >= 0`  → proceed to fd_read.
//!         * any negative rc → fatal, exit 12. Includes
//!           `-EAGAIN` (not expected: blocking calls never
//!           return EAGAIN), `-EINTR` (slice 2b does not yet
//!           wire signal-driven exit), and `-ESRCH` (surfaces
//!           under vitest `runAllSpawns` where spawned pids
//!           stay Ready and `park_on_accept` fails its state
//!           transition — matches slice 1's "non-EAGAIN error"
//!           exit semantics).
//!      b. `fd_read(server, buf)` (EAGAIN poll via `MAX_POLLS`)
//!         — read pixel payload. `MAX_POLLS` remains alive for
//!         `fd_read` until a future slice migrates it to blocking
//!         semantics too.
//!      c. `fd_write(fb_fd, buf)`                — relay to `/dev/fb0`.
//!      d. `fd_close(server)`                    — release the client fd
//!                                                  so the next iteration's
//!                                                  accept starts against a
//!                                                  clean fd table.
//!      e. `println!("display-server served client {i}")`.
//!   4. `println!("display-server fb blit ok")` — trailing observable
//!      line, same as the pre-slice shape.
//!   5. fall off the end of `main`            — std emits
//!      `__wasi_proc_exit(0)`.
//!
//! `MAX_CLIENTS = 4` is a testing fiction. The outer loop is still
//! bounded because (a) the vitest composition helper (`runAllSpawns`,
//! strictly sequential) would otherwise spin forever when no more
//! clients arrive, and (b) signal-driven exit via fd 3 polling is
//! not yet wired. A future slice removes the ceiling and adds the
//! fd-3 poll.
//!
//! Exit codes:
//!
//!   * 0  = success (loop completed `MAX_CLIENTS` iterations)
//!   * 10 = `display_bind` failed
//!   * 12 = `ipc_accept` returned any negative rc (fatal: any
//!          error path on the blocking accept — includes
//!          `-EAGAIN`, `-EINTR`, `-ESRCH`)
//!   * 14 = `fd_read` returned a non-EAGAIN error or read 0 bytes
//!   * 15 = `path_open("/dev/fb0")` failed
//!   * 16 = framebuffer `fd_write` failed or short-wrote
//!   * 17 = (reserved: first-accept poll exhaustion from slice
//!          1, no longer reachable now that the inner busy-poll
//!          is gone)
//!   * 18 = `fd_read` poll exhausted for a connected client
//!   * 19 = `fd_close` on the client fd returned a non-zero errno

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
    fn fd_close(fd: i32) -> i32;
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
    // (`fd_read`, `fd_write`, `path_open`, `fd_close`) surface
    // errno directly as positive on error; PMos extension syscalls
    // (`ipc_accept`, `display_bind`) negate into `-errno`. Both
    // conventions agree on the numeric value.
    const EAGAIN: i32 = 6;
    // Bounded-iteration safety valve for the vitest harness'
    // `fd_read` path. Slice 2a migrated `ipc_accept` to blocking
    // kernel semantics; `fd_read` remains EAGAIN-polled until a
    // future slice applies the same park/wake pattern to it.
    const MAX_POLLS: u32 = 10_000;
    // Outer-loop ceiling. Bounded because (a) the vitest harness
    // is sequential and would otherwise spin forever when no more
    // clients arrive, and (b) signal-driven exit via fd 3 polling
    // is not yet wired. Removed by a future slice.
    const MAX_CLIENTS: u32 = 4;

    unsafe {
        let listener = display_bind();
        if listener < 0 {
            std::process::exit(10);
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

        for i in 0..MAX_CLIENTS {
            // Slice 2a's kernel park/wake makes this a blocking
            // call by default (flags=0). The inner 10k-iteration
            // busy-poll is gone — every prior iteration succeeded
            // on pass 0 anyway once blocking semantics landed.
            // Any non-zero return is fatal (exit 12): includes
            // `-EAGAIN` (not expected from a blocking call),
            // `-EINTR` (slice 2b doesn't yet wire signal-driven
            // exit), and `-ESRCH` (vitest `runAllSpawns` where
            // `park_on_accept`'s state transition fails —
            // matches slice 1's "non-EAGAIN error" semantic).
            let rc = ipc_accept(listener);
            if rc < 0 {
                std::process::exit(12);
            }
            let server: i32 = rc;

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

            // Writing from `recv_buf` (not a local constant) is
            // deliberate: it pins the IPC round-trip as load-
            // bearing, so a regression that broke `fd_read` but
            // left everything else intact surfaces as wrong
            // framebuffer bytes rather than a green test.
            let fb_iov = Ciovec {
                buf: recv_buf.as_ptr(),
                buf_len: nread,
            };
            let mut fb_written: u32 = 0;
            let rc = fd_write(fb_fd as i32, &fb_iov, 1, &mut fb_written);
            if rc != 0 || fb_written != nread {
                std::process::exit(16);
            }

            // Release the client-side server fd so the next
            // iteration's `ipc_accept` starts against a clean
            // fd table. `fd_close` returns positive errno per
            // the WASI convention.
            let rc = fd_close(server);
            if rc != 0 {
                std::process::exit(19);
            }

            println!("display-server served client {}", i);
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
