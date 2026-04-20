//! PMos display server binary — persistent-accept-loop std binary.
//!
//! Long-running server over the M1 multi-process substrate. Binds
//! `/run/display`, opens `/dev/fb0` once, then enters an **unbounded**
//! outer accept loop that exits on SIGTERM (delivered by init once
//! every display-client-demo child has been reaped). Each served
//! iteration relays the client's 16-byte RGBA payload to `/dev/fb0`.
//! Signal-driven exit is the leg that closes the long-running-server
//! arc — `MAX_CLIENTS = 4` has been removed along with the bounded-
//! exit semantics.
//!
//! Flow:
//!   1. `display_bind()`                      — claim `/run/display`.
//!   2. `path_open("/dev/fb0")`                — open framebuffer once,
//!                                                reused for every served
//!                                                client.
//!   3. Outer loop (unbounded `'outer: loop`):
//!      a. **Pre-accept signal poll** — non-blocking `fd_read` on
//!         fd 3 (the auto-installed `SignalChannel`) drains up to
//!         four queued signals as u16 LE pairs; if any of them is
//!         SIGTERM (signum 15), `break 'outer` + clean exit 0.
//!         `-EAGAIN` (empty inbox) → continue to the blocking
//!         accept.
//!      b. `ipc_accept(listener)` (blocking — slice 2a's kernel
//!          park/wake; flags=0 is the blocking default).
//!         * returns `server fd >= 0`  → proceed to fd_read.
//!         * `-EINTR` (slice 2b — SIGTERM interrupted the parked
//!           accept; kernel queued `Response::err(req_id, EINTR)`
//!           alongside the signal delivery on fd 3) → re-poll fd 3
//!           for SIGTERM + break-or-continue.
//!         * `-ESRCH` (vitest `runAllSpawns` where spawned pids
//!           stay Ready and `park_on_accept` fails its state
//!           transition) → fatal exit 12. Preserves the sequential
//!           harness's termination shape.
//!         * any other negative rc → fatal exit 12.
//!      c. `fd_read(server, buf)` (EAGAIN poll via `MAX_POLLS`)
//!         — read pixel payload. `MAX_POLLS` remains alive for
//!         `fd_read` until a future slice migrates it to blocking
//!         semantics too.
//!      d. `fd_write(fb_fd, buf)`                — relay to `/dev/fb0`.
//!      e. `fd_close(server)`                    — release the client fd
//!                                                  so the next iteration's
//!                                                  accept starts against a
//!                                                  clean fd table.
//!      f. `println!("display-server served client {i}")`.
//!   4. `println!("display-server fb blit ok")` — trailing observable
//!      line printed after SIGTERM-driven exit.
//!   5. fall off the end of `main`            — std emits
//!      `__wasi_proc_exit(0)`.
//!
//! Exit codes:
//!
//!   * 0  = success (outer loop broke on SIGTERM)
//!   * 10 = `display_bind` failed
//!   * 12 = `ipc_accept` returned an unexpected negative rc
//!          (ESRCH under vitest sequential harness; not EINTR)
//!   * 14 = `fd_read` returned a non-EAGAIN error or read 0 bytes
//!   * 15 = `path_open("/dev/fb0")` failed
//!   * 16 = framebuffer `fd_write` failed or short-wrote
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

/// Fd 3 is the auto-installed per-process `SignalChannel` — every
/// proc_spawn'd child gets one for free. A non-blocking `fd_read`
/// on this fd drains pending signals as u16 LE pairs.
#[cfg(target_arch = "wasm32")]
const SIGNAL_FD: i32 = 3;

/// Drain up to four signals from fd 3. Returns `true` iff SIGTERM
/// (signum 15) was observed in the drained batch.
#[cfg(target_arch = "wasm32")]
unsafe fn poll_sigterm() -> bool {
    // 8 bytes = 4 u16 signum records — plenty for v1's coalesced
    // inbox (max 8 distinct signals) and we only care whether
    // SIGTERM is among them.
    let mut buf = [0u8; 8];
    let iov = Iovec {
        buf: buf.as_mut_ptr(),
        buf_len: buf.len() as u32,
    };
    let mut nread: u32 = 0;
    let rc = fd_read(SIGNAL_FD, &iov, 1, &mut nread);
    // Positive errno on error (WASI convention). `EAGAIN = 6`
    // means the inbox is empty — nothing to do. Any other error
    // (including an unexpected bad fd) is silent here: the
    // signal-poll is a best-effort pre-accept check.
    if rc != 0 {
        return false;
    }
    let n = nread as usize;
    let mut i = 0;
    while i + 2 <= n {
        let signum = u16::from_le_bytes([buf[i], buf[i + 1]]);
        if signum == 15 {
            return true;
        }
        i += 2;
    }
    false
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
    // EINTR is positive `abi::errno::EINTR = 27`. Extension
    // syscalls return `-EINTR` when a parked call was interrupted
    // by SIGTERM — we handle that by falling back to the signal
    // poll to decide whether to exit or continue.
    const EINTR: i32 = 27;
    // Bounded-iteration safety valve for the vitest harness'
    // `fd_read` path on the accepted client socket. `fd_read` is
    // still EAGAIN-polled until a future slice applies the same
    // park/wake pattern to it.
    const MAX_POLLS: u32 = 10_000;

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

        let mut i: u32 = 0;
        'outer: loop {
            // Pre-accept signal poll: drain fd 3 for queued
            // signals. SIGTERM → clean exit. Anything else (or
            // empty inbox) → fall through to the blocking accept.
            if poll_sigterm() {
                break 'outer;
            }

            // Blocking since slice 2a (flags=0 default).
            let rc = ipc_accept(listener);
            if rc < 0 {
                if rc == -EINTR {
                    // SIGTERM interrupted a parked accept — re-
                    // poll fd 3 to confirm and exit cleanly. If
                    // no SIGTERM surfaces (rare race), fall
                    // through to the next accept attempt.
                    if poll_sigterm() {
                        break 'outer;
                    }
                    continue 'outer;
                }
                // Anything else (notably `-ESRCH` under vitest
                // runAllSpawns where spawned pids stay Ready and
                // park_on_accept fails its state transition, and
                // `-EAGAIN` which blocking accept should never
                // produce) is fatal.
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
            i += 1;
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
