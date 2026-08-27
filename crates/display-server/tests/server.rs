//! Top-level `Server` tests.

use display_proto::events::{
    key_state, pointer_button_state, KeyboardKey, PointerButton, PointerMotion, ShellCloseShortcut,
    ShellWindowFocused,
};
use display_proto::wire::MessageHeader as ProtoHeader;
use display_server::client::ClientError;
use display_server::ids::ObjectId;
use display_server::objects::Interface;
use display_server::server::{
    HitResult, OutputDamageRect, PresentationDamage, Server, ServerError,
};
use display_server::wire::{MessageHeader, WireError, HEADER_SIZE};
use preferences::KeyboardLayout;

/// Build a framed message with the supplied payload.
fn encode_request_bytes(object_id: ObjectId, opcode: u16, payload: &[u8]) -> Vec<u8> {
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
    out.extend(core::iter::repeat_n(0u8, pad));
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

fn surface_patch_payload(x: i32, y: i32, width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + pixels.len());
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(pixels);
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
        &encode_request_bytes(ObjectId::DISPLAY, 2, &registry_id.raw().to_le_bytes()),
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
        &encode_request_bytes(ObjectId::DISPLAY, 2, &registry_id.raw().to_le_bytes()),
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
        &encode_request_bytes(xdg_shell_id, 1, &xdg_get_toplevel_payload(top_a, surface_a)),
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
        &encode_request_bytes(xdg_shell_id, 1, &xdg_get_toplevel_payload(top_b, surface_b)),
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
    assert_eq!(fb.pixel(step, step).unwrap(), &[0x00, 0xff, 0x00, 0xff]);
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
        &encode_request_bytes(ObjectId::DISPLAY, 2, &registry_id.raw().to_le_bytes()),
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

fn bind_active_shortcut_shell(
    server: &mut Server,
) -> (display_server::ClientId, ObjectId, ObjectId) {
    bind_active_shortcut_shell_version(server, 2)
}

fn bind_active_shortcut_shell_version(
    server: &mut Server,
    version: u32,
) -> (display_server::ClientId, ObjectId, ObjectId) {
    use abi::cap::{Cap, CapSet};

    let shell = server.accept_with_caps(CapSet::from_caps(&[Cap::Shell]));
    let registry = ObjectId::new(3);
    let manager = ObjectId::new(5);
    server
        .dispatch_request(
            shell,
            &encode_request_bytes(ObjectId::DISPLAY, 2, &registry.raw().to_le_bytes()),
        )
        .unwrap();
    server
        .dispatch_request(
            shell,
            &encode_request_bytes(
                registry,
                1,
                &registry_bind_payload(5, "pmd_shell_manager", version, manager),
            ),
        )
        .unwrap();
    server
        .dispatch_request(
            shell,
            &encode_request_bytes(manager, 6, &32_u32.to_le_bytes()),
        )
        .unwrap();
    let _ = server.drain_client_events(shell);
    (shell, registry, manager)
}

fn keyboard_events(bytes: &[u8], keyboard: ObjectId) -> Vec<KeyboardKey> {
    let mut decoded = Vec::new();
    let mut remaining = bytes;
    while !remaining.is_empty() {
        let header = ProtoHeader::decode(remaining).expect("valid event header");
        let length = header.length as usize;
        assert!(length <= remaining.len(), "complete event frame");
        if header.object_id == keyboard && header.opcode == 1 {
            decoded.push(
                KeyboardKey::decode(&remaining[HEADER_SIZE..length])
                    .expect("keyboard event payload"),
            );
        }
        remaining = &remaining[length..];
    }
    decoded
}

fn close_shortcut_events(bytes: &[u8], shell_manager: ObjectId) -> Vec<ShellCloseShortcut> {
    let mut decoded = Vec::new();
    let mut remaining = bytes;
    while !remaining.is_empty() {
        let header = ProtoHeader::decode(remaining).expect("valid event header");
        let length = header.length as usize;
        assert!(length <= remaining.len(), "complete event frame");
        if header.object_id == shell_manager && header.opcode == 9 {
            decoded.push(
                ShellCloseShortcut::decode(&remaining[HEADER_SIZE..length])
                    .expect("close shortcut payload"),
            );
        }
        remaining = &remaining[length..];
    }
    decoded
}

#[test]
fn patch_current_publishes_one_atomic_scene_mutation_without_release() {
    let (mut s, c, compositor_id, shm_id, _, _) = boot_server_and_bind_everything();
    let surface_id = ObjectId::new(13);
    let pool_id = ObjectId::new(15);
    let buffer_id = ObjectId::new(17);
    make_surface_with_buffer(
        &mut s,
        c,
        compositor_id,
        shm_id,
        surface_id,
        pool_id,
        buffer_id,
    );

    let red = [0, 0, 0xff, 0xff];
    let mut write = Vec::with_capacity(20);
    write.extend_from_slice(&0u32.to_le_bytes());
    for _ in 0..4 {
        write.extend_from_slice(&red);
    }
    s.dispatch_request(c, &encode_request_bytes(pool_id, 4, &write))
        .unwrap();
    s.dispatch_request(
        c,
        &encode_request_bytes(surface_id, 2, &surface_attach_payload(buffer_id, 0, 0)),
    )
    .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface_id, 7, &[]))
        .unwrap();
    assert!(s.drain_client_events(c).unwrap().is_empty());
    assert_eq!(s.framebuffer().pixel(0, 0).unwrap(), &red);
    assert_eq!(s.framebuffer().pixel(1, 0).unwrap(), &red);
    s.clear_presentation_damage();

    let generation = s.scene_generation();
    let serial = s.recomposition_serial();
    let pool_before = s.client(c).unwrap().pool_bytes(pool_id).unwrap().to_vec();
    let green = [0, 0xff, 0, 0xff];
    let mut forbidden_write = Vec::with_capacity(8);
    forbidden_write.extend_from_slice(&0u32.to_le_bytes());
    forbidden_write.extend_from_slice(&green);
    assert!(matches!(
        s.dispatch_request(c, &encode_request_bytes(pool_id, 4, &forbidden_write),)
            .unwrap_err(),
        ServerError::Client(ClientError::PoolWriteIntersectsCurrentBuffer { .. })
    ));
    assert_eq!(s.scene_generation(), generation);
    assert_eq!(s.recomposition_serial(), serial);
    assert_eq!(
        s.client(c).unwrap().pool_bytes(pool_id).unwrap(),
        pool_before
    );
    assert_eq!(s.framebuffer().pixel(0, 0).unwrap(), &red);

    let blue = [0xff, 0, 0, 0xff];
    let patch = surface_patch_payload(1, 0, 1, 1, &blue);
    s.dispatch_request(c, &encode_request_bytes(surface_id, 8, &patch))
        .unwrap();
    assert_eq!(
        s.presentation_damage(),
        &PresentationDamage::Bounded(vec![OutputDamageRect {
            x: 1,
            y: 0,
            width: 1,
            height: 1,
        }]),
    );
    assert_eq!(s.recomposition_serial(), serial + 1);
    assert_eq!(s.scene_generation(), generation + 1);
    assert_eq!(s.framebuffer().pixel(0, 0).unwrap(), &red);
    assert_eq!(s.framebuffer().pixel(1, 0).unwrap(), &blue);
    assert_eq!(
        s.client(c)
            .unwrap()
            .surface(surface_id)
            .unwrap()
            .commit_count,
        2
    );
    assert!(s.drain_client_events(c).unwrap().is_empty());
}

#[test]
fn same_geometry_commit_uses_its_complete_declared_damage() {
    let (mut s, c, compositor, shm, xdg_shell, _) = boot_server_and_bind_everything();
    let surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let buffer = ObjectId::new(17);
    let toplevel = ObjectId::new(19);
    make_surface_with_buffer(&mut s, c, compositor, shm, surface, pool, buffer);
    promote_to_toplevel_and_commit_red(&mut s, c, xdg_shell, surface, pool, buffer, toplevel);
    s.clear_presentation_damage();

    let mut damage = Vec::new();
    for value in [0i32, 0, 1, 2] {
        damage.extend_from_slice(&value.to_le_bytes());
    }
    s.dispatch_request(c, &encode_request_bytes(surface, 3, &damage))
        .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface, 7, &[]))
        .unwrap();

    assert_eq!(
        s.presentation_damage(),
        &PresentationDamage::Bounded(vec![OutputDamageRect {
            x: 0,
            y: 0,
            width: 1,
            height: 2,
        }]),
    );
}

#[test]
fn same_geometry_alternate_buffer_swap_uses_declared_damage() {
    let (mut s, c, compositor, shm, xdg_shell, _) = boot_server_and_bind_everything();
    let surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let first = ObjectId::new(17);
    let toplevel = ObjectId::new(19);
    let alternate = ObjectId::new(21);
    make_surface_with_buffer(&mut s, c, compositor, shm, surface, pool, first);
    s.dispatch_request(c, &encode_request_bytes(pool, 2, &32u32.to_le_bytes()))
        .unwrap();
    s.dispatch_request(
        c,
        &encode_request_bytes(
            pool,
            1,
            &shm_pool_create_buffer_payload(alternate, 16, 2, 2, 8, 0),
        ),
    )
    .unwrap();
    promote_to_toplevel_and_commit_red(&mut s, c, xdg_shell, surface, pool, first, toplevel);

    let red = [0, 0, 0xff, 0xff];
    let green = [0, 0xff, 0, 0xff];
    let mut write = Vec::new();
    write.extend_from_slice(&16u32.to_le_bytes());
    write.extend_from_slice(&green);
    for _ in 0..3 {
        write.extend_from_slice(&red);
    }
    s.dispatch_request(c, &encode_request_bytes(pool, 4, &write))
        .unwrap();
    s.clear_presentation_damage();
    s.dispatch_request(
        c,
        &encode_request_bytes(surface, 2, &surface_attach_payload(alternate, 0, 0)),
    )
    .unwrap();
    let mut damage = Vec::new();
    for value in [0i32, 0, 1, 1] {
        damage.extend_from_slice(&value.to_le_bytes());
    }
    s.dispatch_request(c, &encode_request_bytes(surface, 3, &damage))
        .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface, 7, &[]))
        .unwrap();

    assert_eq!(s.framebuffer().pixel(0, 0).unwrap(), &green);
    assert_eq!(s.framebuffer().pixel(1, 1).unwrap(), &red);
    assert_eq!(
        s.presentation_damage(),
        &PresentationDamage::Bounded(vec![OutputDamageRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        }]),
    );
}

#[test]
fn omitted_or_invalid_commit_damage_falls_back_to_full_output() {
    let (mut s, c, compositor, shm, xdg_shell, _) = boot_server_and_bind_everything();
    let surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let buffer = ObjectId::new(17);
    let toplevel = ObjectId::new(19);
    make_surface_with_buffer(&mut s, c, compositor, shm, surface, pool, buffer);
    promote_to_toplevel_and_commit_red(&mut s, c, xdg_shell, surface, pool, buffer, toplevel);

    s.clear_presentation_damage();
    s.dispatch_request(c, &encode_request_bytes(surface, 7, &[]))
        .unwrap();
    assert_eq!(s.presentation_damage(), &PresentationDamage::Full);

    s.clear_presentation_damage();
    s.dispatch_request(c, &encode_request_bytes(surface, 3, &[0; 16]))
        .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface, 7, &[]))
        .unwrap();
    assert_eq!(s.presentation_damage(), &PresentationDamage::Full);
}

#[test]
fn invalid_overflow_damage_cannot_be_normalized_into_a_bounded_hint() {
    let (mut s, c, compositor, shm, xdg_shell, _) = boot_server_and_bind_everything();
    let surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let buffer = ObjectId::new(17);
    let toplevel = ObjectId::new(19);
    make_surface_with_buffer(&mut s, c, compositor, shm, surface, pool, buffer);
    promote_to_toplevel_and_commit_red(&mut s, c, xdg_shell, surface, pool, buffer, toplevel);
    s.clear_presentation_damage();

    let mut valid = Vec::new();
    for value in [0i32, 0, 1, 1] {
        valid.extend_from_slice(&value.to_le_bytes());
    }
    for _ in 0..display_server::MAX_SURFACE_DAMAGE_RECTS {
        s.dispatch_request(c, &encode_request_bytes(surface, 3, &valid))
            .unwrap();
    }
    // Coalescing signed extents would normalize this to the valid [0, 1)
    // interval. The raw invalidity must survive that bounded storage step.
    let mut invalid = Vec::new();
    for value in [1i32, 0, -1, 1] {
        invalid.extend_from_slice(&value.to_le_bytes());
    }
    s.dispatch_request(c, &encode_request_bytes(surface, 3, &invalid))
        .unwrap();
    let pending = s.client(c).unwrap().surface(surface).unwrap();
    assert_eq!(pending.pending_damage.len(), 1);
    assert!(pending.pending_damage_unprovable);

    s.dispatch_request(c, &encode_request_bytes(surface, 7, &[]))
        .unwrap();
    assert_eq!(s.presentation_damage(), &PresentationDamage::Full);
}

#[test]
fn attachment_origin_change_ignores_declared_damage_and_falls_back() {
    let (mut s, c, compositor, shm, xdg_shell, _) = boot_server_and_bind_everything();
    let surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let buffer = ObjectId::new(17);
    let toplevel = ObjectId::new(19);
    make_surface_with_buffer(&mut s, c, compositor, shm, surface, pool, buffer);
    promote_to_toplevel_and_commit_red(&mut s, c, xdg_shell, surface, pool, buffer, toplevel);
    s.clear_presentation_damage();

    s.dispatch_request(
        c,
        &encode_request_bytes(surface, 2, &surface_attach_payload(buffer, 1, 0)),
    )
    .unwrap();
    let mut damage = Vec::new();
    for value in [0i32, 0, 1, 1] {
        damage.extend_from_slice(&value.to_le_bytes());
    }
    s.dispatch_request(c, &encode_request_bytes(surface, 3, &damage))
        .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface, 7, &[]))
        .unwrap();

    assert_eq!(s.presentation_damage(), &PresentationDamage::Full);
}

#[test]
fn patch_current_damage_is_clipped_to_the_output() {
    let (mut s, c, compositor, shm, _, _) = boot_server_and_bind_everything();
    let surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let buffer = ObjectId::new(17);
    make_surface_with_buffer(&mut s, c, compositor, shm, surface, pool, buffer);
    s.dispatch_request(
        c,
        &encode_request_bytes(surface, 2, &surface_attach_payload(buffer, 63, 63)),
    )
    .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface, 7, &[]))
        .unwrap();
    s.clear_presentation_damage();

    let patch = surface_patch_payload(0, 0, 2, 2, &[7; 16]);
    s.dispatch_request(c, &encode_request_bytes(surface, 8, &patch))
        .unwrap();
    assert_eq!(
        s.presentation_damage(),
        &PresentationDamage::Bounded(vec![OutputDamageRect {
            x: 63,
            y: 63,
            width: 1,
            height: 1,
        }]),
    );
}

#[test]
fn adjacent_exact_patches_union_without_becoming_full_output() {
    let (mut s, c, compositor, shm, _, _) = boot_server_and_bind_everything();
    let surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let buffer = ObjectId::new(17);
    make_surface_with_buffer(&mut s, c, compositor, shm, surface, pool, buffer);
    s.dispatch_request(
        c,
        &encode_request_bytes(surface, 2, &surface_attach_payload(buffer, 0, 0)),
    )
    .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface, 7, &[]))
        .unwrap();
    s.clear_presentation_damage();

    for (x, value) in [(0, 1u8), (1, 2u8)] {
        let patch = surface_patch_payload(x, 0, 1, 1, &[value; 4]);
        s.dispatch_request(c, &encode_request_bytes(surface, 8, &patch))
            .unwrap();
    }
    assert_eq!(
        s.presentation_damage(),
        &PresentationDamage::Bounded(vec![OutputDamageRect {
            x: 0,
            y: 0,
            width: 2,
            height: 1,
        }]),
    );
}

#[test]
fn hidden_surface_patch_stays_invisible_and_uses_the_safe_fallback() {
    let (mut s, c, compositor, shm, xdg_shell, _) = boot_server_and_bind_everything();
    let surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let buffer = ObjectId::new(17);
    let toplevel = ObjectId::new(19);
    make_surface_with_buffer(&mut s, c, compositor, shm, surface, pool, buffer);
    promote_to_toplevel_and_commit_red(&mut s, c, xdg_shell, surface, pool, buffer, toplevel);
    let window = s.window_id(c, toplevel).unwrap();
    assert!(s.set_window_minimized(window, true));
    assert_eq!(s.framebuffer().pixel(0, 0).unwrap(), &[0, 0, 0, 0]);
    s.clear_presentation_damage();

    let patch = surface_patch_payload(0, 0, 1, 1, &[0, 0xff, 0, 0xff]);
    s.dispatch_request(c, &encode_request_bytes(surface, 8, &patch))
        .unwrap();
    assert_eq!(s.framebuffer().pixel(0, 0).unwrap(), &[0, 0, 0, 0]);
    assert_eq!(s.presentation_damage(), &PresentationDamage::Full);
}

#[test]
fn occluded_patch_candidate_may_compare_equal_without_exposing_lower_pixels() {
    let (mut s, c, compositor, shm, xdg_shell, _) = boot_server_and_bind_everything();
    let lower_surface = ObjectId::new(13);
    let lower_pool = ObjectId::new(15);
    let lower_buffer = ObjectId::new(17);
    let lower_toplevel = ObjectId::new(19);
    make_surface_with_buffer(
        &mut s,
        c,
        compositor,
        shm,
        lower_surface,
        lower_pool,
        lower_buffer,
    );
    promote_to_toplevel_and_commit_red(
        &mut s,
        c,
        xdg_shell,
        lower_surface,
        lower_pool,
        lower_buffer,
        lower_toplevel,
    );
    let upper_surface = ObjectId::new(21);
    let upper_pool = ObjectId::new(23);
    let upper_buffer = ObjectId::new(25);
    let upper_toplevel = ObjectId::new(27);
    make_surface_with_buffer(
        &mut s,
        c,
        compositor,
        shm,
        upper_surface,
        upper_pool,
        upper_buffer,
    );
    promote_to_toplevel_and_commit_red(
        &mut s,
        c,
        xdg_shell,
        upper_surface,
        upper_pool,
        upper_buffer,
        upper_toplevel,
    );
    {
        let upper = s
            .client_mut(c)
            .unwrap()
            .toplevels
            .get_mut(&upper_toplevel)
            .unwrap();
        upper.x = 0;
        upper.y = 0;
    }
    s.dispatch_request(c, &encode_request_bytes(upper_surface, 7, &[]))
        .unwrap();
    let blue = [0xff, 0, 0, 0xff];
    s.dispatch_request(
        c,
        &encode_request_bytes(
            upper_surface,
            8,
            &surface_patch_payload(0, 0, 2, 2, &blue.repeat(4)),
        ),
    )
    .unwrap();
    assert_eq!(s.framebuffer().pixel(0, 0).unwrap(), &blue);
    s.clear_presentation_damage();

    let green = [0, 0xff, 0, 0xff];
    s.dispatch_request(
        c,
        &encode_request_bytes(lower_surface, 8, &surface_patch_payload(0, 0, 1, 1, &green)),
    )
    .unwrap();
    assert_eq!(s.framebuffer().pixel(0, 0).unwrap(), &blue);
    assert_eq!(
        s.presentation_damage(),
        &PresentationDamage::Bounded(vec![OutputDamageRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        }]),
    );
}

#[test]
fn toplevel_role_loss_releases_its_live_current_buffer_once() {
    let (mut s, c, compositor_id, shm_id, xdg_shell_id, _) = boot_server_and_bind_everything();
    let surface_id = ObjectId::new(13);
    let pool_id = ObjectId::new(15);
    let buffer_id = ObjectId::new(17);
    let toplevel_id = ObjectId::new(19);
    make_surface_with_buffer(
        &mut s,
        c,
        compositor_id,
        shm_id,
        surface_id,
        pool_id,
        buffer_id,
    );
    promote_to_toplevel_and_commit_red(
        &mut s,
        c,
        xdg_shell_id,
        surface_id,
        pool_id,
        buffer_id,
        toplevel_id,
    );
    s.drain_client_events(c).unwrap();

    s.dispatch_request(c, &encode_request_bytes(toplevel_id, 3, &[]))
        .unwrap();
    let events = s.drain_client_events(c).unwrap();
    let mut offset = 0;
    let mut releases = 0;
    while offset < events.len() {
        let header = MessageHeader::decode(&events[offset..]).unwrap();
        if header.object_id == buffer_id && header.opcode == 1 {
            releases += 1;
        }
        offset += header.length as usize;
    }
    assert_eq!(releases, 1);
    assert!(s
        .client(c)
        .unwrap()
        .surface(surface_id)
        .unwrap()
        .current_buffer
        .is_none());
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
    make_surface_with_buffer(&mut s, c, compositor_id, shm_id, surface, pool, buffer);
    promote_to_toplevel_and_commit_red(&mut s, c, xdg_shell_id, surface, pool, buffer, toplevel);

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
    make_surface_with_buffer(&mut s, c, compositor_id, shm_id, surface, pool, buffer);
    promote_to_toplevel_and_commit_red(&mut s, c, xdg_shell_id, surface, pool, buffer, toplevel);
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
fn focus_and_click_raise_the_explicit_global_z_order_and_recompose_overlap() {
    let (mut s, c, compositor_id, shm_id, xdg_shell_id, _seat_id) =
        boot_server_and_bind_everything();

    let surface_a = ObjectId::new(13);
    let pool_a = ObjectId::new(15);
    let buffer_a = ObjectId::new(17);
    let top_a = ObjectId::new(19);
    make_surface_with_buffer(
        &mut s,
        c,
        compositor_id,
        shm_id,
        surface_a,
        pool_a,
        buffer_a,
    );
    promote_to_toplevel_and_commit_red(&mut s, c, xdg_shell_id, surface_a, pool_a, buffer_a, top_a);

    let surface_b = ObjectId::new(21);
    let pool_b = ObjectId::new(23);
    let buffer_b = ObjectId::new(25);
    let top_b = ObjectId::new(27);
    make_surface_with_buffer(
        &mut s,
        c,
        compositor_id,
        shm_id,
        surface_b,
        pool_b,
        buffer_b,
    );
    promote_to_toplevel_and_commit_red(&mut s, c, xdg_shell_id, surface_b, pool_b, buffer_b, top_b);
    for pixel in s
        .client_mut(c)
        .unwrap()
        .pool_bytes_mut(pool_b)
        .unwrap()
        .chunks_exact_mut(4)
    {
        pixel.copy_from_slice(&[0x00, 0xff, 0x00, 0xff]);
    }
    {
        let client = s.client_mut(c).unwrap();
        let top = client.toplevels.get_mut(&top_b).unwrap();
        top.x = 1;
        top.y = 0;
    }
    s.dispatch_request(c, &encode_request_bytes(surface_b, 7, &[]))
        .unwrap();

    let window_a = s.window_id(c, top_a).unwrap();
    let window_b = s.window_id(c, top_b).unwrap();
    assert_eq!(
        s.window_z_order()
            .iter()
            .map(|window| window.0)
            .collect::<Vec<_>>(),
        vec![window_a, window_b]
    );
    assert_eq!(
        s.framebuffer().pixel(1, 0).unwrap(),
        &[0x00, 0xff, 0x00, 0xff],
        "newer window starts on top"
    );

    s.set_keyboard_focus(Some((c, surface_a)));
    assert_eq!(
        s.window_z_order()
            .iter()
            .map(|window| window.0)
            .collect::<Vec<_>>(),
        vec![window_b, window_a]
    );
    assert_eq!(
        s.framebuffer().pixel(1, 0).unwrap(),
        &[0x00, 0x00, 0xff, 0xff],
        "focusing the lower window raises and redraws it"
    );

    // (2,0) belongs only to B. Clicking it must raise B, which
    // changes the colour in the overlap at (1,0).
    assert_eq!(s.inject_pointer_motion(2, 0).unwrap().surface_id, surface_b);
    let recomposition_before_click = s.recomposition_serial();
    assert_eq!(
        s.inject_pointer_button(1, pointer_button_state::PRESSED)
            .unwrap()
            .surface_id,
        surface_b
    );
    assert!(
        s.recomposition_serial() > recomposition_before_click,
        "a pointer focus that changes z-order must retain its direct scene-dirty signal",
    );
    assert_eq!(
        s.window_z_order()
            .iter()
            .map(|window| window.0)
            .collect::<Vec<_>>(),
        vec![window_a, window_b]
    );
    assert_eq!(
        s.framebuffer().pixel(1, 0).unwrap(),
        &[0x00, 0xff, 0x00, 0xff],
        "clicking raises and redraws the target"
    );
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
    make_surface_with_buffer(&mut s, c, compositor_id, shm_id, surface, pool, buffer);
    promote_to_toplevel_and_commit_red(&mut s, c, xdg_shell_id, surface, pool, buffer, toplevel);

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
    let (mut s, c, _compositor, _shm, _xdg, seat_id) = boot_server_and_bind_everything();
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
    make_surface_with_buffer(&mut s, c, compositor_id, shm_id, surface, pool, buffer);
    promote_to_toplevel_and_commit_red(&mut s, c, xdg_shell_id, surface, pool, buffer, toplevel);
    // First map conventionally focuses the new window. Clear it so
    // this test isolates the distinct click-to-focus transition.
    s.set_keyboard_focus(None);
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
            assert_eq!(event.serial, 1, "first routed button gets first serial");
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
fn focus_switch_configures_old_and_new_toplevels_with_composed_state() {
    use display_proto::events::XdgToplevelConfigure;
    use display_proto::xdg_toplevel_state;

    let (mut s, c, compositor_id, shm_id, xdg_shell_id, _) = boot_server_and_bind_everything();
    let surface_a = ObjectId::new(13);
    let pool_a = ObjectId::new(15);
    let buffer_a = ObjectId::new(17);
    let top_a = ObjectId::new(19);
    make_surface_with_buffer(
        &mut s,
        c,
        compositor_id,
        shm_id,
        surface_a,
        pool_a,
        buffer_a,
    );
    promote_to_toplevel_and_commit_red(&mut s, c, xdg_shell_id, surface_a, pool_a, buffer_a, top_a);

    let surface_b = ObjectId::new(21);
    let pool_b = ObjectId::new(23);
    let buffer_b = ObjectId::new(25);
    let top_b = ObjectId::new(27);
    make_surface_with_buffer(
        &mut s,
        c,
        compositor_id,
        shm_id,
        surface_b,
        pool_b,
        buffer_b,
    );
    promote_to_toplevel_and_commit_red(&mut s, c, xdg_shell_id, surface_b, pool_b, buffer_b, top_b);
    assert_eq!(s.keyboard_focus(), Some((c, surface_b)));
    let _ = s.drain_client_events(c);

    s.dispatch_request(c, &encode_request_bytes(top_b, 5 /* set_maximized */, &[]))
        .unwrap();
    let maximized_bytes = s.drain_client_events(c).unwrap();
    let maximized_header = ProtoHeader::decode(&maximized_bytes).unwrap();
    let maximized = XdgToplevelConfigure::decode(
        &maximized_bytes[display_proto::wire::HEADER_SIZE..maximized_header.length as usize],
    )
    .unwrap();
    assert_eq!((maximized.width, maximized.height), (64, 64));
    assert_eq!(
        maximized.states,
        xdg_toplevel_state::MAXIMIZED | xdg_toplevel_state::ACTIVATED
    );

    s.set_keyboard_focus(Some((c, surface_a)));
    let bytes = s.drain_client_events(c).unwrap();
    let mut remaining = bytes.as_slice();
    let mut configures = Vec::new();
    while !remaining.is_empty() {
        let header = ProtoHeader::decode(remaining).unwrap();
        let message_len = header.length as usize;
        let payload = &remaining[display_proto::wire::HEADER_SIZE..message_len];
        if header.opcode == 1 && (header.object_id == top_a || header.object_id == top_b) {
            configures.push((
                header.object_id,
                XdgToplevelConfigure::decode(payload).unwrap(),
            ));
        }
        remaining = &remaining[message_len..];
    }
    assert_eq!(configures.len(), 2);
    let old = configures
        .iter()
        .find(|(id, _)| *id == top_b)
        .map(|(_, configure)| configure)
        .unwrap();
    assert_eq!((old.width, old.height), (64, 64));
    assert_eq!(old.states, xdg_toplevel_state::MAXIMIZED);
    let new = configures
        .iter()
        .find(|(id, _)| *id == top_a)
        .map(|(_, configure)| configure)
        .unwrap();
    assert_eq!((new.width, new.height), (2, 2));
    assert_eq!(new.states, xdg_toplevel_state::ACTIVATED);
}

#[test]
fn minimizing_focused_toplevel_emits_activation_clear_configure() {
    use display_proto::events::XdgToplevelConfigure;

    let (mut s, c, compositor_id, shm_id, xdg_shell_id, _) = boot_server_and_bind_everything();
    let surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let buffer = ObjectId::new(17);
    let toplevel = ObjectId::new(19);
    make_surface_with_buffer(&mut s, c, compositor_id, shm_id, surface, pool, buffer);
    promote_to_toplevel_and_commit_red(&mut s, c, xdg_shell_id, surface, pool, buffer, toplevel);
    assert_eq!(s.keyboard_focus(), Some((c, surface)));
    let _ = s.drain_client_events(c);

    let window_id = s.window_id(c, toplevel).unwrap();
    assert!(s.set_window_minimized(window_id, true));
    assert_eq!(s.keyboard_focus(), None);
    let bytes = s.drain_client_events(c).unwrap();
    let header = ProtoHeader::decode(&bytes).unwrap();
    assert_eq!(header.object_id, toplevel);
    assert_eq!(header.opcode, 1);
    let configure = XdgToplevelConfigure::decode(
        &bytes[display_proto::wire::HEADER_SIZE..header.length as usize],
    )
    .unwrap();
    assert_eq!((configure.width, configure.height), (2, 2));
    assert_eq!(configure.states, 0);
}

#[test]
fn destroying_focused_toplevel_activates_topmost_survivor_before_removal() {
    use display_proto::events::XdgToplevelConfigure;
    use display_proto::xdg_toplevel_state;

    let (mut s, c, compositor_id, shm_id, xdg_shell_id, _) = boot_server_and_bind_everything();
    let surface_a = ObjectId::new(13);
    let pool_a = ObjectId::new(15);
    let buffer_a = ObjectId::new(17);
    let top_a = ObjectId::new(19);
    make_surface_with_buffer(
        &mut s,
        c,
        compositor_id,
        shm_id,
        surface_a,
        pool_a,
        buffer_a,
    );
    promote_to_toplevel_and_commit_red(&mut s, c, xdg_shell_id, surface_a, pool_a, buffer_a, top_a);
    let surface_b = ObjectId::new(21);
    let pool_b = ObjectId::new(23);
    let buffer_b = ObjectId::new(25);
    let top_b = ObjectId::new(27);
    make_surface_with_buffer(
        &mut s,
        c,
        compositor_id,
        shm_id,
        surface_b,
        pool_b,
        buffer_b,
    );
    promote_to_toplevel_and_commit_red(&mut s, c, xdg_shell_id, surface_b, pool_b, buffer_b, top_b);
    assert_eq!(s.keyboard_focus(), Some((c, surface_b)));
    let _ = s.drain_client_events(c);

    s.dispatch_request(c, &encode_request_bytes(top_b, 3 /* destroy */, &[]))
        .unwrap();
    assert_eq!(s.keyboard_focus(), Some((c, surface_a)));

    let bytes = s.drain_client_events(c).unwrap();
    let mut remaining = bytes.as_slice();
    let mut old_states = None;
    let mut new_states = None;
    while !remaining.is_empty() {
        let header = ProtoHeader::decode(remaining).unwrap();
        let message_len = header.length as usize;
        let payload = &remaining[display_proto::wire::HEADER_SIZE..message_len];
        if header.opcode == 1 && header.object_id == top_b {
            old_states = Some(XdgToplevelConfigure::decode(payload).unwrap().states);
        } else if header.opcode == 1 && header.object_id == top_a {
            new_states = Some(XdgToplevelConfigure::decode(payload).unwrap().states);
        }
        remaining = &remaining[message_len..];
    }
    assert_eq!(old_states, Some(0));
    assert_eq!(new_states, Some(xdg_toplevel_state::ACTIVATED));
}

#[test]
fn destroying_explicitly_focused_roleless_surface_activates_mapped_fallback() {
    use display_proto::events::XdgToplevelConfigure;
    use display_proto::xdg_toplevel_state;

    let (mut s, c, compositor_id, shm_id, xdg_shell_id, _) = boot_server_and_bind_everything();
    let mapped_surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let buffer = ObjectId::new(17);
    let toplevel = ObjectId::new(19);
    make_surface_with_buffer(
        &mut s,
        c,
        compositor_id,
        shm_id,
        mapped_surface,
        pool,
        buffer,
    );
    promote_to_toplevel_and_commit_red(
        &mut s,
        c,
        xdg_shell_id,
        mapped_surface,
        pool,
        buffer,
        toplevel,
    );
    let roleless_surface = ObjectId::new(21);
    s.dispatch_request(
        c,
        &encode_request_bytes(compositor_id, 1, &roleless_surface.raw().to_le_bytes()),
    )
    .unwrap();
    s.set_keyboard_focus(Some((c, roleless_surface)));
    let _ = s.drain_client_events(c);

    s.dispatch_request(
        c,
        &encode_request_bytes(roleless_surface, 1 /* destroy */, &[]),
    )
    .unwrap();
    assert_eq!(s.keyboard_focus(), Some((c, mapped_surface)));

    let bytes = s.drain_client_events(c).unwrap();
    let mut remaining = bytes.as_slice();
    let mut activated = None;
    while !remaining.is_empty() {
        let header = ProtoHeader::decode(remaining).unwrap();
        let message_len = header.length as usize;
        if header.object_id == toplevel && header.opcode == 1 {
            activated = Some(
                XdgToplevelConfigure::decode(
                    &remaining[display_proto::wire::HEADER_SIZE..message_len],
                )
                .unwrap()
                .states,
            );
        }
        remaining = &remaining[message_len..];
    }
    assert_eq!(activated, Some(xdg_toplevel_state::ACTIVATED));
}

#[test]
fn reserved_shell_focus_coalesces_target_raise_with_shell_commit() {
    use abi::cap::{Cap, CapSet};

    let (mut s, app, compositor_id, shm_id, xdg_shell_id, _seat_id) =
        boot_server_and_bind_everything();
    let app_surface = ObjectId::new(13);
    let app_pool = ObjectId::new(15);
    let app_buffer = ObjectId::new(17);
    let app_toplevel = ObjectId::new(19);
    make_surface_with_buffer(
        &mut s,
        app,
        compositor_id,
        shm_id,
        app_surface,
        app_pool,
        app_buffer,
    );
    promote_to_toplevel_and_commit_red(
        &mut s,
        app,
        xdg_shell_id,
        app_surface,
        app_pool,
        app_buffer,
        app_toplevel,
    );

    let shell = s.accept_with_caps(CapSet::from_caps(&[Cap::Shell]));
    let registry = ObjectId::new(3);
    let compositor = ObjectId::new(5);
    let shm = ObjectId::new(7);
    let xdg_shell = ObjectId::new(9);
    let seat = ObjectId::new(11);
    let shell_manager = ObjectId::new(21);
    s.dispatch_request(
        shell,
        &encode_request_bytes(ObjectId::DISPLAY, 2, &registry.raw().to_le_bytes()),
    )
    .unwrap();
    for (name, interface, object) in [
        (1, "pmd_compositor", compositor),
        (2, "pmd_shm", shm),
        (3, "pmd_xdg_shell", xdg_shell),
        (4, "pmd_seat", seat),
        (5, "pmd_shell_manager", shell_manager),
    ] {
        s.dispatch_request(
            shell,
            &encode_request_bytes(
                registry,
                1,
                &registry_bind_payload(name, interface, 1, object),
            ),
        )
        .unwrap();
    }
    let pointer = ObjectId::new(23);
    s.dispatch_request(
        shell,
        &encode_request_bytes(seat, 1, &new_id_payload(pointer)),
    )
    .unwrap();
    let shell_surface = ObjectId::new(13);
    let shell_pool = ObjectId::new(15);
    let shell_buffer = ObjectId::new(17);
    let shell_toplevel = ObjectId::new(19);
    make_surface_with_buffer(
        &mut s,
        shell,
        compositor,
        shm,
        shell_surface,
        shell_pool,
        shell_buffer,
    );
    promote_to_toplevel_and_commit_red(
        &mut s,
        shell,
        xdg_shell,
        shell_surface,
        shell_pool,
        shell_buffer,
        shell_toplevel,
    );
    s.dispatch_request(
        shell,
        &encode_request_bytes(shell_manager, 6, &63_u32.to_le_bytes()),
    )
    .unwrap();

    // Leave one shell-only pixel in the reserved strip while retaining an
    // overlap that proves the ordinary app stays above the shell.
    s.client_mut(app)
        .unwrap()
        .toplevels
        .get_mut(&app_toplevel)
        .unwrap()
        .x = 1;
    s.dispatch_request(app, &encode_request_bytes(app_surface, 7, &[]))
        .unwrap();
    s.set_keyboard_focus(Some((app, app_surface)));
    s.dispatch_request(shell, &encode_request_bytes(shell_manager, 1, &[]))
        .unwrap();
    let _ = s.drain_client_events(shell);
    let z_order_before = s.window_z_order().to_vec();
    let generation_before = s.scene_generation();
    assert_eq!(s.inject_pointer_motion(0, 1).unwrap().client_id, shell);
    let _ = s.drain_client_events(shell);

    let hit = s
        .inject_pointer_button(1, pointer_button_state::PRESSED)
        .expect("reserved strip hits shell");
    assert_eq!(hit.client_id, shell);
    assert_eq!(s.keyboard_focus(), Some((app, app_surface)));
    assert_eq!(s.window_z_order(), z_order_before.as_slice());
    assert_eq!(s.scene_generation(), generation_before);

    let bytes = s.drain_client_events(shell).unwrap();
    let header = ProtoHeader::decode(&bytes).unwrap();
    assert_eq!(header.length as usize, bytes.len());
    assert_eq!(header.object_id, pointer);
    assert_eq!(header.opcode, 2);
    let payload = &bytes[HEADER_SIZE..header.length as usize];
    let event = PointerButton::decode(payload).unwrap();
    assert_eq!(event.surface_id, shell_surface);
    assert_eq!(event.state, pointer_button_state::PRESSED);

    // The shell's focus request changes keyboard focus and logical z-order
    // immediately, but it must not compose a target-only intermediate frame.
    // The initiating shell's next surface commit publishes both the raise and
    // its matching taskbar/menu feedback as one transaction.
    let shell_window = s.window_id(shell, shell_toplevel).unwrap();
    let app_window = s.window_id(app, app_toplevel).unwrap();
    let before_focus_generation = s.scene_generation();
    s.clear_presentation_damage();
    s.begin_recomposition_batch();
    s.dispatch_request(
        shell,
        &encode_request_bytes(shell_manager, 2, &shell_window.to_le_bytes()),
    )
    .unwrap();
    assert_eq!(s.keyboard_focus(), Some((shell, shell_surface)));
    assert_eq!(s.window_z_order().last().unwrap().0, shell_window);
    assert_eq!(s.scene_generation(), before_focus_generation);
    assert!(s.presentation_deferred());
    let shell_origin = s
        .client(shell)
        .unwrap()
        .toplevels
        .get(&shell_toplevel)
        .map(|toplevel| (toplevel.x, toplevel.y))
        .unwrap();
    let raised_footprint = OutputDamageRect {
        x: shell_origin.0 as u32,
        y: shell_origin.1 as u32,
        width: 2,
        height: 2,
    };
    assert_eq!(
        s.presentation_damage(),
        &PresentationDamage::Bounded(vec![raised_footprint]),
    );

    let mut damage = Vec::new();
    for value in [0i32, 0, 1, 1] {
        damage.extend_from_slice(&value.to_le_bytes());
    }
    s.dispatch_request(shell, &encode_request_bytes(shell_surface, 3, &damage))
        .unwrap();
    s.dispatch_request(shell, &encode_request_bytes(shell_surface, 7, &[]))
        .unwrap();
    assert!(
        s.presentation_deferred(),
        "the shell commit clears the focus owner but remains unpresentable until the batch is composed",
    );
    assert_eq!(s.scene_generation(), before_focus_generation);
    assert_eq!(
        s.presentation_damage(),
        &PresentationDamage::Bounded(vec![raised_footprint]),
        "the exact shell damage stays inside the already-conservative raise footprint",
    );
    assert!(s.finish_recomposition_batch());
    assert!(!s.presentation_deferred());
    assert!(s.scene_generation() > before_focus_generation);

    // A later manager focus that did not originate in the reserved strip
    // retains the protocol's ordinary immediate-composition semantics.
    let before_ordinary_focus = s.scene_generation();
    s.dispatch_request(
        shell,
        &encode_request_bytes(shell_manager, 2, &app_window.to_le_bytes()),
    )
    .unwrap();
    assert!(!s.presentation_deferred());
    assert!(s.scene_generation() > before_ordinary_focus);
    assert_eq!(s.keyboard_focus(), Some((app, app_surface)));

    // If a replacement shell ever fails to commit, the next independent
    // press must materialize the deferred z-order and resume presentation.
    assert_eq!(s.inject_pointer_motion(0, 1).unwrap().client_id, shell);
    s.inject_pointer_button(1, pointer_button_state::PRESSED)
        .unwrap();
    s.dispatch_request(
        shell,
        &encode_request_bytes(shell_manager, 2, &shell_window.to_le_bytes()),
    )
    .unwrap();
    assert!(s.presentation_deferred());
    let before_cancel = s.scene_generation();
    assert_eq!(s.inject_pointer_motion(2, 0).unwrap().client_id, app);
    s.inject_pointer_button(1, pointer_button_state::PRESSED)
        .unwrap();
    assert!(!s.presentation_deferred());
    assert!(s.scene_generation() > before_cancel);
    assert_eq!(s.keyboard_focus(), Some((app, app_surface)));
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
    make_surface_with_buffer(&mut s, c, compositor_id, shm_id, surface, pool, buffer);
    promote_to_toplevel_and_commit_red(&mut s, c, xdg_shell_id, surface, pool, buffer, toplevel);

    // Explicit focus so we don't need to click first.
    s.set_keyboard_focus(Some((c, surface)));
    let _ = s.drain_client_events(c);

    let recomposition_before_key = s.recomposition_serial();
    let routed = s.inject_keyboard_key(0x1e, key_state::PRESSED).unwrap();
    assert_eq!(routed, (c, surface));
    assert_eq!(
        s.recomposition_serial(),
        recomposition_before_key,
        "routing a key event must not manufacture framebuffer damage",
    );

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
fn alt_f4_emits_one_exact_shell_shortcut_and_consumes_the_f4_pair() {
    let (mut server, app, compositor, shm, xdg_shell, seat) = boot_server_and_bind_everything();
    let keyboard = ObjectId::new(33);
    server
        .dispatch_request(
            app,
            &encode_request_bytes(seat, 2, &new_id_payload(keyboard)),
        )
        .unwrap();
    let surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let buffer = ObjectId::new(17);
    let toplevel = ObjectId::new(19);
    make_surface_with_buffer(&mut server, app, compositor, shm, surface, pool, buffer);
    promote_to_toplevel_and_commit_red(
        &mut server,
        app,
        xdg_shell,
        surface,
        pool,
        buffer,
        toplevel,
    );
    server.set_keyboard_focus(Some((app, surface)));
    let target_window = server.window_id(app, toplevel).unwrap();
    let (shell, _, shell_manager) = bind_active_shortcut_shell(&mut server);
    let _ = server.drain_client_events(app);

    // Releasing only left Alt must leave right Alt active. The first F4 press
    // becomes one shell event; browser repeats and the physical release are
    // consumed so the app sees neither half of the shortcut key.
    server.inject_keyboard_key(0xE2, key_state::PRESSED);
    server.inject_keyboard_key(0xE6, key_state::PRESSED);
    server.inject_keyboard_key(0xE2, key_state::RELEASED);
    server.inject_keyboard_key(0x3D, key_state::PRESSED);
    server.inject_keyboard_key(0x3D, key_state::PRESSED);
    server.inject_keyboard_key(0x3D, key_state::RELEASED);
    server.inject_keyboard_key(0xE6, key_state::RELEASED);

    assert_eq!(
        close_shortcut_events(&server.drain_client_events(shell).unwrap(), shell_manager,),
        vec![ShellCloseShortcut {
            window_id: target_window,
        }],
    );
    let app_keys = keyboard_events(&server.drain_client_events(app).unwrap(), keyboard);
    assert_eq!(
        app_keys
            .iter()
            .map(|event| (event.key, event.state))
            .collect::<Vec<_>>(),
        vec![
            (0xE2, key_state::PRESSED),
            (0xE6, key_state::PRESSED),
            (0xE2, key_state::RELEASED),
            (0xE6, key_state::RELEASED),
        ],
    );

    // The release reset the latch: an unmodified F4 remains ordinary app
    // input and does not produce another privileged event.
    server.inject_keyboard_key(0x3D, key_state::PRESSED);
    server.inject_keyboard_key(0x3D, key_state::RELEASED);
    assert_eq!(
        keyboard_events(&server.drain_client_events(app).unwrap(), keyboard)
            .iter()
            .map(|event| (event.key, event.state))
            .collect::<Vec<_>>(),
        vec![(0x3D, key_state::PRESSED), (0x3D, key_state::RELEASED),],
    );
    assert!(server.drain_client_events(shell).unwrap().is_empty());
}

#[test]
fn alt_f4_routes_normally_when_no_authenticated_shell_can_receive_it() {
    let (mut server, app, compositor, shm, xdg_shell, seat) = boot_server_and_bind_everything();
    let keyboard = ObjectId::new(33);
    server
        .dispatch_request(
            app,
            &encode_request_bytes(seat, 2, &new_id_payload(keyboard)),
        )
        .unwrap();
    let surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let buffer = ObjectId::new(17);
    let toplevel = ObjectId::new(19);
    make_surface_with_buffer(&mut server, app, compositor, shm, surface, pool, buffer);
    promote_to_toplevel_and_commit_red(
        &mut server,
        app,
        xdg_shell,
        surface,
        pool,
        buffer,
        toplevel,
    );
    server.set_keyboard_focus(Some((app, surface)));
    let _ = server.drain_client_events(app);

    for (key, state) in [
        (0xE2, key_state::PRESSED),
        (0x3D, key_state::PRESSED),
        (0x3D, key_state::RELEASED),
        (0xE2, key_state::RELEASED),
    ] {
        server.inject_keyboard_key(key, state);
    }

    assert_eq!(
        keyboard_events(&server.drain_client_events(app).unwrap(), keyboard)
            .iter()
            .map(|event| (event.key, event.state))
            .collect::<Vec<_>>(),
        vec![
            (0xE2, key_state::PRESSED),
            (0x3D, key_state::PRESSED),
            (0x3D, key_state::RELEASED),
            (0xE2, key_state::RELEASED),
        ],
    );
}

#[test]
fn alt_f4_routes_normally_for_an_explicit_v1_shell_manager_binding() {
    let (mut server, app, compositor, shm, xdg_shell, seat) = boot_server_and_bind_everything();
    let keyboard = ObjectId::new(33);
    server
        .dispatch_request(
            app,
            &encode_request_bytes(seat, 2, &new_id_payload(keyboard)),
        )
        .unwrap();
    let surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let buffer = ObjectId::new(17);
    let toplevel = ObjectId::new(19);
    make_surface_with_buffer(&mut server, app, compositor, shm, surface, pool, buffer);
    promote_to_toplevel_and_commit_red(
        &mut server,
        app,
        xdg_shell,
        surface,
        pool,
        buffer,
        toplevel,
    );
    server.set_keyboard_focus(Some((app, surface)));
    let (shell, _, shell_manager) = bind_active_shortcut_shell_version(&mut server, 1);
    assert_eq!(server.client(shell).unwrap().shell_manager_version, Some(1));
    let _ = server.drain_client_events(app);

    for (key, state) in [
        (0xE2, key_state::PRESSED),
        (0x3D, key_state::PRESSED),
        (0x3D, key_state::RELEASED),
        (0xE2, key_state::RELEASED),
    ] {
        server.inject_keyboard_key(key, state);
    }

    assert!(
        close_shortcut_events(&server.drain_client_events(shell).unwrap(), shell_manager,)
            .is_empty()
    );
    assert_eq!(
        keyboard_events(&server.drain_client_events(app).unwrap(), keyboard)
            .iter()
            .map(|event| (event.key, event.state))
            .collect::<Vec<_>>(),
        vec![
            (0xE2, key_state::PRESSED),
            (0x3D, key_state::PRESSED),
            (0x3D, key_state::RELEASED),
            (0xE2, key_state::RELEASED),
        ],
    );
}

#[test]
fn explicit_v1_shell_manager_cannot_invoke_v2_state_or_restore_requests() {
    let mut server = Server::new();
    let (shell, _, manager) = bind_active_shortcut_shell_version(&mut server, 1);

    for (opcode, payload) in [
        (9, 7_u32.to_le_bytes().to_vec()),
        (10, [1_u32.to_le_bytes(), 250_u32.to_le_bytes()].concat()),
        (11, vec![0; 32]),
        (12, vec![0; 8]),
    ] {
        assert_eq!(
            server.dispatch_request(shell, &encode_request_bytes(manager, opcode, &payload)),
            Err(ServerError::Client(ClientError::RequestRequiresVersion {
                interface: Interface::ShellManager,
                object_id: manager,
                opcode,
                negotiated: 1,
                required: 2,
            }))
        );
        assert_eq!(server.restore_transaction_owner(), None);
        assert_eq!(
            server
                .client(shell)
                .unwrap()
                .shell_manager_state_snapshot_id,
            None
        );
        assert!(server.drain_client_events(shell).unwrap().is_empty());
    }
}

#[test]
fn negotiated_v2_shell_manager_accepts_the_authoritative_subscription() {
    let mut server = Server::new();
    let (shell, _, manager) = bind_active_shortcut_shell_version(&mut server, 2);
    server
        .dispatch_request(
            shell,
            &encode_request_bytes(manager, 9, &7_u32.to_le_bytes()),
        )
        .unwrap();

    assert_eq!(
        server
            .client(shell)
            .unwrap()
            .shell_manager_state_snapshot_id,
        Some(7)
    );
    let bytes = server.drain_client_events(shell).unwrap();
    let header = ProtoHeader::decode(&bytes).expect("snapshot terminator event");
    assert_eq!(header.object_id, manager);
    assert_eq!(header.opcode, 7 /* window_snapshot_done */);
}

#[test]
fn alt_f4_does_not_target_no_focus_hidden_or_minimized_windows() {
    let (mut server, app, compositor, shm, xdg_shell, seat) = boot_server_and_bind_everything();
    let keyboard = ObjectId::new(33);
    server
        .dispatch_request(
            app,
            &encode_request_bytes(seat, 2, &new_id_payload(keyboard)),
        )
        .unwrap();
    let surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let buffer = ObjectId::new(17);
    let toplevel = ObjectId::new(19);
    make_surface_with_buffer(&mut server, app, compositor, shm, surface, pool, buffer);
    promote_to_toplevel_and_commit_red(
        &mut server,
        app,
        xdg_shell,
        surface,
        pool,
        buffer,
        toplevel,
    );
    let (shell, _, shell_manager) = bind_active_shortcut_shell(&mut server);
    let _ = server.drain_client_events(app);

    let press_chord = |server: &mut Server| {
        server.inject_keyboard_key(0xE2, key_state::PRESSED);
        server.inject_keyboard_key(0x3D, key_state::PRESSED);
        server.inject_keyboard_key(0x3D, key_state::RELEASED);
        server.inject_keyboard_key(0xE2, key_state::RELEASED);
    };

    server.set_keyboard_focus(None);
    press_chord(&mut server);
    assert!(
        close_shortcut_events(&server.drain_client_events(shell).unwrap(), shell_manager,)
            .is_empty()
    );

    server.set_keyboard_focus(Some((app, surface)));
    server
        .client_mut(app)
        .unwrap()
        .toplevels
        .get_mut(&toplevel)
        .unwrap()
        .hidden_for_restore = true;
    press_chord(&mut server);
    assert!(
        close_shortcut_events(&server.drain_client_events(shell).unwrap(), shell_manager,)
            .is_empty()
    );

    server
        .client_mut(app)
        .unwrap()
        .toplevels
        .get_mut(&toplevel)
        .unwrap()
        .hidden_for_restore = false;
    server
        .client_mut(app)
        .unwrap()
        .toplevels
        .get_mut(&toplevel)
        .unwrap()
        .minimized = true;
    press_chord(&mut server);
    assert!(
        close_shortcut_events(&server.drain_client_events(shell).unwrap(), shell_manager,)
            .is_empty()
    );
}

#[test]
fn alt_f4_never_exposes_a_shell_desktop_window_as_the_close_target() {
    let (mut server, _app, _, _, _, _) = boot_server_and_bind_everything();
    let (shell, registry, shell_manager) = bind_active_shortcut_shell(&mut server);
    let compositor = ObjectId::new(7);
    let shm = ObjectId::new(9);
    let xdg_shell = ObjectId::new(11);
    let seat = ObjectId::new(13);
    for (name, interface, object) in [
        (1, "pmd_compositor", compositor),
        (2, "pmd_shm", shm),
        (3, "pmd_xdg_shell", xdg_shell),
        (4, "pmd_seat", seat),
    ] {
        server
            .dispatch_request(
                shell,
                &encode_request_bytes(
                    registry,
                    1,
                    &registry_bind_payload(name, interface, 1, object),
                ),
            )
            .unwrap();
    }
    let keyboard = ObjectId::new(15);
    server
        .dispatch_request(
            shell,
            &encode_request_bytes(seat, 2, &new_id_payload(keyboard)),
        )
        .unwrap();
    let surface = ObjectId::new(17);
    let pool = ObjectId::new(19);
    let buffer = ObjectId::new(21);
    let toplevel = ObjectId::new(23);
    make_surface_with_buffer(&mut server, shell, compositor, shm, surface, pool, buffer);
    promote_to_toplevel_and_commit_red(
        &mut server,
        shell,
        xdg_shell,
        surface,
        pool,
        buffer,
        toplevel,
    );
    server.set_keyboard_focus(Some((shell, surface)));
    let _ = server.drain_client_events(shell);

    for (key, state) in [
        (0xE2, key_state::PRESSED),
        (0x3D, key_state::PRESSED),
        (0x3D, key_state::RELEASED),
        (0xE2, key_state::RELEASED),
    ] {
        server.inject_keyboard_key(key, state);
    }

    let events = server.drain_client_events(shell).unwrap();
    assert!(close_shortcut_events(&events, shell_manager).is_empty());
    assert_eq!(
        keyboard_events(&events, keyboard)
            .iter()
            .map(|event| (event.key, event.state))
            .collect::<Vec<_>>(),
        vec![
            (0xE2, key_state::PRESSED),
            (0x3D, key_state::PRESSED),
            (0x3D, key_state::RELEASED),
            (0xE2, key_state::RELEASED),
        ],
    );
}

#[test]
fn changing_layout_remaps_subsequent_keys_without_restarting_server_or_client() {
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
    make_surface_with_buffer(&mut s, c, compositor_id, shm_id, surface, pool, buffer);
    promote_to_toplevel_and_commit_red(&mut s, c, xdg_shell_id, surface, pool, buffer, toplevel);
    s.set_keyboard_focus(Some((c, surface)));
    let _ = s.drain_client_events(c);

    assert!(s.set_keyboard_layout(KeyboardLayout::Dvorak).unwrap());
    assert_eq!(s.keyboard_layout(), KeyboardLayout::Dvorak);
    s.inject_keyboard_key(0x16 /* physical KeyS */, key_state::PRESSED)
        .unwrap();
    let bytes = s.drain_client_events(c).unwrap();
    let header = ProtoHeader::decode(&bytes).unwrap();
    let payload = &bytes[display_proto::wire::HEADER_SIZE..header.length as usize];
    assert_eq!(
        KeyboardKey::decode(payload).unwrap().key,
        0x12, /* logical KeyO */
    );

    assert!(s.set_keyboard_layout(KeyboardLayout::UkQwerty).unwrap());
    s.inject_keyboard_key(0xE1 /* ShiftLeft */, key_state::PRESSED)
        .unwrap();
    let _ = s.drain_client_events(c);
    s.inject_keyboard_key(0x1F /* physical Digit2 */, key_state::PRESSED)
        .unwrap();
    let bytes = s.drain_client_events(c).unwrap();
    let header = ProtoHeader::decode(&bytes).unwrap();
    let payload = &bytes[display_proto::wire::HEADER_SIZE..header.length as usize];
    assert_eq!(
        KeyboardKey::decode(payload).unwrap().key,
        0x34, /* logical Quote */
    );
}

#[test]
fn inject_keyboard_key_without_focus_is_a_none_result() {
    let mut s = Server::with_framebuffer_size(32, 32);
    assert!(s.inject_keyboard_key(0x1e, key_state::PRESSED).is_none());
}

#[test]
fn first_mapped_buffer_focuses_new_toplevel_once_and_broadcasts_global_id() {
    use abi::cap::{Cap, CapSet};

    let (mut s, app, compositor_id, shm_id, xdg_shell_id, _seat_id) =
        boot_server_and_bind_everything();
    let shell = s.accept_with_caps(CapSet::from_caps(&[Cap::Shell]));
    let shell_registry = ObjectId::new(3);
    let shell_manager = ObjectId::new(5);
    s.dispatch_request(
        shell,
        &encode_request_bytes(ObjectId::DISPLAY, 2, &shell_registry.raw().to_le_bytes()),
    )
    .unwrap();
    s.dispatch_request(
        shell,
        &encode_request_bytes(
            shell_registry,
            1,
            &registry_bind_payload(5, "pmd_shell_manager", 1, shell_manager),
        ),
    )
    .unwrap();
    s.dispatch_request(shell, &encode_request_bytes(shell_manager, 1, &[]))
        .unwrap();
    let _ = s.drain_client_events(shell);

    let surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let buffer = ObjectId::new(17);
    let toplevel = ObjectId::new(19);
    make_surface_with_buffer(&mut s, app, compositor_id, shm_id, surface, pool, buffer);
    promote_to_toplevel_and_commit_red(&mut s, app, xdg_shell_id, surface, pool, buffer, toplevel);
    let window_id = s.window_id(app, toplevel).unwrap();

    assert_eq!(s.keyboard_focus(), Some((app, surface)));
    assert_eq!(s.window_z_order().last().unwrap().0, window_id);
    let bytes = s.drain_client_events(shell).unwrap();
    let mut offset = 0;
    let mut focused = Vec::new();
    while offset < bytes.len() {
        let header = ProtoHeader::decode(&bytes[offset..]).unwrap();
        let end = offset + header.length as usize;
        if header.object_id == shell_manager && header.opcode == 3 {
            focused.push(
                ShellWindowFocused::decode(&bytes[offset + HEADER_SIZE..end])
                    .unwrap()
                    .window_id,
            );
        }
        offset = end;
    }
    assert_eq!(focused, vec![window_id]);

    // A later damage-only commit or detach/reattach is not a new map
    // and must not steal focus a second time.
    s.set_keyboard_focus(None);
    s.dispatch_request(app, &encode_request_bytes(surface, 7, &[]))
        .unwrap();
    assert_eq!(s.keyboard_focus(), None);
    assert!(s.drain_client_events(shell).unwrap().is_empty());
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
fn ordinary_client_keeps_seat_input_but_cannot_bind_or_invoke_shell_manager() {
    use abi::cap::Cap;

    let mut s = Server::new();
    let app = s.accept();
    let registry_id = ObjectId::new(3);
    let forged_shell_manager_id = ObjectId::new(5);
    s.dispatch_request(
        app,
        &encode_request_bytes(ObjectId::DISPLAY, 2, &registry_id.raw().to_le_bytes()),
    )
    .unwrap();

    s.advertise_globals_to(app, registry_id);
    let advertisements = s.drain_client_events(app).unwrap();
    assert!(advertisements
        .windows(b"pmd_seat".len())
        .any(|bytes| bytes == b"pmd_seat"));
    assert!(!advertisements
        .windows(b"pmd_shell_manager".len())
        .any(|bytes| bytes == b"pmd_shell_manager"));

    // Seat input is intentionally universal. Ordinary apps use
    // App::connect_with_shell to request pointer/keyboard objects;
    // withholding Shell must only hide shell-manager control.
    let seat_id = ObjectId::new(7);
    let pointer_id = ObjectId::new(9);
    let keyboard_id = ObjectId::new(11);
    let bind_seat = registry_bind_payload(4, "pmd_seat", 1, seat_id);
    s.dispatch_request(app, &encode_request_bytes(registry_id, 1, &bind_seat))
        .unwrap();
    s.dispatch_request(
        app,
        &encode_request_bytes(seat_id, 1, &new_id_payload(pointer_id)),
    )
    .unwrap();
    s.dispatch_request(
        app,
        &encode_request_bytes(seat_id, 2, &new_id_payload(keyboard_id)),
    )
    .unwrap();
    assert_eq!(s.client(app).unwrap().get(seat_id), Some(Interface::Seat));
    assert_eq!(
        s.client(app).unwrap().get(pointer_id),
        Some(Interface::Pointer),
    );
    assert_eq!(
        s.client(app).unwrap().get(keyboard_id),
        Some(Interface::Keyboard),
    );

    let bind = registry_bind_payload(5, "pmd_shell_manager", 1, forged_shell_manager_id);
    let error = s
        .dispatch_request(app, &encode_request_bytes(registry_id, 1, &bind))
        .unwrap_err();
    assert_eq!(
        error,
        ServerError::Client(ClientError::PermissionDenied {
            interface: Interface::ShellManager,
            required: Cap::Shell,
            new_id: forged_shell_manager_id,
        }),
    );
    assert_eq!(s.client(app).unwrap().get(forged_shell_manager_id), None);
    assert_eq!(s.client(app).unwrap().shell_manager_id, None);

    let forged_focus = 1u32.to_le_bytes();
    let error = s
        .dispatch_request(
            app,
            &encode_request_bytes(forged_shell_manager_id, 2, &forged_focus),
        )
        .unwrap_err();
    assert_eq!(
        error,
        ServerError::Client(ClientError::UnknownObject {
            id: forged_shell_manager_id,
        }),
    );
}

#[test]
fn desktop_ready_queues_one_authenticated_fence_per_live_shell_connection() {
    use abi::cap::{Cap, CapSet};

    fn bind_shell_manager(server: &mut Server, client: display_server::ClientId) -> ObjectId {
        let registry = ObjectId::new(3);
        let shell_manager = ObjectId::new(5);
        server
            .dispatch_request(
                client,
                &encode_request_bytes(ObjectId::DISPLAY, 2, &registry.raw().to_le_bytes()),
            )
            .unwrap();
        let bind = registry_bind_payload(5, "pmd_shell_manager", 1, shell_manager);
        server
            .dispatch_request(client, &encode_request_bytes(registry, 1, &bind))
            .unwrap();
        shell_manager
    }

    let mut server = Server::new();

    // Defence in depth: even a forged local object-table entry cannot arm a
    // host fence without the kernel-authenticated connection capability.
    let ordinary = server.accept();
    let forged_shell_manager = ObjectId::new(3);
    server
        .client_mut(ordinary)
        .unwrap()
        .install_client_object(forged_shell_manager, Interface::ShellManager)
        .unwrap();
    server
        .dispatch_request(
            ordinary,
            &encode_request_bytes(forged_shell_manager, 8, &[]),
        )
        .unwrap();
    assert_eq!(server.pending_present_fence_count(), 0);
    assert_eq!(server.take_pending_present_fence(), None);

    let first_shell = server.accept_with_caps(CapSet::from_caps(&[Cap::Shell]));
    let first_manager = bind_shell_manager(&mut server, first_shell);
    let malformed = server
        .dispatch_request(first_shell, &encode_request_bytes(first_manager, 8, &[0]))
        .unwrap_err();
    assert_eq!(
        malformed,
        ServerError::Client(ClientError::Malformed {
            interface: Interface::ShellManager,
            opcode: 8,
            error: display_proto::DecodeError::PayloadLengthMismatch {
                expected: 0,
                actual: 1,
            },
        }),
    );
    assert_eq!(server.pending_present_fence_count(), 0);
    server
        .dispatch_request(first_shell, &encode_request_bytes(first_manager, 8, &[]))
        .unwrap();
    server
        .dispatch_request(first_shell, &encode_request_bytes(first_manager, 8, &[]))
        .unwrap();
    assert_eq!(server.pending_present_fence_count(), 1);
    assert_eq!(server.take_pending_present_fence(), Some(1));
    assert_eq!(server.take_pending_present_fence(), None);

    // Idempotence lasts for the complete live connection, not merely until
    // its first marker is drained.
    server
        .dispatch_request(first_shell, &encode_request_bytes(first_manager, 8, &[]))
        .unwrap();
    assert_eq!(server.pending_present_fence_count(), 0);

    let replacement = server.accept_with_caps(CapSet::from_caps(&[Cap::Shell]));
    let replacement_manager = bind_shell_manager(&mut server, replacement);
    server
        .dispatch_request(
            replacement,
            &encode_request_bytes(replacement_manager, 8, &[]),
        )
        .unwrap();
    assert_eq!(server.pending_present_fence_count(), 1);

    // A readiness request from a shell that exits before the presentation
    // boundary must never survive as a trusted host marker.
    server.disconnect(replacement).unwrap();
    assert_eq!(server.pending_present_fence_count(), 0);
    assert_eq!(server.take_pending_present_fence(), None);

    // A new authenticated connection gets its own one-shot state and the
    // server-global fence serial remains monotonic across replacement.
    let next_shell = server.accept_with_caps(CapSet::from_caps(&[Cap::Shell]));
    let next_manager = bind_shell_manager(&mut server, next_shell);
    server
        .dispatch_request(next_shell, &encode_request_bytes(next_manager, 8, &[]))
        .unwrap();
    assert_eq!(server.take_pending_present_fence(), Some(3));
    assert_eq!(server.take_pending_present_fence(), None);
}

fn create_colliding_local_window(
    s: &mut Server,
    client_id: display_server::ClientId,
) -> (ObjectId, ObjectId, u32) {
    let toplevel_id = ObjectId::new(3);
    let registry_id = ObjectId::new(5);
    let compositor_id = ObjectId::new(7);
    let xdg_shell_id = ObjectId::new(9);
    let surface_id = ObjectId::new(11);

    s.dispatch_request(
        client_id,
        &encode_request_bytes(ObjectId::DISPLAY, 2, &registry_id.raw().to_le_bytes()),
    )
    .unwrap();
    for (name, interface, target) in [
        (1u32, "pmd_compositor", compositor_id),
        (3u32, "pmd_xdg_shell", xdg_shell_id),
    ] {
        let bind = registry_bind_payload(name, interface, 1, target);
        s.dispatch_request(client_id, &encode_request_bytes(registry_id, 1, &bind))
            .unwrap();
    }
    s.dispatch_request(
        client_id,
        &encode_request_bytes(compositor_id, 1, &surface_id.raw().to_le_bytes()),
    )
    .unwrap();
    s.dispatch_request(
        client_id,
        &encode_request_bytes(
            xdg_shell_id,
            1,
            &xdg_get_toplevel_payload(toplevel_id, surface_id),
        ),
    )
    .unwrap();
    let window_id = s.window_id(client_id, toplevel_id).unwrap();
    (surface_id, toplevel_id, window_id)
}

#[test]
fn global_window_ids_route_colliding_client_object_ids_and_retire_on_disconnect() {
    use abi::cap::{Cap, CapSet};

    let mut s = Server::new();
    let app_a = s.accept();
    let app_b = s.accept();
    let shell = s.accept_with_caps(CapSet::from_caps(&[Cap::Shell]));
    let (_surface_a, local_toplevel_a, window_a) = create_colliding_local_window(&mut s, app_a);
    let (surface_b, local_toplevel_b, window_b) = create_colliding_local_window(&mut s, app_b);

    assert_eq!(local_toplevel_a, ObjectId::new(3));
    assert_eq!(local_toplevel_b, ObjectId::new(3));
    assert_ne!(window_a, window_b);
    assert_eq!(s.window_owner(window_a), Some((app_a, local_toplevel_a)));
    assert_eq!(s.window_owner(window_b), Some((app_b, local_toplevel_b)));

    let shell_registry = ObjectId::new(3);
    let shell_manager = ObjectId::new(5);
    s.dispatch_request(
        shell,
        &encode_request_bytes(ObjectId::DISPLAY, 2, &shell_registry.raw().to_le_bytes()),
    )
    .unwrap();
    let bind = registry_bind_payload(5, "pmd_shell_manager", 1, shell_manager);
    s.dispatch_request(shell, &encode_request_bytes(shell_registry, 1, &bind))
        .unwrap();

    // Clear registry advertisements before inspecting control events.
    let _ = s.drain_client_events(app_a);
    let _ = s.drain_client_events(app_b);
    let _ = s.drain_client_events(shell);

    s.dispatch_request(
        shell,
        &encode_request_bytes(shell_manager, 2, &window_b.to_le_bytes()),
    )
    .unwrap();
    assert_eq!(s.keyboard_focus(), Some((app_b, surface_b)));

    s.dispatch_request(
        shell,
        &encode_request_bytes(shell_manager, 4, &window_a.to_le_bytes()),
    )
    .unwrap();
    assert!(
        s.client(app_a)
            .unwrap()
            .toplevel(local_toplevel_a)
            .unwrap()
            .minimized
    );
    assert!(
        !s.client(app_b)
            .unwrap()
            .toplevel(local_toplevel_b)
            .unwrap()
            .minimized
    );

    // Put a different window in front before asking app B to close. A close
    // request is advisory: app B may veto it to show an unsaved-changes
    // prompt, so the server must reactivate B before delivering the event.
    s.dispatch_request(
        shell,
        &encode_request_bytes(shell_manager, 2, &window_a.to_le_bytes()),
    )
    .unwrap();
    assert_ne!(s.keyboard_focus(), Some((app_b, surface_b)));

    s.dispatch_request(
        shell,
        &encode_request_bytes(shell_manager, 3, &window_b.to_le_bytes()),
    )
    .unwrap();
    assert!(s.drain_client_events(app_a).unwrap().is_empty());
    let close = s.drain_client_events(app_b).unwrap();
    let close_header = ProtoHeader::decode(&close).unwrap();
    assert_eq!(close_header.object_id, local_toplevel_b);
    assert_eq!(close_header.opcode, 2 /* xdg_toplevel.close */);
    assert_eq!(s.keyboard_focus(), Some((app_b, surface_b)));
    assert_eq!(
        s.window_owner(window_b),
        Some((app_b, local_toplevel_b)),
        "close is advisory until the client destroys its toplevel",
    );

    s.disconnect(app_b).unwrap();
    assert_eq!(s.window_owner(window_b), None);
    assert_eq!(s.window_id(app_b, local_toplevel_b), None);
    assert_eq!(s.window_owner(window_a), Some((app_a, local_toplevel_a)));
    assert_eq!(s.keyboard_focus(), None);

    let app_c = s.accept();
    let (_, _, window_c) = create_colliding_local_window(&mut s, app_c);
    assert_ne!(window_c, window_b, "retired global IDs must not be reused");
    assert_eq!(s.window_owner(window_c), Some((app_c, ObjectId::new(3))));
}

#[test]
fn shell_toggle_maximize_routes_exact_owner_and_honors_work_area() {
    use abi::cap::{Cap, CapSet};
    use display_proto::events::XdgToplevelConfigure;
    use display_proto::wire::HEADER_SIZE as PROTO_HEADER_SIZE;
    use display_proto::xdg_toplevel_state;

    let mut s = Server::with_framebuffer_size(800, 600);
    let app_a = s.accept();
    let app_b = s.accept();
    let shell = s.accept_with_caps(CapSet::from_caps(&[Cap::Shell]));
    let (_surface_a, local_a, window_a) = create_colliding_local_window(&mut s, app_a);
    let (surface_b, local_b, window_b) = create_colliding_local_window(&mut s, app_b);
    assert_eq!(local_a, local_b, "fixture must collide client-local IDs");
    assert_ne!(window_a, window_b, "global window IDs must remain distinct");

    let shell_registry = ObjectId::new(3);
    let shell_manager = ObjectId::new(5);
    s.dispatch_request(
        shell,
        &encode_request_bytes(ObjectId::DISPLAY, 2, &shell_registry.raw().to_le_bytes()),
    )
    .unwrap();
    let bind = registry_bind_payload(5, "pmd_shell_manager", 1, shell_manager);
    s.dispatch_request(shell, &encode_request_bytes(shell_registry, 1, &bind))
        .unwrap();
    s.set_taskbar_height_px(40);
    let normal_origin = {
        let top = s.client(app_b).unwrap().toplevel(local_b).unwrap();
        (top.x, top.y)
    };
    let _ = s.drain_client_events(app_a);
    let _ = s.drain_client_events(app_b);
    let _ = s.drain_client_events(shell);

    s.dispatch_request(
        shell,
        &encode_request_bytes(shell_manager, 7, &window_b.to_le_bytes()),
    )
    .unwrap();
    let a = s.client(app_a).unwrap().toplevel(local_a).unwrap();
    assert!(
        !a.maximized,
        "colliding local ID on app A must be untouched"
    );
    let b = s.client(app_b).unwrap().toplevel(local_b).unwrap();
    assert!(b.maximized);
    assert_eq!((b.x, b.y), (0, 0));
    assert_eq!(b.restore_origin, Some(normal_origin));
    assert_eq!(s.keyboard_focus(), Some((app_b, surface_b)));
    let bytes = s.drain_client_events(app_b).unwrap();
    let header = ProtoHeader::decode(&bytes).unwrap();
    assert_eq!(header.object_id, local_b);
    let configure =
        XdgToplevelConfigure::decode(&bytes[PROTO_HEADER_SIZE..header.length as usize]).unwrap();
    assert_eq!((configure.width, configure.height), (800, 560));
    assert_ne!(configure.states & xdg_toplevel_state::MAXIMIZED, 0);

    s.dispatch_request(
        shell,
        &encode_request_bytes(shell_manager, 7, &window_b.to_le_bytes()),
    )
    .unwrap();
    let b = s.client(app_b).unwrap().toplevel(local_b).unwrap();
    assert!(!b.maximized);
    assert_eq!((b.x, b.y), normal_origin);
    let bytes = s.drain_client_events(app_b).unwrap();
    let header = ProtoHeader::decode(&bytes).unwrap();
    let configure =
        XdgToplevelConfigure::decode(&bytes[PROTO_HEADER_SIZE..header.length as usize]).unwrap();
    assert_eq!(
        (configure.width, configure.height, configure.states),
        (0, 0, xdg_toplevel_state::ACTIVATED)
    );

    s.dispatch_request(
        shell,
        &encode_request_bytes(shell_manager, 7, &0xdead_beefu32.to_le_bytes()),
    )
    .unwrap();
    assert!(
        !s.client(app_a)
            .unwrap()
            .toplevel(local_a)
            .unwrap()
            .maximized
    );
    assert!(
        !s.client(app_b)
            .unwrap()
            .toplevel(local_b)
            .unwrap()
            .maximized
    );
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
        &encode_request_bytes(ObjectId::DISPLAY, 2, &registry_id.raw().to_le_bytes()),
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
    let window_id = s.window_id(app, toplevel_id).unwrap();
    assert!(
        !s.client(app)
            .unwrap()
            .toplevel(toplevel_id)
            .unwrap()
            .minimized
    );

    // Shell client: bind shell_manager (only privileged
    // clients can). The shell's registry id can reuse 3 —
    // every client has its own table.
    let shell_registry_id = ObjectId::new(3);
    let shell_manager_id = ObjectId::new(5);
    s.dispatch_request(
        shell,
        &encode_request_bytes(ObjectId::DISPLAY, 2, &shell_registry_id.raw().to_le_bytes()),
    )
    .unwrap();
    let payload = registry_bind_payload(1, "pmd_shell_manager", 1, shell_manager_id);
    s.dispatch_request(shell, &encode_request_bytes(shell_registry_id, 1, &payload))
        .unwrap();

    // Shell sends minimize_window(window_id) targeting the app
    // client's toplevel through its server-global identity.
    let mut min_payload = Vec::new();
    min_payload.extend_from_slice(&window_id.to_le_bytes());
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

    s.dispatch_request(
        shell,
        &encode_request_bytes(
            shell_manager_id,
            5, /* unminimize_window */
            &min_payload,
        ),
    )
    .unwrap();
    assert!(
        !s.client(app)
            .unwrap()
            .toplevel(toplevel_id)
            .unwrap()
            .minimized,
        "unminimize_window must restore the exact globally-addressed toplevel",
    );
    assert_eq!(
        s.keyboard_focus(),
        Some((app, surface_id)),
        "taskbar restore must activate the restored window",
    );

    // The direct helper remains idempotent for a known restored window.
    assert!(s.restore_window(window_id));
    assert!(
        !s.client(app)
            .unwrap()
            .toplevel(toplevel_id)
            .unwrap()
            .minimized
    );
}

#[test]
fn restore_window_returns_false_for_unknown_id() {
    let mut s = Server::new();
    let _c = s.accept();
    assert!(!s.restore_window(0xdead_beef));
}

#[test]
fn minimized_toplevel_skips_the_composite_blit() {
    // Paint a 2x2 red buffer + commit, observe the framebuffer
    // pixel-by-pixel. Then minimize the toplevel via the
    // direct setter (skipping the cross-client request to
    // keep the test self-contained) and verify the scene is
    // cleared immediately, without waiting for another commit.
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
        &encode_request_bytes(ObjectId::DISPLAY, 2, &registry_id.raw().to_le_bytes()),
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
    let window_id = s.window_id(c, toplevel_id).unwrap();

    // Paint pool red, attach + damage + commit.
    for px in s
        .client_mut(c)
        .unwrap()
        .pool_bytes_mut(pool_id)
        .unwrap()
        .chunks_exact_mut(4)
    {
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
    assert_eq!(
        s.framebuffer().pixel(0, 0).unwrap(),
        &[0x00, 0x00, 0xff, 0xff]
    );

    // Reset framebuffer to a known sentinel + minimize.
    let fb = s.framebuffer_mut();
    for px in fb.pixels_mut().chunks_exact_mut(4) {
        px[0] = 0xab;
        px[1] = 0xcd;
        px[2] = 0xef;
        px[3] = 0xff;
    }
    assert!(s.set_window_minimized(window_id, true));

    assert_eq!(
        s.framebuffer().pixel(0, 0).unwrap(),
        &[0, 0, 0, 0],
        "minimize must clear the old window pixels immediately"
    );
    assert!(s.hit_test(0, 0).is_none());

    // Restore also recomposes immediately from the retained current
    // buffer; the client does not need to submit another frame.
    assert!(s.restore_window(window_id));
    assert_eq!(
        s.framebuffer().pixel(0, 0).unwrap(),
        &[0x00, 0x00, 0xff, 0xff]
    );
}

#[test]
fn move_detach_and_toplevel_destroy_recompose_without_pixel_trails() {
    let (mut s, c, compositor_id, shm_id, xdg_shell_id, _seat_id) =
        boot_server_and_bind_everything();
    let surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let buffer = ObjectId::new(17);
    let toplevel = ObjectId::new(19);
    make_surface_with_buffer(&mut s, c, compositor_id, shm_id, surface, pool, buffer);
    promote_to_toplevel_and_commit_red(&mut s, c, xdg_shell_id, surface, pool, buffer, toplevel);
    let window_id = s.window_id(c, toplevel).unwrap();

    s.inject_pointer_motion(0, 0);
    s.dispatch_request(
        c,
        &encode_request_bytes(toplevel, 7 /* move */, &1u32.to_le_bytes()),
    )
    .unwrap();
    let recomposition_before_drag = s.recomposition_serial();
    s.inject_pointer_motion(3, 0);
    assert!(
        s.recomposition_serial() > recomposition_before_drag,
        "interactive pointer motion must retain its direct scene-dirty signal",
    );
    assert_eq!(
        s.framebuffer().pixel(0, 0).unwrap(),
        &[0, 0, 0, 0],
        "moving must clear the old origin"
    );
    assert_eq!(
        s.framebuffer().pixel(3, 0).unwrap(),
        &[0x00, 0x00, 0xff, 0xff]
    );
    s.inject_pointer_button(1, pointer_button_state::RELEASED);

    s.dispatch_request(
        c,
        &encode_request_bytes(surface, 2, &surface_attach_payload(ObjectId::NULL, 0, 0)),
    )
    .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface, 7, &[]))
        .unwrap();
    assert_eq!(
        s.framebuffer().pixel(3, 0).unwrap(),
        &[0, 0, 0, 0],
        "attach(NULL) + commit must remove the current buffer"
    );

    s.dispatch_request(
        c,
        &encode_request_bytes(surface, 2, &surface_attach_payload(buffer, 0, 0)),
    )
    .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface, 7, &[]))
        .unwrap();
    assert_eq!(
        s.framebuffer().pixel(3, 0).unwrap(),
        &[0x00, 0x00, 0xff, 0xff]
    );

    s.dispatch_request(c, &encode_request_bytes(toplevel, 3 /* destroy */, &[]))
        .unwrap();
    assert_eq!(s.framebuffer().pixel(3, 0).unwrap(), &[0, 0, 0, 0]);
    assert_eq!(s.window_owner(window_id), None);
    assert!(s.window_z_order().is_empty());
}

#[test]
fn disconnect_recomposes_and_retires_window_pixels_and_z_order() {
    let (mut s, c, compositor_id, shm_id, xdg_shell_id, _seat_id) =
        boot_server_and_bind_everything();
    let surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let buffer = ObjectId::new(17);
    let toplevel = ObjectId::new(19);
    make_surface_with_buffer(&mut s, c, compositor_id, shm_id, surface, pool, buffer);
    promote_to_toplevel_and_commit_red(&mut s, c, xdg_shell_id, surface, pool, buffer, toplevel);
    let window_id = s.window_id(c, toplevel).unwrap();
    let generation = s.scene_generation();

    assert!(s.disconnect(c).is_some());
    assert!(s.scene_generation() > generation);
    assert_eq!(s.framebuffer().pixel(0, 0).unwrap(), &[0, 0, 0, 0]);
    assert_eq!(s.window_owner(window_id), None);
    assert!(s.window_z_order().is_empty());
}

#[test]
fn recomposition_batch_coalesces_window_mutations_and_disconnect() {
    let (mut s, c, compositor_id, shm_id, xdg_shell_id, _seat_id) =
        boot_server_and_bind_everything();
    let surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let buffer = ObjectId::new(17);
    let toplevel = ObjectId::new(19);
    make_surface_with_buffer(&mut s, c, compositor_id, shm_id, surface, pool, buffer);
    promote_to_toplevel_and_commit_red(&mut s, c, xdg_shell_id, surface, pool, buffer, toplevel);
    let window_id = s.window_id(c, toplevel).unwrap();
    let generation = s.scene_generation();

    s.begin_recomposition_batch();
    assert!(s.set_window_minimized(window_id, true));
    assert!(s.set_window_minimized(window_id, false));
    assert!(s.disconnect(c).is_some());
    assert_eq!(
        s.scene_generation(),
        generation,
        "logical mutations must not rebuild the complete scene mid-turn"
    );
    assert!(s.finish_recomposition_batch());
    assert_eq!(s.scene_generation(), generation + 1);
    assert_eq!(s.framebuffer().pixel(0, 0).unwrap(), &[0, 0, 0, 0]);

    s.begin_recomposition_batch();
    assert!(!s.finish_recomposition_batch());
    assert_eq!(s.scene_generation(), generation + 1);
}

#[test]
fn set_maximized_uses_work_area_origin_and_unmaximize_restores_origin() {
    use display_proto::events::XdgToplevelConfigure;
    use display_proto::wire::HEADER_SIZE as PROTO_HEADER_SIZE;
    use display_proto::xdg_toplevel_state;
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
    for (name, iface, bound) in [
        (1u32, "pmd_compositor", compositor_id),
        (2, "pmd_xdg_shell", xdg_shell_id),
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
        &encode_request_bytes(
            xdg_shell_id,
            1,
            &xdg_get_toplevel_payload(toplevel_id, surface_id),
        ),
    )
    .unwrap();

    s.set_taskbar_height_px(40);
    {
        let toplevel = s
            .client_mut(c)
            .unwrap()
            .toplevels
            .get_mut(&toplevel_id)
            .unwrap();
        toplevel.x = 32;
        toplevel.y = 32;
    }

    // Drain the bind events so the next pending event is the configure.
    let _ = s.drain_client_events(c);

    // Send set_maximized — should trigger a configure emit on the same client.
    s.dispatch_request(
        c,
        &encode_request_bytes(toplevel_id, 5 /* set_maximized */, &[]),
    )
    .unwrap();
    assert!(
        s.client(c)
            .unwrap()
            .toplevel(toplevel_id)
            .unwrap()
            .maximized
    );
    let maximized = s.client(c).unwrap().toplevel(toplevel_id).unwrap();
    assert_eq!((maximized.x, maximized.y), (0, 0));
    assert_eq!(maximized.restore_origin, Some((32, 32)));

    // The next event in the queue is the configure. Decode it.
    let bytes = s.drain_client_events(c).expect("client must exist");
    assert!(!bytes.is_empty(), "set_maximized must emit a configure");
    let header = display_proto::wire::MessageHeader::decode(&bytes).unwrap();
    assert_eq!(header.object_id, toplevel_id);
    assert_eq!(header.opcode, 1 /* configure */);
    let payload = &bytes[PROTO_HEADER_SIZE..header.length as usize];
    let configure = XdgToplevelConfigure::decode(payload).unwrap();
    assert_eq!(configure.width, 800);
    assert_eq!(configure.height, 560);
    assert_eq!(
        configure.states & xdg_toplevel_state::MAXIMIZED,
        xdg_toplevel_state::MAXIMIZED
    );
    assert!(configure.serial > 0);

    s.dispatch_request(
        c,
        &encode_request_bytes(toplevel_id, 6 /* unset_maximized */, &[]),
    )
    .unwrap();
    let restored = s.client(c).unwrap().toplevel(toplevel_id).unwrap();
    assert_eq!((restored.x, restored.y), (32, 32));
    assert_eq!(restored.restore_origin, None);
    let bytes = s.drain_client_events(c).unwrap();
    let header = display_proto::wire::MessageHeader::decode(&bytes).unwrap();
    let payload = &bytes[PROTO_HEADER_SIZE..header.length as usize];
    let configure = XdgToplevelConfigure::decode(payload).unwrap();
    assert_eq!(
        (configure.width, configure.height, configure.states),
        (0, 0, 0)
    );
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
    s.dispatch_request(
        c,
        &encode_request_bytes(ObjectId::DISPLAY, 2, &registry_id.raw().to_le_bytes()),
    )
    .unwrap();
    for (name, iface, bound) in [
        (1u32, "pmd_compositor", compositor_id),
        (2, "pmd_xdg_shell", xdg_shell_id),
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
        &encode_request_bytes(
            xdg_shell_id,
            1,
            &xdg_get_toplevel_payload(toplevel_id, surface_id),
        ),
    )
    .unwrap();
    let _ = s.drain_client_events(c);

    s.dispatch_request(
        c,
        &encode_request_bytes(toplevel_id, 6 /* unset_maximized */, &[]),
    )
    .unwrap();
    assert!(
        !s.client(c)
            .unwrap()
            .toplevel(toplevel_id)
            .unwrap()
            .maximized
    );

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
    s.dispatch_request(
        c,
        &encode_request_bytes(ObjectId::DISPLAY, 2, &registry_id.raw().to_le_bytes()),
    )
    .unwrap();
    for (name, iface, bound) in [
        (1u32, "pmd_compositor", compositor_id),
        (2, "pmd_xdg_shell", xdg_shell_id),
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
        &encode_request_bytes(
            xdg_shell_id,
            1,
            &xdg_get_toplevel_payload(toplevel_id, surface_id),
        ),
    )
    .unwrap();
    let initial_origin = (
        s.client(c).unwrap().toplevel(toplevel_id).unwrap().x,
        s.client(c).unwrap().toplevel(toplevel_id).unwrap().y,
    );

    // Plant the pointer at (100, 100) and send move(serial=1).
    s.inject_pointer_motion(100, 100);
    assert!(!s.is_dragging());
    let mut payload = Vec::new();
    payload.extend_from_slice(&1u32.to_le_bytes()); // serial
    s.dispatch_request(
        c,
        &encode_request_bytes(toplevel_id, 7 /* move */, &payload),
    )
    .unwrap();
    assert!(s.is_dragging(), "move request must start a drag");

    // Pointer moves to (140, 130) — toplevel origin should
    // translate by (40, 30).
    s.inject_pointer_motion(140, 130);
    let new_origin = (
        s.client(c).unwrap().toplevel(toplevel_id).unwrap().x,
        s.client(c).unwrap().toplevel(toplevel_id).unwrap().y,
    );
    assert_eq!(new_origin.0, initial_origin.0 + 40);
    assert_eq!(new_origin.1, initial_origin.1 + 30);

    // Pointer button release ends the drag.
    s.inject_pointer_button(1, display_proto::events::pointer_button_state::RELEASED);
    assert!(!s.is_dragging(), "release must end the drag");
}

#[test]
fn resize_request_emits_resizing_configures_during_drag_and_final_configure_on_release() {
    use display_proto::events::XdgToplevelConfigure;
    use display_proto::wire::HEADER_SIZE as PROTO_HEADER_SIZE;
    use display_proto::xdg_toplevel_resize_edge as edge;
    use display_proto::xdg_toplevel_state;
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
    s.dispatch_request(
        c,
        &encode_request_bytes(ObjectId::DISPLAY, 2, &registry_id.raw().to_le_bytes()),
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
        &encode_request_bytes(shm_id, 1, &shm_create_pool_payload(pool_id, 32)),
    )
    .unwrap();
    s.dispatch_request(
        c,
        &encode_request_bytes(
            pool_id,
            1,
            &shm_pool_create_buffer_payload(buffer_id, 0, 4, 2, 16, 0),
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
    s.dispatch_request(
        c,
        &encode_request_bytes(surface_id, 2, &surface_attach_payload(buffer_id, 0, 0)),
    )
    .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface_id, 7, &[]))
        .unwrap();
    let _ = s.drain_client_events(c);

    // Plant pointer + send resize(serial=1, edges=BOTTOM_RIGHT).
    s.inject_pointer_motion(50, 50);
    let _ = s.drain_client_events(c);
    let mut payload = Vec::new();
    payload.extend_from_slice(&1u32.to_le_bytes()); // serial
    payload.extend_from_slice(&edge::BOTTOM_RIGHT.to_le_bytes());
    s.dispatch_request(
        c,
        &encode_request_bytes(toplevel_id, 8 /* resize */, &payload),
    )
    .unwrap();
    assert!(s.is_dragging());
    // The dispatch shouldn't emit anything yet — drain is empty.
    assert!(s
        .drain_client_events(c)
        .map(|b| b.is_empty())
        .unwrap_or(true));

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
    assert_eq!(
        configure.states,
        xdg_toplevel_state::RESIZING | xdg_toplevel_state::ACTIVATED
    );

    // Release ends the drag + emits a final configure with
    // RESIZING clears while activation remains composed.
    s.inject_pointer_button(1, display_proto::events::pointer_button_state::RELEASED);
    assert!(!s.is_dragging());
    let bytes = s.drain_client_events(c).unwrap();
    let header = display_proto::wire::MessageHeader::decode(&bytes).unwrap();
    let p = &bytes[PROTO_HEADER_SIZE..header.length as usize];
    let configure = XdgToplevelConfigure::decode(p).unwrap();
    assert_eq!(configure.states, xdg_toplevel_state::ACTIVATED);
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
fn shell_work_area_reservation_constrains_app_configures_and_keeps_shell_at_origin() {
    use abi::cap::{Cap, CapSet};
    use display_proto::events::XdgToplevelConfigure;

    let mut s = Server::with_framebuffer_size(64, 64);
    let shell = s.accept_with_caps(CapSet::from_caps(&[Cap::Shell]));
    let shell_registry = ObjectId::new(3);
    let shell_compositor = ObjectId::new(5);
    let shell_xdg = ObjectId::new(7);
    let shell_manager = ObjectId::new(9);
    s.dispatch_request(
        shell,
        &encode_request_bytes(ObjectId::DISPLAY, 2, &shell_registry.raw().to_le_bytes()),
    )
    .unwrap();
    for (name, interface, object) in [
        (1, "pmd_compositor", shell_compositor),
        (3, "pmd_xdg_shell", shell_xdg),
        (5, "pmd_shell_manager", shell_manager),
    ] {
        s.dispatch_request(
            shell,
            &encode_request_bytes(
                shell_registry,
                1,
                &registry_bind_payload(name, interface, 1, object),
            ),
        )
        .unwrap();
    }
    let shell_surface = ObjectId::new(11);
    let shell_toplevel = ObjectId::new(13);
    s.dispatch_request(
        shell,
        &encode_request_bytes(shell_compositor, 1, &shell_surface.raw().to_le_bytes()),
    )
    .unwrap();
    s.dispatch_request(
        shell,
        &encode_request_bytes(
            shell_xdg,
            1,
            &xdg_get_toplevel_payload(shell_toplevel, shell_surface),
        ),
    )
    .unwrap();
    let _ = s.drain_client_events(shell);
    s.dispatch_request(shell, &encode_request_bytes(shell_surface, 7, &[]))
        .unwrap();
    let shell_events = s.drain_client_events(shell).unwrap();
    let shell_header = ProtoHeader::decode(&shell_events).unwrap();
    let shell_configure =
        XdgToplevelConfigure::decode(&shell_events[HEADER_SIZE..shell_header.length as usize])
            .unwrap();
    assert_eq!((shell_configure.width, shell_configure.height), (64, 64));

    s.dispatch_request(
        shell,
        &encode_request_bytes(shell_manager, 6, &8u32.to_le_bytes()),
    )
    .unwrap();
    assert_eq!(s.work_area_height(), 56);

    let app = s.accept();
    let app_registry = ObjectId::new(3);
    let app_compositor = ObjectId::new(5);
    let app_xdg = ObjectId::new(7);
    s.dispatch_request(
        app,
        &encode_request_bytes(ObjectId::DISPLAY, 2, &app_registry.raw().to_le_bytes()),
    )
    .unwrap();
    for (name, interface, object) in [
        (1, "pmd_compositor", app_compositor),
        (3, "pmd_xdg_shell", app_xdg),
    ] {
        s.dispatch_request(
            app,
            &encode_request_bytes(
                app_registry,
                1,
                &registry_bind_payload(name, interface, 1, object),
            ),
        )
        .unwrap();
    }
    let app_surface = ObjectId::new(9);
    let app_toplevel = ObjectId::new(11);
    s.dispatch_request(
        app,
        &encode_request_bytes(app_compositor, 1, &app_surface.raw().to_le_bytes()),
    )
    .unwrap();
    s.dispatch_request(
        app,
        &encode_request_bytes(
            app_xdg,
            1,
            &xdg_get_toplevel_payload(app_toplevel, app_surface),
        ),
    )
    .unwrap();
    let _ = s.drain_client_events(app);
    s.dispatch_request(app, &encode_request_bytes(app_surface, 7, &[]))
        .unwrap();
    let app_events = s.drain_client_events(app).unwrap();
    let app_header = ProtoHeader::decode(&app_events).unwrap();
    let app_configure =
        XdgToplevelConfigure::decode(&app_events[HEADER_SIZE..app_header.length as usize]).unwrap();
    assert_eq!((app_configure.width, app_configure.height), (64, 56));

    let second_surface = ObjectId::new(13);
    let second_toplevel = ObjectId::new(15);
    s.dispatch_request(
        app,
        &encode_request_bytes(app_compositor, 1, &second_surface.raw().to_le_bytes()),
    )
    .unwrap();
    s.dispatch_request(
        app,
        &encode_request_bytes(
            app_xdg,
            1,
            &xdg_get_toplevel_payload(second_toplevel, second_surface),
        ),
    )
    .unwrap();
    let _ = s.drain_client_events(app);
    s.dispatch_request(app, &encode_request_bytes(second_surface, 7, &[]))
        .unwrap();
    let second_events = s.drain_client_events(app).unwrap();
    let second_header = ProtoHeader::decode(&second_events).unwrap();
    let second_configure =
        XdgToplevelConfigure::decode(&second_events[HEADER_SIZE..second_header.length as usize])
            .unwrap();
    assert_eq!(
        (second_configure.width, second_configure.height),
        (32, 24),
        "the second app starts at (32,32), so its buffer must end above the 8px strip",
    );

    s.disconnect(shell);
    assert_eq!(s.work_area_height(), 64);
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
    use display_server::ids::ObjectId;
    use display_server::{
        drain_mouse_events_into, mouse_button_state, mouse_event_kind, MOUSE_EVENT_SIZE,
    };
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
    s.dispatch_request(
        c,
        &encode_request_bytes(ObjectId::DISPLAY, 2, &registry_id.raw().to_le_bytes()),
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
    s.dispatch_request(
        c,
        &encode_request_bytes(surface_id, 2, &surface_attach_payload(buffer_id, 0, 0)),
    )
    .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface_id, 7, &[]))
        .unwrap();

    // A button packet must be self-contained: browsers may coalesce the
    // preceding motion event, but the packed button still carries its exact
    // framebuffer position.
    let mut event = [0u8; MOUSE_EVENT_SIZE];
    event[0..4].copy_from_slice(&mouse_event_kind::BUTTON.to_le_bytes());
    event[4..8].copy_from_slice(&1i32.to_le_bytes());
    event[8..12].copy_from_slice(&1i32.to_le_bytes());
    event[12..16].copy_from_slice(&1u32.to_le_bytes());
    event[16..20].copy_from_slice(&mouse_button_state::PRESSED.to_le_bytes());
    let n = drain_mouse_events_into(&event, &mut s);
    assert_eq!(n, 1);
    assert_eq!(s.pointer_position(), (1, 1));
    // The coordinate embedded in the press hits and focuses the toplevel.
    assert_eq!(s.keyboard_focus(), Some((c, surface_id)));
}

#[test]
fn drain_mouse_events_drives_active_drag_advance() {
    // Full T133 round-trip through the input path: a client
    // sends move(serial=1), the server starts a drag, then a
    // packed motion event from /dev/input/mouse arrives and
    // drives the drag. The toplevel's origin updates.
    use display_server::ids::ObjectId;
    use display_server::{drain_mouse_events_into, mouse_event_kind, MOUSE_EVENT_SIZE};
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
    for (name, iface, bound) in [
        (1u32, "pmd_compositor", compositor_id),
        (2, "pmd_xdg_shell", xdg_shell_id),
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
        &encode_request_bytes(
            xdg_shell_id,
            1,
            &xdg_get_toplevel_payload(toplevel_id, surface_id),
        ),
    )
    .unwrap();

    // Plant pointer + send move(serial=1) — the dispatch fires
    // start_drag_from_request which captures the pointer + origin.
    let initial_origin = (
        s.client(c).unwrap().toplevel(toplevel_id).unwrap().x,
        s.client(c).unwrap().toplevel(toplevel_id).unwrap().y,
    );
    s.inject_pointer_motion(100, 100);
    let mut payload = Vec::new();
    payload.extend_from_slice(&1u32.to_le_bytes());
    s.dispatch_request(
        c,
        &encode_request_bytes(toplevel_id, 7 /* move */, &payload),
    )
    .unwrap();
    assert!(s.is_dragging());

    // A coalesced/high-rate ring batch advances to the final coordinates and
    // schedules only one complete-scene rebuild for this outer turn.
    let mut events = Vec::new();
    for offset in 1i32..=32 {
        let mut event = [0u8; MOUSE_EVENT_SIZE];
        event[0..4].copy_from_slice(&mouse_event_kind::MOTION.to_le_bytes());
        event[4..8].copy_from_slice(&(100 + offset).to_le_bytes());
        event[8..12].copy_from_slice(&(100 + offset).to_le_bytes());
        events.extend_from_slice(&event);
    }
    let generation = s.scene_generation();
    s.begin_recomposition_batch();
    let n = drain_mouse_events_into(&events, &mut s);
    assert_eq!(n, 32);
    assert_eq!(s.scene_generation(), generation);
    assert!(s.finish_recomposition_batch());
    assert_eq!(s.scene_generation(), generation + 1);
    let new_origin = (
        s.client(c).unwrap().toplevel(toplevel_id).unwrap().x,
        s.client(c).unwrap().toplevel(toplevel_id).unwrap().y,
    );
    assert_eq!(new_origin.0, initial_origin.0 + 32);
    assert_eq!(new_origin.1, initial_origin.1 + 32);
}

#[test]
fn drain_kbd_events_routes_keys_to_focused_surface() {
    use display_server::ids::ObjectId;
    use display_server::{drain_kbd_events_into, kbd_key_state, KBD_EVENT_SIZE};
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
    s.dispatch_request(
        c,
        &encode_request_bytes(ObjectId::DISPLAY, 2, &registry_id.raw().to_le_bytes()),
    )
    .unwrap();
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
    s.dispatch_request(
        c,
        &encode_request_bytes(surface_id, 2, &surface_attach_payload(buffer_id, 0, 0)),
    )
    .unwrap();
    s.dispatch_request(c, &encode_request_bytes(surface_id, 7, &[]))
        .unwrap();
    // get_keyboard payload is just the new_id.
    s.dispatch_request(
        c,
        &encode_request_bytes(
            seat_id,
            2, /* get_keyboard */
            &kbd_object_id.raw().to_le_bytes(),
        ),
    )
    .unwrap();
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
    assert!(
        !bytes.is_empty(),
        "key inject must emit a keyboard.key event"
    );
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
