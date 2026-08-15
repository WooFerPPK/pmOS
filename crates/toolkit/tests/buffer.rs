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

use display_proto::events::{CallbackDone, DisplayDeleteId, RegistryGlobal};
use display_proto::requests::{
    buffer_format, ShmCreatePool, ShmPoolCreateBuffer, ShmPoolWrite, ShmPoolWriteRows,
    SurfaceAttach, SurfaceDamage, SurfaceFrame, SurfacePatchCurrent,
};

use toolkit::draw::{Rect, BYTES_PER_PIXEL};
use toolkit::protocol::Connection;
use toolkit::{
    App, BufferPool, CommitProgress, CurrentPatch, Interface, MessageHeader, ObjectId, Window,
    HEADER_SIZE,
};

/// Bidirectional in-memory [`Connection`] — same shape as
/// `tests/window.rs`. Outbound buffering plus a queued
/// inbound stream.
#[derive(Default)]
struct LoopbackConnection {
    outbound: Vec<u8>,
    inbound: VecDeque<Vec<u8>>,
    incremental: bool,
    blocked: bool,
    flush_quanta: VecDeque<usize>,
    flush_calls: usize,
    write_waits: usize,
    read_waits: usize,
    recv_calls: usize,
}

impl LoopbackConnection {
    fn new() -> Self {
        LoopbackConnection::default()
    }

    fn push_inbound(&mut self, bytes: Vec<u8>) {
        self.inbound.push_back(bytes);
    }

    fn incremental() -> Self {
        Self {
            incremental: true,
            ..Self::default()
        }
    }
}

impl Connection for LoopbackConnection {
    fn send(&mut self, bytes: &[u8]) {
        self.outbound.extend_from_slice(bytes);
        self.blocked = true;
        if let Some(done) = sync_done_for(bytes) {
            self.inbound.push_back(done);
        }
    }

    fn drain_outbound(&mut self) -> Vec<u8> {
        self.blocked = false;
        core::mem::take(&mut self.outbound)
    }

    fn recv(&mut self) -> Vec<u8> {
        self.recv_calls += 1;
        self.inbound.pop_front().unwrap_or_default()
    }

    fn flush_outbound(&mut self) -> Result<(), i32> {
        if !self.incremental || self.outbound.is_empty() {
            return Ok(());
        }
        self.flush_calls += 1;
        let quota = self.flush_quanta.pop_front().unwrap_or(usize::MAX);
        let written = quota.min(self.outbound.len());
        self.outbound.drain(..written);
        self.blocked = !self.outbound.is_empty();
        Ok(())
    }

    fn outbound_pending(&self) -> bool {
        self.incremental && self.blocked
    }

    fn incremental_uploads(&self) -> bool {
        self.incremental
    }

    fn wait(&mut self, _timeout: Option<std::time::Duration>) -> Result<(), i32> {
        if self.outbound_pending() {
            self.write_waits += 1;
        } else {
            self.read_waits += 1;
        }
        Ok(())
    }
}

fn sync_done_for(request: &[u8]) -> Option<Vec<u8>> {
    let header = MessageHeader::decode(request).ok()?;
    if header.object_id != ObjectId::DISPLAY || header.opcode != 1 {
        return None;
    }
    let callback = ObjectId::new(u32::from_le_bytes(
        request.get(HEADER_SIZE..HEADER_SIZE + 4)?.try_into().ok()?,
    ));
    let payload = 0u32.to_le_bytes();
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    MessageHeader::try_new(callback, 1, payload.len(), 0)
        .ok()?
        .encode(&mut out[..HEADER_SIZE])
        .ok()?;
    out[HEADER_SIZE..].copy_from_slice(&payload);
    Some(out)
}

/// The toolkit's id allocator hands out registry first.
const REGISTRY_ID: ObjectId = ObjectId::new(3);

/// Build a framed `pmd_registry.global(name, interface,
/// version)` event targeting `registry_id`.
fn build_global_event(registry_id: ObjectId, name: u32, interface: &str, version: u32) -> Vec<u8> {
    let event = RegistryGlobal {
        name,
        interface: interface.to_string(),
        version,
    };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    let header = MessageHeader::try_new(registry_id, 1 /* global */, payload.len(), 0).unwrap();
    header.encode(&mut out[..HEADER_SIZE]).unwrap();
    out[HEADER_SIZE..].copy_from_slice(&payload);
    out
}

fn build_event(object_id: ObjectId, opcode: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    MessageHeader::try_new(object_id, opcode, payload.len(), 0)
        .unwrap()
        .encode(&mut out[..HEADER_SIZE])
        .unwrap();
    out[HEADER_SIZE..].copy_from_slice(payload);
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

fn connected_app() -> App<LoopbackConnection> {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    App::connect(conn).expect("bootstrap must succeed")
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
    let create_pool =
        ShmCreatePool::decode(&requests[0].payload).expect("create_pool payload must decode");
    assert_eq!(create_pool.new_id, pool_id);
    assert_eq!(create_pool.size, PER_BUFFER_BYTES * 2);

    // 2. shm_pool.create_buffer(new_id=buf0, offset=0, w,
    //    h, stride=w*4, format=ARGB8888)
    assert_eq!(requests[1].object_id, pool_id);
    assert_eq!(requests[1].opcode, 1 /* create_buffer */);
    let buf_a =
        ShmPoolCreateBuffer::decode(&requests[1].payload).expect("buffer 0 payload must decode");
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
    let buf_b =
        ShmPoolCreateBuffer::decode(&requests[2].payload).expect("buffer 1 payload must decode");
    assert_eq!(buf_b.new_id, buffer_1);
    assert_eq!(buf_b.offset, PER_BUFFER_BYTES);
    assert_eq!(buf_b.width, WIDTH);
    assert_eq!(buf_b.height, HEIGHT);
    assert_eq!(buf_b.stride, WIDTH * BYTES_PER_PIXEL as u32);
    assert_eq!(buf_b.format, buffer_format::ARGB8888);
}

#[test]
fn buffer_pool_rejects_every_u32_geometry_overflow_before_allocating_or_sending() {
    let mut app = connected_app();
    let mut control = connected_app();
    let _ = app.client_mut().connection_mut().drain_outbound();
    let _ = control.client_mut().connection_mut().drain_outbound();
    let object_count = app.client().object_count();

    for (width, height, boundary) in [
        (u32::MAX, 0, "stride, even for zero height"),
        (65_536, 16_384, "one-buffer size"),
        (536_870_912, 1, "double-buffered pool size"),
    ] {
        let error = BufferPool::new(&mut app, width, height)
            .err()
            .unwrap_or_else(|| panic!("{boundary} overflow must be rejected"));
        assert_eq!(
            error,
            toolkit::ClientError::Wire(display_proto::wire::WireError::Overflow),
            "{boundary} overflow has one exact public error",
        );
        assert_eq!(app.client().object_count(), object_count);
        assert!(
            app.client_mut()
                .connection_mut()
                .drain_outbound()
                .is_empty(),
            "overflow validation must precede every protocol request",
        );
    }

    assert_eq!(
        app.client_mut().allocate_id().expect("subject next id"),
        control.client_mut().allocate_id().expect("control next id"),
        "overflow validation must not consume an object id",
    );
}

#[test]
fn zero_dimension_pools_are_valid_and_commits_are_noops() {
    for (width, height) in [(0, 480), (720, 0), (0, 0)] {
        let mut app = connected_app();
        let _ = app.client_mut().connection_mut().drain_outbound();
        let mut window = Window::new(&mut app).expect("window must create");
        let _ = window
            .app_mut()
            .client_mut()
            .connection_mut()
            .drain_outbound();
        let mut pool =
            BufferPool::new(window.app_mut(), width, height).expect("zero dimension is valid");

        assert_eq!(pool.width(), width);
        assert_eq!(pool.height(), height);
        assert_eq!(pool.stride(), width * BYTES_PER_PIXEL as u32);
        assert_eq!(pool.size(), 0);
        assert_eq!(pool.buffer_offset(0), 0);
        assert_eq!(pool.buffer_offset(1), 0);
        assert!(pool.back_buffer().is_empty());
        {
            let canvas = pool.acquire_back_canvas().expect("empty back canvas");
            assert_eq!(canvas.width(), width);
            assert_eq!(canvas.height(), height);
            assert!(canvas.pixels().is_empty());
        }

        let requests = parse_requests(
            &window
                .app_mut()
                .client_mut()
                .connection_mut()
                .drain_outbound(),
        );
        assert_eq!(requests.len(), 3);
        let create_pool = ShmCreatePool::decode(&requests[0].payload).expect("create_pool payload");
        assert_eq!(create_pool.size, 0);
        for request in &requests[1..] {
            let buffer =
                ShmPoolCreateBuffer::decode(&request.payload).expect("create_buffer payload");
            assert_eq!(buffer.offset, 0);
            assert_eq!(buffer.width, width);
            assert_eq!(buffer.height, height);
            assert_eq!(buffer.stride, width * BYTES_PER_PIXEL as u32);
        }

        assert_eq!(
            pool.commit_and_swap(&mut window).expect("empty commit"),
            CommitProgress::Committed
        );
        assert!(!pool.commit_pending());
        assert_eq!(pool.back_index(), 0, "a no-op commit must not swap slots");
        assert!(
            window
                .app_mut()
                .client_mut()
                .connection_mut()
                .drain_outbound()
                .is_empty(),
            "a zero-area commit emits no surface transaction",
        );
    }
}

#[test]
fn buffer_pool_replace_creates_first_then_retires_every_old_object() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();

    let mut slot = None;
    BufferPool::replace(&mut slot, &mut app, WIDTH, HEIGHT).expect("initial pool");
    let old = slot.as_ref().unwrap();
    let old_pool = old.pool_id();
    let old_buffers = [old.buffer_id(0), old.buffer_id(1)];
    let object_count = app.client().object_count();
    let _ = app.client_mut().connection_mut().drain_outbound();

    BufferPool::replace(&mut slot, &mut app, WIDTH + 1, HEIGHT).expect("replacement pool");
    let replacement = slot.as_ref().unwrap();
    assert_ne!(replacement.pool_id(), old_pool);
    assert_eq!(app.client().object_count(), object_count);
    assert_eq!(app.client().get(old_pool), None);
    for buffer in old_buffers {
        assert_eq!(app.client().get(buffer), None);
    }

    let requests = parse_requests(&app.client_mut().connection_mut().drain_outbound());
    assert_eq!(requests.len(), 6);
    assert_eq!(requests[0].opcode, 1 /* create_pool */);
    assert_eq!(requests[1].opcode, 1 /* create_buffer */);
    assert_eq!(requests[2].opcode, 1 /* create_buffer */);
    assert_eq!(requests[3].object_id, old_buffers[0]);
    assert_eq!(requests[3].opcode, 1 /* buffer.destroy */);
    assert_eq!(requests[4].object_id, old_buffers[1]);
    assert_eq!(requests[4].opcode, 1 /* buffer.destroy */);
    assert_eq!(requests[5].object_id, old_pool);
    assert_eq!(requests[5].opcode, 3 /* shm_pool.destroy */);
}

#[test]
fn buffer_pool_replace_dispatches_release_queued_before_destroy_until_delete_id() {
    let mut app = connected_app();
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window");
    let mut slot = None;
    BufferPool::replace(&mut slot, window.app_mut(), WIDTH, HEIGHT).expect("initial pool");

    // Two ordered paints make old buffer 1 current and cause the server to
    // queue release for old buffer 0 before it can process the later destroy.
    for color in [0x11, 0x22] {
        slot.as_mut()
            .unwrap()
            .acquire_back_canvas()
            .expect("paintable back buffer")
            .clear(toolkit::draw::Color::rgb(color, color, color));
        assert_eq!(
            slot.as_mut().unwrap().commit_and_swap(&mut window),
            Ok(CommitProgress::Committed),
        );
    }
    let old_pool = slot.as_ref().unwrap().pool_id();
    let old_buffers = [
        slot.as_ref().unwrap().buffer_id(0),
        slot.as_ref().unwrap().buffer_id(1),
    ];

    BufferPool::replace(&mut slot, window.app_mut(), WIDTH + 1, HEIGHT)
        .expect("create-first replacement");
    for id in [old_buffers[0], old_buffers[1], old_pool] {
        assert!(window.app().client().is_retired(id));
    }
    assert_eq!(
        window
            .app_mut()
            .client_mut()
            .send_request(old_buffers[0], 1, &[]),
        Err(toolkit::ClientError::UnknownObject { id: old_buffers[0] }),
        "retirement rejects new requests immediately",
    );

    // This is the production race: release was already queued in the reverse
    // direction, while delete_id follows destroy after server-side reclamation.
    // App must parse the complete batch before delete_id retires metadata.
    let mut inbound = build_event(old_buffers[0], 1 /* buffer.release */, &[]);
    for id in [old_buffers[0], old_buffers[1], old_pool] {
        let mut payload = Vec::new();
        DisplayDeleteId { id }.encode(&mut payload);
        inbound.extend(build_event(
            ObjectId::DISPLAY,
            2, /* delete_id */
            &payload,
        ));
    }
    window
        .app_mut()
        .client_mut()
        .connection_mut()
        .push_inbound(inbound);

    let events = window
        .dispatch()
        .expect("late release and following delete_id events remain valid");
    assert_eq!(events.len(), 4);
    assert_eq!(events[0].object_id, old_buffers[0]);
    assert_eq!(events[0].interface, toolkit::Interface::Buffer);
    assert_eq!(events[0].opcode_name, "release");
    assert!(events[1..]
        .iter()
        .all(|event| { event.object_id == ObjectId::DISPLAY && event.opcode_name == "delete_id" }));
    for id in [old_buffers[0], old_buffers[1], old_pool] {
        assert!(!window.app().client().is_retired(id));
        assert!(!window.app_mut().client_mut().acknowledge_delete_id(id));
    }

    window
        .app_mut()
        .client_mut()
        .connection_mut()
        .push_inbound(build_event(
            old_buffers[0],
            1, /* buffer.release */
            &[],
        ));
    assert_eq!(
        window.dispatch(),
        Err(toolkit::ClientError::UnknownObject { id: old_buffers[0] }),
        "events after delete_id are no longer valid",
    );
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
        let mut canvas = pool
            .acquire_back_canvas()
            .expect("back buffer should be free");
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
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    let mut pool = BufferPool::new(window.app_mut(), WIDTH, HEIGHT).expect("pool must create");
    let buffer_0 = pool.buffer_id(0);
    let buffer_1 = pool.buffer_id(1);
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    // Paint something so the back buffer is non-empty.
    {
        let mut canvas = pool
            .acquire_back_canvas()
            .expect("back buffer should be free");
        canvas.clear(toolkit::draw::Color::rgb(0xaa, 0xbb, 0xcc));
    }

    pool.commit_and_swap(&mut window)
        .expect("commit must succeed");

    // Back index flipped from 0 → 1, and buffer 0 is now
    // in-use (awaiting buffer.release).
    assert_eq!(pool.back_index(), 1);
    assert!(pool.is_in_use(0));
    assert!(!pool.is_in_use(1));

    let bytes = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let requests = parse_requests(&bytes);
    // commit_and_swap emits N × shm_pool.write (chunked at 24 KiB
    // per syscall) followed by attach + damage + commit. The
    // last 3 requests are the surface ones; the leading ones
    // are the pixel-transfer chunks.
    assert!(requests.len() >= 3);
    let attach_idx = requests.len() - 3;
    let damage_idx = requests.len() - 2;
    let commit_idx = requests.len() - 1;

    // Every leading request must be a shm_pool.write on the pool.
    for r in &requests[..attach_idx] {
        assert_eq!(r.object_id, pool.pool_id());
        assert_eq!(r.opcode, 4 /* write */);
    }

    // surface.attach(buffer_0, 0, 0)
    assert_eq!(requests[attach_idx].object_id, surface_id);
    assert_eq!(requests[attach_idx].opcode, 2 /* attach */);
    let attach =
        SurfaceAttach::decode(&requests[attach_idx].payload).expect("attach payload must decode");
    assert_eq!(attach.buffer_id, buffer_0);
    assert_eq!(attach.x, 0);
    assert_eq!(attach.y, 0);

    // surface.damage(0, 0, w, h)
    assert_eq!(requests[damage_idx].object_id, surface_id);
    assert_eq!(requests[damage_idx].opcode, 3 /* damage */);
    let damage =
        SurfaceDamage::decode(&requests[damage_idx].payload).expect("damage payload must decode");
    assert_eq!(damage.x, 0);
    assert_eq!(damage.y, 0);
    assert_eq!(damage.width, WIDTH as i32);
    assert_eq!(damage.height, HEIGHT as i32);

    // surface.commit()
    assert_eq!(requests[commit_idx].object_id, surface_id);
    assert_eq!(requests[commit_idx].opcode, 7 /* commit */);
    assert!(requests[commit_idx].payload.is_empty());

    // Sanity: buffer 1 is still unattached.
    let _ = buffer_1;
}

#[test]
fn incremental_commit_queues_one_upload_per_turn_and_publishes_last() {
    const LARGE_WIDTH: u32 = 128;
    const LARGE_HEIGHT: u32 = 128;

    let mut conn = LoopbackConnection::incremental();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let mut pool =
        BufferPool::new(window.app_mut(), LARGE_WIDTH, LARGE_HEIGHT).expect("pool must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    pool.acquire_back_canvas()
        .expect("back canvas")
        .clear(toolkit::draw::Color::rgb(1, 2, 3));

    assert_eq!(
        pool.commit_and_swap(&mut window).expect("stage frame"),
        CommitProgress::Pending
    );
    assert!(pool.commit_pending());
    assert_eq!(
        pool.commit_in_place(&mut window),
        Err(toolkit::ClientError::CommitInProgress)
    );
    // A retained outbound request suppresses production of the next chunk.
    // `send` marks this mock pending, and only `drain_outbound` clears it.
    assert_eq!(
        pool.progress_commit(&mut window).expect("held progress"),
        CommitProgress::Pending
    );
    let first = parse_requests(
        &window
            .app_mut()
            .client_mut()
            .connection_mut()
            .drain_outbound(),
    );
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].opcode, 4 /* shm_pool.write */);
    let first_write = ShmPoolWrite::decode(&first[0].payload).expect("write payload");
    assert_eq!(first_write.bytes.len(), toolkit::SHM_WRITE_CHUNK_BYTES);

    let mut turns = 1usize;
    loop {
        let progress = pool.progress_commit(&mut window).expect("advance frame");
        let requests = parse_requests(
            &window
                .app_mut()
                .client_mut()
                .connection_mut()
                .drain_outbound(),
        );
        let upload_count = requests
            .iter()
            .filter(|request| request.object_id == pool.pool_id() && request.opcode == 4)
            .count();
        assert!(upload_count <= 1, "at most one upload request per turn");
        turns += 1;
        if progress == CommitProgress::Committed {
            assert_eq!(requests.len(), upload_count + 3);
            assert_eq!(requests[requests.len() - 3].opcode, 2 /* attach */);
            assert_eq!(requests[requests.len() - 2].opcode, 3 /* damage */);
            assert_eq!(requests[requests.len() - 1].opcode, 7 /* commit */);
            break;
        }
        assert_eq!(requests.len(), upload_count);
    }
    assert_eq!(turns, 3, "64 KiB frame requires exactly three upload turns");
    assert!(!pool.commit_pending());
    assert_eq!(pool.back_index(), 1);
}

#[test]
fn identical_720x480_linear_commit_takes_exactly_eight_progress_calls() {
    const FRAME_WIDTH: u32 = 720;
    const FRAME_HEIGHT: u32 = 480;

    let mut conn = LoopbackConnection::incremental();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let mut pool =
        BufferPool::new(window.app_mut(), FRAME_WIDTH, FRAME_HEIGHT).expect("pool must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    // Seed a real nonzero shadow in slot 0, then publish the exact same pixels
    // from that slot again. This keeps the counted commit production-shaped
    // without relying on zero-initialized client and server storage matching.
    pool.acquire_back_canvas()
        .expect("back canvas")
        .clear(toolkit::draw::Color::rgb(0x11, 0x22, 0x33));
    let mut seed_progress = pool.commit_in_place(&mut window).expect("stage seed frame");
    loop {
        let _ = window
            .app_mut()
            .client_mut()
            .connection_mut()
            .drain_outbound();
        if seed_progress == CommitProgress::Committed {
            break;
        }
        seed_progress = pool
            .progress_commit(&mut window)
            .expect("advance seed frame");
    }
    pool.acquire_back_canvas()
        .expect("back canvas")
        .clear(toolkit::draw::Color::rgb(0x11, 0x22, 0x33));

    // The 1,382,400-byte frame spans 57 chunks, so an eight-chunk scan quantum
    // must finish in ceil(57 / 8) == 8 bounded calls, including this staging
    // call itself.
    let mut calls = 1usize;
    let mut progress = pool
        .commit_in_place(&mut window)
        .expect("stage identical frame");
    loop {
        let requests = parse_requests(
            &window
                .app_mut()
                .client_mut()
                .connection_mut()
                .drain_outbound(),
        );
        assert!(
            requests
                .iter()
                .all(|request| request.object_id != pool.pool_id() || request.opcode != 4),
            "an identical frame must not upload any chunk",
        );
        if progress == CommitProgress::Committed {
            assert_eq!(requests.len(), 3);
            assert_eq!(requests[0].opcode, 2 /* attach */);
            assert_eq!(requests[1].opcode, 3 /* damage */);
            assert_eq!(requests[2].opcode, 7 /* commit */);
            break;
        }
        assert!(requests.is_empty(), "scan-only turns emit no requests");
        calls += 1;
        progress = pool
            .progress_commit(&mut window)
            .expect("advance identical frame");
    }

    assert_eq!(calls, 8);
    assert!(!pool.commit_pending());
    assert_eq!(pool.back_index(), 0);
}

#[test]
fn linear_progress_stops_after_first_changed_chunk() {
    const FRAME_WIDTH: u32 = 256;
    const FRAME_HEIGHT: u32 = 256;
    const FIRST_CHANGED_CHUNK: usize = 3;
    const SECOND_CHANGED_CHUNK: usize = 4;

    let mut conn = LoopbackConnection::incremental();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let mut pool =
        BufferPool::new(window.app_mut(), FRAME_WIDTH, FRAME_HEIGHT).expect("pool must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    {
        let mut canvas = pool.acquire_back_canvas().expect("back canvas");
        for chunk in [FIRST_CHANGED_CHUNK, SECOND_CHANGED_CHUNK] {
            let offset = chunk * toolkit::SHM_WRITE_CHUNK_BYTES;
            canvas.pixels_mut()[offset..offset + BYTES_PER_PIXEL].fill(0x7f);
        }
    }

    assert_eq!(
        pool.commit_and_swap(&mut window).expect("stage frame"),
        CommitProgress::Pending
    );
    let first = parse_requests(
        &window
            .app_mut()
            .client_mut()
            .connection_mut()
            .drain_outbound(),
    );
    assert_eq!(first.len(), 1, "the first changed chunk ends this call");
    assert_eq!(first[0].object_id, pool.pool_id());
    assert_eq!(first[0].opcode, 4 /* shm_pool.write */);
    let first_write = ShmPoolWrite::decode(&first[0].payload).expect("first write payload");
    assert_eq!(
        first_write.offset,
        (FIRST_CHANGED_CHUNK * toolkit::SHM_WRITE_CHUNK_BYTES) as u32
    );
    assert_eq!(first_write.bytes.len(), toolkit::SHM_WRITE_CHUNK_BYTES);

    assert_eq!(
        pool.progress_commit(&mut window).expect("advance frame"),
        CommitProgress::Pending
    );
    let second = parse_requests(
        &window
            .app_mut()
            .client_mut()
            .connection_mut()
            .drain_outbound(),
    );
    assert_eq!(
        second.len(),
        1,
        "the adjacent changed chunk must remain for the next call"
    );
    assert_eq!(second[0].object_id, pool.pool_id());
    assert_eq!(second[0].opcode, 4 /* shm_pool.write */);
    let second_write = ShmPoolWrite::decode(&second[0].payload).expect("second write payload");
    assert_eq!(
        second_write.offset,
        (SECOND_CHANGED_CHUNK * toolkit::SHM_WRITE_CHUNK_BYTES) as u32
    );
    assert_eq!(second_write.bytes.len(), toolkit::SHM_WRITE_CHUNK_BYTES);
}

#[test]
fn linear_progress_does_not_scan_while_outbound_is_pending() {
    const FRAME_WIDTH: u32 = 128;
    const FRAME_HEIGHT: u32 = 128;

    let mut conn = LoopbackConnection::incremental();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let mut pool =
        BufferPool::new(window.app_mut(), FRAME_WIDTH, FRAME_HEIGHT).expect("pool must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    {
        let mut canvas = pool.acquire_back_canvas().expect("back canvas");
        for offset in [0, toolkit::SHM_WRITE_CHUNK_BYTES] {
            canvas.pixels_mut()[offset..offset + BYTES_PER_PIXEL].fill(0x55);
        }
    }

    assert_eq!(
        pool.commit_and_swap(&mut window).expect("stage frame"),
        CommitProgress::Pending
    );
    let retained = {
        let conn = window.app_mut().client_mut().connection_mut();
        assert!(conn.outbound_pending());
        conn.outbound.clone()
    };
    for _ in 0..3 {
        assert_eq!(
            pool.progress_commit(&mut window)
                .expect("retained output blocks progress"),
            CommitProgress::Pending
        );
        let conn = window.app_mut().client_mut().connection_mut();
        assert_eq!(
            conn.outbound, retained,
            "the cursor and queue must not advance"
        );
    }

    let first = parse_requests(
        &window
            .app_mut()
            .client_mut()
            .connection_mut()
            .drain_outbound(),
    );
    assert_eq!(first.len(), 1);
    let first_write = ShmPoolWrite::decode(&first[0].payload).expect("first write payload");
    assert_eq!(first_write.offset, 0);

    assert_eq!(
        pool.progress_commit(&mut window)
            .expect("advance after drain"),
        CommitProgress::Pending
    );
    let second = parse_requests(
        &window
            .app_mut()
            .client_mut()
            .connection_mut()
            .drain_outbound(),
    );
    assert_eq!(second.len(), 1);
    let second_write = ShmPoolWrite::decode(&second[0].payload).expect("second write payload");
    assert_eq!(
        second_write.offset,
        toolkit::SHM_WRITE_CHUNK_BYTES as u32,
        "the blocked call must not skip the next changed chunk"
    );
}

#[test]
fn linear_progress_retries_the_same_changed_chunk_after_send_error() {
    const FRAME_WIDTH: u32 = 256;
    const FRAME_HEIGHT: u32 = 256;
    const CHANGED_CHUNK: usize = 3;

    let mut conn = LoopbackConnection::incremental();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let mut pool =
        BufferPool::new(window.app_mut(), FRAME_WIDTH, FRAME_HEIGHT).expect("pool must create");
    let pool_id = pool.pool_id();
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    {
        let mut canvas = pool.acquire_back_canvas().expect("back canvas");
        let offset = CHANGED_CHUNK * toolkit::SHM_WRITE_CHUNK_BYTES;
        canvas.pixels_mut()[offset..offset + BYTES_PER_PIXEL].fill(0x33);
    }

    assert!(window.app_mut().client_mut().drop_object(pool_id));
    let expected = toolkit::ClientError::UnknownObject { id: pool_id };
    assert_eq!(pool.commit_and_swap(&mut window), Err(expected));
    assert!(pool.commit_pending());
    assert!(
        window
            .app_mut()
            .client_mut()
            .connection_mut()
            .drain_outbound()
            .is_empty(),
        "the rejected write must not queue bytes",
    );

    assert_eq!(
        pool.progress_commit(&mut window),
        Err(expected),
        "retry must attempt the same changed chunk instead of advancing past it",
    );
    assert!(pool.commit_pending());
    assert!(window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound()
        .is_empty(),);
}

#[test]
fn linear_scan_honors_the_eighth_and_ninth_chunk_boundary() {
    const FRAME_WIDTH: u32 = 256;
    const FRAME_HEIGHT: u32 = 256;

    let run = |changed_chunk: usize| {
        let mut conn = LoopbackConnection::incremental();
        seed_registry(&mut conn);
        let mut app = App::connect(conn).expect("bootstrap must succeed");
        let _ = app.client_mut().connection_mut().drain_outbound();
        let mut window = Window::new(&mut app).expect("window must create");
        let _ = window
            .app_mut()
            .client_mut()
            .connection_mut()
            .drain_outbound();
        let mut pool =
            BufferPool::new(window.app_mut(), FRAME_WIDTH, FRAME_HEIGHT).expect("pool must create");
        let _ = window
            .app_mut()
            .client_mut()
            .connection_mut()
            .drain_outbound();
        {
            let mut canvas = pool.acquire_back_canvas().expect("back canvas");
            let offset = changed_chunk * toolkit::SHM_WRITE_CHUNK_BYTES;
            canvas.pixels_mut()[offset..offset + BYTES_PER_PIXEL].fill(0x66);
        }

        let first_progress = pool.commit_and_swap(&mut window).expect("stage frame");
        let first = parse_requests(
            &window
                .app_mut()
                .client_mut()
                .connection_mut()
                .drain_outbound(),
        );
        let second_progress = pool.progress_commit(&mut window).expect("advance frame");
        let second = parse_requests(
            &window
                .app_mut()
                .client_mut()
                .connection_mut()
                .drain_outbound(),
        );
        (first_progress, first, second_progress, second)
    };

    let (first_progress, first, _, _) = run(7);
    assert_eq!(first_progress, CommitProgress::Pending);
    assert_eq!(
        first.len(),
        1,
        "the eighth chunk is inside the scan quantum"
    );
    let eighth = ShmPoolWrite::decode(&first[0].payload).expect("eighth-chunk write");
    assert_eq!(eighth.offset, (7 * toolkit::SHM_WRITE_CHUNK_BYTES) as u32);

    let (first_progress, first, second_progress, second) = run(8);
    assert_eq!(first_progress, CommitProgress::Pending);
    assert!(
        first.is_empty(),
        "eight unchanged chunks exhaust the first scan quantum",
    );
    assert_eq!(second_progress, CommitProgress::Pending);
    assert_eq!(second.len(), 1, "the ninth chunk is handled next");
    let ninth = ShmPoolWrite::decode(&second[0].payload).expect("ninth-chunk write");
    assert_eq!(ninth.offset, (8 * toolkit::SHM_WRITE_CHUNK_BYTES) as u32);
}

#[test]
fn incremental_loop_wait_preserves_partial_write_for_local_commit_work() {
    const LARGE_WIDTH: u32 = 128;
    const LARGE_HEIGHT: u32 = 128;

    let mut conn = LoopbackConnection::incremental();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must explicitly flush its requests");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let mut pool =
        BufferPool::new(window.app_mut(), LARGE_WIDTH, LARGE_HEIGHT).expect("pool must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    pool.acquire_back_canvas()
        .expect("back canvas")
        .clear(toolkit::draw::Color::rgb(1, 2, 3));
    assert_eq!(
        pool.commit_and_swap(&mut window).expect("stage frame"),
        CommitProgress::Pending
    );

    {
        let conn = window.app_mut().client_mut().connection_mut();
        let len = conn.outbound.len();
        assert!(len > 1, "the first upload must admit a retained suffix");
        conn.flush_quanta = VecDeque::from([len - 1, 1]);
        conn.flush_calls = 0;
        conn.write_waits = 0;
        conn.read_waits = 0;
        conn.recv_calls = 0;
    }

    // One production-shaped turn: dispatch does not hide a write, the
    // explicit flush leaves a suffix, and wait observes FD_WRITE without
    // draining that suffix or switching to an FD_READ park.
    let _ = window.dispatch().expect("nonblocking dispatch");
    assert_eq!(
        window.app_mut().client_mut().connection_mut().flush_calls,
        0,
        "recv must not advance the outbound queue",
    );
    window.flush_outbound().expect("first bounded flush");
    {
        let conn = window.app_mut().client_mut().connection_mut();
        assert_eq!(conn.flush_calls, 1);
        assert_eq!(conn.outbound.len(), 1);
        assert!(conn.outbound_pending());
    }
    window.wait(None).expect("write-side park");
    {
        let conn = window.app_mut().client_mut().connection_mut();
        assert_eq!(conn.flush_calls, 1, "wait must not hide a second write");
        assert_eq!(conn.outbound.len(), 1, "wait must preserve the suffix");
        assert_eq!(conn.write_waits, 1);
        assert_eq!(conn.read_waits, 0);
    }

    // On the next turn the pending suffix suppresses a second upload. The
    // sole explicit flush drains it, after which the outer loop observes
    // local BufferPool work and continues immediately instead of parking.
    let _ = window.dispatch().expect("next nonblocking dispatch");
    assert_eq!(
        pool.progress_commit(&mut window)
            .expect("suffix blocks local chunk"),
        CommitProgress::Pending
    );
    window.flush_outbound().expect("second bounded flush");
    assert!(pool.commit_pending());
    assert!(!window.outbound_pending());
    {
        let conn = window.app_mut().client_mut().connection_mut();
        assert_eq!(conn.flush_calls, 2);
        assert_eq!(conn.write_waits, 1);
        assert_eq!(conn.read_waits, 0);
        assert_eq!(conn.recv_calls, 2);
    }

    // The immediate continuation stages the next local 24 KiB chunk. No wait
    // was needed between draining the suffix and making that local progress.
    assert_eq!(
        pool.progress_commit(&mut window)
            .expect("stage next local chunk"),
        CommitProgress::Pending
    );
    let conn = window.app_mut().client_mut().connection_mut();
    assert!(conn.outbound_pending());
    assert_eq!(conn.write_waits, 1);
    assert_eq!(conn.read_waits, 0);
}

#[test]
fn buffer_pool_commit_in_place_reuses_uploaded_slot() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();

    let mut window = Window::new(&mut app).expect("window must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let mut pool = BufferPool::new(window.app_mut(), WIDTH, HEIGHT).expect("pool must create");
    let buffer_0 = pool.buffer_id(0);
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    {
        let mut canvas = pool.acquire_back_canvas().expect("back canvas");
        canvas.clear(toolkit::draw::Color::rgb(0x11, 0x22, 0x33));
    }
    pool.commit_in_place(&mut window).expect("first frame");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    assert_eq!(pool.back_index(), 0);

    // Painting the identical frame into the retained slot requires no cold
    // shm upload: only attach, damage, and commit remain.
    {
        let mut canvas = pool.acquire_back_canvas().expect("back canvas");
        canvas.clear(toolkit::draw::Color::rgb(0x11, 0x22, 0x33));
    }
    pool.commit_in_place(&mut window).expect("second frame");
    let bytes = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let requests = parse_requests(&bytes);
    assert_eq!(requests.len(), 3);
    let attach = SurfaceAttach::decode(&requests[0].payload).expect("attach payload");
    assert_eq!(attach.buffer_id, buffer_0);
    assert_eq!(requests[1].opcode, 3 /* damage */);
    assert_eq!(requests[2].opcode, 7 /* commit */);
    assert_eq!(pool.back_index(), 0);
}

#[test]
fn buffer_pool_in_place_damage_clips_and_packs_changed_rows() {
    const DAMAGE_WIDTH: u32 = 64;
    const DAMAGE_HEIGHT: u32 = 200;

    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window must create");
    let surface_id = window.surface();
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let mut pool =
        BufferPool::new(window.app_mut(), DAMAGE_WIDTH, DAMAGE_HEIGHT).expect("pool must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    // Establish an uploaded baseline, then change one row. A damage-aware
    // commit transports that row densely instead of the surrounding stride.
    {
        let mut canvas = pool.acquire_back_canvas().expect("back canvas");
        canvas.clear(toolkit::draw::Color::rgb(1, 2, 3));
    }
    pool.commit_in_place(&mut window).expect("baseline frame");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    {
        let mut canvas = pool.acquire_back_canvas().expect("back canvas");
        canvas.fill_rect(
            Rect::new(0, 100, DAMAGE_WIDTH, 1),
            toolkit::draw::Color::rgb(4, 5, 6),
        );
    }
    pool.commit_in_place_damage(&mut window, Rect::new(-5, 100, DAMAGE_WIDTH + 10, 1))
        .expect("partial frame");

    let bytes = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let requests = parse_requests(&bytes);
    assert_eq!(requests.len(), 4, "one write plus attach, damage, commit");
    assert_eq!(requests[0].opcode, 5 /* write_rows */);
    let write = ShmPoolWriteRows::decode(&requests[0].payload).expect("write_rows payload");
    assert_eq!(write.offset, 100 * DAMAGE_WIDTH * BYTES_PER_PIXEL as u32);
    assert_eq!(write.row_bytes, DAMAGE_WIDTH * BYTES_PER_PIXEL as u32);
    assert_eq!(write.rows, 1);
    assert_eq!(write.stride, DAMAGE_WIDTH * BYTES_PER_PIXEL as u32);
    assert_eq!(write.bytes.len(), write.row_bytes as usize);
    assert_eq!(requests[1].object_id, surface_id);
    assert_eq!(requests[1].opcode, 2 /* attach */);
    let damage = SurfaceDamage::decode(&requests[2].payload).expect("damage payload");
    assert_eq!((damage.x, damage.y), (0, 100));
    assert_eq!((damage.width, damage.height), (DAMAGE_WIDTH as i32, 1));
    assert_eq!(requests[3].opcode, 7 /* commit */);
    assert_eq!(pool.back_index(), 0);
}

#[test]
fn current_patch_is_one_surface_request_and_keeps_local_shadows_coherent() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window must create");
    let surface_id = window.surface();
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let mut pool = BufferPool::new(window.app_mut(), WIDTH, HEIGHT).expect("pool must create");
    let buffer_0 = pool.buffer_id(0);
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    {
        let mut canvas = pool.acquire_back_canvas().expect("initial back canvas");
        canvas.clear(toolkit::draw::Color::rgb(1, 2, 3));
    }
    pool.commit_and_swap(&mut window).expect("initial frame");
    assert_eq!(pool.current_index(), Some(0));
    assert_eq!(pool.back_index(), 1);
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    let damage = Rect::new(1, 0, 2, 1);
    let packed = vec![0x5a; 2 * BYTES_PER_PIXEL];
    assert_eq!(
        pool.patch_current(&mut window, damage, &packed)
            .expect("atomic current patch"),
        CurrentPatch::Patched { buffer_index: 0 },
    );
    let requests = parse_requests(
        &window
            .app_mut()
            .client_mut()
            .connection_mut()
            .drain_outbound(),
    );
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].object_id, surface_id);
    assert_eq!(requests[0].opcode, 8 /* patch_current */);
    let patch = SurfacePatchCurrent::decode(&requests[0].payload).expect("typed patch");
    assert_eq!((patch.x, patch.y, patch.width, patch.height), (1, 0, 2, 1));
    assert_eq!(patch.pixels, packed);
    assert_eq!(pool.current_index(), Some(0));
    assert_eq!(pool.back_index(), 1, "current patch must not swap slots");

    // Move through slot 1 so the patched slot becomes back. A damage commit
    // without another paint must see both local shadows as already current and
    // therefore emit no shm write before attaching buffer 0.
    {
        let mut canvas = pool.acquire_back_canvas().expect("alternate canvas");
        canvas.clear(toolkit::draw::Color::rgb(7, 8, 9));
    }
    pool.commit_and_swap(&mut window).expect("alternate frame");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    assert_eq!(pool.back_index(), 0);
    pool.commit_and_swap_damage(&mut window, damage)
        .expect("unchanged patched slot");
    let requests = parse_requests(
        &window
            .app_mut()
            .client_mut()
            .connection_mut()
            .drain_outbound(),
    );
    assert_eq!(requests.len(), 3, "attach, damage, commit only");
    let attach = SurfaceAttach::decode(&requests[0].payload).expect("attach payload");
    assert_eq!(attach.buffer_id, buffer_0);
    assert_eq!(requests[1].opcode, 3 /* damage */);
    assert_eq!(requests[2].opcode, 7 /* commit */);
}

#[test]
fn current_patch_defers_staged_work_and_rejects_invalid_geometry_pre_send() {
    let mut conn = LoopbackConnection::incremental();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let mut pool = BufferPool::new(window.app_mut(), 128, 128).expect("pool must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    {
        let mut canvas = pool.acquire_back_canvas().expect("initial back canvas");
        canvas.clear(toolkit::draw::Color::rgb(1, 2, 3));
    }
    assert_eq!(
        pool.commit_and_swap(&mut window)
            .expect("stage initial frame"),
        CommitProgress::Pending,
    );
    assert_eq!(
        pool.patch_current(&mut window, Rect::new(0, 0, 1, 1), &[0; 4])
            .expect("deferred patch"),
        CurrentPatch::Deferred { buffer_index: None },
    );

    while pool.commit_pending() {
        let _ = window
            .app_mut()
            .client_mut()
            .connection_mut()
            .drain_outbound();
        let _ = pool
            .progress_commit(&mut window)
            .expect("finish full frame");
    }
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    assert_eq!(pool.current_index(), Some(0));

    for (damage, pixels) in [
        (Rect::new(-1, 0, 1, 1), vec![0; 4]),
        (Rect::new(127, 127, 2, 1), vec![0; 8]),
        (Rect::new(0, 0, 2, 2), vec![0; 15]),
    ] {
        assert!(matches!(
            pool.patch_current(&mut window, damage, &pixels),
            Err(toolkit::ClientError::InvalidSurfacePatch { .. })
        ));
    }
    assert!(window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound()
        .is_empty());
}

#[test]
fn current_patch_rejects_an_unrelated_window_without_mirroring_or_sending() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut first_window = Window::new(&mut app).expect("first window");
    let _ = first_window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let mut pool =
        BufferPool::new(first_window.app_mut(), WIDTH, HEIGHT).expect("pool must create");
    let _ = first_window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    {
        let mut canvas = pool.acquire_back_canvas().expect("initial canvas");
        canvas.clear(toolkit::draw::Color::rgb(1, 2, 3));
    }
    pool.commit_and_swap(&mut first_window)
        .expect("initial frame");
    let _ = first_window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    drop(first_window);

    let mut unrelated_window = Window::new(&mut app).expect("unrelated window");
    let _ = unrelated_window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    assert_eq!(
        pool.patch_current(
            &mut unrelated_window,
            Rect::new(0, 0, 1, 1),
            &[0x5a; BYTES_PER_PIXEL],
        )
        .expect("unrelated surface is a non-mutating miss"),
        CurrentPatch::Unavailable,
    );
    assert!(unrelated_window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound()
        .is_empty());
}

#[test]
fn completed_pool_rejects_a_different_surface_before_staging_or_sending() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut first_window = Window::new(&mut app).expect("first window");
    let expected_surface = first_window.surface();
    let _ = first_window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let mut pool =
        BufferPool::new(first_window.app_mut(), WIDTH, HEIGHT).expect("pool must create");
    let _ = first_window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    {
        let mut canvas = pool.acquire_back_canvas().expect("initial canvas");
        canvas.clear(toolkit::draw::Color::rgb(1, 2, 3));
    }
    pool.commit_and_swap(&mut first_window)
        .expect("initial frame");
    let _ = first_window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    drop(first_window);

    let mut unrelated_window = Window::new(&mut app).expect("unrelated window");
    let actual_surface = unrelated_window.surface();
    let _ = unrelated_window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let before = pool.back_buffer().to_vec();
    assert_eq!(
        pool.commit_and_swap(&mut unrelated_window),
        Err(toolkit::ClientError::BufferPoolSurfaceMismatch {
            expected_surface,
            actual_surface,
        }),
    );
    assert_eq!(pool.current_index(), Some(0));
    assert_eq!(pool.back_index(), 1);
    assert!(!pool.commit_pending());
    assert_eq!(pool.back_buffer(), before);
    assert!(unrelated_window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound()
        .is_empty());
}

#[test]
fn staged_pool_rejects_progress_through_a_different_surface_without_mutation() {
    let mut conn = LoopbackConnection::incremental();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut first_window = Window::new(&mut app).expect("first window");
    let expected_surface = first_window.surface();
    let _ = first_window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let mut pool = BufferPool::new(first_window.app_mut(), 128, 128).expect("pool must create");
    let _ = first_window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    {
        let mut canvas = pool.acquire_back_canvas().expect("initial canvas");
        canvas.clear(toolkit::draw::Color::rgb(1, 2, 3));
    }
    assert_eq!(
        pool.commit_and_swap(&mut first_window)
            .expect("stage initial frame"),
        CommitProgress::Pending,
    );
    let _ = first_window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    drop(first_window);

    let mut unrelated_window = Window::new(&mut app).expect("unrelated window");
    let actual_surface = unrelated_window.surface();
    let _ = unrelated_window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let before = pool.back_buffer().to_vec();
    assert_eq!(
        pool.progress_commit(&mut unrelated_window),
        Err(toolkit::ClientError::BufferPoolSurfaceMismatch {
            expected_surface,
            actual_surface,
        }),
    );
    assert!(pool.commit_pending());
    assert_eq!(pool.current_index(), None);
    assert_eq!(pool.back_index(), 0);
    assert_eq!(pool.back_buffer(), before);
    assert!(unrelated_window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound()
        .is_empty());
}

#[test]
fn buffer_pool_swap_damage_writes_unattached_slot_and_advances_index() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window must create");
    let surface_id = window.surface();
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let mut pool = BufferPool::new(window.app_mut(), WIDTH, HEIGHT).expect("pool must create");
    let buffer_0 = pool.buffer_id(0);
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    // Seed both slots with the same complete frame. Buffer 1 is attached and
    // buffer 0 is the next back slot when the partial repaint begins.
    for _ in 0..2 {
        let mut canvas = pool.acquire_back_canvas().expect("back canvas");
        canvas.clear(toolkit::draw::Color::rgb(1, 2, 3));
        drop(canvas);
        pool.commit_and_swap(&mut window).expect("seed frame");
    }
    assert_eq!(pool.back_index(), 0);
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    {
        let mut canvas = pool.acquire_back_canvas().expect("unattached back canvas");
        canvas.set_pixel(1, 0, toolkit::draw::Color::rgb(4, 5, 6));
    }
    pool.commit_and_swap_damage(&mut window, Rect::new(1, 0, 1, 1))
        .expect("partial swap frame");

    let requests = parse_requests(
        &window
            .app_mut()
            .client_mut()
            .connection_mut()
            .drain_outbound(),
    );
    assert_eq!(
        requests.len(),
        4,
        "one packed write plus surface transaction"
    );
    assert_eq!(requests[0].opcode, 5 /* write_rows */);
    let write = ShmPoolWriteRows::decode(&requests[0].payload).expect("write_rows payload");
    assert_eq!(write.offset, BYTES_PER_PIXEL as u32);
    assert_eq!(write.row_bytes, BYTES_PER_PIXEL as u32);
    assert_eq!(write.rows, 1);
    assert_eq!(requests[1].object_id, surface_id);
    assert_eq!(requests[1].opcode, 2 /* attach */);
    let attach = SurfaceAttach::decode(&requests[1].payload).expect("attach payload");
    assert_eq!(attach.buffer_id, buffer_0);
    assert_eq!(requests[2].opcode, 3 /* damage */);
    assert_eq!(requests[3].opcode, 7 /* commit */);
    assert_eq!(pool.back_index(), 1);
}

#[test]
fn buffer_pool_handle_release_flips_in_use_flag() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();

    let mut window = Window::new(&mut app).expect("window must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let mut pool = BufferPool::new(window.app_mut(), WIDTH, HEIGHT).expect("pool must create");
    let buffer_0 = pool.buffer_id(0);
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    // Paint + commit once — marks buffer_0 as in-use.
    {
        let mut canvas = pool
            .acquire_back_canvas()
            .expect("back buffer should be free");
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
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let mut pool = BufferPool::new(window.app_mut(), WIDTH, HEIGHT).expect("pool must create");
    let buffer_0 = pool.buffer_id(0);
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    // Frame 1: paint red into buffer 0, commit.
    {
        let mut canvas = pool.acquire_back_canvas().expect("buffer 0 should be free");
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
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    // Frame 2: now back_index is 1 and buffer 1 was never
    // attached, so acquire returns Some with a view onto
    // the buffer-1 region.
    {
        let mut canvas = pool.acquire_back_canvas().expect("buffer 1 should be free");
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
    // bytes from the second commit only. The trailing 3
    // requests are attach + damage + commit; preceding ones
    // are shm_pool.write chunks.
    let bytes = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let requests = parse_requests(&bytes);
    assert!(requests.len() >= 3);
    let attach_idx = requests.len() - 3;
    let attach =
        SurfaceAttach::decode(&requests[attach_idx].payload).expect("attach payload must decode");
    assert_eq!(attach.buffer_id, pool.buffer_id(1));

    // silence unused warnings
    let _ = second_start;
}

#[test]
fn buffer_pool_reused_unchanged_buffer_skips_pixel_upload() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let mut pool = BufferPool::new(window.app_mut(), WIDTH, HEIGHT).expect("pool must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    // Publish distinct contents from both slots, then return to
    // buffer 0 without changing its already-uploaded red pixels.
    {
        let mut canvas = pool.acquire_back_canvas().expect("buffer 0");
        canvas.clear(toolkit::draw::Color::rgb(0xff, 0, 0));
    }
    pool.commit_and_swap(&mut window).expect("frame 1");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    {
        let mut canvas = pool.acquire_back_canvas().expect("buffer 1");
        canvas.clear(toolkit::draw::Color::rgb(0, 0, 0xff));
    }
    pool.commit_and_swap(&mut window).expect("frame 2");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    pool.commit_and_swap(&mut window).expect("unchanged frame");
    let bytes = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let requests = parse_requests(&bytes);
    assert_eq!(requests.len(), 3, "only attach, damage, and commit remain");
    assert_eq!(requests[0].opcode, 2 /* attach */);
    assert_eq!(requests[1].opcode, 3 /* damage */);
    assert_eq!(requests[2].opcode, 7 /* commit */);
}

#[test]
fn buffer_pool_reused_changed_buffer_uploads_changed_chunk() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let mut pool = BufferPool::new(window.app_mut(), WIDTH, HEIGHT).expect("pool must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    // Cycle through both slots so buffer 0 has a server shadow,
    // then alter one pixel when it becomes the back buffer again.
    {
        let mut canvas = pool.acquire_back_canvas().expect("buffer 0");
        canvas.clear(toolkit::draw::Color::rgb(0x11, 0x22, 0x33));
    }
    pool.commit_and_swap(&mut window).expect("frame 1");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    pool.commit_and_swap(&mut window).expect("frame 2");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    {
        let mut canvas = pool.acquire_back_canvas().expect("buffer 0 reused");
        canvas.set_pixel(0, 0, toolkit::draw::Color::rgb(0xaa, 0xbb, 0xcc));
    }

    pool.commit_and_swap(&mut window).expect("changed frame");
    let bytes = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let requests = parse_requests(&bytes);
    assert_eq!(
        requests.len(),
        4,
        "one write plus the three surface requests"
    );
    assert_eq!(requests[0].object_id, pool.pool_id());
    assert_eq!(requests[0].opcode, 4 /* write */);
    let write = ShmPoolWrite::decode(&requests[0].payload).expect("write payload");
    assert_eq!(write.offset, 0);
    assert_eq!(write.bytes.len(), PER_BUFFER_BYTES as usize);
}

#[test]
fn multi_region_damage_normalizes_overlap_and_commits_once() {
    const TEST_WIDTH: u32 = 16;
    const TEST_HEIGHT: u32 = 8;

    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window must create");
    let surface_id = window.surface();
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let mut pool =
        BufferPool::new(window.app_mut(), TEST_WIDTH, TEST_HEIGHT).expect("pool must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    let left = Rect::new(0, 0, 8, 4);
    let overlapping = Rect::new(4, 0, 8, 4);
    {
        let mut canvas = pool.acquire_back_canvas().expect("back canvas");
        canvas.fill_rect(left, toolkit::draw::Color::rgb(1, 2, 3));
        canvas.fill_rect(overlapping, toolkit::draw::Color::rgb(4, 5, 6));
    }
    assert_eq!(
        pool.commit_and_swap_damage_regions(&mut window, &[left, overlapping])
            .expect("multi-region commit"),
        CommitProgress::Committed
    );

    let requests = parse_requests(
        &window
            .app_mut()
            .client_mut()
            .connection_mut()
            .drain_outbound(),
    );
    let writes: Vec<_> = requests
        .iter()
        .take_while(|request| request.object_id == pool.pool_id())
        .map(|request| {
            assert_eq!(request.opcode, 5 /* write_rows */);
            ShmPoolWriteRows::decode(&request.payload).expect("write_rows payload")
        })
        .collect();
    assert_eq!(writes.len(), 2);
    assert_eq!(
        writes.iter().map(|write| write.bytes.len()).sum::<usize>(),
        12 * 4 * BYTES_PER_PIXEL,
        "the 4x4 overlap must be uploaded only once",
    );
    assert!(writes
        .iter()
        .all(|write| write.bytes.len() <= toolkit::SHM_WRITE_CHUNK_BYTES));

    let attach_index = writes.len();
    assert_eq!(requests[attach_index].object_id, surface_id);
    assert_eq!(requests[attach_index].opcode, 2 /* attach */);
    let damages: Vec<_> = requests[attach_index + 1..requests.len() - 1]
        .iter()
        .map(|request| {
            assert_eq!(request.object_id, surface_id);
            assert_eq!(request.opcode, 3 /* damage */);
            SurfaceDamage::decode(&request.payload).expect("damage payload")
        })
        .collect();
    assert_eq!(damages.len(), 2);
    assert_eq!(
        damages
            .iter()
            .map(|damage| damage.width as usize * damage.height as usize)
            .sum::<usize>(),
        12 * 4,
    );
    for (index, damage) in damages.iter().enumerate() {
        for other in &damages[index + 1..] {
            let disjoint = damage.x + damage.width <= other.x
                || other.x + other.width <= damage.x
                || damage.y + damage.height <= other.y
                || other.y + other.height <= damage.y;
            assert!(disjoint, "normalized damage rectangles must not overlap");
        }
    }
    assert_eq!(requests.last().unwrap().object_id, surface_id);
    assert_eq!(requests.last().unwrap().opcode, 7 /* commit */);
}

fn incremental_region_upload_turns(damages: &[Rect]) -> (usize, Vec<SurfaceDamage>) {
    const FRAME_WIDTH: u32 = 1024;
    const FRAME_HEIGHT: u32 = 768;

    let mut conn = LoopbackConnection::incremental();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window must create");
    let surface_id = window.surface();
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let mut pool =
        BufferPool::new(window.app_mut(), FRAME_WIDTH, FRAME_HEIGHT).expect("pool must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    {
        let mut canvas = pool.acquire_back_canvas().expect("back canvas");
        for damage in damages {
            canvas.fill_rect(*damage, toolkit::draw::Color::rgb(1, 2, 3));
        }
    }

    let mut progress = pool
        .commit_and_swap_damage_regions(&mut window, damages)
        .expect("stage damage commit");
    let mut turns = 0;
    let mut committed_damages = Vec::new();
    loop {
        turns += 1;
        let requests = parse_requests(
            &window
                .app_mut()
                .client_mut()
                .connection_mut()
                .drain_outbound(),
        );
        let writes: Vec<_> = requests
            .iter()
            .filter(|request| request.object_id == pool.pool_id() && request.opcode == 5)
            .map(|request| ShmPoolWriteRows::decode(&request.payload).expect("write_rows payload"))
            .collect();
        assert_eq!(writes.len(), 1, "each changed tile gets one progress turn");
        assert!(writes[0].bytes.len() <= toolkit::SHM_WRITE_CHUNK_BYTES);

        if progress == CommitProgress::Committed {
            let attach = requests
                .iter()
                .position(|request| request.object_id == surface_id && request.opcode == 2)
                .expect("final turn attaches");
            committed_damages.extend(requests[attach + 1..requests.len() - 1].iter().map(
                |request| {
                    assert_eq!(request.object_id, surface_id);
                    assert_eq!(request.opcode, 3 /* damage */);
                    SurfaceDamage::decode(&request.payload).expect("damage payload")
                },
            ));
            assert_eq!(requests.last().unwrap().opcode, 7 /* commit */);
            break;
        }
        progress = pool.progress_commit(&mut window).expect("advance commit");
    }
    assert!(!pool.commit_pending());
    assert_eq!(pool.back_index(), 1);
    (turns, committed_damages)
}

#[test]
fn production_launcher_regions_take_exactly_six_or_eleven_upload_turns() {
    let popup_and_button = Rect::new(4, 608, 200, 158);
    let (turns, damages) = incremental_region_upload_turns(&[popup_and_button]);
    assert_eq!(turns, 6);
    assert_eq!(damages.len(), 1);
    assert_eq!(
        (
            damages[0].x,
            damages[0].y,
            damages[0].width,
            damages[0].height
        ),
        (4, 608, 200, 158),
    );

    let taskbar = Rect::new(0, 736, 1024, 32);
    let popup_above_taskbar = Rect::new(4, 608, 200, 128);
    let (turns, damages) = incremental_region_upload_turns(&[taskbar, popup_above_taskbar]);
    assert_eq!(turns, 11, "taskbar-first ordering is six plus five turns");
    assert_eq!(damages.len(), 2);
    assert_eq!(
        (
            damages[0].x,
            damages[0].y,
            damages[0].width,
            damages[0].height
        ),
        (0, 736, 1024, 32),
    );
    assert_eq!(
        (
            damages[1].x,
            damages[1].y,
            damages[1].width,
            damages[1].height
        ),
        (4, 608, 200, 128),
    );
}

#[test]
fn empty_clipped_or_excessive_damage_never_swaps() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let mut pool = BufferPool::new(window.app_mut(), WIDTH, HEIGHT).expect("pool must create");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    for damages in [&[][..], &[Rect::new(50, 50, 2, 2)][..]] {
        assert_eq!(
            pool.commit_and_swap_damage_regions(&mut window, damages)
                .expect("empty damage is a no-op"),
            CommitProgress::Committed,
        );
        assert_eq!(pool.back_index(), 0);
        assert!(window
            .app_mut()
            .client_mut()
            .connection_mut()
            .drain_outbound()
            .is_empty());
    }

    let excessive = vec![Rect::new(0, 0, 1, 1); toolkit::MAX_DAMAGE_REGIONS + 1];
    assert_eq!(
        pool.commit_and_swap_damage_regions(&mut window, &excessive),
        Err(toolkit::ClientError::TooManyDamageRegions {
            supplied: toolkit::MAX_DAMAGE_REGIONS + 1,
            max: toolkit::MAX_DAMAGE_REGIONS,
        }),
    );
    assert_eq!(pool.back_index(), 0);
    assert!(!pool.commit_pending());
}

#[test]
fn frame_request_tracks_typed_done_then_delete_as_one_shot() {
    let mut app = connected_app();
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let mut pool = BufferPool::new(window.app_mut(), WIDTH, HEIGHT).expect("pool");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    let callback = pool.request_frame(&mut window).unwrap();
    assert_eq!(pool.pending_frames(), 1);
    assert_eq!(
        window.app_mut().client().get(callback),
        Some(Interface::Callback)
    );
    let requests = parse_requests(
        &window
            .app_mut()
            .client_mut()
            .connection_mut()
            .drain_outbound(),
    );
    assert_eq!(requests.len(), 1);
    assert_eq!(
        (requests[0].object_id, requests[0].opcode),
        (window.surface(), 4)
    );
    assert_eq!(
        SurfaceFrame::decode(&requests[0].payload).unwrap().new_id,
        callback
    );

    window
        .app_mut()
        .client_mut()
        .connection_mut()
        .push_inbound(build_event(callback, 1, &88u32.to_le_bytes()));
    let events = window.app_mut().dispatch().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(
        CallbackDone::decode(&events[0].payload)
            .unwrap()
            .callback_data,
        88
    );
    assert!(pool.handle_frame_done(events[0].object_id));
    assert!(!pool.handle_frame_done(callback));
    assert_eq!(pool.pending_frames(), 0);
    assert!(window.app_mut().client().is_retired(callback));

    let mut delete_payload = Vec::new();
    DisplayDeleteId { id: callback }.encode(&mut delete_payload);
    window
        .app_mut()
        .client_mut()
        .connection_mut()
        .push_inbound(build_event(ObjectId::DISPLAY, 2, &delete_payload));
    let events = window.app_mut().dispatch().unwrap();
    let deleted = DisplayDeleteId::decode(&events[0].payload).unwrap();
    assert!(!pool.handle_frame_deleted(deleted.id));
    assert!(!window.app_mut().client().is_retired(callback));
    assert_eq!(window.app_mut().client().get(callback), None);
}

#[test]
fn frame_delete_without_done_cancels_surviving_pool_tracking() {
    let mut app = connected_app();
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window");
    let mut pool = BufferPool::new(window.app_mut(), WIDTH, HEIGHT).expect("pool");
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let callback = pool.request_frame(&mut window).unwrap();
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    let mut payload = Vec::new();
    DisplayDeleteId { id: callback }.encode(&mut payload);
    window
        .app_mut()
        .client_mut()
        .connection_mut()
        .push_inbound(build_event(ObjectId::DISPLAY, 2, &payload));
    let events = window.app_mut().dispatch().unwrap();
    let deleted = DisplayDeleteId::decode(&events[0].payload).unwrap();
    assert!(pool.handle_frame_deleted(deleted.id));
    assert!(!pool.handle_frame_done(callback));
    assert_eq!(pool.pending_frames(), 0);
    assert_eq!(window.app_mut().client().get(callback), None);
}

#[test]
fn failed_frame_request_never_enters_the_pool_pending_list() {
    let mut app = connected_app();
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window");
    let mut pool = BufferPool::new(window.app_mut(), WIDTH, HEIGHT).expect("pool");
    let surface = window.surface();
    window
        .app_mut()
        .client_mut()
        .surface_destroy(surface)
        .unwrap();

    assert_eq!(
        pool.request_frame(&mut window),
        Err(toolkit::ClientError::UnknownObject { id: surface })
    );
    assert_eq!(pool.pending_frames(), 0);
}
