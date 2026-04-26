//! Top-level `Server` tests.

use display_proto::events::{key_state, pointer_button_state, KeyboardKey, PointerButton, PointerMotion};
use display_proto::wire::MessageHeader as ProtoHeader;
use display_server::client::ClientError;
use display_server::ids::ObjectId;
use display_server::objects::Interface;
use display_server::server::{HitResult, Server, ServerError};
use display_server::wire::{MessageHeader, WireError, HEADER_SIZE};

/// Build a framed message with the supplied payload.
fn encode_request_bytes(
    object_id: ObjectId,
    opcode: u16,
    payload: &[u8],
) -> Vec<u8> {
    let mut buf = vec![0u8; HEADER_SIZE + payload.len()];
    let h = MessageHeader::try_new(object_id, opcode, payload.len(), 0).unwrap();
    h.encode(&mut buf[..HEADER_SIZE]).unwrap();
    buf[HEADER_SIZE..].copy_from_slice(payload);
    buf
}

/// `registry.bind` payload per spec §4: u32 name + wire
/// string + u32 version + u32 new_id, string content padded
/// to a 4-byte boundary.
fn registry_bind_payload(
    name: u32,
    interface_name: &str,
    version: u32,
    new_id: ObjectId,
) -> Vec<u8> {
    let bytes = interface_name.as_bytes();
    let pad = (4 - (bytes.len() % 4)) % 4;
    let mut out = Vec::with_capacity(4 + 4 + bytes.len() + pad + 4 + 4);
    out.extend_from_slice(&name.to_le_bytes());
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
    out.extend(core::iter::repeat(0u8).take(pad));
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&new_id.raw().to_le_bytes());
    out
}

#[test]
fn new_server_has_no_clients() {
    let s = Server::new();
    assert_eq!(s.client_count(), 0);
}

#[test]
fn accept_allocates_monotonic_client_ids() {
    let mut s = Server::new();
    let c1 = s.accept();
    let c2 = s.accept();
    let c3 = s.accept();
    assert_eq!(c1.0, 1);
    assert_eq!(c2.0, 2);
    assert_eq!(c3.0, 3);
    assert_eq!(s.client_count(), 3);
}

#[test]
fn accept_pre_binds_the_display_object_on_every_client() {
    let mut s = Server::new();
    let c = s.accept();
    let client = s.client(c).unwrap();
    assert_eq!(client.get(ObjectId::DISPLAY), Some(Interface::Display));
    assert_eq!(client.object_count(), 1);
}

#[test]
fn disconnect_removes_the_client() {
    let mut s = Server::new();
    let c = s.accept();
    assert_eq!(s.client_count(), 1);
    let removed = s.disconnect(c);
    assert!(removed.is_some());
    assert_eq!(s.client_count(), 0);
    assert!(s.client(c).is_none());
    // Disconnecting again is harmless.
    assert!(s.disconnect(c).is_none());
}

#[test]
fn dispatch_request_routes_bytes_through_the_client_state_machine() {
    let mut s = Server::new();
    let c = s.accept();
    // display.get_registry(new_id=3).
    let payload = ObjectId::new(3).raw().to_le_bytes();
    let bytes = encode_request_bytes(ObjectId::DISPLAY, 2, &payload);
    s.dispatch_request(c, &bytes).unwrap();

    let client = s.client_mut(c).unwrap();
    let journal = client.drain_journal();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].opcode_name, "get_registry");
    // Auto-install put the registry in the table.
    assert_eq!(client.get(ObjectId::new(3)), Some(Interface::Registry));
}

#[test]
fn dispatch_on_unknown_client_is_no_such_client() {
    let mut s = Server::new();
    // Use sync (opcode 1) with a new_id payload — sync also
    // creates an object but the server's skeleton doesn't
    // yet auto-install it, so any payload shape that satisfies
    // "length >= 4" works. But for this test we never reach
    // dispatch_request's decoder: the client-lookup fails
    // first.
    let payload = ObjectId::new(99).raw().to_le_bytes();
    let bytes = encode_request_bytes(ObjectId::DISPLAY, 1, &payload);
    let stray = display_server::ClientId(99);
    let err = s.dispatch_request(stray, &bytes).unwrap_err();
    assert_eq!(err, ServerError::NoSuchClient { id: stray });
}

#[test]
fn dispatch_with_truncated_input_surfaces_a_wire_error() {
    let mut s = Server::new();
    let c = s.accept();
    let short = vec![0u8; HEADER_SIZE - 1];
    let err = s.dispatch_request(c, &short).unwrap_err();
    assert_eq!(err, ServerError::Wire(WireError::Truncated));
}

#[test]
fn dispatch_with_unknown_object_surfaces_a_client_error() {
    let mut s = Server::new();
    let c = s.accept();
    // Object 99 isn't bound, so dispatch returns UnknownObject
    // BEFORE any payload decoding is attempted.
    let bytes = encode_request_bytes(ObjectId::new(99), 1, &[]);
    let err = s.dispatch_request(c, &bytes).unwrap_err();
    assert_eq!(
        err,
        ServerError::Client(ClientError::UnknownObject {
            id: ObjectId::new(99)
        })
    );
}

#[test]
fn drain_client_events_returns_none_for_unknown_client() {
    let mut s = Server::new();
    let stray = display_server::ClientId(99);
    assert!(s.drain_client_events(stray).is_none());
}

#[test]
fn drain_client_events_returns_empty_for_client_with_no_pending_events() {
    let mut s = Server::new();
    let c = s.accept();
    assert_eq!(s.drain_client_events(c), Some(Vec::new()));
}

#[test]
fn drain_client_events_returns_bytes_from_a_prior_client_emit() {
    use display_proto::wire::MessageHeader;
    let mut s = Server::new();
    let c = s.accept();
    s.client_mut(c)
        .unwrap()
        .emit_error(ObjectId::DISPLAY, 42, "demo")
        .unwrap();

    let bytes = s.drain_client_events(c).unwrap();
    assert!(!bytes.is_empty());
    let header = MessageHeader::decode(&bytes).unwrap();
    assert_eq!(header.object_id, ObjectId::DISPLAY);
    assert_eq!(header.opcode, 1 /* error */);

    // Second drain is empty — the queue has been flushed.
    assert_eq!(s.drain_client_events(c), Some(Vec::new()));
}

#[test]
fn pending_events_are_per_client_not_shared_across_the_server() {
    let mut s = Server::new();
    let a = s.accept();
    let b = s.accept();
    s.client_mut(a)
        .unwrap()
        .emit_error(ObjectId::DISPLAY, 1, "a")
        .unwrap();
    assert!(!s.drain_client_events(a).unwrap().is_empty());
    assert!(s.drain_client_events(b).unwrap().is_empty());
}

// ---- xdg-shell → framebuffer integration ------------------------

/// Build a `shm.create_pool(new_id, size)` payload.
fn shm_create_pool_payload(new_id: ObjectId, size: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(&new_id.raw().to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out
}

/// Build a `shm_pool.create_buffer(...)` payload.
fn shm_pool_create_buffer_payload(
    new_id: ObjectId,
    offset: u32,
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(24);
    out.extend_from_slice(&new_id.raw().to_le_bytes());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&stride.to_le_bytes());
    out.extend_from_slice(&format.to_le_bytes());
    out
}

/// Build an `xdg_shell.get_toplevel(new_id, surface_id)` payload.
fn xdg_get_toplevel_payload(new_id: ObjectId, surface_id: ObjectId) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(&new_id.raw().to_le_bytes());
    out.extend_from_slice(&surface_id.raw().to_le_bytes());
    out
}

/// Build a `surface.attach(buffer_id, x, y)` payload.
fn surface_attach_payload(buffer_id: ObjectId, x: i32, y: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    out.extend_from_slice(&buffer_id.raw().to_le_bytes());
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    out
}

#[test]
fn toplevel_blit_lands_at_server_assigned_origin_not_surface_origin() {
    // End-to-end: a client binds every interesting global,
    // creates a surface, wraps it in an xdg_toplevel, and
    // commits a 2x2 red buffer. The committed pixels must
    // appear in the framebuffer at the toplevel's
    // auto-layout origin — which for the first toplevel is
    // (0, 0), same as the surface-origin path — so this
    // test mainly proves the plumbing doesn't break for a
    // single window. The following test shows two
    // toplevels land at DIFFERENT origins.
    let mut s = Server::with_framebuffer_size(16, 16);
    let c = s.accept();

    let registry_id = ObjectId::new(3);
    let compositor_id = ObjectId::new(5);
    let shm_id = ObjectId::new(7);
    let xdg_shell_id = ObjectId::new(9);
    let surface_id = ObjectId::new(11);
    let pool_id = ObjectId::new(13);
    let buffer_id = ObjectId::new(15);
    let toplevel_id = ObjectId::new(17);

    // get_registry + binds.
    s.dispatch_request(
        c,
        &encode_request_bytes(
            ObjectId::DISPLAY,
            2,
            &registry_id.raw().to_le_bytes(),
        ),
    )
    .unwrap();
    for (name, iface, bound) in [
        (1u32, "pmd_compositor", compositor_id),
        (2, "pmd_shm", shm_id),
        (3, "pmd_xdg_shell", xdg_shell_id),
    ] {
        let payload = registry_bind_payload(name, iface, 1, bound);
        s.dispatch_request(c, &encode_request_bytes(registry_id, 1, &payload))
            .unwrap();
    }

    // create_surface + pool + buffer + xdg_get_toplevel.
    s.dispatch_request(
        c,
        &encode_request_bytes(compositor_id, 1, &surface_id.raw().to_le_bytes()),
    )
    .unwrap();
    s.dispatch_request(
        c,
        &encode_request_bytes(shm_id, 1, &shm_create_pool_payload(pool_id, 16)),
    )
    .unwrap();
    s.dispatch_request(
        c,
        &encode_request_bytes(
            pool_id,
            1,
            &shm_pool_create_buffer_payload(buffer_id, 0, 2, 2, 8, 0),
        ),
    )
    .unwrap();
    s.dispatch_request(
        c,
        &encode_request_bytes(
            xdg_shell_id,
            1,
            &xdg_get_toplevel_payload(toplevel_id, surface_id),
        ),
    )
    .unwrap();

    // Paint the pool bright red.
    let bytes = s.client_mut(c).unwrap().pool_bytes_mut(pool_id).unwrap();
    for px in bytes.chunks_exact_mut(4) {
        px[0] = 0x00; // B
        px[1] = 0x00; // G
        px[2] = 0xff; // R
        px[3] = 0xff; // A
    }

    // attach + damage + commit.
    s.dispatch_request(
        c,
        &encode_request_bytes(surface_id, 2, &surface_attach_payload(buffer_id, 0, 0)),
    )
    .unwrap();
    // damage is all zeros → full frame.
    s.dispatch_request(c, &encode_request_bytes(surface_id, 3, &[0u8; 16]))
        .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface_id, 7, &[]))
        .unwrap();

    // Framebuffer now has a red 2x2 at (0, 0).
    let fb = s.framebuffer();
    assert_eq!(fb.pixel(0, 0).unwrap(), &[0x00, 0x00, 0xff, 0xff]);
    assert_eq!(fb.pixel(1, 1).unwrap(), &[0x00, 0x00, 0xff, 0xff]);
    assert_eq!(fb.pixel(2, 2).unwrap(), &[0, 0, 0, 0]); // bg
}

#[test]
fn two_toplevels_blit_at_distinct_auto_layout_origins() {
    // A client creates two surfaces, wraps each in an
    // xdg_toplevel, and commits a distinct colour for
    // each. The framebuffer should carry both windows at
    // DIFFERENT origins — (0, 0) and (STEP, STEP).
    let mut s = Server::with_framebuffer_size(64, 64);
    let c = s.accept();

    let registry_id = ObjectId::new(3);
    let compositor_id = ObjectId::new(5);
    let shm_id = ObjectId::new(7);
    let xdg_shell_id = ObjectId::new(9);

    s.dispatch_request(
        c,
        &encode_request_bytes(
            ObjectId::DISPLAY,
            2,
            &registry_id.raw().to_le_bytes(),
        ),
    )
    .unwrap();
    for (name, iface, bound) in [
        (1u32, "pmd_compositor", compositor_id),
        (2, "pmd_shm", shm_id),
        (3, "pmd_xdg_shell", xdg_shell_id),
    ] {
        let payload = registry_bind_payload(name, iface, 1, bound);
        s.dispatch_request(c, &encode_request_bytes(registry_id, 1, &payload))
            .unwrap();
    }

    // First window: 2x2 red at (0, 0).
    let surface_a = ObjectId::new(11);
    let pool_a = ObjectId::new(13);
    let buf_a = ObjectId::new(15);
    let top_a = ObjectId::new(17);
    s.dispatch_request(
        c,
        &encode_request_bytes(compositor_id, 1, &surface_a.raw().to_le_bytes()),
    )
    .unwrap();
    s.dispatch_request(
        c,
        &encode_request_bytes(shm_id, 1, &shm_create_pool_payload(pool_a, 16)),
    )
    .unwrap();
    s.dispatch_request(
        c,
        &encode_request_bytes(
            pool_a,
            1,
            &shm_pool_create_buffer_payload(buf_a, 0, 2, 2, 8, 0),
        ),
    )
    .unwrap();
    s.dispatch_request(
        c,
        &encode_request_bytes(
            xdg_shell_id,
            1,
            &xdg_get_toplevel_payload(top_a, surface_a),
        ),
    )
    .unwrap();
    for px in s
        .client_mut(c)
        .unwrap()
        .pool_bytes_mut(pool_a)
        .unwrap()
        .chunks_exact_mut(4)
    {
        px.copy_from_slice(&[0x00, 0x00, 0xff, 0xff]); // red
    }
    s.dispatch_request(
        c,
        &encode_request_bytes(surface_a, 2, &surface_attach_payload(buf_a, 0, 0)),
    )
    .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface_a, 3, &[0u8; 16]))
        .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface_a, 7, &[]))
        .unwrap();

    // Second window: 2x2 green at the auto-layout step.
    let surface_b = ObjectId::new(19);
    let pool_b = ObjectId::new(21);
    let buf_b = ObjectId::new(23);
    let top_b = ObjectId::new(25);
    s.dispatch_request(
        c,
        &encode_request_bytes(compositor_id, 1, &surface_b.raw().to_le_bytes()),
    )
    .unwrap();
    s.dispatch_request(
        c,
        &encode_request_bytes(shm_id, 1, &shm_create_pool_payload(pool_b, 16)),
    )
    .unwrap();
    s.dispatch_request(
        c,
        &encode_request_bytes(
            pool_b,
            1,
            &shm_pool_create_buffer_payload(buf_b, 0, 2, 2, 8, 0),
        ),
    )
    .unwrap();
    s.dispatch_request(
        c,
        &encode_request_bytes(
            xdg_shell_id,
            1,
            &xdg_get_toplevel_payload(top_b, surface_b),
        ),
    )
    .unwrap();
    for px in s
        .client_mut(c)
        .unwrap()
        .pool_bytes_mut(pool_b)
        .unwrap()
        .chunks_exact_mut(4)
    {
        px.copy_from_slice(&[0x00, 0xff, 0x00, 0xff]); // green
    }
    s.dispatch_request(
        c,
        &encode_request_bytes(surface_b, 2, &surface_attach_payload(buf_b, 0, 0)),
    )
    .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface_b, 3, &[0u8; 16]))
        .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface_b, 7, &[]))
        .unwrap();

    let fb = s.framebuffer();
    // Window A: red at (0, 0).
    assert_eq!(fb.pixel(0, 0).unwrap(), &[0x00, 0x00, 0xff, 0xff]);
    assert_eq!(fb.pixel(1, 1).unwrap(), &[0x00, 0x00, 0xff, 0xff]);
    // Window B: green at (STEP, STEP) — 32, 32 for v1.
    let step = display_server::AUTO_LAYOUT_STEP as u32;
    assert_eq!(
        fb.pixel(step, step).unwrap(),
        &[0x00, 0xff, 0x00, 0xff]
    );
    assert_eq!(
        fb.pixel(step + 1, step + 1).unwrap(),
        &[0x00, 0xff, 0x00, 0xff]
    );
    // Between the two windows — still background (zero).
    assert_eq!(fb.pixel(10, 10).unwrap(), &[0, 0, 0, 0]);
}

// ---- input routing: seat/pointer/keyboard ---------------------

/// Build a `seat.get_pointer(new_id)` or
/// `seat.get_keyboard(new_id)` payload — both are a
/// single 4-byte object id.
fn new_id_payload(id: ObjectId) -> Vec<u8> {
    id.raw().to_le_bytes().to_vec()
}

/// Bring a fresh server up with one client that has bound
/// every global needed to hit-test, input-route, and blit:
/// compositor, shm, xdg_shell, seat. Returns the client id
/// plus the object ids it can use in further dispatch.
///
/// The layout: fresh 64x64 framebuffer. Up to the caller
/// to create surfaces, pools, buffers, toplevels.
fn boot_server_and_bind_everything() -> (
    Server,
    display_server::ClientId,
    ObjectId,
    ObjectId,
    ObjectId,
    ObjectId,
) {
    let mut s = Server::with_framebuffer_size(64, 64);
    let c = s.accept();
    let registry_id = ObjectId::new(3);
    let compositor_id = ObjectId::new(5);
    let shm_id = ObjectId::new(7);
    let xdg_shell_id = ObjectId::new(9);
    let seat_id = ObjectId::new(11);

    // get_registry.
    s.dispatch_request(
        c,
        &encode_request_bytes(
            ObjectId::DISPLAY,
            2,
            &registry_id.raw().to_le_bytes(),
        ),
    )
    .unwrap();
    // Bind everything we need.
    for (name, iface, bound) in [
        (1u32, "pmd_compositor", compositor_id),
        (2, "pmd_shm", shm_id),
        (3, "pmd_xdg_shell", xdg_shell_id),
        (4, "pmd_seat", seat_id),
    ] {
        let payload = registry_bind_payload(name, iface, 1, bound);
        s.dispatch_request(c, &encode_request_bytes(registry_id, 1, &payload))
            .unwrap();
    }
    (s, c, compositor_id, shm_id, xdg_shell_id, seat_id)
}

/// Allocate a 2x2 ARGB8888 buffer from a fresh pool and
/// return its id. Also calls create_surface to produce a
/// fresh surface id. Returns (surface_id, pool_id,
/// buffer_id).
fn make_surface_with_buffer(
    s: &mut Server,
    c: display_server::ClientId,
    compositor_id: ObjectId,
    shm_id: ObjectId,
    surface_id: ObjectId,
    pool_id: ObjectId,
    buffer_id: ObjectId,
) {
    // create_surface.
    s.dispatch_request(
        c,
        &encode_request_bytes(compositor_id, 1, &surface_id.raw().to_le_bytes()),
    )
    .unwrap();
    // shm.create_pool(pool_id, 16).
    let mut pool_payload = Vec::with_capacity(8);
    pool_payload.extend_from_slice(&pool_id.raw().to_le_bytes());
    pool_payload.extend_from_slice(&16u32.to_le_bytes());
    s.dispatch_request(c, &encode_request_bytes(shm_id, 1, &pool_payload))
        .unwrap();
    // shm_pool.create_buffer(buffer_id, 0, 2, 2, 8, 0).
    let mut buf_payload = Vec::with_capacity(24);
    buf_payload.extend_from_slice(&buffer_id.raw().to_le_bytes());
    for v in [0u32, 2, 2, 8, 0] {
        buf_payload.extend_from_slice(&v.to_le_bytes());
    }
    s.dispatch_request(c, &encode_request_bytes(pool_id, 1, &buf_payload))
        .unwrap();
}

/// Wrap a surface in a toplevel via xdg_shell.get_toplevel
/// and commit a red buffer at origin so the window
/// becomes hit-testable.
fn promote_to_toplevel_and_commit_red(
    s: &mut Server,
    c: display_server::ClientId,
    xdg_shell_id: ObjectId,
    surface_id: ObjectId,
    pool_id: ObjectId,
    buffer_id: ObjectId,
    toplevel_id: ObjectId,
) {
    let mut top_payload = Vec::with_capacity(8);
    top_payload.extend_from_slice(&toplevel_id.raw().to_le_bytes());
    top_payload.extend_from_slice(&surface_id.raw().to_le_bytes());
    s.dispatch_request(c, &encode_request_bytes(xdg_shell_id, 1, &top_payload))
        .unwrap();
    // Paint pool red.
    let bytes = s.client_mut(c).unwrap().pool_bytes_mut(pool_id).unwrap();
    for px in bytes.chunks_exact_mut(4) {
        px.copy_from_slice(&[0, 0, 0xff, 0xff]);
    }
    // attach + damage + commit.
    let mut attach_payload = Vec::with_capacity(12);
    attach_payload.extend_from_slice(&buffer_id.raw().to_le_bytes());
    attach_payload.extend_from_slice(&0i32.to_le_bytes());
    attach_payload.extend_from_slice(&0i32.to_le_bytes());
    s.dispatch_request(c, &encode_request_bytes(surface_id, 2, &attach_payload))
        .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface_id, 3, &[0u8; 16]))
        .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface_id, 7, &[]))
        .unwrap();
}

#[test]
fn seat_get_pointer_auto_installs_pointer_object() {
    let (mut s, c, _, _, _, seat_id) = boot_server_and_bind_everything();
    let pointer_id = ObjectId::new(13);
    s.dispatch_request(
        c,
        &encode_request_bytes(seat_id, 1, &new_id_payload(pointer_id)),
    )
    .unwrap();
    let client = s.client(c).unwrap();
    assert_eq!(client.get(pointer_id), Some(Interface::Pointer));
    assert_eq!(client.pointer_id, Some(pointer_id));
}

#[test]
fn seat_get_keyboard_auto_installs_keyboard_object() {
    let (mut s, c, _, _, _, seat_id) = boot_server_and_bind_everything();
    let kbd_id = ObjectId::new(33);
    s.dispatch_request(
        c,
        &encode_request_bytes(seat_id, 2, &new_id_payload(kbd_id)),
    )
    .unwrap();
    let client = s.client(c).unwrap();
    assert_eq!(client.get(kbd_id), Some(Interface::Keyboard));
    assert_eq!(client.keyboard_id, Some(kbd_id));
}

#[test]
fn seat_get_pointer_twice_errors_with_pointer_already_bound() {
    let (mut s, c, _, _, _, seat_id) = boot_server_and_bind_everything();
    let pointer_a = ObjectId::new(31);
    let pointer_b = ObjectId::new(33);
    s.dispatch_request(
        c,
        &encode_request_bytes(seat_id, 1, &new_id_payload(pointer_a)),
    )
    .unwrap();
    let err = s
        .dispatch_request(
            c,
            &encode_request_bytes(seat_id, 1, &new_id_payload(pointer_b)),
        )
        .unwrap_err();
    match err {
        ServerError::Client(ClientError::PointerAlreadyBound { existing }) => {
            assert_eq!(existing, pointer_a);
        }
        other => panic!("expected PointerAlreadyBound, got {other:?}"),
    }
}

#[test]
fn hit_test_returns_none_on_a_server_with_no_toplevels() {
    let s = Server::with_framebuffer_size(32, 32);
    assert_eq!(s.hit_test(10, 10), None);
}

#[test]
fn hit_test_returns_the_toplevels_surface_when_point_is_inside() {
    let (mut s, c, compositor_id, shm_id, xdg_shell_id, _seat_id) =
        boot_server_and_bind_everything();
    let surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let buffer = ObjectId::new(17);
    let toplevel = ObjectId::new(19);
    make_surface_with_buffer(
        &mut s, c, compositor_id, shm_id, surface, pool, buffer,
    );
    promote_to_toplevel_and_commit_red(
        &mut s,
        c,
        xdg_shell_id,
        surface,
        pool,
        buffer,
        toplevel,
    );

    // Window is at (0, 0) with a 2x2 buffer.
    let hit = s.hit_test(1, 0).expect("hit a window");
    assert_eq!(hit.client_id, c);
    assert_eq!(hit.surface_id, surface);
    assert_eq!(hit.local_x, 1);
    assert_eq!(hit.local_y, 0);
}

#[test]
fn hit_test_returns_none_when_point_is_outside_the_toplevel_rectangle() {
    let (mut s, c, compositor_id, shm_id, xdg_shell_id, _seat_id) =
        boot_server_and_bind_everything();
    let surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let buffer = ObjectId::new(17);
    let toplevel = ObjectId::new(19);
    make_surface_with_buffer(
        &mut s, c, compositor_id, shm_id, surface, pool, buffer,
    );
    promote_to_toplevel_and_commit_red(
        &mut s,
        c,
        xdg_shell_id,
        surface,
        pool,
        buffer,
        toplevel,
    );
    // Window spans (0, 0) to (1, 1) inclusive; (2, 0) is
    // just past the right edge.
    assert!(s.hit_test(2, 0).is_none());
    assert!(s.hit_test(0, 2).is_none());
    assert!(s.hit_test(-1, 0).is_none());
    assert!(s.hit_test(0, -1).is_none());
}

#[test]
fn hit_test_picks_the_top_most_toplevel_when_two_overlap() {
    // Two toplevels at staircase origins: top_a at (0, 0),
    // top_b at (STEP, STEP). If we put a 64x64 buffer on
    // each, they overlap. The point at (STEP, STEP) must
    // resolve to top_b because it was created second and
    // the z-order is "newer wins" (reverse iteration).
    let (mut s, c, compositor_id, shm_id, xdg_shell_id, _seat_id) =
        boot_server_and_bind_everything();

    // First window.
    let surface_a = ObjectId::new(13);
    let pool_a = ObjectId::new(15);
    let buffer_a = ObjectId::new(17);
    let top_a = ObjectId::new(19);
    // create_surface at (13) first.
    s.dispatch_request(
        c,
        &encode_request_bytes(compositor_id, 1, &surface_a.raw().to_le_bytes()),
    )
    .unwrap();
    // 64x64 ARGB8888 = 16384 bytes.
    let mut pool_payload = Vec::with_capacity(8);
    pool_payload.extend_from_slice(&pool_a.raw().to_le_bytes());
    pool_payload.extend_from_slice(&(64u32 * 64 * 4).to_le_bytes());
    s.dispatch_request(c, &encode_request_bytes(shm_id, 1, &pool_payload))
        .unwrap();
    let mut buf_payload = Vec::with_capacity(24);
    buf_payload.extend_from_slice(&buffer_a.raw().to_le_bytes());
    for v in [0u32, 64, 64, 64 * 4, 0] {
        buf_payload.extend_from_slice(&v.to_le_bytes());
    }
    s.dispatch_request(c, &encode_request_bytes(pool_a, 1, &buf_payload))
        .unwrap();
    let mut top_payload = Vec::with_capacity(8);
    top_payload.extend_from_slice(&top_a.raw().to_le_bytes());
    top_payload.extend_from_slice(&surface_a.raw().to_le_bytes());
    s.dispatch_request(c, &encode_request_bytes(xdg_shell_id, 1, &top_payload))
        .unwrap();
    let mut attach_payload = Vec::with_capacity(12);
    attach_payload.extend_from_slice(&buffer_a.raw().to_le_bytes());
    attach_payload.extend_from_slice(&0i32.to_le_bytes());
    attach_payload.extend_from_slice(&0i32.to_le_bytes());
    s.dispatch_request(c, &encode_request_bytes(surface_a, 2, &attach_payload))
        .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface_a, 3, &[0u8; 16]))
        .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface_a, 7, &[]))
        .unwrap();

    // Second window — same 64x64 geometry, different ids.
    let surface_b = ObjectId::new(21);
    let pool_b = ObjectId::new(23);
    let buffer_b = ObjectId::new(25);
    let top_b = ObjectId::new(27);
    s.dispatch_request(
        c,
        &encode_request_bytes(compositor_id, 1, &surface_b.raw().to_le_bytes()),
    )
    .unwrap();
    let mut pool_payload = Vec::with_capacity(8);
    pool_payload.extend_from_slice(&pool_b.raw().to_le_bytes());
    pool_payload.extend_from_slice(&(64u32 * 64 * 4).to_le_bytes());
    s.dispatch_request(c, &encode_request_bytes(shm_id, 1, &pool_payload))
        .unwrap();
    let mut buf_payload = Vec::with_capacity(24);
    buf_payload.extend_from_slice(&buffer_b.raw().to_le_bytes());
    for v in [0u32, 64, 64, 64 * 4, 0] {
        buf_payload.extend_from_slice(&v.to_le_bytes());
    }
    s.dispatch_request(c, &encode_request_bytes(pool_b, 1, &buf_payload))
        .unwrap();
    let mut top_payload = Vec::with_capacity(8);
    top_payload.extend_from_slice(&top_b.raw().to_le_bytes());
    top_payload.extend_from_slice(&surface_b.raw().to_le_bytes());
    s.dispatch_request(c, &encode_request_bytes(xdg_shell_id, 1, &top_payload))
        .unwrap();
    let mut attach_payload = Vec::with_capacity(12);
    attach_payload.extend_from_slice(&buffer_b.raw().to_le_bytes());
    attach_payload.extend_from_slice(&0i32.to_le_bytes());
    attach_payload.extend_from_slice(&0i32.to_le_bytes());
    s.dispatch_request(c, &encode_request_bytes(surface_b, 2, &attach_payload))
        .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface_b, 3, &[0u8; 16]))
        .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface_b, 7, &[]))
        .unwrap();

    // Top A lives at (0..64, 0..64); top B lives at
    // (32..96, 32..96). Their overlap is (32..64, 32..64).
    // A point in the overlap should resolve to the newer
    // window (B).
    let step = display_server::AUTO_LAYOUT_STEP;
    let hit = s.hit_test(step + 5, step + 5).unwrap();
    assert_eq!(hit.surface_id, surface_b);
    // Surface-local coords are relative to top_b's origin.
    assert_eq!(hit.local_x, 5);
    assert_eq!(hit.local_y, 5);

    // Outside the overlap but inside top A's rectangle —
    // should resolve to top A.
    let hit = s.hit_test(5, 5).unwrap();
    assert_eq!(hit.surface_id, surface_a);
}

#[test]
fn inject_pointer_motion_updates_pointer_position_and_emits_motion_event() {
    let (mut s, c, compositor_id, shm_id, xdg_shell_id, seat_id) =
        boot_server_and_bind_everything();
    let pointer_id = ObjectId::new(31);
    s.dispatch_request(
        c,
        &encode_request_bytes(seat_id, 1, &new_id_payload(pointer_id)),
    )
    .unwrap();

    let surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let buffer = ObjectId::new(17);
    let toplevel = ObjectId::new(19);
    make_surface_with_buffer(
        &mut s, c, compositor_id, shm_id, surface, pool, buffer,
    );
    promote_to_toplevel_and_commit_red(
        &mut s,
        c,
        xdg_shell_id,
        surface,
        pool,
        buffer,
        toplevel,
    );

    // Drain prior pending events so the test only sees
    // what `inject_pointer_motion` produces.
    let _ = s.drain_client_events(c);

    let hit = s.inject_pointer_motion(1, 0).expect("hit a window");
    assert_eq!(hit.surface_id, surface);
    assert_eq!(s.pointer_position(), (1, 0));

    let bytes = s.drain_client_events(c).unwrap();
    let header = ProtoHeader::decode(&bytes).unwrap();
    assert_eq!(header.object_id, pointer_id);
    assert_eq!(header.opcode, 1 /* motion */);
    let payload = &bytes[display_proto::wire::HEADER_SIZE..header.length as usize];
    let event = PointerMotion::decode(payload).unwrap();
    assert_eq!(event.surface_id, surface);
    assert_eq!(event.x, 1);
    assert_eq!(event.y, 0);
}

#[test]
fn inject_pointer_motion_over_empty_space_is_a_none_result() {
    let (mut s, c, _compositor, _shm, _xdg, seat_id) =
        boot_server_and_bind_everything();
    let pointer_id = ObjectId::new(31);
    s.dispatch_request(
        c,
        &encode_request_bytes(seat_id, 1, &new_id_payload(pointer_id)),
    )
    .unwrap();

    // No toplevels created → no hit.
    assert_eq!(s.inject_pointer_motion(10, 10), None);
    // But the pointer position DID move — the server
    // tracks it regardless of whether a window is under
    // the pointer.
    assert_eq!(s.pointer_position(), (10, 10));
}

#[test]
fn inject_pointer_button_sets_keyboard_focus_on_press() {
    let (mut s, c, compositor_id, shm_id, xdg_shell_id, seat_id) =
        boot_server_and_bind_everything();
    let pointer_id = ObjectId::new(31);
    s.dispatch_request(
        c,
        &encode_request_bytes(seat_id, 1, &new_id_payload(pointer_id)),
    )
    .unwrap();

    let surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let buffer = ObjectId::new(17);
    let toplevel = ObjectId::new(19);
    make_surface_with_buffer(
        &mut s, c, compositor_id, shm_id, surface, pool, buffer,
    );
    promote_to_toplevel_and_commit_red(
        &mut s,
        c,
        xdg_shell_id,
        surface,
        pool,
        buffer,
        toplevel,
    );
    let _ = s.drain_client_events(c);

    // Pointer at (1, 1) — inside the window.
    s.inject_pointer_motion(1, 1);
    // No focus yet — motion alone doesn't click-to-focus.
    assert_eq!(s.keyboard_focus(), None);

    // Press the left button.
    let hit = s
        .inject_pointer_button(1, pointer_button_state::PRESSED)
        .unwrap();
    assert_eq!(hit.surface_id, surface);
    assert_eq!(s.keyboard_focus(), Some((c, surface)));

    // A button event arrived on the pointer object.
    let bytes = s.drain_client_events(c).unwrap();
    // Find the button event (after the motion event).
    let mut remaining: &[u8] = &bytes;
    let mut saw_button = false;
    while !remaining.is_empty() {
        let header = ProtoHeader::decode(remaining).unwrap();
        let msg_len = header.length as usize;
        let payload = &remaining[display_proto::wire::HEADER_SIZE..msg_len];
        if header.object_id == pointer_id && header.opcode == 2 {
            let event = PointerButton::decode(payload).unwrap();
            assert_eq!(event.surface_id, surface);
            assert_eq!(event.button, 1);
            assert_eq!(event.state, pointer_button_state::PRESSED);
            saw_button = true;
        }
        remaining = &remaining[msg_len..];
    }
    assert!(saw_button, "expected a button event");
}

#[test]
fn inject_keyboard_key_routes_to_the_focused_client() {
    let (mut s, c, compositor_id, shm_id, xdg_shell_id, seat_id) =
        boot_server_and_bind_everything();
    let pointer_id = ObjectId::new(31);
    let kbd_id = ObjectId::new(33);
    s.dispatch_request(
        c,
        &encode_request_bytes(seat_id, 1, &new_id_payload(pointer_id)),
    )
    .unwrap();
    s.dispatch_request(
        c,
        &encode_request_bytes(seat_id, 2, &new_id_payload(kbd_id)),
    )
    .unwrap();

    let surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let buffer = ObjectId::new(17);
    let toplevel = ObjectId::new(19);
    make_surface_with_buffer(
        &mut s, c, compositor_id, shm_id, surface, pool, buffer,
    );
    promote_to_toplevel_and_commit_red(
        &mut s,
        c,
        xdg_shell_id,
        surface,
        pool,
        buffer,
        toplevel,
    );

    // Explicit focus so we don't need to click first.
    s.set_keyboard_focus(Some((c, surface)));
    let _ = s.drain_client_events(c);

    let routed = s.inject_keyboard_key(0x1e, key_state::PRESSED).unwrap();
    assert_eq!(routed, (c, surface));

    let bytes = s.drain_client_events(c).unwrap();
    let header = ProtoHeader::decode(&bytes).unwrap();
    assert_eq!(header.object_id, kbd_id);
    assert_eq!(header.opcode, 1 /* key */);
    let payload = &bytes[display_proto::wire::HEADER_SIZE..header.length as usize];
    let event = KeyboardKey::decode(payload).unwrap();
    assert_eq!(event.surface_id, surface);
    assert_eq!(event.key, 0x1e);
    assert_eq!(event.state, key_state::PRESSED);
}

#[test]
fn inject_keyboard_key_without_focus_is_a_none_result() {
    let mut s = Server::with_framebuffer_size(32, 32);
    assert!(s.inject_keyboard_key(0x1e, key_state::PRESSED).is_none());
}

// Silence the `HitResult` import if no earlier test
// references it directly — it's used as the return type
// of `hit_test` but the pattern matches extract fields.
#[allow(dead_code)]
fn _keep_imports_honest(_h: HitResult) {}

#[test]
fn multiple_clients_have_independent_object_tables() {
    let mut s = Server::new();
    let a = s.accept();
    let b = s.accept();

    // Client a acquires a registry via the normal flow:
    // display.get_registry(new_id=3) auto-installs it.
    let payload_a = ObjectId::new(3).raw().to_le_bytes();
    s.dispatch_request(a, &encode_request_bytes(ObjectId::DISPLAY, 2, &payload_a))
        .unwrap();

    // Client a can now dispatch registry.bind(compositor) on
    // object 3 — the payload names pmd_compositor and carries
    // a fresh new_id.
    let bind_payload = registry_bind_payload(1, "pmd_compositor", 1, ObjectId::new(5));
    s.dispatch_request(a, &encode_request_bytes(ObjectId::new(3), 1, &bind_payload))
        .unwrap();

    // Client b does NOT have object 3 — same registry.bind
    // bytes fail the object-lookup check.
    let err = s
        .dispatch_request(b, &encode_request_bytes(ObjectId::new(3), 1, &bind_payload))
        .unwrap_err();
    assert_eq!(
        err,
        ServerError::Client(ClientError::UnknownObject {
            id: ObjectId::new(3)
        })
    );

    // Client a's table now holds registry (3) and
    // compositor (5); client b still only has display.
    assert_eq!(s.client(a).unwrap().object_count(), 3);
    assert_eq!(s.client(b).unwrap().object_count(), 1);
}

#[test]
fn shell_manager_minimize_window_flips_target_clients_toplevel_minimized_flag() {
    // T131: a shell client (Cap::Shell) sends
    // pmd_shell_manager.minimize_window(target_id) where
    // target_id is a toplevel owned by a different client.
    // The server must locate the target across clients and
    // flip the minimized flag.
    use abi::cap::{Cap, CapSet};

    use display_server::ids::ObjectId;

    let mut s = Server::new();
    let app = s.accept(); // ordinary client
    let shell = s.accept_with_caps(CapSet::from_caps(&[Cap::Shell]));

    // App client: bind compositor + xdg_shell, create
    // surface + toplevel.
    let registry_id = ObjectId::new(3);
    let compositor_id = ObjectId::new(5);
    let xdg_shell_id = ObjectId::new(7);
    let surface_id = ObjectId::new(9);
    let toplevel_id = ObjectId::new(11);
    s.dispatch_request(
        app,
        &encode_request_bytes(
            ObjectId::DISPLAY,
            2,
            &registry_id.raw().to_le_bytes(),
        ),
    )
    .unwrap();
    for (name, iface, bound) in [
        (1u32, "pmd_compositor", compositor_id),
        (2, "pmd_xdg_shell", xdg_shell_id),
    ] {
        let payload = registry_bind_payload(name, iface, 1, bound);
        s.dispatch_request(app, &encode_request_bytes(registry_id, 1, &payload))
            .unwrap();
    }
    s.dispatch_request(
        app,
        &encode_request_bytes(compositor_id, 1, &surface_id.raw().to_le_bytes()),
    )
    .unwrap();
    s.dispatch_request(
        app,
        &encode_request_bytes(
            xdg_shell_id,
            1,
            &xdg_get_toplevel_payload(toplevel_id, surface_id),
        ),
    )
    .unwrap();
    assert!(!s.client(app).unwrap().toplevel(toplevel_id).unwrap().minimized);

    // Shell client: bind shell_manager (only privileged
    // clients can). The shell's registry id can reuse 3 —
    // every client has its own table.
    let shell_registry_id = ObjectId::new(3);
    let shell_manager_id = ObjectId::new(5);
    s.dispatch_request(
        shell,
        &encode_request_bytes(
            ObjectId::DISPLAY,
            2,
            &shell_registry_id.raw().to_le_bytes(),
        ),
    )
    .unwrap();
    let payload = registry_bind_payload(1, "pmd_shell_manager", 1, shell_manager_id);
    s.dispatch_request(
        shell,
        &encode_request_bytes(shell_registry_id, 1, &payload),
    )
    .unwrap();

    // Shell sends minimize_window(toplevel_id) targeting
    // the app client's toplevel. The payload is u32 raw id.
    let mut min_payload = Vec::new();
    min_payload.extend_from_slice(&toplevel_id.raw().to_le_bytes());
    s.dispatch_request(
        shell,
        &encode_request_bytes(shell_manager_id, 4 /* minimize_window */, &min_payload),
    )
    .unwrap();

    // The app client's toplevel is now minimized.
    assert!(
        s.client(app)
            .unwrap()
            .toplevel(toplevel_id)
            .unwrap()
            .minimized,
        "minimize_window must flip the target client's toplevel.minimized"
    );

    // Server::restore_toplevel clears the flag.
    assert!(s.restore_toplevel(toplevel_id));
    assert!(
        !s.client(app)
            .unwrap()
            .toplevel(toplevel_id)
            .unwrap()
            .minimized
    );
}

#[test]
fn restore_toplevel_returns_false_for_unknown_id() {
    use display_server::ids::ObjectId;
    let mut s = Server::new();
    let _c = s.accept();
    assert!(!s.restore_toplevel(ObjectId::new(0xdead_beef)));
}

#[test]
fn minimized_toplevel_skips_the_composite_blit() {
    // Paint a 2x2 red buffer + commit, observe the framebuffer
    // pixel-by-pixel. Then minimize the toplevel via the
    // direct setter (skipping the cross-client request to
    // keep the test self-contained), commit again, and
    // observe that the framebuffer pixels DON'T change.
    use display_server::ids::ObjectId;
    let mut s = Server::with_framebuffer_size(16, 16);
    let c = s.accept();

    let registry_id = ObjectId::new(3);
    let compositor_id = ObjectId::new(5);
    let shm_id = ObjectId::new(7);
    let xdg_shell_id = ObjectId::new(9);
    let surface_id = ObjectId::new(11);
    let pool_id = ObjectId::new(13);
    let buffer_id = ObjectId::new(15);
    let toplevel_id = ObjectId::new(17);

    s.dispatch_request(
        c,
        &encode_request_bytes(
            ObjectId::DISPLAY,
            2,
            &registry_id.raw().to_le_bytes(),
        ),
    )
    .unwrap();
    for (name, iface, bound) in [
        (1u32, "pmd_compositor", compositor_id),
        (2, "pmd_shm", shm_id),
        (3, "pmd_xdg_shell", xdg_shell_id),
    ] {
        let payload = registry_bind_payload(name, iface, 1, bound);
        s.dispatch_request(c, &encode_request_bytes(registry_id, 1, &payload))
            .unwrap();
    }
    s.dispatch_request(
        c,
        &encode_request_bytes(compositor_id, 1, &surface_id.raw().to_le_bytes()),
    )
    .unwrap();
    s.dispatch_request(
        c,
        &encode_request_bytes(shm_id, 1, &shm_create_pool_payload(pool_id, 16)),
    )
    .unwrap();
    s.dispatch_request(
        c,
        &encode_request_bytes(
            pool_id,
            1,
            &shm_pool_create_buffer_payload(buffer_id, 0, 2, 2, 8, 0),
        ),
    )
    .unwrap();
    s.dispatch_request(
        c,
        &encode_request_bytes(
            xdg_shell_id,
            1,
            &xdg_get_toplevel_payload(toplevel_id, surface_id),
        ),
    )
    .unwrap();

    // Paint pool red, attach + damage + commit.
    for px in s.client_mut(c).unwrap().pool_bytes_mut(pool_id).unwrap().chunks_exact_mut(4) {
        px[0] = 0x00;
        px[1] = 0x00;
        px[2] = 0xff;
        px[3] = 0xff;
    }
    s.dispatch_request(
        c,
        &encode_request_bytes(surface_id, 2, &surface_attach_payload(buffer_id, 0, 0)),
    )
    .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface_id, 3, &[0u8; 16]))
        .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface_id, 7, &[]))
        .unwrap();
    assert_eq!(s.framebuffer().pixel(0, 0).unwrap(), &[0x00, 0x00, 0xff, 0xff]);

    // Reset framebuffer to a known sentinel + minimize.
    let fb = s.framebuffer_mut();
    for px in fb.pixels_mut().chunks_exact_mut(4) {
        px[0] = 0xab;
        px[1] = 0xcd;
        px[2] = 0xef;
        px[3] = 0xff;
    }
    assert!(s.set_toplevel_minimized_across_clients(toplevel_id, true));

    // Re-commit. The blit should be skipped — sentinel
    // bytes remain at (0, 0).
    s.dispatch_request(c, &encode_request_bytes(surface_id, 7, &[]))
        .unwrap();
    assert_eq!(
        s.framebuffer().pixel(0, 0).unwrap(),
        &[0xab, 0xcd, 0xef, 0xff],
        "minimized toplevel must not blit"
    );

    // Restore + re-commit; the red pixels return.
    assert!(s.restore_toplevel(toplevel_id));
    s.dispatch_request(c, &encode_request_bytes(surface_id, 7, &[]))
        .unwrap();
    assert_eq!(s.framebuffer().pixel(0, 0).unwrap(), &[0x00, 0x00, 0xff, 0xff]);
}

#[test]
fn set_maximized_request_emits_configure_with_maximized_state_bit() {
    use display_proto::xdg_toplevel_state;
    use display_proto::events::XdgToplevelConfigure;
    use display_proto::wire::HEADER_SIZE as PROTO_HEADER_SIZE;
    use display_server::ids::ObjectId;

    let mut s = Server::with_framebuffer_size(800, 600);
    let c = s.accept();

    let registry_id = ObjectId::new(3);
    let compositor_id = ObjectId::new(5);
    let xdg_shell_id = ObjectId::new(7);
    let surface_id = ObjectId::new(9);
    let toplevel_id = ObjectId::new(11);
    s.dispatch_request(
        c,
        &encode_request_bytes(ObjectId::DISPLAY, 2, &registry_id.raw().to_le_bytes()),
    )
    .unwrap();
    for (name, iface, bound) in [(1u32, "pmd_compositor", compositor_id), (2, "pmd_xdg_shell", xdg_shell_id)] {
        let payload = registry_bind_payload(name, iface, 1, bound);
        s.dispatch_request(c, &encode_request_bytes(registry_id, 1, &payload)).unwrap();
    }
    s.dispatch_request(c, &encode_request_bytes(compositor_id, 1, &surface_id.raw().to_le_bytes())).unwrap();
    s.dispatch_request(c, &encode_request_bytes(xdg_shell_id, 1, &xdg_get_toplevel_payload(toplevel_id, surface_id))).unwrap();

    // Drain the bind events so the next pending event is the configure.
    let _ = s.drain_client_events(c);

    // Send set_maximized — should trigger a configure emit on the same client.
    s.dispatch_request(c, &encode_request_bytes(toplevel_id, 5 /* set_maximized */, &[])).unwrap();
    assert!(s.client(c).unwrap().toplevel(toplevel_id).unwrap().maximized);

    // The next event in the queue is the configure. Decode it.
    let bytes = s.drain_client_events(c).expect("client must exist");
    assert!(!bytes.is_empty(), "set_maximized must emit a configure");
    let header = display_proto::wire::MessageHeader::decode(&bytes).unwrap();
    assert_eq!(header.object_id, toplevel_id);
    assert_eq!(header.opcode, 1 /* configure */);
    let payload = &bytes[PROTO_HEADER_SIZE..header.length as usize];
    let configure = XdgToplevelConfigure::decode(payload).unwrap();
    assert_eq!(configure.width, 800);
    assert_eq!(configure.height, 600);
    assert_eq!(configure.states & xdg_toplevel_state::MAXIMIZED, xdg_toplevel_state::MAXIMIZED);
    assert!(configure.serial > 0);
}

#[test]
fn unset_maximized_request_emits_configure_with_no_states_bits() {
    use display_proto::events::XdgToplevelConfigure;
    use display_proto::wire::HEADER_SIZE as PROTO_HEADER_SIZE;
    use display_server::ids::ObjectId;

    let mut s = Server::with_framebuffer_size(800, 600);
    let c = s.accept();
    let registry_id = ObjectId::new(3);
    let compositor_id = ObjectId::new(5);
    let xdg_shell_id = ObjectId::new(7);
    let surface_id = ObjectId::new(9);
    let toplevel_id = ObjectId::new(11);
    s.dispatch_request(c, &encode_request_bytes(ObjectId::DISPLAY, 2, &registry_id.raw().to_le_bytes())).unwrap();
    for (name, iface, bound) in [(1u32, "pmd_compositor", compositor_id), (2, "pmd_xdg_shell", xdg_shell_id)] {
        let payload = registry_bind_payload(name, iface, 1, bound);
        s.dispatch_request(c, &encode_request_bytes(registry_id, 1, &payload)).unwrap();
    }
    s.dispatch_request(c, &encode_request_bytes(compositor_id, 1, &surface_id.raw().to_le_bytes())).unwrap();
    s.dispatch_request(c, &encode_request_bytes(xdg_shell_id, 1, &xdg_get_toplevel_payload(toplevel_id, surface_id))).unwrap();
    let _ = s.drain_client_events(c);

    s.dispatch_request(c, &encode_request_bytes(toplevel_id, 6 /* unset_maximized */, &[])).unwrap();
    assert!(!s.client(c).unwrap().toplevel(toplevel_id).unwrap().maximized);

    let bytes = s.drain_client_events(c).unwrap();
    let header = display_proto::wire::MessageHeader::decode(&bytes).unwrap();
    assert_eq!(header.opcode, 1 /* configure */);
    let payload = &bytes[PROTO_HEADER_SIZE..header.length as usize];
    let configure = XdgToplevelConfigure::decode(payload).unwrap();
    assert_eq!(configure.states, 0);
}

#[test]
fn move_request_starts_a_drag_and_pointer_motion_translates_origin() {
    use display_server::ids::ObjectId;

    let mut s = Server::with_framebuffer_size(800, 600);
    let c = s.accept();
    let registry_id = ObjectId::new(3);
    let compositor_id = ObjectId::new(5);
    let xdg_shell_id = ObjectId::new(7);
    let surface_id = ObjectId::new(9);
    let toplevel_id = ObjectId::new(11);
    s.dispatch_request(c, &encode_request_bytes(ObjectId::DISPLAY, 2, &registry_id.raw().to_le_bytes())).unwrap();
    for (name, iface, bound) in [(1u32, "pmd_compositor", compositor_id), (2, "pmd_xdg_shell", xdg_shell_id)] {
        let payload = registry_bind_payload(name, iface, 1, bound);
        s.dispatch_request(c, &encode_request_bytes(registry_id, 1, &payload)).unwrap();
    }
    s.dispatch_request(c, &encode_request_bytes(compositor_id, 1, &surface_id.raw().to_le_bytes())).unwrap();
    s.dispatch_request(c, &encode_request_bytes(xdg_shell_id, 1, &xdg_get_toplevel_payload(toplevel_id, surface_id))).unwrap();
    let initial_origin = (s.client(c).unwrap().toplevel(toplevel_id).unwrap().x,
                          s.client(c).unwrap().toplevel(toplevel_id).unwrap().y);

    // Plant the pointer at (100, 100) and send move(serial=1).
    s.inject_pointer_motion(100, 100);
    assert!(!s.is_dragging());
    let mut payload = Vec::new();
    payload.extend_from_slice(&1u32.to_le_bytes()); // serial
    s.dispatch_request(c, &encode_request_bytes(toplevel_id, 7 /* move */, &payload)).unwrap();
    assert!(s.is_dragging(), "move request must start a drag");

    // Pointer moves to (140, 130) — toplevel origin should
    // translate by (40, 30).
    s.inject_pointer_motion(140, 130);
    let new_origin = (s.client(c).unwrap().toplevel(toplevel_id).unwrap().x,
                      s.client(c).unwrap().toplevel(toplevel_id).unwrap().y);
    assert_eq!(new_origin.0, initial_origin.0 + 40);
    assert_eq!(new_origin.1, initial_origin.1 + 30);

    // Pointer button release ends the drag.
    s.inject_pointer_button(1, display_proto::events::pointer_button_state::RELEASED);
    assert!(!s.is_dragging(), "release must end the drag");
}

#[test]
fn resize_request_emits_resizing_configures_during_drag_and_final_configure_on_release() {
    use display_proto::xdg_toplevel_state;
    use display_proto::xdg_toplevel_resize_edge as edge;
    use display_proto::events::XdgToplevelConfigure;
    use display_proto::wire::HEADER_SIZE as PROTO_HEADER_SIZE;
    use display_server::ids::ObjectId;

    let mut s = Server::with_framebuffer_size(800, 600);
    let c = s.accept();
    let registry_id = ObjectId::new(3);
    let compositor_id = ObjectId::new(5);
    let shm_id = ObjectId::new(7);
    let xdg_shell_id = ObjectId::new(9);
    let surface_id = ObjectId::new(11);
    let pool_id = ObjectId::new(13);
    let buffer_id = ObjectId::new(15);
    let toplevel_id = ObjectId::new(17);
    s.dispatch_request(c, &encode_request_bytes(ObjectId::DISPLAY, 2, &registry_id.raw().to_le_bytes())).unwrap();
    for (name, iface, bound) in [(1u32, "pmd_compositor", compositor_id), (2, "pmd_shm", shm_id), (3, "pmd_xdg_shell", xdg_shell_id)] {
        let payload = registry_bind_payload(name, iface, 1, bound);
        s.dispatch_request(c, &encode_request_bytes(registry_id, 1, &payload)).unwrap();
    }
    s.dispatch_request(c, &encode_request_bytes(compositor_id, 1, &surface_id.raw().to_le_bytes())).unwrap();
    s.dispatch_request(c, &encode_request_bytes(shm_id, 1, &shm_create_pool_payload(pool_id, 32))).unwrap();
    s.dispatch_request(c, &encode_request_bytes(pool_id, 1, &shm_pool_create_buffer_payload(buffer_id, 0, 4, 2, 16, 0))).unwrap();
    s.dispatch_request(c, &encode_request_bytes(xdg_shell_id, 1, &xdg_get_toplevel_payload(toplevel_id, surface_id))).unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface_id, 2, &surface_attach_payload(buffer_id, 0, 0))).unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface_id, 7, &[])).unwrap();
    let _ = s.drain_client_events(c);

    // Plant pointer + send resize(serial=1, edges=BOTTOM_RIGHT).
    s.inject_pointer_motion(50, 50);
    let _ = s.drain_client_events(c);
    let mut payload = Vec::new();
    payload.extend_from_slice(&1u32.to_le_bytes()); // serial
    payload.extend_from_slice(&edge::BOTTOM_RIGHT.to_le_bytes());
    s.dispatch_request(c, &encode_request_bytes(toplevel_id, 8 /* resize */, &payload)).unwrap();
    assert!(s.is_dragging());
    // The dispatch shouldn't emit anything yet — drain is empty.
    assert!(s.drain_client_events(c).map(|b| b.is_empty()).unwrap_or(true));

    // Pointer drags by (+10, +5). The resize-drag emits a
    // RESIZING configure with the new (4+10, 2+5) size.
    s.inject_pointer_motion(60, 55);
    let bytes = s.drain_client_events(c).unwrap();
    assert!(!bytes.is_empty(), "resize-drag must emit a configure");
    let header = display_proto::wire::MessageHeader::decode(&bytes).unwrap();
    assert_eq!(header.opcode, 1 /* configure */);
    let p = &bytes[PROTO_HEADER_SIZE..header.length as usize];
    let configure = XdgToplevelConfigure::decode(p).unwrap();
    assert_eq!(configure.width, 14);
    assert_eq!(configure.height, 7);
    assert_eq!(configure.states, xdg_toplevel_state::RESIZING);

    // Release ends the drag + emits a final configure with
    // states = 0.
    s.inject_pointer_button(1, display_proto::events::pointer_button_state::RELEASED);
    assert!(!s.is_dragging());
    let bytes = s.drain_client_events(c).unwrap();
    let header = display_proto::wire::MessageHeader::decode(&bytes).unwrap();
    let p = &bytes[PROTO_HEADER_SIZE..header.length as usize];
    let configure = XdgToplevelConfigure::decode(p).unwrap();
    assert_eq!(configure.states, 0);
}

#[test]
fn taskbar_height_reservation_subtracts_from_work_area_height() {
    let mut s = Server::with_framebuffer_size(800, 600);
    assert_eq!(s.work_area_width(), 800);
    assert_eq!(s.work_area_height(), 600);
    s.set_taskbar_height_px(40);
    assert_eq!(s.work_area_width(), 800);
    assert_eq!(s.work_area_height(), 560);
    s.set_taskbar_height_px(0);
    assert_eq!(s.work_area_height(), 600);
}

#[test]
fn drain_mouse_events_routes_motion_to_inject_pointer_motion() {
    use display_server::{drain_mouse_events_into, mouse_event_kind, MOUSE_EVENT_SIZE};
    let mut s = Server::with_framebuffer_size(800, 600);
    let _c = s.accept();
    // Build two motion events back-to-back.
    let mut bytes = Vec::new();
    for (x, y) in [(100i32, 50i32), (250, 175)] {
        let mut event = [0u8; MOUSE_EVENT_SIZE];
        event[0..4].copy_from_slice(&mouse_event_kind::MOTION.to_le_bytes());
        event[4..8].copy_from_slice(&x.to_le_bytes());
        event[8..12].copy_from_slice(&y.to_le_bytes());
        bytes.extend_from_slice(&event);
    }
    let n = drain_mouse_events_into(&bytes, &mut s);
    assert_eq!(n, 2);
    // Final pointer position is the last motion's coordinates.
    assert_eq!(s.pointer_position(), (250, 175));
}

#[test]
fn drain_mouse_events_routes_button_event_through_inject_pointer_button() {
    use display_server::{drain_mouse_events_into, mouse_event_kind, mouse_button_state, MOUSE_EVENT_SIZE};
    use display_server::ids::ObjectId;
    let mut s = Server::with_framebuffer_size(800, 600);
    let c = s.accept();
    // Set up a toplevel + buffer so hit_test has a window to find.
    let registry_id = ObjectId::new(3);
    let compositor_id = ObjectId::new(5);
    let shm_id = ObjectId::new(7);
    let xdg_shell_id = ObjectId::new(9);
    let surface_id = ObjectId::new(11);
    let pool_id = ObjectId::new(13);
    let buffer_id = ObjectId::new(15);
    let toplevel_id = ObjectId::new(17);
    s.dispatch_request(c, &encode_request_bytes(ObjectId::DISPLAY, 2, &registry_id.raw().to_le_bytes())).unwrap();
    for (name, iface, bound) in [(1u32, "pmd_compositor", compositor_id), (2, "pmd_shm", shm_id), (3, "pmd_xdg_shell", xdg_shell_id)] {
        let payload = registry_bind_payload(name, iface, 1, bound);
        s.dispatch_request(c, &encode_request_bytes(registry_id, 1, &payload)).unwrap();
    }
    s.dispatch_request(c, &encode_request_bytes(compositor_id, 1, &surface_id.raw().to_le_bytes())).unwrap();
    s.dispatch_request(c, &encode_request_bytes(shm_id, 1, &shm_create_pool_payload(pool_id, 16))).unwrap();
    s.dispatch_request(c, &encode_request_bytes(pool_id, 1, &shm_pool_create_buffer_payload(buffer_id, 0, 2, 2, 8, 0))).unwrap();
    s.dispatch_request(c, &encode_request_bytes(xdg_shell_id, 1, &xdg_get_toplevel_payload(toplevel_id, surface_id))).unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface_id, 2, &surface_attach_payload(buffer_id, 0, 0))).unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface_id, 7, &[])).unwrap();

    // Build a motion + press sequence.
    let mut bytes = Vec::new();
    for (kind, x, y, button, state) in [
        (mouse_event_kind::MOTION, 1i32, 1i32, 0u32, 0u32),
        (mouse_event_kind::BUTTON, 1, 1, 1, mouse_button_state::PRESSED),
    ] {
        let mut event = [0u8; MOUSE_EVENT_SIZE];
        event[0..4].copy_from_slice(&kind.to_le_bytes());
        event[4..8].copy_from_slice(&x.to_le_bytes());
        event[8..12].copy_from_slice(&y.to_le_bytes());
        event[12..16].copy_from_slice(&button.to_le_bytes());
        event[16..20].copy_from_slice(&state.to_le_bytes());
        bytes.extend_from_slice(&event);
    }
    let n = drain_mouse_events_into(&bytes, &mut s);
    assert_eq!(n, 2);
    // The press inside the toplevel sets keyboard focus to that surface.
    assert_eq!(s.keyboard_focus(), Some((c, surface_id)));
}

#[test]
fn drain_mouse_events_drives_active_drag_advance() {
    // Full T133 round-trip through the input path: a client
    // sends move(serial=1), the server starts a drag, then a
    // packed motion event from /dev/input/mouse arrives and
    // drives the drag. The toplevel's origin updates.
    use display_server::{drain_mouse_events_into, mouse_event_kind, MOUSE_EVENT_SIZE};
    use display_server::ids::ObjectId;
    let mut s = Server::with_framebuffer_size(800, 600);
    let c = s.accept();
    let registry_id = ObjectId::new(3);
    let compositor_id = ObjectId::new(5);
    let xdg_shell_id = ObjectId::new(7);
    let surface_id = ObjectId::new(9);
    let toplevel_id = ObjectId::new(11);
    s.dispatch_request(c, &encode_request_bytes(ObjectId::DISPLAY, 2, &registry_id.raw().to_le_bytes())).unwrap();
    for (name, iface, bound) in [(1u32, "pmd_compositor", compositor_id), (2, "pmd_xdg_shell", xdg_shell_id)] {
        let payload = registry_bind_payload(name, iface, 1, bound);
        s.dispatch_request(c, &encode_request_bytes(registry_id, 1, &payload)).unwrap();
    }
    s.dispatch_request(c, &encode_request_bytes(compositor_id, 1, &surface_id.raw().to_le_bytes())).unwrap();
    s.dispatch_request(c, &encode_request_bytes(xdg_shell_id, 1, &xdg_get_toplevel_payload(toplevel_id, surface_id))).unwrap();

    // Plant pointer + send move(serial=1) — the dispatch fires
    // start_drag_from_request which captures the pointer + origin.
    let initial_origin = (s.client(c).unwrap().toplevel(toplevel_id).unwrap().x,
                          s.client(c).unwrap().toplevel(toplevel_id).unwrap().y);
    s.inject_pointer_motion(100, 100);
    let mut payload = Vec::new();
    payload.extend_from_slice(&1u32.to_le_bytes());
    s.dispatch_request(c, &encode_request_bytes(toplevel_id, 7 /* move */, &payload)).unwrap();
    assert!(s.is_dragging());

    // Mouse motion event from the /dev/input/mouse ring.
    let mut event = [0u8; MOUSE_EVENT_SIZE];
    event[0..4].copy_from_slice(&mouse_event_kind::MOTION.to_le_bytes());
    event[4..8].copy_from_slice(&140i32.to_le_bytes());
    event[8..12].copy_from_slice(&130i32.to_le_bytes());
    let n = drain_mouse_events_into(&event, &mut s);
    assert_eq!(n, 1);
    let new_origin = (s.client(c).unwrap().toplevel(toplevel_id).unwrap().x,
                      s.client(c).unwrap().toplevel(toplevel_id).unwrap().y);
    assert_eq!(new_origin.0, initial_origin.0 + 40);
    assert_eq!(new_origin.1, initial_origin.1 + 30);
}

#[test]
fn drain_kbd_events_routes_keys_to_focused_surface() {
    use display_server::{drain_kbd_events_into, kbd_key_state, KBD_EVENT_SIZE};
    use display_server::ids::ObjectId;
    let mut s = Server::with_framebuffer_size(800, 600);
    let c = s.accept();
    // Set up a toplevel with a buffer + bind keyboard so we
    // can observe the routed key event.
    let registry_id = ObjectId::new(3);
    let compositor_id = ObjectId::new(5);
    let shm_id = ObjectId::new(7);
    let xdg_shell_id = ObjectId::new(9);
    let seat_id = ObjectId::new(11);
    let surface_id = ObjectId::new(13);
    let pool_id = ObjectId::new(15);
    let buffer_id = ObjectId::new(17);
    let toplevel_id = ObjectId::new(19);
    let kbd_object_id = ObjectId::new(21);
    s.dispatch_request(c, &encode_request_bytes(ObjectId::DISPLAY, 2, &registry_id.raw().to_le_bytes())).unwrap();
    for (name, iface, bound) in [
        (1u32, "pmd_compositor", compositor_id),
        (2, "pmd_shm", shm_id),
        (3, "pmd_xdg_shell", xdg_shell_id),
        (4, "pmd_seat", seat_id),
    ] {
        let payload = registry_bind_payload(name, iface, 1, bound);
        s.dispatch_request(c, &encode_request_bytes(registry_id, 1, &payload)).unwrap();
    }
    s.dispatch_request(c, &encode_request_bytes(compositor_id, 1, &surface_id.raw().to_le_bytes())).unwrap();
    s.dispatch_request(c, &encode_request_bytes(shm_id, 1, &shm_create_pool_payload(pool_id, 16))).unwrap();
    s.dispatch_request(c, &encode_request_bytes(pool_id, 1, &shm_pool_create_buffer_payload(buffer_id, 0, 2, 2, 8, 0))).unwrap();
    s.dispatch_request(c, &encode_request_bytes(xdg_shell_id, 1, &xdg_get_toplevel_payload(toplevel_id, surface_id))).unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface_id, 2, &surface_attach_payload(buffer_id, 0, 0))).unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface_id, 7, &[])).unwrap();
    // get_keyboard payload is just the new_id.
    s.dispatch_request(c, &encode_request_bytes(seat_id, 2 /* get_keyboard */, &kbd_object_id.raw().to_le_bytes())).unwrap();
    s.set_keyboard_focus(Some((c, surface_id)));

    // Build a key-press event.
    let mut event = [0u8; KBD_EVENT_SIZE];
    event[0..4].copy_from_slice(&65u32.to_le_bytes()); // 'A'
    event[4..8].copy_from_slice(&kbd_key_state::PRESSED.to_le_bytes());
    let n = drain_kbd_events_into(&event, &mut s);
    assert_eq!(n, 1);
    // The injected key reaches the focused client's keyboard
    // event queue. Drain client events and look for the key.
    let bytes = s.drain_client_events(c).unwrap_or_default();
    assert!(!bytes.is_empty(), "key inject must emit a keyboard.key event");
}

#[test]
fn decode_mouse_event_rejects_unknown_kind() {
    use display_server::{decode_mouse_event, MOUSE_EVENT_SIZE};
    let mut event = [0u8; MOUSE_EVENT_SIZE];
    event[0..4].copy_from_slice(&999u32.to_le_bytes());
    assert!(decode_mouse_event(&event).is_none());
}

#[test]
fn decode_mouse_event_rejects_short_buffer() {
    use display_server::decode_mouse_event;
    let event = [0u8; 10];
    assert!(decode_mouse_event(&event).is_none());
}

#[test]
fn decode_kbd_event_rejects_unknown_state() {
    use display_server::{decode_kbd_event, KBD_EVENT_SIZE};
    let mut event = [0u8; KBD_EVENT_SIZE];
    event[0..4].copy_from_slice(&65u32.to_le_bytes());
    event[4..8].copy_from_slice(&999u32.to_le_bytes());
    assert!(decode_kbd_event(&event).is_none());
}
