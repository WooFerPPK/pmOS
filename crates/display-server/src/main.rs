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
    fn sched_yield() -> i32;
}

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "pmos_ext")]
extern "C" {
    fn display_bind() -> i32;
    fn ipc_accept_nonblock(listener_fd: i32) -> i32;
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

/// If `chunk` is a `pmd_display.get_registry` request, push
/// the v1 globals catalog onto the client's pending events
/// queue so the toolkit's App::connect bind handshake can
/// complete on the next recv. The advertise lives in the
/// binary (not the library's `Server::dispatch_request`)
/// because integration tests under `crates/integration-tests/`
/// drive Server directly and emit globals manually — keeping
/// auto-advertise here keeps the binary's production path
/// self-contained without changing those tests' assumptions.
#[cfg(target_arch = "wasm32")]
fn advertise_globals_for_get_registry(
    server: &mut display_server::Server,
    client_id: display_server::ClientId,
    chunk: &[u8],
) {
    use display_server::HEADER_SIZE;
    let Ok(header) = display_server::MessageHeader::decode(chunk) else {
        return;
    };
    if header.object_id != display_server::ObjectId::DISPLAY || header.opcode != 2 {
        return;
    }
    let payload_end = header.length as usize;
    if payload_end < HEADER_SIZE || payload_end > chunk.len() {
        return;
    }
    let payload = &chunk[HEADER_SIZE..payload_end];
    if let Ok(req) = display_proto::requests::DisplayGetRegistry::decode(payload) {
        server.advertise_globals_to(client_id, req.new_id);
    }
}

#[cfg(target_arch = "wasm32")]
unsafe fn open_dev(path: &[u8]) -> Option<u32> {
    let mut fd: u32 = 0;
    let rc = path_open(
        0, 0, path.as_ptr(), path.len() as i32, 0, 0, 0, 0, &mut fd,
    );
    if rc != 0 {
        return None;
    }
    Some(fd)
}

/// Drain every available input event from the kernel's
/// `/dev/input/{mouse,kbd}` rings and inject each into
/// `server`. Returns true when at least one event was
/// processed (the caller can use this as a "pixels may be
/// dirty" hint and re-present).
#[cfg(target_arch = "wasm32")]
unsafe fn drain_input_events(
    server: &mut display_server::Server,
    mouse_fd: i32,
    kbd_fd: i32,
) -> bool {
    let mut any = false;
    // Mouse events are 20 bytes each; read up to 32 events (640
    // bytes) per drain to keep the buffer small while not
    // capping common sequences.
    let mut mouse_buf = [0u8; 640];
    let iov = Iovec {
        buf: mouse_buf.as_mut_ptr(),
        buf_len: mouse_buf.len() as u32,
    };
    let mut nread: u32 = 0;
    let rc = fd_read(mouse_fd, &iov, 1, &mut nread);
    if rc == 0 && nread > 0 {
        let n = display_server::drain_mouse_events_into(
            &mouse_buf[..nread as usize],
            server,
        );
        if n > 0 {
            any = true;
        }
    }
    // Keyboard events are 8 bytes; read up to 64 (512 bytes).
    let mut kbd_buf = [0u8; 512];
    let iov = Iovec {
        buf: kbd_buf.as_mut_ptr(),
        buf_len: kbd_buf.len() as u32,
    };
    let mut nread: u32 = 0;
    let rc = fd_read(kbd_fd, &iov, 1, &mut nread);
    if rc == 0 && nread > 0 {
        let n = display_server::drain_kbd_events_into(
            &kbd_buf[..nread as usize],
            server,
        );
        if n > 0 {
            any = true;
        }
    }
    any
}

/// FB driver op code for set-mode (matches OP_SET_MODE in
/// `web/src/drivers/fb.ts`).
#[cfg(target_arch = "wasm32")]
const FB_OP_SET_MODE: u8 = 0x01;
/// FB driver op code for blit (matches OP_BLIT in
/// `web/src/drivers/fb.ts`).
#[cfg(target_arch = "wasm32")]
#[allow(dead_code)]
const FB_OP_BLIT: u8 = 0x02;
/// Begin a chunked blit (allocates accumulator). Payload:
/// `(width: u32 LE, height: u32 LE)`.
#[cfg(target_arch = "wasm32")]
const FB_OP_BLIT_BEGIN: u8 = 0x03;
/// Append pixels to the chunked blit. Payload:
/// `(offset: u32 LE) || pixel_bytes`.
#[cfg(target_arch = "wasm32")]
const FB_OP_BLIT_CHUNK: u8 = 0x04;
/// Finalize the chunked blit — driver posts `fb:blit` to main.
#[cfg(target_arch = "wasm32")]
const FB_OP_BLIT_END: u8 = 0x05;

/// Send `OP_SET_MODE(width, height)` to the framebuffer
/// driver. Called once at startup so the host-side renderer
/// knows the framebuffer's pixel dimensions before the first
/// blit lands. Payload layout (matching
/// `crates/kernel/src/dev/mod.rs::framebuffer_write` +
/// `web/src/drivers/fb.ts::handleSetMode`):
///
///   ```text
///   [op:u8 = OP_SET_MODE] [width:u32 LE] [height:u32 LE]
///   ```
#[cfg(target_arch = "wasm32")]
unsafe fn fb_set_mode(fb_fd: i32, width: u32, height: u32) -> bool {
    let mut buf = [0u8; 9];
    buf[0] = FB_OP_SET_MODE;
    buf[1..5].copy_from_slice(&width.to_le_bytes());
    buf[5..9].copy_from_slice(&height.to_le_bytes());
    let iov = Ciovec {
        buf: buf.as_ptr(),
        buf_len: buf.len() as u32,
    };
    let mut written: u32 = 0;
    let rc = fd_write(fb_fd, &iov, 1, &mut written);
    rc == 0
}

/// Present the server's composed framebuffer to `/dev/fb0`
/// using the chunked-blit op sequence. The SAB ring's per-
/// syscall heap window is 32 KiB; a full frame (e.g. 800×600
/// = 1.9 MiB) doesn't fit in one fd_write, so the binary
/// splits the frame into a `BEGIN` + N × `CHUNK` + `END`
/// sequence. The FB driver's TS side accumulates the chunks
/// into a single `fb:blit` postMessage, indistinguishable
/// from a non-chunked blit on the receiving side.
///
/// Returns `true` if every chunk's fd_write succeeded.
#[cfg(target_arch = "wasm32")]
unsafe fn present_framebuffer(
    server: &display_server::Server,
    fb_fd: i32,
) -> bool {
    /// Per-fd_write payload cap. Must fit alongside the
    /// 4-byte (`offset: u32`) chunk header in the SAB ring's
    /// 32 KiB heap window. 24 KiB leaves comfortable headroom
    /// for the request slot's other fields.
    const CHUNK_BYTES: usize = 24 * 1024;
    let fb = server.framebuffer();
    let width = fb.width();
    let height = fb.height();
    let pixels = fb.pixels();

    // BEGIN: payload (width, height).
    {
        let mut hdr = [0u8; 9];
        hdr[0] = FB_OP_BLIT_BEGIN;
        hdr[1..5].copy_from_slice(&width.to_le_bytes());
        hdr[5..9].copy_from_slice(&height.to_le_bytes());
        let iov = Ciovec { buf: hdr.as_ptr(), buf_len: hdr.len() as u32 };
        let mut written: u32 = 0;
        let rc = fd_write(fb_fd, &iov, 1, &mut written);
        if rc != 0 {
            return false;
        }
    }

    // CHUNK: walk the pixels, sending CHUNK_BYTES at a time.
    let mut offset: usize = 0;
    while offset < pixels.len() {
        let end = core::cmp::min(offset + CHUNK_BYTES, pixels.len());
        let slice = &pixels[offset..end];
        let mut buf: Vec<u8> = Vec::with_capacity(5 + slice.len());
        buf.push(FB_OP_BLIT_CHUNK);
        buf.extend_from_slice(&(offset as u32).to_le_bytes());
        buf.extend_from_slice(slice);
        let iov = Ciovec { buf: buf.as_ptr(), buf_len: buf.len() as u32 };
        let mut written: u32 = 0;
        let rc = fd_write(fb_fd, &iov, 1, &mut written);
        if rc != 0 {
            return false;
        }
        offset = end;
    }

    // END: payload empty. Driver posts the assembled blit to main.
    {
        let hdr = [FB_OP_BLIT_END];
        let iov = Ciovec { buf: hdr.as_ptr(), buf_len: hdr.len() as u32 };
        let mut written: u32 = 0;
        let rc = fd_write(fb_fd, &iov, 1, &mut written);
        if rc != 0 {
            return false;
        }
    }
    true
}

#[cfg(target_arch = "wasm32")]
fn main() {
    println!("display-server starting");

    const EAGAIN: i32 = 6;
    const EINTR: i32 = 27;

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

        // T133 finishing touch: input device fds. Open
        // `/dev/input/{mouse,kbd}` so the server can drain
        // pointer + keyboard events between accept iterations
        // and inject them through Server::inject_*. If either
        // device is missing (older kernel build) the open fails
        // and we fall back to "no input" mode — the protocol
        // path keeps working without input.
        // Note: the v1 devfs is FLAT — the input nodes live
        // at /dev/input_mouse and /dev/input_kbd (not under
        // a /dev/input/ subdirectory). See crates/kernel/
        // src/fs/devfs.rs.
        // T133: open /dev/input_mouse + /dev/input_kbd so the
        // server can drain pointer + keyboard events between
        // accept iterations and inject them through
        // Server::inject_*. When the open fails (older kernel
        // build, devfs missing the node) the server falls back
        // to "no input" mode silently.
        // Note: the v1 devfs is FLAT — the input nodes live at
        // /dev/input_mouse / /dev/input_kbd (not under a
        // /dev/input/ subdirectory). See crates/kernel/src/fs/
        // devfs.rs.
        let mouse_fd = open_dev(b"/dev/input_mouse")
            .map(|fd| fd as i32)
            .unwrap_or(-1);
        let kbd_fd = open_dev(b"/dev/input_kbd")
            .map(|fd| fd as i32)
            .unwrap_or(-1);

        // Compositor + protocol state. The library owns the
        // framebuffer + every client's surface tree.
        let mut server = display_server::Server::new();
        let mut i: u32 = 0;

        // Tell the host-side framebuffer driver about the
        // composed framebuffer's pixel dimensions BEFORE the
        // first present. The FB driver's TS side
        // (`web/src/drivers/fb.ts`) sizes its `OffscreenCanvas`
        // off the SET_MODE width/height; subsequent BLIT ops
        // must match these dimensions exactly.
        let fb_w = server.framebuffer().width();
        let fb_h = server.framebuffer().height();
        if !fb_set_mode(fb_fd as i32, fb_w, fb_h) {
            std::process::exit(16);
        }

        // Connected clients we are multiplexing across. Each
        // entry is `(server_fd, client_id, pending)`: the wasi
        // socket fd, the protocol-layer ClientId, and a
        // re-assembly buffer for messages split across
        // fd_read boundaries. `legacy_demo` clients (raw-blit
        // path) are handled inline at accept time and never
        // land in this list.
        struct Conn {
            server_fd: i32,
            client_id: display_server::ClientId,
            pending: Vec<u8>,
        }
        let mut conns: Vec<Conn> = Vec::new();
        let mut recv_buf = [0u8; 32 * 1024];
        let read_iov = Iovec {
            buf: recv_buf.as_mut_ptr(),
            buf_len: recv_buf.len() as u32,
        };

        'outer: loop {
            if poll_sigterm() {
                break 'outer;
            }

            // Drain input events at the top of every iteration.
            // Pointer motion + button events feed the active-drag
            // machinery and the click-to-focus path; geometry-
            // dirtying events trigger a re-present after the
            // poll cycle.
            let mut frame_dirty = false;
            if mouse_fd >= 0 || kbd_fd >= 0 {
                if drain_input_events(&mut server, mouse_fd, kbd_fd) {
                    frame_dirty = true;
                }
            }

            // Accept any pending connections in a tight loop —
            // ipc_accept returns EAGAIN when the backlog is empty,
            // EINTR if a signal landed mid-call. Other negatives
            // are fatal as before.
            loop {
                let rc = ipc_accept_nonblock(listener);
                if rc >= 0 {
                    let new_fd = rc;
                    // Grant Cap::Shell to every protocol-speaking
                    // client so the desktop shell can bind
                    // pmd_shell_manager. The kernel's
                    // display_connect already gates who is
                    // permitted to connect at all (Cap::DisplayClient);
                    // cross-client cap-checking inside the
                    // display server is stronger than what the
                    // single-process v1 substrate needs.
                    let mut caps = abi::cap::CapSet::EMPTY;
                    caps.insert(abi::cap::Cap::Shell);
                    let client_id = server.accept_with_caps(caps);
                    conns.push(Conn {
                        server_fd: new_fd,
                        client_id,
                        pending: Vec::with_capacity(64 * 1024),
                    });
                    continue;
                }
                if rc == -EINTR {
                    if poll_sigterm() {
                        break 'outer;
                    }
                    continue;
                }
                if rc == -EAGAIN {
                    break;
                }
                std::process::exit(12);
            }

            // Walk every connected client. For each, do one
            // non-blocking fd_read; if bytes came in, run the
            // first-chunk protocol-vs-raw-blit detection if
            // we haven't dispatched anything for this client
            // yet (pending is empty AND no journal entries
            // have been recorded). Otherwise run the streamed
            // dispatch loop.
            let mut commit_dirty = false;
            let mut idx = 0;
            while idx < conns.len() {
                let server_fd = conns[idx].server_fd;
                let client_id = conns[idx].client_id;

                let mut nread: u32 = 0;
                let rc = fd_read(server_fd, &read_iov, 1, &mut nread);
                if rc == 0 && nread > 0 {
                    let chunk = &recv_buf[..nread as usize];

                    // Legacy raw-blit detection: only consider it
                    // for the very first read on this fd, before
                    // any protocol bytes have been accumulated.
                    let no_pending_yet = conns[idx].pending.is_empty();
                    let no_journal_yet = server
                        .client(client_id)
                        .map(|c| c.journal.is_empty())
                        .unwrap_or(false);
                    if no_pending_yet
                        && no_journal_yet
                        && display_server::detect_protocol_message(chunk).is_none()
                    {
                        let fb_iov = Ciovec {
                            buf: chunk.as_ptr(),
                            buf_len: chunk.len() as u32,
                        };
                        let mut fb_written: u32 = 0;
                        let rc = fd_write(fb_fd as i32, &fb_iov, 1, &mut fb_written);
                        if rc != 0 || fb_written != chunk.len() as u32 {
                            std::process::exit(16);
                        }
                        let _ = fd_close(server_fd);
                        let _ = server.disconnect(client_id);
                        conns.swap_remove(idx);
                        println!("display-server served client {}", i);
                        i += 1;
                        continue;
                    }

                    // Protocol path: append + walk full messages.
                    conns[idx].pending.extend_from_slice(chunk);
                    let mut had_commit = false;
                    let mut consumed_total = 0usize;
                    let mut first_dispatch_for_this_client =
                        no_journal_yet && no_pending_yet;
                    loop {
                        let remaining = &conns[idx].pending[consumed_total..];
                        if remaining.len() < display_server::HEADER_SIZE {
                            break;
                        }
                        let len_field =
                            u16::from_le_bytes([remaining[6], remaining[7]]) as usize;
                        if len_field < display_server::HEADER_SIZE {
                            // Spec violation — discard everything.
                            consumed_total = conns[idx].pending.len();
                            break;
                        }
                        if len_field > remaining.len() {
                            break;
                        }
                        let msg_start = consumed_total;
                        let msg_end = consumed_total + len_field;
                        let opcode = u16::from_le_bytes([
                            conns[idx].pending[msg_start + 4],
                            conns[idx].pending[msg_start + 5],
                        ]);
                        // Borrow the pending slice JUST for the
                        // dispatch call, then drop the borrow so
                        // we can mutate `conns[idx].pending` later.
                        let msg_owned: Vec<u8> =
                            conns[idx].pending[msg_start..msg_end].to_vec();
                        let _ = server.dispatch_request(client_id, &msg_owned);
                        advertise_globals_for_get_registry(
                            &mut server,
                            client_id,
                            &msg_owned,
                        );
                        if opcode == 7 {
                            had_commit = true;
                        }
                        consumed_total += len_field;
                    }
                    if consumed_total > 0 {
                        conns[idx].pending.drain(..consumed_total);
                        if first_dispatch_for_this_client {
                            // Mark the first served-client marker
                            // exactly once, on the first chunk
                            // that produced any dispatched message.
                            // (The journal check earlier is the
                            // canonical "did we dispatch anything"
                            // test; this branch is the FIRST time
                            // dispatched is non-empty for this fd.)
                            println!("display-server served client {}", i);
                            i += 1;
                            first_dispatch_for_this_client = false;
                        }
                    }
                    let _ = first_dispatch_for_this_client;
                    if had_commit {
                        commit_dirty = true;
                    }
                } else if rc == 0 || rc == EAGAIN {
                    // No bytes ready right now. Move on to the
                    // next client.
                } else {
                    // Real error — disconnect this client.
                    let _ = fd_close(server_fd);
                    let _ = server.disconnect(client_id);
                    conns.swap_remove(idx);
                    continue;
                }
                idx += 1;
            }

            // After the read pass, flush every client's
            // pending events. Events can land on clients OTHER
            // than the one we just dispatched a message for —
            // pmd_shell_manager broadcasts events to every
            // subscribed shell when ANY app's toplevel
            // mutates, so the broadcast pass below has to
            // reach the shell's fd even if the only fd we
            // read this tick was the app's.
            for c in conns.iter_mut() {
                if let Some(events) = server.drain_client_events(c.client_id) {
                    if events.is_empty() {
                        continue;
                    }
                    let ev_iov = Ciovec {
                        buf: events.as_ptr(),
                        buf_len: events.len() as u32,
                    };
                    let mut written: u32 = 0;
                    let _ = fd_write(c.server_fd, &ev_iov, 1, &mut written);
                }
            }

            if commit_dirty || frame_dirty {
                if !present_framebuffer(&server, fb_fd as i32) {
                    std::process::exit(16);
                }
            }

            // Yield so other workers (kernel-worker, app
            // workers) get a chance to run between poll passes.
            // Without this we burn the worker until SIGTERM.
            let _ = sched_yield();
        }

        for c in &conns {
            let _ = fd_close(c.server_fd);
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
