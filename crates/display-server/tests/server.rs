//! Top-level `Server` tests.

use display_server::client::ClientError;
use display_server::ids::ObjectId;
use display_server::objects::Interface;
use display_server::server::{Server, ServerError};
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
