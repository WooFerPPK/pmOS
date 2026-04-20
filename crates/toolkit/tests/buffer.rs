//! [`toolkit::BufferPool`] isolation tests.
//!
//! Pairs the application-level [`toolkit::App`] + [`toolkit::Window`]
//! facades with a bidirectional mock connection that
//! pre-seeds `registry.global` advertisements (so
//! `App::connect` completes its bind handshake) and then
//! exercises the BufferPool through its public API:
//! construct (emits shm_create_pool + two
//! shm_pool_create_buffer requests), acquire + paint,
//! commit + swap (emits surface_attach + surface_damage +
//! surface_commit), handle release, ping-pong two frames.
//!
//! The mock-server scaffold mirrors `tests/window.rs`
//! exactly — every outbound byte sequence is parsed by
//! `parse_requests` into a `ParsedRequest` list that each
//! test asserts against.
//!
//! These tests pin the shm-pool + attach/commit wire
//! sequence defined by `contracts/display-protocol.md` §§7–9
//! so changes to the request shapes surface as test
//! failures before they can reach the server-side
//! dispatcher.

use std::collections::VecDeque;

use display_proto::events::RegistryGlobal;
use display_proto::requests::{
    buffer_format, ShmCreatePool, ShmPoolCreateBuffer, SurfaceAttach, SurfaceDamage,
};

use toolkit::draw::BYTES_PER_PIXEL;
use toolkit::protocol::Connection;
use toolkit::{App, BufferPool, HEADER_SIZE, MessageHeader, ObjectId, Window};

/// Bidirectional in-memory [`Connection`] — same shape as
/// `tests/window.rs`. Outbound buffering plus a queued
/// inbound stream.
#[derive(Default)]
struct LoopbackConnection {
    outbound: Vec<u8>,
    inbound: VecDeque<Vec<u8>>,
}

impl LoopbackConnection {
    fn new() -> Self {
        LoopbackConnection::default()
    }

    fn push_inbound(&mut self, bytes: Vec<u8>) {
        self.inbound.push_back(bytes);
    }
}

impl Connection for LoopbackConnection {
    fn send(&mut self, bytes: &[u8]) {
        self.outbound.extend_from_slice(bytes);
    }

    fn drain_outbound(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.outbound)
    }

    fn recv(&mut self) -> Vec<u8> {
        self.inbound.pop_front().unwrap_or_default()
    }
}

/// The toolkit's id allocator hands out registry first.
const REGISTRY_ID: ObjectId = ObjectId::new(3);

/// Build a framed `pmd_registry.global(name, interface,
/// version)` event targeting `registry_id`.
fn build_global_event(
    registry_id: ObjectId,
    name: u32,
    interface: &str,
    version: u32,
) -> Vec<u8> {
    let event = RegistryGlobal {
        name,
        interface: interface.to_string(),
        version,
    };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    let header =
        MessageHeader::try_new(registry_id, 1 /* global */, payload.len(), 0).unwrap();
    header.encode(&mut out[..HEADER_SIZE]).unwrap();
    out[HEADER_SIZE..].copy_from_slice(&payload);
    out
}

/// A parsed outbound request, ready to assert against.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ParsedRequest {
    object_id: ObjectId,
    opcode: u16,
    payload: Vec<u8>,
}

/// Parse every framed message out of `bytes` into
/// [`ParsedRequest`]s for assertion.
fn parse_requests(mut bytes: &[u8]) -> Vec<ParsedRequest> {
    let mut out = Vec::new();
    while bytes.len() >= HEADER_SIZE {
        let header = MessageHeader::decode(bytes).expect("valid framed request");
        let msg_len = header.length as usize;
        assert!(bytes.len() >= msg_len, "truncated framed request");
        out.push(ParsedRequest {
            object_id: header.object_id,
            opcode: header.opcode,
            payload: bytes[HEADER_SIZE..msg_len].to_vec(),
        });
        bytes = &bytes[msg_len..];
    }
    assert!(bytes.is_empty(), "leftover bytes after parse");
    out
}

/// Pre-seed the three required globals so `App::connect`
/// succeeds immediately.
fn seed_registry(conn: &mut LoopbackConnection) {
    let mut batch = Vec::new();
    batch.extend(build_global_event(REGISTRY_ID, 1, "pmd_compositor", 1));
    batch.extend(build_global_event(REGISTRY_ID, 2, "pmd_shm", 1));
    batch.extend(build_global_event(REGISTRY_ID, 3, "pmd_xdg_shell", 1));
    conn.push_inbound(batch);
}

/// Shared test width / height. Intentionally tiny so the
/// back-buffer offset (`width * height * 4`) is a small
/// number the assertions can state directly.
const WIDTH: u32 = 4;
const HEIGHT: u32 = 2;

/// Per-buffer size in bytes for the shared WIDTH x HEIGHT:
/// 4 * 2 * 4 = 32 bytes per buffer.
const PER_BUFFER_BYTES: u32 = WIDTH * HEIGHT * BYTES_PER_PIXEL as u32;

#[test]
fn buffer_pool_new_sends_shm_create_pool_and_two_buffers() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");

    // Drop the bind-phase outbound bytes — those are
    // covered in tests/app.rs.
    let _ = app.client_mut().connection_mut().drain_outbound();

    let shm_id = app.shm();
    let pool = BufferPool::new(&mut app, WIDTH, HEIGHT).expect("pool must create");

    assert_eq!(pool.width(), WIDTH);
    assert_eq!(pool.height(), HEIGHT);
    assert_eq!(pool.stride(), WIDTH * BYTES_PER_PIXEL as u32);
    assert_eq!(pool.size(), PER_BUFFER_BYTES * 2);
    assert_eq!(pool.buffer_offset(0), 0);
    assert_eq!(pool.buffer_offset(1), PER_BUFFER_BYTES as usize);
    assert_eq!(pool.back_index(), 0);
    assert!(!pool.is_in_use(0));
    assert!(!pool.is_in_use(1));

    let pool_id = pool.pool_id();
    let buffer_0 = pool.buffer_id(0);
    let buffer_1 = pool.buffer_id(1);
    assert_ne!(pool_id, buffer_0);
    assert_ne!(buffer_0, buffer_1);

    let bytes = app.client_mut().connection_mut().drain_outbound();
    let requests = parse_requests(&bytes);
    assert_eq!(requests.len(), 3);

    // 1. shm.create_pool(new_id=pool, size=2*w*h*4)
    assert_eq!(requests[0].object_id, shm_id);
    assert_eq!(requests[0].opcode, 1 /* create_pool */);
    let create_pool = ShmCreatePool::decode(&requests[0].payload)
        .expect("create_pool payload must decode");
    assert_eq!(create_pool.new_id, pool_id);
    assert_eq!(create_pool.size, PER_BUFFER_BYTES * 2);

    // 2. shm_pool.create_buffer(new_id=buf0, offset=0, w,
    //    h, stride=w*4, format=ARGB8888)
    assert_eq!(requests[1].object_id, pool_id);
    assert_eq!(requests[1].opcode, 1 /* create_buffer */);
    let buf_a = ShmPoolCreateBuffer::decode(&requests[1].payload)
        .expect("buffer 0 payload must decode");
    assert_eq!(buf_a.new_id, buffer_0);
    assert_eq!(buf_a.offset, 0);
    assert_eq!(buf_a.width, WIDTH);
    assert_eq!(buf_a.height, HEIGHT);
    assert_eq!(buf_a.stride, WIDTH * BYTES_PER_PIXEL as u32);
    assert_eq!(buf_a.format, buffer_format::ARGB8888);

    // 3. shm_pool.create_buffer(new_id=buf1,
    //    offset=w*h*4, w, h, stride, format)
    assert_eq!(requests[2].object_id, pool_id);
    assert_eq!(requests[2].opcode, 1 /* create_buffer */);
    let buf_b = ShmPoolCreateBuffer::decode(&requests[2].payload)
        .expect("buffer 1 payload must decode");
    assert_eq!(buf_b.new_id, buffer_1);
    assert_eq!(buf_b.offset, PER_BUFFER_BYTES);
    assert_eq!(buf_b.width, WIDTH);
    assert_eq!(buf_b.height, HEIGHT);
    assert_eq!(buf_b.stride, WIDTH * BYTES_PER_PIXEL as u32);
    assert_eq!(buf_b.format, buffer_format::ARGB8888);
}

#[test]
fn buffer_pool_acquire_paints_into_back_buffer() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();

    let mut pool = BufferPool::new(&mut app, WIDTH, HEIGHT).expect("pool must create");
    let _ = app.client_mut().connection_mut().drain_outbound();

    {
        let mut canvas =
            pool.acquire_back_canvas().expect("back buffer should be free");
        assert_eq!(canvas.width(), WIDTH);
        assert_eq!(canvas.height(), HEIGHT);
        canvas.clear(toolkit::draw::Color::rgba(0x11, 0x22, 0x33, 0xff));
    }

    // The back buffer is at offset 0 (back_index == 0
    // immediately after construction). Assert every pixel
    // in the 4x2 region has the painted bytes.
    let back = pool.back_buffer();
    assert_eq!(back.len(), PER_BUFFER_BYTES as usize);
    for chunk in back.chunks_exact(BYTES_PER_PIXEL) {
        assert_eq!(chunk, &[0x11, 0x22, 0x33, 0xff]);
    }
}

#[test]
fn buffer_pool_commit_and_swap_attaches_and_damages() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();

    let mut window = Window::new(&mut app).expect("window must create");
    let surface_id = window.surface();
    let _ = window.app_mut().client_mut().connection_mut().drain_outbound();

    let mut pool = BufferPool::new(window.app_mut(), WIDTH, HEIGHT)
        .expect("pool must create");
    let buffer_0 = pool.buffer_id(0);
    let buffer_1 = pool.buffer_id(1);
    let _ = window.app_mut().client_mut().connection_mut().drain_outbound();

    // Paint something so the back buffer is non-empty.
    {
        let mut canvas =
            pool.acquire_back_canvas().expect("back buffer should be free");
        canvas.clear(toolkit::draw::Color::rgb(0xaa, 0xbb, 0xcc));
    }

    pool.commit_and_swap(&mut window)
        .expect("commit must succeed");

    // Back index flipped from 0 → 1, and buffer 0 is now
    // in-use (awaiting buffer.release).
    assert_eq!(pool.back_index(), 1);
    assert!(pool.is_in_use(0));
    assert!(!pool.is_in_use(1));

    let bytes = window.app_mut().client_mut().connection_mut().drain_outbound();
    let requests = parse_requests(&bytes);
    assert_eq!(requests.len(), 3);

    // 1. surface.attach(buffer_0, 0, 0)
    assert_eq!(requests[0].object_id, surface_id);
    assert_eq!(requests[0].opcode, 2 /* attach */);
    let attach =
        SurfaceAttach::decode(&requests[0].payload).expect("attach payload must decode");
    assert_eq!(attach.buffer_id, buffer_0);
    assert_eq!(attach.x, 0);
    assert_eq!(attach.y, 0);

    // 2. surface.damage(0, 0, w, h)
    assert_eq!(requests[1].object_id, surface_id);
    assert_eq!(requests[1].opcode, 3 /* damage */);
    let damage =
        SurfaceDamage::decode(&requests[1].payload).expect("damage payload must decode");
    assert_eq!(damage.x, 0);
    assert_eq!(damage.y, 0);
    assert_eq!(damage.width, WIDTH as i32);
    assert_eq!(damage.height, HEIGHT as i32);

    // 3. surface.commit()
    assert_eq!(requests[2].object_id, surface_id);
    assert_eq!(requests[2].opcode, 7 /* commit */);
    assert!(requests[2].payload.is_empty());

    // Sanity: buffer 1 is still unattached.
    let _ = buffer_1;
}

#[test]
fn buffer_pool_handle_release_flips_in_use_flag() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();

    let mut window = Window::new(&mut app).expect("window must create");
    let _ = window.app_mut().client_mut().connection_mut().drain_outbound();
    let mut pool = BufferPool::new(window.app_mut(), WIDTH, HEIGHT)
        .expect("pool must create");
    let buffer_0 = pool.buffer_id(0);
    let _ = window.app_mut().client_mut().connection_mut().drain_outbound();

    // Paint + commit once — marks buffer_0 as in-use.
    {
        let mut canvas =
            pool.acquire_back_canvas().expect("back buffer should be free");
        canvas.clear(toolkit::draw::Color::rgb(0, 0xff, 0));
    }
    pool.commit_and_swap(&mut window)
        .expect("commit must succeed");
    assert!(pool.is_in_use(0));

    // handle_release(buffer_0) → flag flips to false.
    assert!(pool.handle_release(buffer_0));
    assert!(!pool.is_in_use(0));

    // handle_release(buffer_0) again — now it's already
    // free, so the return is false (nothing flipped) but
    // the id is still recognised.
    assert!(!pool.handle_release(buffer_0));
    assert!(!pool.is_in_use(0));

    // handle_release with an unrelated id returns false
    // even on the first call.
    let stranger = ObjectId::new(9999);
    assert!(!pool.handle_release(stranger));
}

#[test]
fn buffer_pool_double_buffered_ping_pong() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();

    let mut window = Window::new(&mut app).expect("window must create");
    let _ = window.app_mut().client_mut().connection_mut().drain_outbound();
    let mut pool = BufferPool::new(window.app_mut(), WIDTH, HEIGHT)
        .expect("pool must create");
    let buffer_0 = pool.buffer_id(0);
    let _ = window.app_mut().client_mut().connection_mut().drain_outbound();

    // Frame 1: paint red into buffer 0, commit.
    {
        let mut canvas =
            pool.acquire_back_canvas().expect("buffer 0 should be free");
        canvas.clear(toolkit::draw::Color::rgb(0xff, 0, 0));
    }
    pool.commit_and_swap(&mut window)
        .expect("commit frame 1 must succeed");
    assert_eq!(pool.back_index(), 1);
    assert!(pool.is_in_use(0));
    assert!(!pool.is_in_use(1));

    // Simulate the server emitting buffer.release for
    // buffer 0 — caller routes it to the pool.
    assert!(pool.handle_release(buffer_0));

    // Drain frame-1 outbound before frame 2 so the later
    // assert can inspect frame 2's requests alone.
    let _ = window.app_mut().client_mut().connection_mut().drain_outbound();

    // Frame 2: now back_index is 1 and buffer 1 was never
    // attached, so acquire returns Some with a view onto
    // the buffer-1 region.
    {
        let mut canvas =
            pool.acquire_back_canvas().expect("buffer 1 should be free");
        canvas.clear(toolkit::draw::Color::rgb(0, 0, 0xff));
    }
    // The freshly-painted bytes live at offset
    // PER_BUFFER_BYTES (the buffer-1 region).
    let second_start = PER_BUFFER_BYTES as usize;
    let second_slice = &pool.back_buffer();
    assert_eq!(second_slice.len(), PER_BUFFER_BYTES as usize);
    for chunk in second_slice.chunks_exact(BYTES_PER_PIXEL) {
        assert_eq!(chunk, &[0, 0, 0xff, 0xff]);
    }

    pool.commit_and_swap(&mut window)
        .expect("commit frame 2 must succeed");
    assert_eq!(pool.back_index(), 0);
    assert!(!pool.is_in_use(0));
    assert!(pool.is_in_use(1));

    // The second attach targets buffer 1 — parse outbound
    // bytes from the second commit only.
    let bytes = window.app_mut().client_mut().connection_mut().drain_outbound();
    let requests = parse_requests(&bytes);
    assert_eq!(requests.len(), 3);
    let attach =
        SurfaceAttach::decode(&requests[0].payload).expect("attach payload must decode");
    assert_eq!(attach.buffer_id, pool.buffer_id(1));

    // silence unused warnings
    let _ = second_start;
}
