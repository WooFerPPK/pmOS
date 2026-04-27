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

/// True iff any framed message in `chunk` is a
/// `pmd_surface.commit` request (interface = Surface,
/// opcode = 7). Used by the main loop to decide whether
/// to present the framebuffer after dispatching a chunk —
/// only commits change pixels, so other requests skip the
/// (expensive, chunked) present path. Walks the chunk
/// header-by-header so a multi-message batch with a commit
/// at the end still triggers a present.
#[cfg(target_arch = "wasm32")]
fn chunk_contains_commit(chunk: &[u8]) -> bool {
    use display_server::HEADER_SIZE;
    let mut offset = 0usize;
    while offset + HEADER_SIZE <= chunk.len() {
        let Ok(header) = display_server::MessageHeader::decode(&chunk[offset..]) else {
            return false;
        };
        let msg_len = header.length as usize;
        if msg_len < HEADER_SIZE || offset + msg_len > chunk.len() {
            return false;
        }
        if header.opcode == 7 {
            return true;
        }
        offset += msg_len;
    }
    false
}

/// Dispatch every framed message in `chunk` against
/// `server` for the given client. The toolkit's send-side
/// stream batches consecutive requests into a single fd_write,
/// and the kernel coalesces them into one rx_buf, so by the
/// time `fd_read` returns a chunk it usually contains 1-N
/// complete protocol messages. Walks the chunk header-by-
/// header until either the buffer is consumed or a malformed
/// header surfaces. Each message's request is fed through
/// `Server::dispatch_request`; the binary's auto-advertise
/// hook fires per-message so multi-batch chunks containing a
/// `display.get_registry` still produce the globals catalog.
#[cfg(target_arch = "wasm32")]
fn dispatch_all_messages(
    server: &mut display_server::Server,
    client_id: display_server::ClientId,
    chunk: &[u8],
) {
    use display_server::HEADER_SIZE;
    let mut offset = 0usize;
    while offset + HEADER_SIZE <= chunk.len() {
        let Ok(header) = display_server::MessageHeader::decode(&chunk[offset..]) else {
            return;
        };
        let msg_len = header.length as usize;
        if msg_len < HEADER_SIZE || offset + msg_len > chunk.len() {
            return;
        }
        let msg = &chunk[offset..offset + msg_len];
        let _ = server.dispatch_request(client_id, msg);
        advertise_globals_for_get_registry(server, client_id, msg);
        offset += msg_len;
    }
}

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

        'outer: loop {
            if poll_sigterm() {
                break 'outer;
            }

            // Drain input events at the top of every iteration
            // — the kernel's input rings buffer events while we
            // were busy serving the previous client. Any motion
            // / button events feed the active drag state machine
            // (T133 server-side); on a non-empty drain we
            // re-present so the user sees the geometry update.
            let mut input_dirty = false;
            if mouse_fd >= 0 || kbd_fd >= 0 {
                input_dirty = drain_input_events(&mut server, mouse_fd, kbd_fd);
            }
            if input_dirty {
                // Re-present so any drag-driven geometry update
                // reaches the framebuffer before we block on the
                // next client accept. The composite cache lives
                // inside the server library; this is a one-shot
                // pixel write.
                if !present_framebuffer(&server, fb_fd as i32) {
                    std::process::exit(16);
                }
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
            // ipc_accept. Each retry also drains input events
            // so an active drag continues to advance even while
            // we're waiting for the client's next byte chunk.
            //
            // 32 KiB recv buffer fits a full shm_pool.write
            // chunk (24 KiB pixel data + 4-byte offset + header)
            // plus a bit of headroom; the toolkit's BufferPool
            // sizes its writes to fit in one syscall.
            let mut recv_buf = [0u8; 32 * 1024];
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
                    if mouse_fd >= 0 || kbd_fd >= 0 {
                        if drain_input_events(&mut server, mouse_fd, kbd_fd) {
                            // Geometry may have changed; re-present
                            // before the next accept tick.
                            let _ = present_framebuffer(&server, fb_fd as i32);
                        }
                    }
                    continue;
                }
                std::process::exit(14);
            }
            if !got_bytes {
                std::process::exit(18);
            }

            let first_chunk = &recv_buf[..nread as usize];
            let is_protocol = display_server::detect_protocol_message(first_chunk).is_some();

            if !is_protocol {
                // Legacy raw-blit path: the demo client writes
                // 16 bytes of RGBA that don't form a protocol
                // header. Forward verbatim to /dev/fb0 so the
                // pre-T110 Playwright assertions still match,
                // then close — demo clients are one-shot.
                let fb_iov = Ciovec {
                    buf: first_chunk.as_ptr(),
                    buf_len: first_chunk.len() as u32,
                };
                let mut fb_written: u32 = 0;
                let rc = fd_write(fb_fd as i32, &fb_iov, 1, &mut fb_written);
                if rc != 0 || fb_written != first_chunk.len() as u32 {
                    std::process::exit(16);
                }
                let rc = fd_close(server_fd);
                if rc != 0 {
                    std::process::exit(19);
                }
                let _ = server.disconnect(client_id);
                println!("display-server served client {}", i);
                i += 1;
                continue 'outer;
            }

            // Protocol path: serve this client persistently.
            // Real clients (the desktop shell, apps that link
            // toolkit) keep their connection open across
            // many request batches — initial bind handshake,
            // commits per frame, set_title / set_app_id /
            // set_maximized / move / resize over the lifetime
            // of the window. Process the first batch + every
            // subsequent batch on the same fd until either the
            // client disconnects, SIGTERM lands, or fd_read
            // returns a real error.
            dispatch_all_messages(&mut server, client_id, first_chunk);
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
            // Only present if the first batch contained a
            // surface.commit — get_registry / bind /
            // create_surface paths don't change pixels.
            if chunk_contains_commit(first_chunk) {
                if !present_framebuffer(&server, fb_fd as i32) {
                    std::process::exit(16);
                }
            }
            // Print served-client marker for the FIRST batch
            // so the Playwright spec's served-client poll
            // observes a connected protocol client.
            println!("display-server served client {}", i);
            i += 1;

            // Inner serve loop: drain the same fd, dispatch,
            // reply, present. Spins on EAGAIN with input + signal
            // polling so the user can drag and the server can
            // SIGTERM cleanly. `pending` accumulates bytes
            // across reads so a message split across syscall
            // boundaries (e.g. a chunked shm_pool.write whose
            // tail straddles two fd_reads) is reassembled and
            // dispatched as a single message.
            let mut pending: Vec<u8> = Vec::with_capacity(64 * 1024);
            'inner: loop {
                if poll_sigterm() {
                    break 'outer;
                }
                if mouse_fd >= 0 || kbd_fd >= 0 {
                    if drain_input_events(&mut server, mouse_fd, kbd_fd) {
                        let _ = present_framebuffer(&server, fb_fd as i32);
                    }
                }
                let mut nread: u32 = 0;
                let rc = fd_read(server_fd, &read_iov, 1, &mut nread);
                if rc == 0 && nread > 0 {
                    pending.extend_from_slice(&recv_buf[..nread as usize]);
                    let mut had_commit = false;
                    let mut consumed_total = 0usize;
                    loop {
                        let remaining = &pending[consumed_total..];
                        if remaining.len() < display_server::HEADER_SIZE {
                            break;
                        }
                        let Ok(header) = display_server::MessageHeader::decode(remaining) else {
                            consumed_total = pending.len();
                            break;
                        };
                        let msg_len = header.length as usize;
                        if msg_len < display_server::HEADER_SIZE {
                            consumed_total = pending.len();
                            break;
                        }
                        if msg_len > remaining.len() {
                            break;
                        }
                        let msg = &remaining[..msg_len];
                        let _ = server.dispatch_request(client_id, msg);
                        advertise_globals_for_get_registry(&mut server, client_id, msg);
                        if header.opcode == 7 {
                            had_commit = true;
                        }
                        consumed_total += msg_len;
                    }
                    if consumed_total > 0 {
                        pending.drain(..consumed_total);
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
                        if had_commit {
                            if !present_framebuffer(&server, fb_fd as i32) {
                                std::process::exit(16);
                            }
                        }
                    }
                    continue 'inner;
                }
                if rc == 0 || rc == EAGAIN {
                    continue 'inner;
                }
                break 'inner;
            }

            let _ = fd_close(server_fd);
            let _ = server.disconnect(client_id);
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
