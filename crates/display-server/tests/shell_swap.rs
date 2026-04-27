//! T180 surface-survival guarantee: when the desktop shell client
//! disconnects, app clients' surfaces MUST NOT be destroyed.
//!
//! Models the layering test (T181) at the unit level: spawn two
//! clients (one acting as the shell, one acting as an app), each
//! creating a toplevel surface, then disconnect the shell. Assert
//! the app's toplevel still lives in the server's state and the
//! app's client is still bound.

use display_proto::{Interface, MessageHeader, ObjectId, HEADER_SIZE};
use display_server::Server;

fn frame(object_id: ObjectId, opcode: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    let h = MessageHeader::try_new(object_id, opcode, payload.len(), 0).unwrap();
    h.encode(&mut out[..HEADER_SIZE]).unwrap();
    out[HEADER_SIZE..].copy_from_slice(payload);
    out
}

fn get_registry(client_id_holder: ObjectId, registry_new_id: ObjectId) -> Vec<u8> {
    // pmd_display.get_registry(new_id) — opcode 2
    let payload = registry_new_id.raw().to_le_bytes();
    frame(client_id_holder, 2, &payload)
}

fn registry_bind(
    registry_id: ObjectId,
    name: u32,
    interface: &str,
    version: u32,
    new_id: ObjectId,
) -> Vec<u8> {
    // pmd_registry.bind(name, interface, version, new_id) — opcode 1
    let mut payload = Vec::new();
    payload.extend_from_slice(&name.to_le_bytes());
    let bytes = interface.as_bytes();
    payload.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    payload.extend_from_slice(bytes);
    let pad = (4 - bytes.len() % 4) % 4;
    payload.extend(vec![0u8; pad]);
    payload.extend_from_slice(&version.to_le_bytes());
    payload.extend_from_slice(&new_id.raw().to_le_bytes());
    frame(registry_id, 1, &payload)
}

fn create_surface(compositor_id: ObjectId, surface_new_id: ObjectId) -> Vec<u8> {
    // compositor.create_surface(new_id) — opcode 1
    let mut payload = Vec::new();
    payload.extend_from_slice(&surface_new_id.raw().to_le_bytes());
    frame(compositor_id, 1, &payload)
}

fn get_toplevel(
    xdg_shell_id: ObjectId,
    toplevel_new_id: ObjectId,
    surface_id: ObjectId,
) -> Vec<u8> {
    // xdg_shell.get_toplevel(new_id, surface_id) — opcode 1
    let mut payload = Vec::new();
    payload.extend_from_slice(&toplevel_new_id.raw().to_le_bytes());
    payload.extend_from_slice(&surface_id.raw().to_le_bytes());
    frame(xdg_shell_id, 1, &payload)
}

/// Drive a client through the registry-bind handshake.
/// Returns (compositor_id, xdg_shell_id) for this client.
/// Each client uses an id base offset to avoid id collisions
/// across the test (every client has its own object table, but
/// using distinct ids per client makes the test easier to read).
fn bind_registry(
    s: &mut Server,
    c: display_server::ClientId,
    base: u32,
) -> (ObjectId, ObjectId) {
    let registry = ObjectId::new(base);
    s.dispatch_request(c, &get_registry(ObjectId::DISPLAY, registry))
        .unwrap();
    let compositor = ObjectId::new(base + 2);
    let xdg_shell = ObjectId::new(base + 4);
    s.dispatch_request(c, &registry_bind(registry, 1, "pmd_compositor", 1, compositor))
        .unwrap();
    s.dispatch_request(c, &registry_bind(registry, 3, "pmd_xdg_shell", 1, xdg_shell))
        .unwrap();
    (compositor, xdg_shell)
}

#[test]
fn shell_disconnect_does_not_destroy_app_surfaces() {
    let mut s = Server::new();
    let shell_client = s.accept();
    let app_client = s.accept();

    let (shell_compositor, shell_xdg) = bind_registry(&mut s, shell_client, 101);
    let (app_compositor, app_xdg) = bind_registry(&mut s, app_client, 201);

    // Each client creates its own surface + toplevel.
    let shell_surface = ObjectId::new(111);
    let shell_toplevel = ObjectId::new(113);
    s.dispatch_request(shell_client, &create_surface(shell_compositor, shell_surface))
        .unwrap();
    s.dispatch_request(
        shell_client,
        &get_toplevel(shell_xdg, shell_toplevel, shell_surface),
    )
    .unwrap();

    let app_surface = ObjectId::new(211);
    let app_toplevel = ObjectId::new(213);
    s.dispatch_request(app_client, &create_surface(app_compositor, app_surface))
        .unwrap();
    s.dispatch_request(
        app_client,
        &get_toplevel(app_xdg, app_toplevel, app_surface),
    )
    .unwrap();

    // Sanity: both clients have a toplevel.
    assert!(
        s.client(shell_client)
            .unwrap()
            .toplevels
            .contains_key(&shell_toplevel),
        "shell has its toplevel"
    );
    assert!(
        s.client(app_client)
            .unwrap()
            .toplevels
            .contains_key(&app_toplevel),
        "app has its toplevel"
    );
    assert_eq!(s.client_count(), 2);

    // Disconnect the shell. The shell's own toplevel goes with it
    // (its client object is removed). The app's surfaces survive.
    s.disconnect(shell_client);

    assert_eq!(s.client_count(), 1, "only the app remains");
    assert!(s.client(shell_client).is_none(), "shell client removed");
    assert!(
        s.client(app_client).is_some(),
        "app client survives shell disconnect"
    );
    let app = s.client(app_client).unwrap();
    assert!(
        app.toplevels.contains_key(&app_toplevel),
        "app's toplevel survives shell disconnect"
    );
    assert_eq!(
        app.get(app_surface),
        Some(Interface::Surface),
        "app's surface object is still bound"
    );
}
