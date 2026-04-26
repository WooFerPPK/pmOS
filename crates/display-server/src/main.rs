//! PMos display server binary — protocol-aware compositor (T110).
//!
//! Listens on `/run/display`, accepts clients, dispatches Wayland-
//! inspired protocol messages via `display_server::Server`, composites
//! attached buffers into the server's framebuffer, and presents the
//! composed pixels to `/dev/fb0` on each client turn. SIGTERM-driven
//! exit closes the long-running-server arc.
//!
//! Dual-mode reception: the v1 demo client `display-client-demo`
//! writes raw 16-byte RGBA payloads that pre-date the protocol. If
//! a received chunk decodes as a valid [`MessageHeader`] AND the
//! header's `length` matches the chunk size, the chunk is routed
//! through `Server::dispatch_request` and the resulting compositor
//! state is presented. Otherwise, the chunk is forwarded verbatim
//! to `/dev/fb0` (the legacy raw-blit path). This preserves the
//! pre-T110 Playwright `real-kernel.spec.ts` assertions while
//! adding the full protocol path toolkit clients use.
//!
//! Multi-client multiplexing is "one client at a time": the server
//! blocks on `ipc_accept`, drains the accepted client, presents,
//! closes, loops. The PMos kernel currently has no `poll(2)`
//! equivalent so concurrent clients are queued. A future slice that
//! adds `fd_select` or attaches per-client pids to the dispatch
//! tick can lift this; the protocol layer does not assume single-
//! client semantics — each `Server::dispatch_request` call is
//! per-client-id and the compositor handles overlapping toplevels
//! with proper z-order.
//!
//! Flow:
//!   1. `display_bind()`                        — claim `/run/display`.
//!   2. `path_open("/dev/fb0")`                  — open framebuffer.
//!   3. Outer loop:
//!      a. Pre-accept signal poll for SIGTERM → clean exit 0.
//!      b. `ipc_accept(listener)` (blocking; -EINTR re-polls
//!         signals; any other negative rc → fatal exit 12).
//!      c. `server.accept()` allocates a `ClientId` for the
//!         protocol layer.
//!      d. Drain the client's bytes (EAGAIN-polled `fd_read`) into
//!         a buffer.
//!      e. If the buffer parses as `[MessageHeader, payload]` whose
//!         total length equals the buffer size, call
//!         `server.dispatch_request(client_id, &buf)`. Drain any
//!         emitted server→client events back through `fd_write`.
//!         Present `server.framebuffer().pixels()` to /dev/fb0.
//!      f. Otherwise (legacy demo path), `fd_write(fb_fd, &buf)`
//!         relays the raw bytes verbatim.
//!      g. `fd_close(server)` releases the client fd.
//!      h. `println!("display-server served client {}", N)`.
//!   4. On SIGTERM, `println!("display-server fb blit ok")` and
//!      fall off the end of `main` (std emits `proc_exit(0)`).
//!
//! Exit codes:
//!
//!   * 0  = success (outer loop broke on SIGTERM)
//!   * 10 = `display_bind` failed
//!   * 12 = `ipc_accept` returned an unexpected negative rc
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

#[cfg(target_arch = "wasm32")]
const SIGNAL_FD: i32 = 3;

#[cfg(target_arch = "wasm32")]
unsafe fn poll_sigterm() -> bool {
    let mut buf = [0u8; 8];
    let iov = Iovec {
        buf: buf.as_mut_ptr(),
        buf_len: buf.len() as u32,
    };
    let mut nread: u32 = 0;
    let rc = fd_read(SIGNAL_FD, &iov, 1, &mut nread);
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

// Protocol-vs-raw-blit detection lives in the lib so native
// tests can verify it; main.rs imports it as
// `display_server::detect_protocol_message`.

#[cfg(target_arch = "wasm32")]
fn main() {
    println!("display-server starting");

    const EAGAIN: i32 = 6;
    const EINTR: i32 = 27;
    const MAX_POLLS: u32 = 10_000;

    unsafe {
        let listener = display_bind();
        if listener < 0 {
            std::process::exit(10);
        }

        const FB_PATH: &[u8] = b"/dev/fb0";
        let mut fb_fd: u32 = 0;
        let rc = path_open(
            0, 0, FB_PATH.as_ptr(), FB_PATH.len() as i32, 0, 0, 0, 0, &mut fb_fd,
        );
        if rc != 0 {
            std::process::exit(15);
        }

        // Compositor + protocol state. The library owns the
        // framebuffer + every client's surface tree.
        let mut server = display_server::Server::new();
        let mut i: u32 = 0;

        'outer: loop {
            if poll_sigterm() {
                break 'outer;
            }

            let rc = ipc_accept(listener);
            if rc < 0 {
                if rc == -EINTR {
                    if poll_sigterm() {
                        break 'outer;
                    }
                    continue 'outer;
                }
                std::process::exit(12);
            }
            let server_fd: i32 = rc;

            // Allocate a protocol-side ClientId for this fd. We
            // will dispatch any protocol messages we read against
            // this id.
            let client_id = server.accept();

            // Drain the client's bytes. EAGAIN-polled — a future
            // slice migrates this to blocking semantics matching
            // ipc_accept.
            let mut recv_buf = [0u8; 4096];
            let read_iov = Iovec {
                buf: recv_buf.as_mut_ptr(),
                buf_len: recv_buf.len() as u32,
            };
            let mut nread: u32 = 0;
            let mut got_bytes = false;
            for _ in 0..MAX_POLLS {
                let rc = fd_read(server_fd, &read_iov, 1, &mut nread);
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

            let chunk = &recv_buf[..nread as usize];

            if let Some(_total) = display_server::detect_protocol_message(chunk) {
                // Protocol path: route through the dispatcher.
                // Errors at this layer (unknown opcode, malformed
                // payload, etc.) are protocol-level concerns; we
                // ignore them at the binary level so the server
                // doesn't exit on a misbehaving client. A future
                // slice can post pmd_display.error events back.
                let _ = server.dispatch_request(client_id, chunk);

                // Drain server→client events back through the
                // socket. Empty drain is the common case for a
                // simple commit-only client.
                if let Some(events) = server.drain_client_events(client_id) {
                    if !events.is_empty() {
                        let ev_iov = Ciovec {
                            buf: events.as_ptr(),
                            buf_len: events.len() as u32,
                        };
                        let mut written: u32 = 0;
                        let _ = fd_write(server_fd, &ev_iov, 1, &mut written);
                    }
                }

                // Present the composed framebuffer. The real fb
                // driver consumes whatever bytes land at /dev/fb0;
                // the TS-side `fb:blit` host message carries the
                // full pixel array. For very large framebuffers
                // (default 1024×768×4 = 3 MiB) this is a one-shot
                // write — the kernel's CharDevice write is a
                // memcpy + postMessage, no chunking required.
                let pixels = server.framebuffer().pixels();
                let fb_iov = Ciovec {
                    buf: pixels.as_ptr(),
                    buf_len: pixels.len() as u32,
                };
                let mut fb_written: u32 = 0;
                let rc = fd_write(fb_fd as i32, &fb_iov, 1, &mut fb_written);
                if rc != 0 {
                    std::process::exit(16);
                }
            } else {
                // Legacy raw-blit path: the demo client writes
                // 16 bytes of RGBA that don't form a protocol
                // header. Forward verbatim to /dev/fb0 so the
                // pre-T110 Playwright assertions still match.
                let fb_iov = Ciovec {
                    buf: chunk.as_ptr(),
                    buf_len: chunk.len() as u32,
                };
                let mut fb_written: u32 = 0;
                let rc = fd_write(fb_fd as i32, &fb_iov, 1, &mut fb_written);
                if rc != 0 || fb_written != chunk.len() as u32 {
                    std::process::exit(16);
                }
            }

            let rc = fd_close(server_fd);
            if rc != 0 {
                std::process::exit(19);
            }

            // Disconnect the protocol-side client state too —
            // the next iteration starts with a fresh ClientId.
            let _ = server.disconnect(client_id);

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
    // only one the slice exercises.
}
