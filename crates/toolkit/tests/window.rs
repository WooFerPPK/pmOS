//! [`toolkit::Window`] isolation tests.
//!
//! Pairs the application-level [`toolkit::App`] facade
//! with a bidirectional mock connection that pre-seeds
//! `registry.global` advertisements (so `App::connect`
//! completes its bind handshake) and then exercises the
//! Window facade through its public API: create, set
//! title, dispatch configure, dispatch close. The mock
//! server pattern is lifted from `tests/app.rs` — every
//! inbound batch is pushed through
//! `LoopbackConnection::push_inbound` and observed
//! outbound requests are read back from
//! `LoopbackConnection::drain_outbound`.
//!
//! The configure / ack_configure / close event coverage
//! pins the contract defined in the v1 collapsed
//! `pmd_xdg_toplevel` event table (see
//! `display-proto/src/objects.rs`) — the same table that
//! will carry the spec's `pmd_xdg_surface.configure` +
//! `pmd_xdg_toplevel.close` once the xdg_surface
//! collapse is undone in a future slice.
//!
//! The parse helpers at the bottom decode message headers
//! and payloads from `drain_outbound` so each test can
//! assert on the exact request sequence the Window sent.

use std::collections::VecDeque;

use display_proto::events::{RegistryGlobal, XdgToplevelClose, XdgToplevelConfigure};
use display_proto::requests::{
    CompositorCreateSurface, XdgShellGetToplevel, XdgToplevelAckConfigure, XdgToplevelSetTitle,
};

use toolkit::protocol::Connection;
use toolkit::{App, Interface, MessageHeader, ObjectId, Window, HEADER_SIZE};

/// Bidirectional in-memory [`Connection`] for Window
/// tests. Same shape as the `LoopbackConnection` used by
/// `tests/app.rs` — outbound buffering plus a queued
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
        if let Some(done) = sync_done_for(bytes) {
            self.inbound.push_back(done);
        }
    }

    fn drain_outbound(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.outbound)
    }

    fn recv(&mut self) -> Vec<u8> {
        self.inbound.pop_front().unwrap_or_default()
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

/// The toolkit's id allocator hands the registry id out
/// first: `get_registry` always lands at raw id 3.
const REGISTRY_ID: ObjectId = ObjectId::new(3);

/// Build a single framed `pmd_registry.global(name,
/// interface, version)` event targeting `registry_id`.
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

/// Build a single framed `pmd_xdg_toplevel.configure` event
/// targeting `toplevel_id` with `(serial, width, height)`.
/// The `states` bitfield is fixed at zero — see
/// [`build_configure_event_with_states`] for the
/// state-bearing variant used by maximize/restore tests.
fn build_configure_event(toplevel_id: ObjectId, serial: u32, width: i32, height: i32) -> Vec<u8> {
    build_configure_event_with_states(toplevel_id, serial, width, height, 0)
}

/// Build a `configure` event payload carrying a non-zero
/// states bitfield. Used by maximize/restore tests to
/// inject the `MAXIMIZED` / `ACTIVATED` / etc. state bits
/// the toolkit decodes from the configure event payload.
fn build_configure_event_with_states(
    toplevel_id: ObjectId,
    serial: u32,
    width: i32,
    height: i32,
    states: u32,
) -> Vec<u8> {
    let event = XdgToplevelConfigure {
        serial,
        width,
        height,
        states,
    };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    let header = MessageHeader::try_new(toplevel_id, 1 /* configure */, payload.len(), 0).unwrap();
    header.encode(&mut out[..HEADER_SIZE]).unwrap();
    out[HEADER_SIZE..].copy_from_slice(&payload);
    out
}

/// Build a single framed `pmd_xdg_toplevel.close` event
/// targeting `toplevel_id` (empty payload).
fn build_close_event(toplevel_id: ObjectId) -> Vec<u8> {
    let event = XdgToplevelClose;
    let mut payload = Vec::new();
    event.encode(&mut payload);
    let mut out = vec![0u8; HEADER_SIZE];
    let header = MessageHeader::try_new(toplevel_id, 2 /* close */, payload.len(), 0).unwrap();
    header.encode(&mut out[..HEADER_SIZE]).unwrap();
    // close has no payload bytes to append.
    let _ = payload;
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

/// Pre-seed the three required globals on `conn` so the
/// very next `App::connect` succeeds.
fn seed_registry(conn: &mut LoopbackConnection) {
    let mut batch = Vec::new();
    batch.extend(build_global_event(REGISTRY_ID, 1, "pmd_compositor", 1));
    batch.extend(build_global_event(REGISTRY_ID, 2, "pmd_shm", 1));
    batch.extend(build_global_event(REGISTRY_ID, 3, "pmd_xdg_shell", 1));
    conn.push_inbound(batch);
}

#[test]
fn window_new_sends_create_surface_sequence() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");

    // Drop the bind-phase outbound bytes — the App tests
    // already cover that sequence; we only care about what
    // Window::new puts on the wire.
    let _ = app.client_mut().connection_mut().drain_outbound();

    let mut window = Window::new(&mut app).expect("window creation must succeed");
    let surface_id = window.surface();
    let xdg_toplevel_id = window.xdg_toplevel();
    // xdg_surface aliases the surface in the v1 collapsed
    // protocol (see window.rs module doc).
    assert_eq!(window.xdg_surface(), surface_id);
    assert!(!window.is_configured());
    assert_eq!(window.configured_size(), (0, 0));
    assert!(!window.close_requested());

    // Drain outbound and assert on the request sequence.
    // In the v1 collapsed protocol the sequence is:
    //   compositor.create_surface(new_id=surface)
    //   xdg_shell.get_toplevel(new_id=xdg_toplevel,
    //                          surface_id)
    // (spec §10/§11 would also have a `get_xdg_surface`
    // step between the two; see the window.rs module
    // doc for the collapse rationale.)
    let compositor_id = window.app().compositor();
    let xdg_shell_id = window.app().xdg_shell();
    let bytes = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let requests = parse_requests(&bytes);
    assert_eq!(requests.len(), 2);

    // 1. compositor.create_surface
    assert_eq!(requests[0].object_id, compositor_id);
    assert_eq!(requests[0].opcode, 1 /* create_surface */);
    let create_surface = CompositorCreateSurface::decode(&requests[0].payload)
        .expect("create_surface payload must decode");
    assert_eq!(create_surface.new_id, surface_id);

    // 2. xdg_shell.get_toplevel
    assert_eq!(requests[1].object_id, xdg_shell_id);
    assert_eq!(requests[1].opcode, 1 /* get_toplevel */);
    let get_toplevel = XdgShellGetToplevel::decode(&requests[1].payload)
        .expect("get_toplevel payload must decode");
    assert_eq!(get_toplevel.new_id, xdg_toplevel_id);
    assert_eq!(get_toplevel.surface_id, surface_id);
}

#[test]
fn window_set_title_sends_set_title_request() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");

    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window creation must succeed");
    let toplevel_id = window.xdg_toplevel();
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    window.set_title("Hello").expect("set_title must succeed");
    assert_eq!(window.title(), "Hello");

    let bytes = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let requests = parse_requests(&bytes);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].object_id, toplevel_id);
    assert_eq!(requests[0].opcode, 1 /* set_title */);
    let set_title =
        XdgToplevelSetTitle::decode(&requests[0].payload).expect("set_title payload must decode");
    assert_eq!(set_title.title, "Hello");
}

#[test]
fn window_dispatch_handles_configure() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");

    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window creation must succeed");
    let toplevel_id = window.xdg_toplevel();
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    // Mock the server: a single inbound batch that carries
    // the merged configure event (serial=1, w=800, h=600).
    window
        .app_mut()
        .client_mut()
        .connection_mut()
        .push_inbound(build_configure_event(toplevel_id, 1, 800, 600));

    let passthrough = window.dispatch().expect("dispatch must succeed");
    assert!(passthrough.is_empty(), "configure must not pass through");
    assert!(window.is_configured());
    assert_eq!(window.configured_size(), (800, 600));
    assert!(!window.close_requested());

    // Window must have sent an ack_configure(1) in reply.
    let bytes = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let requests = parse_requests(&bytes);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].object_id, toplevel_id);
    assert_eq!(requests[0].opcode, 4 /* ack_configure */);
    let ack = XdgToplevelAckConfigure::decode(&requests[0].payload)
        .expect("ack_configure payload must decode");
    assert_eq!(ack.serial, 1);
}

#[test]
fn window_dispatch_records_close() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");

    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window creation must succeed");
    let toplevel_id = window.xdg_toplevel();
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    // Mock the server: send xdg_toplevel::close.
    window
        .app_mut()
        .client_mut()
        .connection_mut()
        .push_inbound(build_close_event(toplevel_id));

    let passthrough = window.dispatch().expect("dispatch must succeed");
    assert!(passthrough.is_empty(), "close must not pass through");
    assert!(window.close_requested());
    assert!(!window.is_configured());
    assert_eq!(window.configured_size(), (0, 0));

    // close does not induce any outbound reply.
    let bytes = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    assert!(bytes.is_empty());

    // Interface binding remains intact after the close
    // event — the client hasn't destroyed the toplevel
    // yet.
    let client = window.app().client();
    assert_eq!(client.get(toplevel_id), Some(Interface::XdgToplevel));
}

#[test]
fn take_close_requested_consumes_only_the_pending_event() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window creation must succeed");
    let toplevel_id = window.xdg_toplevel();
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    window
        .app_mut()
        .client_mut()
        .connection_mut()
        .push_inbound(build_close_event(toplevel_id));

    window.dispatch().expect("dispatch must succeed");
    assert!(window.take_close_requested());
    assert!(!window.take_close_requested());
    assert!(!window.close_requested());
}

#[test]
fn window_dispatch_decodes_states_bitfield() {
    use display_proto::xdg_toplevel_state;
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window creation must succeed");
    let toplevel_id = window.xdg_toplevel();
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    let mixed = xdg_toplevel_state::MAXIMIZED | xdg_toplevel_state::ACTIVATED;
    window
        .app_mut()
        .client_mut()
        .connection_mut()
        .push_inbound(build_configure_event_with_states(
            toplevel_id,
            7,
            1024,
            768,
            mixed,
        ));

    let _ = window.dispatch().expect("dispatch must succeed");
    assert!(window.is_configured());
    assert_eq!(window.configured_size(), (1024, 768));
    assert_eq!(window.states(), mixed);
    assert!(window.is_maximized());
    assert!(window.is_activated());
    assert!(!window.is_fullscreen());
}

#[test]
fn window_states_default_to_zero_until_configure_lands() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let window = Window::new(&mut app).expect("window creation must succeed");
    assert_eq!(window.states(), 0);
    assert!(!window.is_maximized());
    assert!(!window.is_fullscreen());
    assert!(!window.is_activated());
    assert!(!window.is_resizing());
    assert!(!window.resizing_seen_in_last_dispatch());
}

#[test]
fn window_preserves_a_resizing_transition_within_one_dispatch_batch() {
    use display_proto::xdg_toplevel_state;
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window creation must succeed");
    let toplevel_id = window.xdg_toplevel();
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    let mut batch =
        build_configure_event_with_states(toplevel_id, 1, 900, 560, xdg_toplevel_state::RESIZING);
    batch.extend(build_configure_event(toplevel_id, 2, 900, 560));
    window
        .app_mut()
        .client_mut()
        .connection_mut()
        .push_inbound(batch);

    window.dispatch().expect("batched configure dispatch");
    assert_eq!(window.configured_size(), (900, 560));
    assert!(!window.is_resizing());
    assert!(window.resizing_seen_in_last_dispatch());

    window.dispatch().expect("empty dispatch");
    assert!(!window.resizing_seen_in_last_dispatch());
}

#[test]
fn window_set_maximized_sends_request_with_empty_payload() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window creation must succeed");
    let toplevel_id = window.xdg_toplevel();
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    window.set_maximized().expect("set_maximized must succeed");

    let bytes = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let requests = parse_requests(&bytes);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].object_id, toplevel_id);
    assert_eq!(requests[0].opcode, 5 /* set_maximized */);
    assert!(requests[0].payload.is_empty());
}

#[test]
fn window_unset_maximized_sends_request_with_empty_payload() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window creation must succeed");
    let toplevel_id = window.xdg_toplevel();
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    window
        .unset_maximized()
        .expect("unset_maximized must succeed");

    let bytes = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let requests = parse_requests(&bytes);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].object_id, toplevel_id);
    assert_eq!(requests[0].opcode, 6 /* unset_maximized */);
    assert!(requests[0].payload.is_empty());
}

#[test]
fn window_maximize_restore_round_trip_via_state_bit() {
    use display_proto::xdg_toplevel_state;
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window creation must succeed");
    let toplevel_id = window.xdg_toplevel();
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    // First configure: not maximized.
    window
        .app_mut()
        .client_mut()
        .connection_mut()
        .push_inbound(build_configure_event_with_states(
            toplevel_id,
            1,
            800,
            600,
            0,
        ));
    let _ = window.dispatch().expect("dispatch 1");
    assert!(!window.is_maximized());
    assert_eq!(window.configured_size(), (800, 600));

    // Server response to set_maximized: configure with MAXIMIZED bit + work-area-sized geometry.
    window
        .app_mut()
        .client_mut()
        .connection_mut()
        .push_inbound(build_configure_event_with_states(
            toplevel_id,
            2,
            1920,
            1080,
            xdg_toplevel_state::MAXIMIZED,
        ));
    let _ = window.dispatch().expect("dispatch 2");
    assert!(window.is_maximized());
    assert_eq!(window.configured_size(), (1920, 1080));

    // Server response to unset_maximized: previous size + clear MAXIMIZED.
    window
        .app_mut()
        .client_mut()
        .connection_mut()
        .push_inbound(build_configure_event_with_states(
            toplevel_id,
            3,
            800,
            600,
            0,
        ));
    let _ = window.dispatch().expect("dispatch 3");
    assert!(!window.is_maximized());
    assert_eq!(window.configured_size(), (800, 600));
}

#[test]
fn window_request_move_sends_move_request_with_serial() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window creation must succeed");
    let toplevel_id = window.xdg_toplevel();
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    window
        .request_move(0x1234_5678)
        .expect("request_move must succeed");

    let bytes = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let requests = parse_requests(&bytes);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].object_id, toplevel_id);
    assert_eq!(requests[0].opcode, 7 /* move */);
    assert_eq!(requests[0].payload.len(), 4);
    assert_eq!(
        u32::from_le_bytes(requests[0].payload[..4].try_into().unwrap()),
        0x1234_5678
    );
}

#[test]
fn window_request_resize_sends_resize_request_with_serial_and_edges() {
    use display_proto::xdg_toplevel_resize_edge as edge;
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap must succeed");
    let _ = app.client_mut().connection_mut().drain_outbound();
    let mut window = Window::new(&mut app).expect("window creation must succeed");
    let toplevel_id = window.xdg_toplevel();
    let _ = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();

    window
        .request_resize(0xCAFE, edge::BOTTOM_RIGHT)
        .expect("request_resize must succeed");

    let bytes = window
        .app_mut()
        .client_mut()
        .connection_mut()
        .drain_outbound();
    let requests = parse_requests(&bytes);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].object_id, toplevel_id);
    assert_eq!(requests[0].opcode, 8 /* resize */);
    assert_eq!(requests[0].payload.len(), 8);
    assert_eq!(
        u32::from_le_bytes(requests[0].payload[..4].try_into().unwrap()),
        0xCAFE
    );
    assert_eq!(
        u32::from_le_bytes(requests[0].payload[4..8].try_into().unwrap()),
        edge::BOTTOM_RIGHT
    );
}
