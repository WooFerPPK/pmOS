use abi::cap::{Cap, CapSet};
use display_proto::events::{
    shell_restore_status, shell_window_state_flags, ShellRestoreFinished, ShellWindowDestroyed,
    ShellWindowSnapshotDone, ShellWindowState, XdgToplevelConfigure,
};
use display_proto::requests::{shell_restore_window_flags, MAX_SHELL_RESTORE_TIMEOUT_MS};
use display_proto::wire::{MessageHeader, HEADER_SIZE};
use display_server::{Client, ClientId, Interface, ObjectId, Server, WindowId};

fn request(object_id: ObjectId, opcode: u16, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0; HEADER_SIZE + payload.len()];
    MessageHeader::try_new(object_id, opcode, payload.len(), 0)
        .unwrap()
        .encode(&mut bytes[..HEADER_SIZE])
        .unwrap();
    bytes[HEADER_SIZE..].copy_from_slice(payload);
    bytes
}

fn bind_payload(name: u32, interface: &str, version: u32, id: ObjectId) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&name.to_le_bytes());
    payload.extend_from_slice(&(interface.len() as u32).to_le_bytes());
    payload.extend_from_slice(interface.as_bytes());
    payload.resize(payload.len() + (4 - interface.len() % 4) % 4, 0);
    payload.extend_from_slice(&version.to_le_bytes());
    payload.extend_from_slice(&id.raw().to_le_bytes());
    payload
}

fn bind_shell(server: &mut Server, pid: u32) -> (ClientId, ObjectId) {
    let shell = server
        .try_accept_with_credentials(CapSet::from_caps(&[Cap::Shell]), pid)
        .unwrap();
    let registry = ObjectId::new(3);
    let manager = ObjectId::new(5);
    server
        .dispatch_request(
            shell,
            &request(ObjectId::DISPLAY, 2, &registry.raw().to_le_bytes()),
        )
        .unwrap();
    server
        .dispatch_request(
            shell,
            &request(
                registry,
                1,
                &bind_payload(5, "pmd_shell_manager", 2, manager),
            ),
        )
        .unwrap();
    let _ = server.drain_client_events(shell);
    (shell, manager)
}

fn create_shell_window(
    server: &mut Server,
    shell: ClientId,
) -> (ObjectId, ObjectId, ObjectId, u32) {
    let registry = ObjectId::new(3);
    let compositor = ObjectId::new(7);
    let shm = ObjectId::new(9);
    let xdg = ObjectId::new(11);
    for (name, interface, id) in [
        (1, "pmd_compositor", compositor),
        (2, "pmd_shm", shm),
        (3, "pmd_xdg_shell", xdg),
    ] {
        server
            .dispatch_request(
                shell,
                &request(registry, 1, &bind_payload(name, interface, 1, id)),
            )
            .unwrap();
    }
    let surface = ObjectId::new(13);
    let pool = ObjectId::new(15);
    let buffer = ObjectId::new(17);
    let toplevel = ObjectId::new(19);
    server
        .dispatch_request(shell, &request(compositor, 1, &surface.raw().to_le_bytes()))
        .unwrap();
    let mut pool_payload = Vec::new();
    pool_payload.extend_from_slice(&pool.raw().to_le_bytes());
    pool_payload.extend_from_slice(&16u32.to_le_bytes());
    server
        .dispatch_request(shell, &request(shm, 1, &pool_payload))
        .unwrap();
    let mut buffer_payload = Vec::new();
    for value in [buffer.raw(), 0, 2, 2, 8, 0] {
        buffer_payload.extend_from_slice(&value.to_le_bytes());
    }
    server
        .dispatch_request(shell, &request(pool, 1, &buffer_payload))
        .unwrap();
    let mut role_payload = Vec::new();
    role_payload.extend_from_slice(&toplevel.raw().to_le_bytes());
    role_payload.extend_from_slice(&surface.raw().to_le_bytes());
    server
        .dispatch_request(shell, &request(xdg, 1, &role_payload))
        .unwrap();
    for pixel in server
        .client_mut(shell)
        .unwrap()
        .pool_bytes_mut(pool)
        .unwrap()
        .chunks_exact_mut(4)
    {
        pixel.copy_from_slice(&[0xff, 0, 0, 0xff]);
    }
    let window_id = server.window_id(shell, toplevel).unwrap();
    (surface, toplevel, buffer, window_id)
}

fn map_shell_window(server: &mut Server, shell: ClientId, surface: ObjectId, buffer: ObjectId) {
    let mut attach = Vec::new();
    attach.extend_from_slice(&buffer.raw().to_le_bytes());
    attach.extend_from_slice(&0i32.to_le_bytes());
    attach.extend_from_slice(&0i32.to_le_bytes());
    server
        .dispatch_request(shell, &request(surface, 2, &attach))
        .unwrap();
    server
        .dispatch_request(shell, &request(surface, 3, &[0; 16]))
        .unwrap();
    server
        .dispatch_request(shell, &request(surface, 7, &[]))
        .unwrap();
}

fn create_mapped_shell_window(server: &mut Server, shell: ClientId) -> (ObjectId, ObjectId, u32) {
    let (surface, toplevel, buffer, window_id) = create_shell_window(server, shell);
    map_shell_window(server, shell, surface, buffer);
    (surface, toplevel, window_id)
}

fn bind_app(server: &mut Server, pid: u32) -> (ClientId, ObjectId, ObjectId, ObjectId) {
    let app = server
        .try_accept_with_credentials(CapSet::EMPTY, pid)
        .unwrap();
    let registry = ObjectId::new(3);
    let compositor = ObjectId::new(5);
    let shm = ObjectId::new(7);
    let xdg = ObjectId::new(9);
    server
        .dispatch_request(
            app,
            &request(ObjectId::DISPLAY, 2, &registry.raw().to_le_bytes()),
        )
        .unwrap();
    for (name, interface, id) in [
        (1, "pmd_compositor", compositor),
        (2, "pmd_shm", shm),
        (3, "pmd_xdg_shell", xdg),
    ] {
        server
            .dispatch_request(
                app,
                &request(registry, 1, &bind_payload(name, interface, 1, id)),
            )
            .unwrap();
    }
    let _ = server.drain_client_events(app);
    (app, compositor, shm, xdg)
}

fn create_mapped_window(
    server: &mut Server,
    app: ClientId,
    compositor: ObjectId,
    shm: ObjectId,
    xdg: ObjectId,
) -> (ObjectId, ObjectId, u32) {
    let surface = ObjectId::new(11);
    let pool = ObjectId::new(13);
    let buffer = ObjectId::new(15);
    let toplevel = ObjectId::new(17);
    server
        .dispatch_request(app, &request(compositor, 1, &surface.raw().to_le_bytes()))
        .unwrap();
    let mut pool_payload = Vec::new();
    pool_payload.extend_from_slice(&pool.raw().to_le_bytes());
    pool_payload.extend_from_slice(&16u32.to_le_bytes());
    server
        .dispatch_request(app, &request(shm, 1, &pool_payload))
        .unwrap();
    let mut buffer_payload = Vec::new();
    for value in [buffer.raw(), 0, 2, 2, 8, 0] {
        buffer_payload.extend_from_slice(&value.to_le_bytes());
    }
    server
        .dispatch_request(app, &request(pool, 1, &buffer_payload))
        .unwrap();
    let mut role_payload = Vec::new();
    role_payload.extend_from_slice(&toplevel.raw().to_le_bytes());
    role_payload.extend_from_slice(&surface.raw().to_le_bytes());
    server
        .dispatch_request(app, &request(xdg, 1, &role_payload))
        .unwrap();
    for pixel in server
        .client_mut(app)
        .unwrap()
        .pool_bytes_mut(pool)
        .unwrap()
        .chunks_exact_mut(4)
    {
        pixel.copy_from_slice(&[0, 0, 0xff, 0xff]);
    }
    let mut attach = Vec::new();
    attach.extend_from_slice(&buffer.raw().to_le_bytes());
    attach.extend_from_slice(&0i32.to_le_bytes());
    attach.extend_from_slice(&0i32.to_le_bytes());
    server
        .dispatch_request(app, &request(surface, 2, &attach))
        .unwrap();
    server
        .dispatch_request(app, &request(surface, 3, &[0; 16]))
        .unwrap();
    server
        .dispatch_request(app, &request(surface, 7, &[]))
        .unwrap();
    let window_id = server.window_id(app, toplevel).unwrap();
    (surface, toplevel, window_id)
}

fn commit_buffer(
    server: &mut Server,
    app: ClientId,
    shm: ObjectId,
    surface: ObjectId,
    width: u32,
    height: u32,
) {
    let pool = ObjectId::new(19);
    let buffer = ObjectId::new(21);
    let byte_len = width.checked_mul(height).unwrap().checked_mul(4).unwrap();
    let mut pool_payload = Vec::new();
    pool_payload.extend_from_slice(&pool.raw().to_le_bytes());
    pool_payload.extend_from_slice(&byte_len.to_le_bytes());
    server
        .dispatch_request(app, &request(shm, 1, &pool_payload))
        .unwrap();
    let mut buffer_payload = Vec::new();
    for value in [buffer.raw(), 0, width, height, width * 4, 0] {
        buffer_payload.extend_from_slice(&value.to_le_bytes());
    }
    server
        .dispatch_request(app, &request(pool, 1, &buffer_payload))
        .unwrap();
    for pixel in server
        .client_mut(app)
        .unwrap()
        .pool_bytes_mut(pool)
        .unwrap()
        .chunks_exact_mut(4)
    {
        pixel.copy_from_slice(&[0, 0, 0xff, 0xff]);
    }
    let mut attach = Vec::new();
    attach.extend_from_slice(&buffer.raw().to_le_bytes());
    attach.extend_from_slice(&0i32.to_le_bytes());
    attach.extend_from_slice(&0i32.to_le_bytes());
    server
        .dispatch_request(app, &request(surface, 2, &attach))
        .unwrap();
    server
        .dispatch_request(app, &request(surface, 3, &[0; 16]))
        .unwrap();
    server
        .dispatch_request(app, &request(surface, 7, &[]))
        .unwrap();
}

fn subscribe_state(server: &mut Server, shell: ClientId, manager: ObjectId, snapshot_id: u32) {
    server
        .dispatch_request(shell, &request(manager, 9, &snapshot_id.to_le_bytes()))
        .unwrap();
}

fn begin_restore(server: &mut Server, shell: ClientId, manager: ObjectId, id: u32, ms: u32) {
    let mut payload = Vec::new();
    payload.extend_from_slice(&id.to_le_bytes());
    payload.extend_from_slice(&ms.to_le_bytes());
    server
        .dispatch_request(shell, &request(manager, 10, &payload))
        .unwrap();
}

fn string_payload(value: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(value.len() as u32).to_le_bytes());
    payload.extend_from_slice(value.as_bytes());
    payload.resize(payload.len() + (4 - value.len() % 4) % 4, 0);
    payload
}

#[allow(clippy::too_many_arguments)]
fn place(
    server: &mut Server,
    shell: ClientId,
    manager: ObjectId,
    restore_id: u32,
    window_id: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    rank: u32,
    flags: u32,
) {
    let mut payload = Vec::new();
    payload.extend_from_slice(&restore_id.to_le_bytes());
    payload.extend_from_slice(&window_id.to_le_bytes());
    payload.extend_from_slice(&x.to_le_bytes());
    payload.extend_from_slice(&y.to_le_bytes());
    payload.extend_from_slice(&width.to_le_bytes());
    payload.extend_from_slice(&height.to_le_bytes());
    payload.extend_from_slice(&rank.to_le_bytes());
    payload.extend_from_slice(&flags.to_le_bytes());
    server
        .dispatch_request(shell, &request(manager, 11, &payload))
        .unwrap();
}

fn end_restore(
    server: &mut Server,
    shell: ClientId,
    manager: ObjectId,
    restore_id: u32,
    focus: u32,
) {
    let mut payload = Vec::new();
    payload.extend_from_slice(&restore_id.to_le_bytes());
    payload.extend_from_slice(&focus.to_le_bytes());
    server
        .dispatch_request(shell, &request(manager, 12, &payload))
        .unwrap();
}

fn events(bytes: &[u8], object: ObjectId) -> Vec<(u16, &[u8])> {
    let mut decoded = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let header = MessageHeader::decode(&bytes[offset..]).unwrap();
        let end = offset + header.length as usize;
        if header.object_id == object {
            decoded.push((header.opcode, &bytes[offset + HEADER_SIZE..end]));
        }
        offset = end;
    }
    decoded
}

#[test]
fn repeated_set_maximized_preserves_authoritative_normal_geometry() {
    let mut server = Server::with_framebuffer_size(800, 600);
    let (app, compositor, shm, xdg) = bind_app(&mut server, 721);
    let (_surface, toplevel, window_id) =
        create_mapped_window(&mut server, app, compositor, shm, xdg);
    {
        let state = server
            .client_mut(app)
            .unwrap()
            .toplevels
            .get_mut(&toplevel)
            .unwrap();
        state.x = 32;
        state.y = 24;
        state.normal_x = 32;
        state.normal_y = 24;
        state.normal_width = 2;
        state.normal_height = 2;
    }

    let (shell, manager) = bind_shell(&mut server, 44);
    subscribe_state(&mut server, shell, manager, 99);
    let _ = server.drain_client_events(shell);

    for _ in 0..2 {
        server
            .dispatch_request(app, &request(toplevel, 5, &[]))
            .unwrap();
    }
    let bytes = server.drain_client_events(shell).unwrap();
    let maximized = events(&bytes, manager)
        .into_iter()
        .filter_map(|(opcode, payload)| {
            (opcode == 6)
                .then(|| ShellWindowState::decode(payload).unwrap())
                .filter(|state| state.window_id == window_id)
        })
        .next_back()
        .expect("repeated maximize must publish authoritative state");
    assert_ne!(maximized.flags & shell_window_state_flags::MAXIMIZED, 0);
    assert_eq!(
        (
            maximized.normal_x,
            maximized.normal_y,
            maximized.normal_width,
            maximized.normal_height,
        ),
        (32, 24, 2, 2)
    );

    server
        .dispatch_request(app, &request(toplevel, 6, &[]))
        .unwrap();
    let restored = server.client(app).unwrap().toplevel(toplevel).unwrap();
    assert_eq!((restored.x, restored.y), (32, 24));
    assert_eq!(
        (
            restored.normal_x,
            restored.normal_y,
            restored.normal_width,
            restored.normal_height,
        ),
        (32, 24, 2, 2)
    );
}

#[test]
fn owner_set_minimized_is_connection_scoped_and_broadcasts_global_state() {
    let mut server = Server::with_framebuffer_size(64, 64);
    let (app_a, compositor_a, shm_a, xdg_a) = bind_app(&mut server, 721);
    let (app_b, compositor_b, shm_b, xdg_b) = bind_app(&mut server, 722);
    let (surface_a, toplevel_a, window_a) =
        create_mapped_window(&mut server, app_a, compositor_a, shm_a, xdg_a);
    let (surface_b, toplevel_b, window_b) =
        create_mapped_window(&mut server, app_b, compositor_b, shm_b, xdg_b);
    assert_eq!(toplevel_a, toplevel_b, "fixture must collide local IDs");
    assert_ne!(window_a, window_b);

    let (shell, manager) = bind_shell(&mut server, 44);
    subscribe_state(&mut server, shell, manager, 99);
    let _ = server.drain_client_events(shell);

    server
        .dispatch_request(shell, &request(manager, 2, &window_a.to_le_bytes()))
        .unwrap();
    let _ = server.drain_client_events(shell);
    assert_eq!(server.keyboard_focus(), Some((app_a, surface_a)));

    let malformed = server
        .dispatch_request(app_a, &request(toplevel_a, 9, &[0]))
        .unwrap_err();
    assert!(matches!(
        malformed,
        display_server::ServerError::Client(display_server::ClientError::Malformed {
            interface: Interface::XdgToplevel,
            opcode: 9,
            error: display_proto::DecodeError::PayloadLengthMismatch {
                expected: 0,
                actual: 1,
            },
        })
    ));
    assert!(
        !server
            .client(app_a)
            .unwrap()
            .toplevel(toplevel_a)
            .unwrap()
            .minimized
    );

    server
        .dispatch_request(app_a, &request(toplevel_a, 9, &[]))
        .unwrap();

    assert!(
        server
            .client(app_a)
            .unwrap()
            .toplevel(toplevel_a)
            .unwrap()
            .minimized
    );
    assert!(
        !server
            .client(app_b)
            .unwrap()
            .toplevel(toplevel_b)
            .unwrap()
            .minimized,
        "the same local ObjectId on another connection must be untouched",
    );
    assert_eq!(
        server.keyboard_focus(),
        Some((app_b, surface_b)),
        "minimizing the active window focuses the topmost mapped survivor",
    );

    let bytes = server.drain_client_events(shell).unwrap();
    let minimized = events(&bytes, manager)
        .into_iter()
        .find_map(|(opcode, payload)| {
            (opcode == 6)
                .then(|| ShellWindowState::decode(payload).unwrap())
                .filter(|state| state.window_id == window_a)
        })
        .expect("owner minimize must publish authoritative global state");
    assert_eq!(minimized.snapshot_id, 99);
    assert_ne!(minimized.flags & shell_window_state_flags::MINIMIZED, 0);
}

#[test]
fn disconnecting_focused_client_deactivates_it_and_activates_survivor() {
    let mut server = Server::with_framebuffer_size(64, 64);
    let (app_a, compositor_a, shm_a, xdg_a) = bind_app(&mut server, 731);
    let (surface_a, toplevel_a, _) =
        create_mapped_window(&mut server, app_a, compositor_a, shm_a, xdg_a);
    let (app_b, compositor_b, shm_b, xdg_b) = bind_app(&mut server, 732);
    let (surface_b, toplevel_b, _) =
        create_mapped_window(&mut server, app_b, compositor_b, shm_b, xdg_b);
    assert_eq!(server.keyboard_focus(), Some((app_b, surface_b)));
    let _ = server.drain_client_events(app_a);
    let _ = server.drain_client_events(app_b);

    let mut removed = server.disconnect(app_b).unwrap();
    assert_eq!(server.keyboard_focus(), Some((app_a, surface_a)));

    let removed_bytes = removed.drain_pending_events();
    let removed_header = MessageHeader::decode(&removed_bytes).unwrap();
    assert_eq!(removed_header.object_id, toplevel_b);
    let deactivated =
        XdgToplevelConfigure::decode(&removed_bytes[HEADER_SIZE..removed_header.length as usize])
            .unwrap();
    assert_eq!(deactivated.states, 0);

    let survivor_bytes = server.drain_client_events(app_a).unwrap();
    let survivor_header = MessageHeader::decode(&survivor_bytes).unwrap();
    assert_eq!(survivor_header.object_id, toplevel_a);
    let activated =
        XdgToplevelConfigure::decode(&survivor_bytes[HEADER_SIZE..survivor_header.length as usize])
            .unwrap();
    assert_eq!(
        activated.states,
        display_proto::xdg_toplevel_state::ACTIVATED
    );
}

#[test]
fn hidden_normal_placement_ignores_owner_maximize_before_end() {
    let mut server = Server::with_framebuffer_size(64, 64);
    let (shell, manager) = bind_shell(&mut server, 44);
    begin_restore(&mut server, shell, manager, 71, 400);
    let (app, compositor, shm, xdg) = bind_app(&mut server, 721);
    let (_surface, toplevel, window_id) =
        create_mapped_window(&mut server, app, compositor, shm, xdg);

    place(&mut server, shell, manager, 71, window_id, 7, 9, 2, 2, 0, 0);
    let _ = server.drain_client_events(app);
    server
        .dispatch_request(app, &request(toplevel, 5, &[]))
        .unwrap();
    assert!(server.drain_client_events(app).unwrap().is_empty());
    let placed = server.client(app).unwrap().toplevel(toplevel).unwrap();
    assert!(!placed.maximized);
    assert!(placed.hidden_for_restore);

    end_restore(&mut server, shell, manager, 71, window_id);
    let revealed = server.client(app).unwrap().toplevel(toplevel).unwrap();
    assert!(!revealed.maximized);
    assert!(!revealed.hidden_for_restore);
    assert_eq!(
        (
            revealed.normal_x,
            revealed.normal_y,
            revealed.normal_width,
            revealed.normal_height,
        ),
        (7, 9, 2, 2)
    );
}

#[test]
fn hidden_maximized_placement_ignores_owner_unmaximize_before_end() {
    let mut server = Server::with_framebuffer_size(64, 64);
    let (shell, manager) = bind_shell(&mut server, 44);
    begin_restore(&mut server, shell, manager, 72, 400);
    let (app, compositor, shm, xdg) = bind_app(&mut server, 722);
    let (surface, toplevel, window_id) =
        create_mapped_window(&mut server, app, compositor, shm, xdg);

    place(
        &mut server,
        shell,
        manager,
        72,
        window_id,
        7,
        9,
        2,
        2,
        0,
        shell_restore_window_flags::MAXIMIZED,
    );
    commit_buffer(&mut server, app, shm, surface, 64, 64);
    let _ = server.drain_client_events(app);
    server
        .dispatch_request(app, &request(toplevel, 6, &[]))
        .unwrap();
    assert!(server.drain_client_events(app).unwrap().is_empty());
    let placed = server.client(app).unwrap().toplevel(toplevel).unwrap();
    assert!(placed.maximized);
    assert!(placed.hidden_for_restore);

    end_restore(&mut server, shell, manager, 72, window_id);
    let revealed = server.client(app).unwrap().toplevel(toplevel).unwrap();
    assert!(revealed.maximized);
    assert!(!revealed.hidden_for_restore);
    assert_eq!((revealed.x, revealed.y), (0, 0));
    assert_eq!(
        (
            revealed.normal_x,
            revealed.normal_y,
            revealed.normal_width,
            revealed.normal_height,
        ),
        (7, 9, 2, 2)
    );
}

#[test]
fn maximized_restore_configures_normal_baseline_before_composed_maximized_state() {
    let mut server = Server::with_framebuffer_size(64, 64);
    let (shell, manager) = bind_shell(&mut server, 44);
    begin_restore(&mut server, shell, manager, 74, 400);
    let (app, compositor, shm, xdg) = bind_app(&mut server, 724);
    let (_surface, toplevel, window_id) =
        create_mapped_window(&mut server, app, compositor, shm, xdg);
    let _ = server.drain_client_events(app);

    place(
        &mut server,
        shell,
        manager,
        74,
        window_id,
        7,
        9,
        12,
        11,
        0,
        shell_restore_window_flags::MAXIMIZED,
    );

    let bytes = server.drain_client_events(app).unwrap();
    let mut remaining = bytes.as_slice();
    let mut configures = Vec::new();
    while !remaining.is_empty() {
        let header = MessageHeader::decode(remaining).unwrap();
        let message_len = header.length as usize;
        if header.object_id == toplevel && header.opcode == 1 {
            configures
                .push(XdgToplevelConfigure::decode(&remaining[HEADER_SIZE..message_len]).unwrap());
        }
        remaining = &remaining[message_len..];
    }
    assert_eq!(configures.len(), 2);
    assert_eq!(
        (
            configures[0].width,
            configures[0].height,
            configures[0].states
        ),
        (12, 11, 0),
        "restore must establish saved normal geometry without maximized state",
    );
    assert_eq!(
        (
            configures[1].width,
            configures[1].height,
            configures[1].states
        ),
        (64, 64, display_proto::xdg_toplevel_state::MAXIMIZED),
    );
}

#[test]
fn hidden_restore_placement_ignores_owner_minimize_before_end() {
    let mut server = Server::with_framebuffer_size(64, 64);
    let (shell, manager) = bind_shell(&mut server, 44);
    begin_restore(&mut server, shell, manager, 73, 400);
    let (app, compositor, shm, xdg) = bind_app(&mut server, 723);
    let (_surface, toplevel, window_id) =
        create_mapped_window(&mut server, app, compositor, shm, xdg);

    place(&mut server, shell, manager, 73, window_id, 7, 9, 2, 2, 0, 0);
    server
        .dispatch_request(app, &request(toplevel, 9, &[]))
        .unwrap();
    let placed = server.client(app).unwrap().toplevel(toplevel).unwrap();
    assert!(!placed.minimized);
    assert!(placed.hidden_for_restore);

    end_restore(&mut server, shell, manager, 73, window_id);
    let revealed = server.client(app).unwrap().toplevel(toplevel).unwrap();
    assert!(!revealed.minimized);
    assert!(!revealed.hidden_for_restore);
}

#[test]
fn v2_snapshot_reports_authenticated_pid_nonzero_ordinal_and_terminator() {
    let mut server = Server::with_framebuffer_size(64, 64);
    let (app, compositor, shm, xdg) = bind_app(&mut server, 721);
    let (_surface, toplevel, window_id) =
        create_mapped_window(&mut server, app, compositor, shm, xdg);
    let (shell, manager) = bind_shell(&mut server, 44);
    subscribe_state(&mut server, shell, manager, 99);

    let bytes = server.drain_client_events(shell).unwrap();
    let stream = events(&bytes, manager);
    let state = stream
        .iter()
        .find_map(|(opcode, payload)| {
            (*opcode == 5).then(|| ShellWindowState::decode(payload).unwrap())
        })
        .unwrap();
    assert_eq!(state.snapshot_id, 99);
    assert_eq!(state.window_id, window_id);
    assert_eq!(state.owner_pid, 721);
    assert_eq!(state.ordinal, 1);
    assert_eq!((state.current_width, state.current_height), (2, 2));
    assert_ne!(state.flags & shell_window_state_flags::MAPPED, 0);
    assert!(stream.iter().any(|(opcode, payload)| {
        *opcode == 7
            && ShellWindowSnapshotDone::decode(payload)
                .unwrap()
                .snapshot_id
                == 99
    }));

    let _ = server.drain_client_events(shell);
    server
        .dispatch_request(app, &request(toplevel, 2, &string_payload("pmos.changed")))
        .unwrap();
    let bytes = server.drain_client_events(shell).unwrap();
    assert!(events(&bytes, manager)
        .into_iter()
        .any(|(opcode, payload)| {
            opcode == 6 && ShellWindowState::decode(payload).unwrap().app_id == "pmos.changed"
        }));
}

#[test]
fn authenticated_pid_ordinals_are_unique_across_multiple_display_connections() {
    let mut server = Server::with_framebuffer_size(64, 64);
    let (app_a, compositor_a, shm_a, xdg_a) = bind_app(&mut server, 800);
    create_mapped_window(&mut server, app_a, compositor_a, shm_a, xdg_a);
    let (app_b, compositor_b, shm_b, xdg_b) = bind_app(&mut server, 800);
    create_mapped_window(&mut server, app_b, compositor_b, shm_b, xdg_b);
    let (shell, manager) = bind_shell(&mut server, 44);
    subscribe_state(&mut server, shell, manager, 100);
    let bytes = server.drain_client_events(shell).unwrap();
    let mut ordinals = events(&bytes, manager)
        .into_iter()
        .filter(|(opcode, _)| *opcode == 5)
        .map(|(_, payload)| ShellWindowState::decode(payload).unwrap())
        .filter(|state| state.owner_pid == 800)
        .map(|state| state.ordinal)
        .collect::<Vec<_>>();
    ordinals.sort_unstable();
    assert_eq!(ordinals, vec![1, 2]);
}

#[test]
fn focusing_a_lower_window_publishes_every_displaced_authoritative_rank() {
    let mut server = Server::with_framebuffer_size(64, 64);
    let (shell, manager) = bind_shell(&mut server, 44);
    subscribe_state(&mut server, shell, manager, 101);
    let _ = server.drain_client_events(shell);

    let (app_a, compositor_a, shm_a, xdg_a) = bind_app(&mut server, 801);
    let (_surface_a, _top_a, window_a) =
        create_mapped_window(&mut server, app_a, compositor_a, shm_a, xdg_a);
    let (app_b, compositor_b, shm_b, xdg_b) = bind_app(&mut server, 802);
    let (_surface_b, _top_b, window_b) =
        create_mapped_window(&mut server, app_b, compositor_b, shm_b, xdg_b);
    let (app_c, compositor_c, shm_c, xdg_c) = bind_app(&mut server, 803);
    let (_surface_c, _top_c, window_c) =
        create_mapped_window(&mut server, app_c, compositor_c, shm_c, xdg_c);
    let _ = server.drain_client_events(shell);

    server
        .dispatch_request(shell, &request(manager, 2, &window_a.to_le_bytes()))
        .unwrap();
    assert_eq!(
        server.window_z_order(),
        &[WindowId(window_b), WindowId(window_c), WindowId(window_a)]
    );

    let bytes = server.drain_client_events(shell).unwrap();
    let ranks = events(&bytes, manager)
        .into_iter()
        .filter(|(opcode, _)| *opcode == 6)
        .map(|(_, payload)| {
            let state = ShellWindowState::decode(payload).unwrap();
            (state.window_id, state.z_rank)
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(ranks.get(&window_b), Some(&0));
    assert_eq!(ranks.get(&window_c), Some(&1));
    assert_eq!(ranks.get(&window_a), Some(&2));
    assert_eq!(ranks.len(), 3);
}

#[test]
fn removing_lower_windows_then_creating_one_preserves_authoritative_stack_order() {
    let mut server = Server::with_framebuffer_size(64, 64);
    let (shell, manager) = bind_shell(&mut server, 44);
    subscribe_state(&mut server, shell, manager, 102);
    let _ = server.drain_client_events(shell);

    let mut apps = Vec::new();
    let mut windows = Vec::new();
    for pid in 811..=815 {
        let (app, compositor, shm, xdg) = bind_app(&mut server, pid);
        let (_, _, window_id) = create_mapped_window(&mut server, app, compositor, shm, xdg);
        apps.push(app);
        windows.push(window_id);
    }
    let _ = server.drain_client_events(shell);

    let mut cached_ranks = windows
        .iter()
        .enumerate()
        .map(|(rank, window_id)| (*window_id, rank as u32))
        .collect::<std::collections::BTreeMap<_, _>>();
    for app in apps.iter().take(3) {
        server.disconnect(*app).unwrap();
    }
    let (app_f, compositor_f, shm_f, xdg_f) = bind_app(&mut server, 816);
    let (_, _, window_f) = create_mapped_window(&mut server, app_f, compositor_f, shm_f, xdg_f);

    let bytes = server.drain_client_events(shell).unwrap();
    for (opcode, payload) in events(&bytes, manager) {
        match opcode {
            2 => {
                let destroyed = ShellWindowDestroyed::decode(payload).unwrap();
                cached_ranks.remove(&destroyed.window_id);
            }
            5 | 6 => {
                let state = ShellWindowState::decode(payload).unwrap();
                cached_ranks.insert(state.window_id, state.z_rank);
            }
            _ => {}
        }
    }

    assert_eq!(
        server.window_z_order(),
        &[
            WindowId(windows[3]),
            WindowId(windows[4]),
            WindowId(window_f),
        ]
    );
    assert_eq!(cached_ranks.get(&windows[3]), Some(&0));
    assert_eq!(cached_ranks.get(&windows[4]), Some(&1));
    assert_eq!(cached_ranks.get(&window_f), Some(&2));
    assert_eq!(cached_ranks.len(), 3);
}

#[test]
fn restore_end_publishes_preexisting_windows_displaced_by_the_final_order() {
    let mut server = Server::with_framebuffer_size(64, 64);
    let (preexisting, pre_compositor, pre_shm, pre_xdg) = bind_app(&mut server, 820);
    let (_, _, window_preexisting) =
        create_mapped_window(&mut server, preexisting, pre_compositor, pre_shm, pre_xdg);
    let (shell, manager) = bind_shell(&mut server, 44);
    subscribe_state(&mut server, shell, manager, 103);
    let _ = server.drain_client_events(shell);
    begin_restore(&mut server, shell, manager, 61, 350);

    let (app_a, compositor_a, shm_a, xdg_a) = bind_app(&mut server, 821);
    let (_, _, window_a) = create_mapped_window(&mut server, app_a, compositor_a, shm_a, xdg_a);
    let (app_b, compositor_b, shm_b, xdg_b) = bind_app(&mut server, 822);
    let (_, _, window_b) = create_mapped_window(&mut server, app_b, compositor_b, shm_b, xdg_b);
    place(&mut server, shell, manager, 61, window_a, 0, 0, 2, 2, 0, 0);
    place(&mut server, shell, manager, 61, window_b, 0, 0, 2, 2, 1, 0);
    server
        .dispatch_request(
            shell,
            &request(manager, 2, &window_preexisting.to_le_bytes()),
        )
        .unwrap();
    assert_eq!(
        server.window_z_order(),
        &[
            WindowId(window_a),
            WindowId(window_b),
            WindowId(window_preexisting),
        ]
    );
    let _ = server.drain_client_events(shell);

    end_restore(&mut server, shell, manager, 61, window_a);
    assert_eq!(
        server.window_z_order(),
        &[
            WindowId(window_preexisting),
            WindowId(window_b),
            WindowId(window_a),
        ]
    );
    let bytes = server.drain_client_events(shell).unwrap();
    let ranks = events(&bytes, manager)
        .into_iter()
        .filter(|(opcode, _)| *opcode == 6)
        .map(|(_, payload)| {
            let state = ShellWindowState::decode(payload).unwrap();
            (state.window_id, state.z_rank)
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(ranks.get(&window_preexisting), Some(&0));
    assert_eq!(ranks.get(&window_b), Some(&1));
    assert_eq!(ranks.get(&window_a), Some(&2));
}

#[test]
fn cold_shell_first_buffer_map_still_takes_initial_focus() {
    let mut server = Server::with_framebuffer_size(64, 64);
    let (shell, _manager) = bind_shell(&mut server, 43);
    let (surface, top, buffer, window) = create_shell_window(&mut server, shell);

    // Production first commits without a buffer to trigger configure, then
    // waits for the selected wallpaper before attaching its first frame.
    server
        .dispatch_request(shell, &request(surface, 7, &[]))
        .unwrap();
    assert_eq!(server.keyboard_focus(), None);
    assert!(
        !server
            .client(shell)
            .unwrap()
            .toplevel(top)
            .unwrap()
            .mapped_once
    );

    map_shell_window(&mut server, shell, surface, buffer);

    assert_eq!(server.keyboard_focus(), Some((shell, surface)));
    assert_eq!(server.window_z_order(), &[WindowId(window)]);
    assert_eq!(
        server.framebuffer().pixel(0, 0).unwrap(),
        &[0xff, 0, 0, 0xff]
    );
}

#[test]
fn late_shell_first_map_preserves_restored_app_focus_stack_and_pixels() {
    let mut server = Server::with_framebuffer_size(64, 64);
    let (shell, manager) = bind_shell(&mut server, 44);
    subscribe_state(&mut server, shell, manager, 10);
    let (shell_surface, shell_top, shell_buffer, shell_window) =
        create_shell_window(&mut server, shell);
    server
        .dispatch_request(shell, &request(shell_surface, 7, &[]))
        .unwrap();
    assert!(
        !server
            .client(shell)
            .unwrap()
            .toplevel(shell_top)
            .unwrap()
            .mapped_once
    );
    let _ = server.drain_client_events(shell);

    begin_restore(&mut server, shell, manager, 55, 350);
    let (app, compositor, shm, xdg) = bind_app(&mut server, 701);
    let (app_surface, _app_top, app_window) =
        create_mapped_window(&mut server, app, compositor, shm, xdg);
    place(
        &mut server,
        shell,
        manager,
        55,
        app_window,
        0,
        0,
        64,
        64,
        0,
        shell_restore_window_flags::MAXIMIZED,
    );
    commit_buffer(&mut server, app, shm, app_surface, 64, 64);
    end_restore(&mut server, shell, manager, 55, app_window);

    assert_eq!(server.keyboard_focus(), Some((app, app_surface)));
    assert_eq!(
        server.window_z_order(),
        &[WindowId(shell_window), WindowId(app_window)]
    );
    assert_eq!(
        server.framebuffer().pixel(0, 0).unwrap(),
        &[0, 0, 0xff, 0xff]
    );
    let _ = server.drain_client_events(shell);

    // The delayed opaque shell frame must map below the restored application
    // without publishing a replacement focus event.
    map_shell_window(&mut server, shell, shell_surface, shell_buffer);

    assert!(
        server
            .client(shell)
            .unwrap()
            .toplevel(shell_top)
            .unwrap()
            .mapped_once
    );
    assert_eq!(server.keyboard_focus(), Some((app, app_surface)));
    assert_eq!(
        server.window_z_order(),
        &[WindowId(shell_window), WindowId(app_window)]
    );
    assert_eq!(
        server.framebuffer().pixel(0, 0).unwrap(),
        &[0, 0, 0xff, 0xff]
    );
    assert!(
        !events(&server.drain_client_events(shell).unwrap(), manager)
            .into_iter()
            .any(|(opcode, _)| opcode == 3)
    );
}

#[test]
fn replacement_shell_registers_and_maps_below_surviving_focused_app() {
    let mut server = Server::with_framebuffer_size(64, 64);
    let (old_shell, _old_manager) = bind_shell(&mut server, 44);
    let (_old_surface, _old_top, old_shell_window) =
        create_mapped_shell_window(&mut server, old_shell);
    let (app, compositor, shm, xdg) = bind_app(&mut server, 701);
    let (app_surface, _app_top, app_window) =
        create_mapped_window(&mut server, app, compositor, shm, xdg);

    assert_eq!(
        server.window_z_order(),
        &[WindowId(old_shell_window), WindowId(app_window)]
    );
    assert_eq!(server.keyboard_focus(), Some((app, app_surface)));
    assert_eq!(
        server.framebuffer().pixel(0, 0).unwrap(),
        &[0, 0, 0xff, 0xff]
    );

    server.disconnect(old_shell).unwrap();
    assert!(server.client(app).is_some());
    assert_eq!(server.window_z_order(), &[WindowId(app_window)]);
    assert_eq!(server.keyboard_focus(), Some((app, app_surface)));
    assert_eq!(
        server.framebuffer().pixel(0, 0).unwrap(),
        &[0, 0, 0xff, 0xff]
    );

    let (replacement, manager) = bind_shell(&mut server, 45);
    subscribe_state(&mut server, replacement, manager, 201);
    let _ = server.drain_client_events(replacement);
    let (surface, top, buffer, shell_window) = create_shell_window(&mut server, replacement);

    assert_eq!(
        server.window_z_order(),
        &[WindowId(shell_window), WindowId(app_window)]
    );
    assert_eq!(server.keyboard_focus(), Some((app, app_surface)));
    let bytes = server.drain_client_events(replacement).unwrap();
    let states = events(&bytes, manager)
        .into_iter()
        .filter(|(opcode, _)| matches!(opcode, 5 | 6))
        .map(|(opcode, payload)| (opcode, ShellWindowState::decode(payload).unwrap()))
        .collect::<Vec<_>>();
    let created_shell = states
        .iter()
        .find(|(opcode, state)| *opcode == 5 && state.window_id == shell_window)
        .expect("replacement shell creation carries authoritative rank");
    assert_eq!(created_shell.1.z_rank, 0);
    let shifted_app = states
        .iter()
        .find(|(opcode, state)| *opcode == 6 && state.window_id == app_window)
        .expect("shell base insertion republishes every shifted application rank");
    assert_eq!(shifted_app.1.z_rank, 1);
    assert_ne!(shifted_app.1.flags & shell_window_state_flags::FOCUSED, 0);

    map_shell_window(&mut server, replacement, surface, buffer);
    assert!(
        server
            .client(replacement)
            .unwrap()
            .toplevel(top)
            .unwrap()
            .mapped_once
    );
    assert_eq!(
        server.window_z_order(),
        &[WindowId(shell_window), WindowId(app_window)]
    );
    assert_eq!(server.keyboard_focus(), Some((app, app_surface)));
    assert_eq!(
        server.framebuffer().pixel(0, 0).unwrap(),
        &[0, 0, 0xff, 0xff]
    );
    let bytes = server.drain_client_events(replacement).unwrap();
    let stream = events(&bytes, manager);
    assert!(!stream.iter().any(|(opcode, _)| *opcode == 3));
    let mapped_shell = stream
        .iter()
        .filter(|(opcode, _)| *opcode == 6)
        .map(|(_, payload)| ShellWindowState::decode(payload).unwrap())
        .find(|state| state.window_id == shell_window)
        .expect("mapped replacement shell publishes its stable rank");
    assert_eq!(mapped_shell.z_rank, 0);
    assert_ne!(mapped_shell.flags & shell_window_state_flags::MAPPED, 0);
    assert_eq!(mapped_shell.flags & shell_window_state_flags::FOCUSED, 0);

    server
        .dispatch_request(
            replacement,
            &request(manager, 2, &shell_window.to_le_bytes()),
        )
        .unwrap();
    assert_eq!(server.keyboard_focus(), Some((replacement, surface)));
    assert_eq!(
        server.window_z_order(),
        &[WindowId(app_window), WindowId(shell_window)]
    );
    assert_eq!(
        server.framebuffer().pixel(0, 0).unwrap(),
        &[0xff, 0, 0, 0xff]
    );
}

#[test]
fn v2_shell_owned_flag_is_authenticated_across_shell_overlap_and_app_id_spoofing() {
    fn latest_state(bytes: &[u8], manager: ObjectId, window_id: u32) -> Option<ShellWindowState> {
        events(bytes, manager)
            .into_iter()
            .rev()
            .filter(|(opcode, _)| matches!(*opcode, 5 | 6))
            .map(|(_, payload)| ShellWindowState::decode(payload).unwrap())
            .find(|state| state.window_id == window_id)
    }

    let mut server = Server::with_framebuffer_size(64, 64);
    let (first, first_manager) = bind_shell(&mut server, 44);
    subscribe_state(&mut server, first, first_manager, 301);
    let _ = server.drain_client_events(first);
    let (_, _, first_window) = create_mapped_shell_window(&mut server, first);
    let first_bytes = server.drain_client_events(first).unwrap();
    let first_state = latest_state(&first_bytes, first_manager, first_window)
        .expect("first shell publishes its authenticated classification");
    assert_eq!(first_state.owner_pid, 44);
    assert_ne!(first_state.flags & shell_window_state_flags::SHELL_OWNED, 0);

    let (replacement, replacement_manager) = bind_shell(&mut server, 45);
    subscribe_state(&mut server, replacement, replacement_manager, 302);
    let catchup = server.drain_client_events(replacement).unwrap();
    let caught_up_first = latest_state(&catchup, replacement_manager, first_window)
        .expect("replacement catch-up includes the existing shell");
    assert_eq!(caught_up_first.owner_pid, 44);
    assert_ne!(
        caught_up_first.flags & shell_window_state_flags::SHELL_OWNED,
        0
    );

    let (_, _, replacement_window) = create_mapped_shell_window(&mut server, replacement);
    let first_overlap = server.drain_client_events(first).unwrap();
    let replacement_overlap = server.drain_client_events(replacement).unwrap();
    for (bytes, manager) in [
        (first_overlap.as_slice(), first_manager),
        (replacement_overlap.as_slice(), replacement_manager),
    ] {
        let state = latest_state(bytes, manager, replacement_window)
            .expect("both authenticated subscribers classify the replacement shell");
        assert_eq!(state.owner_pid, 45);
        assert_ne!(state.flags & shell_window_state_flags::SHELL_OWNED, 0);
    }

    let (app, compositor, shm, xdg) = bind_app(&mut server, 701);
    let (_, app_toplevel, app_window) =
        create_mapped_window(&mut server, app, compositor, shm, xdg);
    server
        .dispatch_request(
            app,
            &request(app_toplevel, 2, &string_payload("pmos.shell")),
        )
        .unwrap();
    let app_bytes = server.drain_client_events(replacement).unwrap();
    let spoof = latest_state(&app_bytes, replacement_manager, app_window)
        .expect("ordinary app metadata update publishes v2 state");
    assert_eq!(spoof.app_id, "pmos.shell");
    assert_eq!(spoof.owner_pid, 701);
    assert_eq!(spoof.flags & shell_window_state_flags::SHELL_OWNED, 0);
}

#[test]
fn replacement_shell_first_map_stays_below_surviving_app_when_focus_is_clear() {
    let mut server = Server::with_framebuffer_size(64, 64);
    let (old_shell, _old_manager) = bind_shell(&mut server, 46);
    create_mapped_shell_window(&mut server, old_shell);
    let (app, compositor, shm, xdg) = bind_app(&mut server, 702);
    let (_app_surface, _app_top, app_window) =
        create_mapped_window(&mut server, app, compositor, shm, xdg);
    server.disconnect(old_shell).unwrap();
    server.set_keyboard_focus(None);

    let (replacement, manager) = bind_shell(&mut server, 47);
    subscribe_state(&mut server, replacement, manager, 202);
    let _ = server.drain_client_events(replacement);
    let (surface, _top, buffer, shell_window) = create_shell_window(&mut server, replacement);
    let _ = server.drain_client_events(replacement);
    map_shell_window(&mut server, replacement, surface, buffer);

    assert_eq!(server.keyboard_focus(), None);
    assert_eq!(
        server.window_z_order(),
        &[WindowId(shell_window), WindowId(app_window)]
    );
    assert_eq!(
        server.framebuffer().pixel(0, 0).unwrap(),
        &[0, 0, 0xff, 0xff]
    );
    let bytes = server.drain_client_events(replacement).unwrap();
    let stream = events(&bytes, manager);
    assert!(!stream.iter().any(|(opcode, _)| *opcode == 3));
    let shell_state = stream
        .iter()
        .filter(|(opcode, _)| *opcode == 6)
        .map(|(_, payload)| ShellWindowState::decode(payload).unwrap())
        .find(|state| state.window_id == shell_window)
        .expect("unfocused replacement shell publishes mapped state");
    assert_eq!(shell_state.z_rank, 0);
    assert_eq!(shell_state.flags & shell_window_state_flags::FOCUSED, 0);
}

#[test]
fn restore_hides_first_maps_then_atomically_clamps_orders_states_and_focuses() {
    let mut server = Server::with_framebuffer_size(64, 64);
    let (shell, manager) = bind_shell(&mut server, 44);
    subscribe_state(&mut server, shell, manager, 10);
    let _ = server.drain_client_events(shell);
    begin_restore(&mut server, shell, manager, 55, 350);

    let (app_a, compositor_a, shm_a, xdg_a) = bind_app(&mut server, 701);
    let (surface_a, top_a, window_a) =
        create_mapped_window(&mut server, app_a, compositor_a, shm_a, xdg_a);
    let (app_b, compositor_b, shm_b, xdg_b) = bind_app(&mut server, 702);
    let (surface_b, top_b, window_b) =
        create_mapped_window(&mut server, app_b, compositor_b, shm_b, xdg_b);

    assert_eq!(server.keyboard_focus(), None);
    assert_eq!(server.hit_test(0, 0), None);
    assert_eq!(server.framebuffer().pixel(0, 0).unwrap(), &[0, 0, 0, 0]);
    assert!(
        server
            .client(app_a)
            .unwrap()
            .toplevel(top_a)
            .unwrap()
            .hidden_for_restore
    );

    place(
        &mut server,
        shell,
        manager,
        55,
        window_a,
        99,
        99,
        100,
        100,
        1,
        shell_restore_window_flags::MAXIMIZED,
    );
    place(
        &mut server,
        shell,
        manager,
        55,
        window_b,
        2,
        3,
        10,
        10,
        0,
        shell_restore_window_flags::MINIMIZED,
    );
    commit_buffer(&mut server, app_a, shm_a, surface_a, 64, 64);
    commit_buffer(&mut server, app_b, shm_b, surface_b, 10, 10);
    assert_eq!(server.hit_test(0, 0), None);
    end_restore(&mut server, shell, manager, 55, window_a);

    assert_eq!(server.restore_transaction_owner(), None);
    assert_eq!(server.restore_poll_timeout_ms(), None);
    assert_eq!(server.keyboard_focus(), Some((app_a, surface_a)));
    assert_eq!(
        server.window_z_order(),
        &[WindowId(window_b), WindowId(window_a)]
    );
    let top_a = server.client(app_a).unwrap().toplevel(top_a).unwrap();
    assert!(!top_a.hidden_for_restore);
    assert!(top_a.maximized);
    assert_eq!(
        (
            top_a.normal_x,
            top_a.normal_y,
            top_a.normal_width,
            top_a.normal_height
        ),
        (0, 0, 64, 64)
    );
    let top_b = server.client(app_b).unwrap().toplevel(top_b).unwrap();
    assert!(top_b.minimized);
    assert!(!top_b.hidden_for_restore);
    assert!(server.hit_test(0, 0).is_some());
    assert_eq!(
        server.framebuffer().pixel(0, 0).unwrap(),
        &[0, 0, 0xff, 0xff]
    );

    let bytes = server.drain_client_events(shell).unwrap();
    let finished = events(&bytes, manager)
        .into_iter()
        .find_map(|(opcode, payload)| {
            (opcode == 8).then(|| ShellRestoreFinished::decode(payload).unwrap())
        })
        .unwrap();
    assert_eq!(finished.restore_id, 55);
    assert_eq!(finished.status, shell_restore_status::COMPLETED);
    assert_eq!(finished.placed, 2);
}

#[test]
fn restore_end_waits_for_a_matching_post_place_commit() {
    let mut server = Server::with_framebuffer_size(64, 64);
    let (shell, manager) = bind_shell(&mut server, 44);
    subscribe_state(&mut server, shell, manager, 12);
    let _ = server.drain_client_events(shell);
    begin_restore(&mut server, shell, manager, 60, 350);
    let (app, compositor, shm, xdg) = bind_app(&mut server, 708);
    let (surface, top, window) = create_mapped_window(&mut server, app, compositor, shm, xdg);
    let _ = server.drain_client_events(shell);

    place(&mut server, shell, manager, 60, window, 4, 5, 4, 3, 0, 0);
    let bytes = server.drain_client_events(shell).unwrap();
    let placed = events(&bytes, manager)
        .into_iter()
        .find_map(|(opcode, payload)| {
            (opcode == 6).then(|| ShellWindowState::decode(payload).unwrap())
        })
        .unwrap();
    assert_ne!(placed.flags & shell_window_state_flags::MAPPED, 0);
    assert_ne!(
        placed.flags & shell_window_state_flags::HIDDEN_FOR_RESTORE,
        0
    );
    assert_eq!(
        placed.flags & shell_window_state_flags::RESTORE_PLACEMENT_APPLIED,
        0
    );

    let before_z = server.window_z_order().to_vec();
    let before_focus = server.keyboard_focus();
    let before_scene = server.scene_generation();
    let before_deadline = server.restore_poll_timeout_ms();
    end_restore(&mut server, shell, manager, 60, window);
    assert_eq!(server.window_z_order(), before_z);
    assert_eq!(server.keyboard_focus(), before_focus);
    assert_eq!(server.scene_generation(), before_scene);
    assert_eq!(server.restore_poll_timeout_ms(), before_deadline);
    assert_eq!(server.restore_transaction_owner(), Some(shell));
    assert!(
        server
            .client(app)
            .unwrap()
            .toplevel(top)
            .unwrap()
            .hidden_for_restore
    );
    assert!(
        !events(&server.drain_client_events(shell).unwrap(), manager)
            .into_iter()
            .any(|(opcode, _)| opcode == 8)
    );

    // A causal commit alone is insufficient when it retains the old default
    // buffer size. It must match the placement's effective dimensions.
    server
        .dispatch_request(app, &request(surface, 7, &[]))
        .unwrap();
    end_restore(&mut server, shell, manager, 60, window);
    assert_eq!(server.restore_transaction_owner(), Some(shell));
    assert!(
        server
            .client(app)
            .unwrap()
            .toplevel(top)
            .unwrap()
            .hidden_for_restore
    );
    let _ = server.drain_client_events(shell);

    commit_buffer(&mut server, app, shm, surface, 4, 3);
    let bytes = server.drain_client_events(shell).unwrap();
    let settled = events(&bytes, manager)
        .into_iter()
        .find_map(|(opcode, payload)| {
            (opcode == 6).then(|| ShellWindowState::decode(payload).unwrap())
        })
        .unwrap();
    assert_eq!((settled.current_width, settled.current_height), (4, 3));
    assert_ne!(
        settled.flags & shell_window_state_flags::RESTORE_PLACEMENT_APPLIED,
        0
    );

    // Settlement describes the current promoted buffer, not a historical
    // one-shot. Reattaching the old 2x2 buffer clears the bit and makes End a
    // no-op again until an exact buffer is current.
    let mut attach = Vec::new();
    attach.extend_from_slice(&ObjectId::new(15).raw().to_le_bytes());
    attach.extend_from_slice(&0i32.to_le_bytes());
    attach.extend_from_slice(&0i32.to_le_bytes());
    server
        .dispatch_request(app, &request(surface, 2, &attach))
        .unwrap();
    server
        .dispatch_request(app, &request(surface, 7, &[]))
        .unwrap();
    let bytes = server.drain_client_events(shell).unwrap();
    let regressed = events(&bytes, manager)
        .into_iter()
        .find_map(|(opcode, payload)| {
            (opcode == 6).then(|| ShellWindowState::decode(payload).unwrap())
        })
        .unwrap();
    assert_eq!((regressed.current_width, regressed.current_height), (2, 2));
    assert_eq!(
        regressed.flags & shell_window_state_flags::RESTORE_PLACEMENT_APPLIED,
        0
    );
    end_restore(&mut server, shell, manager, 60, window);
    assert_eq!(server.restore_transaction_owner(), Some(shell));
    assert!(
        !events(&server.drain_client_events(shell).unwrap(), manager)
            .into_iter()
            .any(|(opcode, _)| opcode == 8)
    );

    attach[..4].copy_from_slice(&ObjectId::new(21).raw().to_le_bytes());
    server
        .dispatch_request(app, &request(surface, 2, &attach))
        .unwrap();
    server
        .dispatch_request(app, &request(surface, 7, &[]))
        .unwrap();
    let bytes = server.drain_client_events(shell).unwrap();
    assert!(events(&bytes, manager)
        .into_iter()
        .any(|(opcode, payload)| {
            opcode == 6
                && ShellWindowState::decode(payload).unwrap().flags
                    & shell_window_state_flags::RESTORE_PLACEMENT_APPLIED
                    != 0
        }));

    end_restore(&mut server, shell, manager, 60, window);
    assert_eq!(server.restore_transaction_owner(), None);
    assert_eq!(server.restore_poll_timeout_ms(), None);
    assert!(server.keyboard_focus().is_some());
    assert!(events(&server.drain_client_events(shell).unwrap(), manager)
        .into_iter()
        .any(|(opcode, payload)| {
            opcode == 8
                && ShellRestoreFinished::decode(payload).unwrap().status
                    == shell_restore_status::COMPLETED
        }));
}

#[test]
fn zero_invalid_and_minimized_focus_targets_fall_back_to_the_restoring_shell() {
    let mut server = Server::with_framebuffer_size(64, 64);
    let (shell, manager) = bind_shell(&mut server, 44);
    let (shell_surface, _shell_top, shell_window) = create_mapped_shell_window(&mut server, shell);

    // A pre-existing ordinary window takes focus before restore. The fallback
    // must still resolve the restoring shell's own mapped toplevel rather than
    // preserving or implicitly selecting an arbitrary application.
    let (existing, existing_compositor, existing_shm, existing_xdg) = bind_app(&mut server, 709);
    create_mapped_window(
        &mut server,
        existing,
        existing_compositor,
        existing_shm,
        existing_xdg,
    );
    assert_ne!(server.keyboard_focus(), Some((shell, shell_surface)));
    let _ = server.drain_client_events(shell);

    for (index, target_kind) in [0u32, 1, 2].into_iter().enumerate() {
        let restore_id = 70 + index as u32;
        begin_restore(&mut server, shell, manager, restore_id, 350);
        let (app, compositor, shm, xdg) = bind_app(&mut server, 710 + index as u32);
        let (_surface, top, window) = create_mapped_window(&mut server, app, compositor, shm, xdg);
        let flags = if target_kind == 2 {
            shell_restore_window_flags::MINIMIZED
        } else {
            0
        };
        place(
            &mut server,
            shell,
            manager,
            restore_id,
            window,
            0,
            0,
            2,
            2,
            0,
            flags,
        );
        let focus = match target_kind {
            0 => 0,
            1 => u32::MAX,
            2 => window,
            _ => unreachable!(),
        };
        end_restore(&mut server, shell, manager, restore_id, focus);

        assert_eq!(server.keyboard_focus(), Some((shell, shell_surface)));
        assert_eq!(
            server.window_z_order().last(),
            Some(&WindowId(shell_window))
        );
        assert_eq!(
            server.framebuffer().pixel(0, 0).unwrap(),
            &[0xff, 0, 0, 0xff]
        );
        let restored = server.client(app).unwrap().toplevel(top).unwrap();
        assert!(!restored.hidden_for_restore);
        assert_eq!(restored.minimized, target_kind == 2);
        assert_eq!(server.restore_transaction_owner(), None);
        let _ = server.drain_client_events(shell);
    }
}

#[test]
fn invalid_rank_set_aborts_and_reveals_without_partial_reorder() {
    let mut server = Server::with_framebuffer_size(64, 64);
    let (shell, manager) = bind_shell(&mut server, 44);
    subscribe_state(&mut server, shell, manager, 10);
    let _ = server.drain_client_events(shell);
    begin_restore(&mut server, shell, manager, 56, 350);
    let (app, compositor, shm, xdg) = bind_app(&mut server, 703);
    let (_surface, top, window) = create_mapped_window(&mut server, app, compositor, shm, xdg);
    place(&mut server, shell, manager, 56, window, 0, 0, 2, 2, 2, 0);
    let before = server.window_z_order().to_vec();
    end_restore(&mut server, shell, manager, 56, window);
    assert_eq!(server.window_z_order(), before);
    assert!(
        !server
            .client(app)
            .unwrap()
            .toplevel(top)
            .unwrap()
            .hidden_for_restore
    );
    let bytes = server.drain_client_events(shell).unwrap();
    assert!(events(&bytes, manager)
        .into_iter()
        .any(|(opcode, payload)| {
            opcode == 8
                && ShellRestoreFinished::decode(payload).unwrap().status
                    == shell_restore_status::ABORTED
        }));
}

#[test]
fn duplicate_rank_set_aborts_before_any_reorder_or_visibility_commit() {
    let mut server = Server::with_framebuffer_size(64, 64);
    let (shell, manager) = bind_shell(&mut server, 44);
    subscribe_state(&mut server, shell, manager, 10);
    let _ = server.drain_client_events(shell);
    begin_restore(&mut server, shell, manager, 59, 350);
    let (app_a, compositor_a, shm_a, xdg_a) = bind_app(&mut server, 706);
    let (_surface_a, top_a, window_a) =
        create_mapped_window(&mut server, app_a, compositor_a, shm_a, xdg_a);
    let (app_b, compositor_b, shm_b, xdg_b) = bind_app(&mut server, 707);
    let (_surface_b, top_b, window_b) =
        create_mapped_window(&mut server, app_b, compositor_b, shm_b, xdg_b);
    place(&mut server, shell, manager, 59, window_a, 0, 0, 2, 2, 0, 0);
    place(&mut server, shell, manager, 59, window_b, 8, 8, 2, 2, 0, 0);
    let before = server.window_z_order().to_vec();
    end_restore(&mut server, shell, manager, 59, window_a);
    assert_eq!(server.window_z_order(), before);
    assert!(
        !server
            .client(app_a)
            .unwrap()
            .toplevel(top_a)
            .unwrap()
            .hidden_for_restore
    );
    assert!(
        !server
            .client(app_b)
            .unwrap()
            .toplevel(top_b)
            .unwrap()
            .hidden_for_restore
    );
    assert_eq!(server.restore_poll_timeout_ms(), None);
}

#[test]
fn timeout_and_shell_disconnect_fail_open_without_spin_or_hidden_windows() {
    let mut server = Server::with_framebuffer_size(64, 64);
    let (shell, manager) = bind_shell(&mut server, 44);
    subscribe_state(&mut server, shell, manager, 10);
    let _ = server.drain_client_events(shell);
    begin_restore(&mut server, shell, manager, 57, 5_000);
    assert_eq!(
        server.restore_poll_timeout_ms(),
        Some(u64::from(MAX_SHELL_RESTORE_TIMEOUT_MS))
    );
    let (app, compositor, shm, xdg) = bind_app(&mut server, 704);
    let (_surface, top, window) = create_mapped_window(&mut server, app, compositor, shm, xdg);
    place(&mut server, shell, manager, 57, window, 9, 10, 8, 7, 0, 0);
    assert_eq!(
        server.restore_poll_timeout_ms(),
        Some(u64::from(MAX_SHELL_RESTORE_TIMEOUT_MS)),
        "placement must not extend the absolute hard deadline",
    );
    assert!(!server.advance_monotonic_time(u64::from(MAX_SHELL_RESTORE_TIMEOUT_MS) - 1));
    assert_eq!(server.restore_poll_timeout_ms(), Some(1));
    assert!(
        server
            .client(app)
            .unwrap()
            .toplevel(top)
            .unwrap()
            .hidden_for_restore
    );
    assert!(server.advance_monotonic_time(u64::from(MAX_SHELL_RESTORE_TIMEOUT_MS)));
    assert_eq!(server.restore_poll_timeout_ms(), None);
    assert!(
        !server
            .client(app)
            .unwrap()
            .toplevel(top)
            .unwrap()
            .hidden_for_restore
    );
    let bytes = server.drain_client_events(shell).unwrap();
    let stream = events(&bytes, manager);
    let fail_open_state = stream
        .iter()
        .find_map(|(opcode, payload)| {
            matches!(*opcode, 5 | 6).then(|| ShellWindowState::decode(payload).unwrap())
        })
        .unwrap();
    assert_eq!(
        (
            fail_open_state.current_width,
            fail_open_state.current_height
        ),
        (2, 2)
    );
    assert_eq!(
        fail_open_state.flags
            & (shell_window_state_flags::HIDDEN_FOR_RESTORE
                | shell_window_state_flags::RESTORE_PLACEMENT_APPLIED),
        0
    );
    assert!(stream.iter().any(|(opcode, payload)| {
        *opcode == 8
            && ShellRestoreFinished::decode(payload).unwrap().status
                == shell_restore_status::TIMED_OUT
    }));

    begin_restore(&mut server, shell, manager, 58, 350);
    let (app_2, compositor_2, shm_2, xdg_2) = bind_app(&mut server, 705);
    let (_surface_2, top_2, _window_2) =
        create_mapped_window(&mut server, app_2, compositor_2, shm_2, xdg_2);
    assert!(
        server
            .client(app_2)
            .unwrap()
            .toplevel(top_2)
            .unwrap()
            .hidden_for_restore
    );
    server.disconnect(shell).unwrap();
    assert_eq!(server.restore_transaction_owner(), None);
    assert_eq!(server.restore_poll_timeout_ms(), None);
    assert!(
        !server
            .client(app_2)
            .unwrap()
            .toplevel(top_2)
            .unwrap()
            .hidden_for_restore
    );
}

#[test]
fn zero_restore_timeout_clamps_to_one_one_shot_wake() {
    let mut server = Server::with_framebuffer_size(64, 64);
    let (shell, manager) = bind_shell(&mut server, 44);

    begin_restore(&mut server, shell, manager, 63, 0);
    assert_eq!(server.restore_poll_timeout_ms(), Some(1));
    assert!(!server.advance_monotonic_time(0));
    assert_eq!(server.restore_poll_timeout_ms(), Some(1));
    assert!(server.advance_monotonic_time(1));
    assert_eq!(server.restore_poll_timeout_ms(), None);
    assert!(!server.advance_monotonic_time(2));

    let bytes = server.drain_client_events(shell).unwrap();
    assert!(events(&bytes, manager)
        .into_iter()
        .any(|(opcode, payload)| {
            opcode == 8
                && ShellRestoreFinished::decode(payload).unwrap().status
                    == shell_restore_status::TIMED_OUT
        }));
}

#[test]
fn v2_state_updates_coalesce_into_creation_and_keep_latest_app_id() {
    let manager = ObjectId::new(3);
    let mut client =
        Client::new_with_credentials(ClientId(1), CapSet::from_caps(&[Cap::Shell]), 44);
    client
        .install_client_object(manager, Interface::ShellManager)
        .unwrap();
    client.shell_manager_id = Some(manager);
    let mut state = ShellWindowState {
        snapshot_id: 9,
        window_id: 7,
        owner_pid: 701,
        ordinal: 1,
        current_x: 0,
        current_y: 0,
        current_width: 2,
        current_height: 2,
        normal_x: 0,
        normal_y: 0,
        normal_width: 2,
        normal_height: 2,
        flags: shell_window_state_flags::MAPPED,
        z_rank: 0,
        title: "old".into(),
        app_id: "old.app".into(),
    };
    client.emit_window_created_v2(manager, &state).unwrap();
    state.title = "new".into();
    state.app_id = "new.app".into();
    client.emit_window_state_changed(manager, &state).unwrap();
    let bytes = client.drain_pending_events();
    let stream = events(&bytes, manager);
    assert_eq!(stream.len(), 1);
    assert_eq!(stream[0].0, 5);
    assert_eq!(ShellWindowState::decode(stream[0].1).unwrap(), state);
}
