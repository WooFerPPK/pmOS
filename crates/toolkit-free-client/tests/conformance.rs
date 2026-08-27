//! Principle VII native conformance layer.
//!
//! This pairs a raw client — whose only runtime dependency is
//! `display-proto` — with a real `display_server::Server` and walks the
//! production state machine over an in-memory byte stream.
//!
//! It is the fast isolation proof, not a substitute for the production
//! transport gate. `web/tests/integration/toolkit-free-client.spec.ts` must
//! also launch the WASI artifact in its own Worker and prove framebuffer,
//! input, close, and exit behavior through `/run/display`.

use display_proto::{events::key_state, wire::MessageHeader, Interface, ObjectId};
use display_server::Server;
use toolkit_free_client::{
    FreeClient, OutboundTurn, SessionDriver, SessionDriverError, SessionSignal, WriteAttempt,
    FRAME_ACCENT_RGBA, FRAME_HEIGHT, FRAME_WIDTH, INITIAL_FRAME_RGBA, INPUT_FRAME_RGBA,
    OP_BUFFER_DESTROY, OP_SHM_POOL_DESTROY, OP_SHM_POOL_WRITE, OP_SURFACE_ATTACH,
    OP_SURFACE_COMMIT, OP_SURFACE_DAMAGE, OP_SURFACE_DESTROY, OP_XDG_TOPLEVEL_ACK_CONFIGURE,
    OP_XDG_TOPLEVEL_DESTROY, OUTBOUND_WRITE_MAX_BYTES,
};

fn split_first_message(bytes: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let header = MessageHeader::decode(bytes).ok()?;
    let msg_len = header.length as usize;
    if bytes.len() < msg_len {
        return None;
    }
    Some((bytes[..msg_len].to_vec(), bytes[msg_len..].to_vec()))
}

#[derive(Default)]
struct LoopbackStream {
    pending: Vec<u8>,
}

impl LoopbackStream {
    fn accept_and_dispatch(
        &mut self,
        server: &mut Server,
        server_client_id: display_server::ClientId,
        bytes: &[u8],
    ) {
        self.pending.extend_from_slice(bytes);
        while let Some((message, rest)) = split_first_message(&self.pending) {
            let header = MessageHeader::decode(&message).expect("session request header");
            server
                .dispatch_request(server_client_id, &message)
                .unwrap_or_else(|error| panic!("raw request dispatch failed: {error:?}"));
            if header.object_id == ObjectId::DISPLAY && header.opcode == 2 {
                let registry = display_proto::requests::DisplayGetRegistry::decode(
                    &message[display_proto::HEADER_SIZE..],
                )
                .expect("get_registry payload")
                .new_id;
                server.advertise_globals_to(server_client_id, registry);
            }
            self.pending = rest;
        }
    }
}

fn write_driver_turn(
    driver: &mut SessionDriver,
    stream: &mut LoopbackStream,
    server: &mut Server,
    server_client_id: display_server::ClientId,
) -> OutboundTurn {
    write_driver_turn_capture(driver, stream, server, server_client_id).0
}

fn write_driver_turn_capture(
    driver: &mut SessionDriver,
    stream: &mut LoopbackStream,
    server: &mut Server,
    server_client_id: display_server::ClientId,
) -> (OutboundTurn, Vec<u8>) {
    let mut accepted = Vec::new();
    let turn = driver
        .write_turn(|bytes| {
            accepted.extend_from_slice(bytes);
            WriteAttempt::Written(bytes.len())
        })
        .expect("bounded outbound turn");
    stream.accept_and_dispatch(server, server_client_id, &accepted);
    (turn, accepted)
}

fn flush_driver(
    driver: &mut SessionDriver,
    stream: &mut LoopbackStream,
    server: &mut Server,
    server_client_id: display_server::ClientId,
) -> Vec<SessionSignal> {
    let mut signals = Vec::new();
    while driver.wants_write() {
        signals.extend(write_driver_turn(driver, stream, server, server_client_id).signals);
    }
    assert!(
        stream.pending.is_empty(),
        "driver left a partial request frame"
    );
    signals
}

fn drain_server(server: &mut Server, client_id: display_server::ClientId) -> Vec<u8> {
    server.drain_client_events(client_id).unwrap_or_default()
}

fn exact_pixel_count(server: &Server, rgba: [u8; 4]) -> usize {
    server
        .framebuffer()
        .pixels()
        .chunks_exact(4)
        .filter(|pixel| **pixel == rgba)
        .count()
}

fn decode_headers(bytes: &[u8]) -> Vec<MessageHeader> {
    let mut remaining = bytes.to_vec();
    let mut headers = Vec::new();
    while let Some((message, rest)) = split_first_message(&remaining) {
        headers.push(MessageHeader::decode(&message).expect("captured request header"));
        remaining = rest;
    }
    assert!(remaining.is_empty(), "captured stream ended mid-request");
    headers
}

fn configured_driver() -> (
    Server,
    display_server::ClientId,
    SessionDriver,
    LoopbackStream,
) {
    let mut server = Server::new();
    let server_client_id = server.accept();
    let mut driver = SessionDriver::new().expect("create raw session");
    let mut stream = LoopbackStream::default();

    flush_driver(&mut driver, &mut stream, &mut server, server_client_id);
    let registry_events = drain_server(&mut server, server_client_id);
    let signals = driver
        .push_server_bytes(&registry_events)
        .expect("consume registry events");
    assert!(signals.contains(&SessionSignal::GlobalsBound));

    flush_driver(&mut driver, &mut stream, &mut server, server_client_id);
    let configure_events = drain_server(&mut server, server_client_id);
    let signals = driver
        .push_server_bytes(&configure_events)
        .expect("consume configure event");
    assert!(signals
        .iter()
        .any(|signal| matches!(signal, SessionSignal::Configured { .. })));
    assert!(!driver.session().is_presented());

    (server, server_client_id, driver, stream)
}

#[test]
fn free_client_get_registry_reaches_server_dispatcher() {
    let mut server = Server::new();
    let server_client_id = server.accept();

    let mut client = FreeClient::new();
    let registry = client.get_registry().unwrap();
    assert_eq!(registry.raw(), 3);

    let wire_bytes = client.drain_outbound();
    server
        .dispatch_request(server_client_id, &wire_bytes)
        .expect("server dispatches the free-client-crafted request");

    let server_view = server.client_mut(server_client_id).unwrap();
    let journal = server_view.drain_journal();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].interface, Interface::Display);
    assert_eq!(journal[0].opcode_name, "get_registry");
    assert_eq!(journal[0].payload_len, 4);
}

#[test]
fn principle_vii_full_walk_reaches_commit_through_a_toolkit_free_client() {
    // A line-for-line match of toolkit/tests/loopback.rs's
    // `full_display_to_surface_walk_stays_in_sync_across_sides`,
    // but driven by `FreeClient` instead of `toolkit::Client`.
    // Identical server-side journal proves the toolkit is
    // optional.
    let mut server = Server::new();
    let server_client_id = server.accept();

    let mut client = FreeClient::new();
    let registry = client.get_registry().unwrap();
    let compositor = client
        .registry_bind(registry, 1, "pmd_compositor", 1)
        .unwrap();
    let surface = client.compositor_create_surface(compositor).unwrap();
    client.surface_commit(surface).unwrap();

    let mut remaining = client.drain_outbound();
    // No hand-installed objects — the server's typed-payload
    // decoders auto-install each new_id as it arrives. This
    // is the structural proof that a hand-written
    // non-toolkit client drives the server through the
    // public wire protocol alone.
    let mut dispatched = 0usize;
    while let Some((msg, rest)) = split_first_message(&remaining) {
        server
            .dispatch_request(server_client_id, &msg)
            .unwrap_or_else(|e| panic!("dispatch {dispatched} failed: {e:?}"));
        dispatched += 1;
        remaining = rest;
    }
    assert_eq!(dispatched, 4);
    assert!(remaining.is_empty());

    let server_client = server.client_mut(server_client_id).unwrap();
    let journal = server_client.drain_journal();
    assert_eq!(journal.len(), 4);
    let names: Vec<(Interface, &str)> = journal
        .iter()
        .map(|r| (r.interface, r.opcode_name))
        .collect();
    assert_eq!(
        names,
        vec![
            (Interface::Display, "get_registry"),
            (Interface::Registry, "bind"),
            (Interface::Compositor, "create_surface"),
            (Interface::Surface, "commit"),
        ]
    );

    // Object IDs the server saw match what the client sent.
    assert_eq!(journal[0].object_id, ObjectId::DISPLAY);
    assert_eq!(journal[1].object_id, registry);
    assert_eq!(journal[2].object_id, compositor);
    assert_eq!(journal[3].object_id, surface);
}

#[test]
fn attach_damage_commit_sequence_reaches_the_server_in_order() {
    // Another walk covering the attach/damage/commit triad
    // a real drawing frame would send. The server still
    // needs a surface object in its table, and the only
    // way to create one through the public protocol is via
    // compositor.create_surface. So we walk the whole
    // bind-compositor-create-surface preamble first.
    //
    // Note: `attach(NULL)` is the "detach" form that
    // bypasses the server's buffer-id validation (added
    // with the minimal compositor slice) without needing
    // to bind shm + allocate a real pool/buffer. This test
    // is about wire ordering, not buffer lifecycle — the
    // `shm_create_pool_*` conformance tests cover the
    // real buffer path.
    let mut server = Server::new();
    let server_client_id = server.accept();

    let mut client = FreeClient::new();
    let registry = client.get_registry().unwrap();
    let compositor = client
        .registry_bind(registry, 1, "pmd_compositor", 1)
        .unwrap();
    let surface = client.compositor_create_surface(compositor).unwrap();

    client
        .surface_attach(surface, ObjectId::NULL, 0, 0)
        .unwrap();
    client.surface_damage(surface, 0, 0, 32, 32).unwrap();
    client.surface_commit(surface).unwrap();

    let mut remaining = client.drain_outbound();
    let mut count = 0usize;
    while let Some((msg, rest)) = split_first_message(&remaining) {
        server.dispatch_request(server_client_id, &msg).unwrap();
        count += 1;
        remaining = rest;
    }
    // 3 preamble messages (get_registry, bind, create_surface)
    // + 3 draw messages (attach, damage, commit).
    assert_eq!(count, 6);

    let journal = server.client_mut(server_client_id).unwrap().drain_journal();
    let names: Vec<&str> = journal.iter().map(|r| r.opcode_name).collect();
    assert_eq!(
        names,
        vec![
            "get_registry",
            "bind",
            "create_surface",
            "attach",
            "damage",
            "commit",
        ]
    );
}

#[test]
fn zero_toolkit_dependency_holds_at_compile_time() {
    let manifest = include_str!("../Cargo.toml");
    let dependencies = manifest
        .split_once("[dependencies]")
        .expect("runtime dependency table")
        .1
        .lines()
        .map(str::trim)
        .take_while(|line| !line.starts_with('['))
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            line.split_once('=')
                .expect("dependency assignment")
                .0
                .trim()
        })
        .collect::<Vec<_>>();
    assert_eq!(dependencies, ["display-proto"]);
}

#[test]
fn production_protocol_session_discovers_maps_handles_input_and_closes_without_toolkit() {
    let mut server = Server::new();
    let server_client_id = server.accept();
    let mut driver = SessionDriver::new().expect("create raw session");
    let mut stream = LoopbackStream::default();

    // Round 1: the only assumed object is pmd_display. The client asks for a
    // registry and discovers the real server's numeric globals from events.
    assert!(flush_driver(&mut driver, &mut stream, &mut server, server_client_id).is_empty());
    let registry_events = drain_server(&mut server, server_client_id);
    let signals = driver
        .push_server_bytes(&registry_events)
        .expect("consume registry events");
    assert!(signals.contains(&SessionSignal::GlobalsBound));
    assert!(driver.session().missing_required_globals().is_empty());
    assert!(driver.session().objects().keyboard.is_some());

    // Round 2: bind compositor/shm/xdg/seat, create the keyboard object and
    // toplevel role, then make the empty initial commit that requests a
    // configure. The server responds on the real collapsed xdg-toplevel.
    assert!(flush_driver(&mut driver, &mut stream, &mut server, server_client_id).is_empty());
    let configure_events = drain_server(&mut server, server_client_id);
    let signals = driver
        .push_server_bytes(&configure_events)
        .expect("consume configure event");
    assert!(signals.iter().any(|signal| matches!(
        signal,
        SessionSignal::Configured {
            width,
            height,
            ..
        } if *width > 0 && *height > 0
    )));
    assert!(!signals.contains(&SessionSignal::FramePresented {
        input_response: false,
    }));

    // Round 3: ack configure, allocate a double-buffered 320x200 pool, upload
    // the distinctive frame through bounded inline writes, attach, damage,
    // and commit. The real software compositor must contain the whole shape.
    let signals = flush_driver(&mut driver, &mut stream, &mut server, server_client_id);
    assert!(signals.contains(&SessionSignal::FramePresented {
        input_response: false,
    }));
    assert!(driver.session().is_configured());
    assert!(driver.session().is_presented());
    assert!(exact_pixel_count(&server, INITIAL_FRAME_RGBA) > 45_000);
    assert!(exact_pixel_count(&server, FRAME_ACCENT_RGBA) > 5_000);
    let surface = driver.session().objects().surface.expect("surface id");
    let toplevel = driver.session().objects().toplevel.expect("toplevel id");
    assert_eq!(server.window_z_order().len(), 1);

    // The first mapped buffer owns keyboard focus. Injecting a normal key on
    // the server routes a pmd_keyboard.key event to this exact client/surface;
    // the raw client responds by uploading and attaching its second buffer.
    assert_eq!(
        server.inject_keyboard_key(0x15, key_state::PRESSED),
        Some((server_client_id, surface))
    );
    let keyboard_events = drain_server(&mut server, server_client_id);
    let signals = driver
        .push_server_bytes(&keyboard_events)
        .expect("consume keyboard event");
    assert!(signals.contains(&SessionSignal::Key {
        key: 0x15,
        state: key_state::PRESSED,
    }));
    assert!(!signals.contains(&SessionSignal::FramePresented {
        input_response: true,
    }));
    let signals = flush_driver(&mut driver, &mut stream, &mut server, server_client_id);
    assert!(signals.contains(&SessionSignal::FramePresented {
        input_response: true,
    }));
    assert!(driver.session().input_repainted());
    assert!(exact_pixel_count(&server, INPUT_FRAME_RGBA) > 45_000);
    assert_eq!(exact_pixel_count(&server, INITIAL_FRAME_RGBA), 0);

    // Finally, the production close event triggers explicit role, surface,
    // buffer, and pool destruction before the standalone process exits.
    server
        .client_mut(server_client_id)
        .expect("server client")
        .emit_xdg_toplevel_close(toplevel)
        .expect("emit close");
    let close_events = drain_server(&mut server, server_client_id);
    let signals = driver
        .push_server_bytes(&close_events)
        .expect("consume close event");
    assert!(signals.contains(&SessionSignal::CloseRequested));
    assert!(driver.session().close_requested());
    assert!(!driver.shutdown_complete());
    assert!(flush_driver(&mut driver, &mut stream, &mut server, server_client_id).is_empty());
    assert!(driver.shutdown_complete());
    assert!(server.window_z_order().is_empty());
    assert_eq!(exact_pixel_count(&server, INPUT_FRAME_RGBA), 0);
}

#[test]
fn production_session_reassembles_registry_events_split_at_every_byte_boundary() {
    let mut server = Server::new();
    let server_client_id = server.accept();
    let mut driver = SessionDriver::new().expect("create raw session");
    let mut stream = LoopbackStream::default();
    flush_driver(&mut driver, &mut stream, &mut server, server_client_id);
    let events = drain_server(&mut server, server_client_id);

    let mut observed = Vec::new();
    for byte in events {
        observed.extend(
            driver
                .push_server_bytes(&[byte])
                .expect("fragmented event stream"),
        );
    }
    assert_eq!(
        observed
            .iter()
            .filter(|signal| **signal == SessionSignal::GlobalsBound)
            .count(),
        1
    );
    assert!(driver.session().missing_required_globals().is_empty());
}

#[test]
fn production_outbound_turn_retains_eagain_and_partial_suffix_with_one_bounded_attempt() {
    let mut driver = SessionDriver::new().expect("create raw session");
    let mut first_offer = Vec::new();
    let mut calls = 0usize;
    let blocked = driver
        .write_turn(|bytes| {
            calls += 1;
            first_offer.extend_from_slice(bytes);
            assert!(bytes.len() <= OUTBOUND_WRITE_MAX_BYTES);
            WriteAttempt::WouldBlock
        })
        .expect("EAGAIN is resumable");
    assert_eq!(calls, 1);
    assert!(blocked.attempted);
    assert_eq!(blocked.written, 0);
    assert!(blocked.pending);

    let mut accepted = Vec::new();
    calls = 0;
    let partial = driver
        .write_turn(|bytes| {
            calls += 1;
            assert_eq!(bytes, first_offer);
            accepted.extend_from_slice(&bytes[..3]);
            WriteAttempt::Written(3)
        })
        .expect("partial write is resumable");
    assert_eq!(calls, 1);
    assert_eq!(partial.written, 3);
    assert!(partial.pending);

    while driver.wants_write() {
        calls = 0;
        let turn = driver
            .write_turn(|bytes| {
                calls += 1;
                assert!(bytes.len() <= OUTBOUND_WRITE_MAX_BYTES);
                accepted.extend_from_slice(bytes);
                WriteAttempt::Written(bytes.len())
            })
            .expect("remaining suffix");
        assert_eq!(calls, 1);
        assert!(turn.signals.is_empty());
    }

    let headers = decode_headers(&accepted);
    assert_eq!(headers.len(), 2);
    assert_eq!(headers[0].object_id, ObjectId::DISPLAY);
    assert_eq!(headers[0].opcode, 2);
    assert_eq!(headers[1].object_id, ObjectId::DISPLAY);
    assert_eq!(headers[1].opcode, 1);

    let mut zero_progress = SessionDriver::new().expect("second raw session");
    assert_eq!(
        zero_progress.write_turn(|_| WriteAttempt::Written(0)),
        Err(SessionDriverError::ZeroProgress)
    );
}

#[test]
fn production_driver_defers_attach_damage_commit_until_upload_batch_is_complete() {
    let (mut server, server_client_id, mut driver, mut stream) = configured_driver();
    let surface = driver.session().objects().surface.expect("surface id");
    let pool = driver.session().objects().pool.expect("pool id");
    let mut turns = Vec::new();

    loop {
        let (turn, accepted) =
            write_driver_turn_capture(&mut driver, &mut stream, &mut server, server_client_id);
        assert!(accepted.len() <= OUTBOUND_WRITE_MAX_BYTES);
        turns.push((turn.clone(), accepted));
        if turn.signals.contains(&SessionSignal::FramePresented {
            input_response: false,
        }) {
            break;
        }
        assert!(!driver.session().is_presented());
    }

    let (_, commit_bytes) = turns.pop().expect("commit turn");
    let upload_bytes = turns
        .into_iter()
        .flat_map(|(_, bytes)| bytes)
        .collect::<Vec<_>>();
    let upload_headers = decode_headers(&upload_bytes);
    assert!(upload_headers
        .iter()
        .any(|header| header.object_id == pool && header.opcode == OP_SHM_POOL_WRITE));
    assert!(!upload_headers.iter().any(|header| {
        header.object_id == surface
            && matches!(
                header.opcode,
                OP_SURFACE_ATTACH | OP_SURFACE_DAMAGE | OP_SURFACE_COMMIT
            )
    }));

    let commit_headers = decode_headers(&commit_bytes);
    assert_eq!(commit_headers.len(), 3);
    assert_eq!(commit_headers[0].object_id, surface);
    assert_eq!(commit_headers[0].opcode, OP_SURFACE_ATTACH);
    assert_eq!(commit_headers[1].object_id, surface);
    assert_eq!(commit_headers[1].opcode, OP_SURFACE_DAMAGE);
    assert_eq!(commit_headers[2].object_id, surface);
    assert_eq!(commit_headers[2].opcode, OP_SURFACE_COMMIT);
    assert!(driver.session().is_presented());
}

#[test]
fn activation_configure_after_initial_frame_batch_acks_and_session_keeps_running() {
    let (mut server, server_client_id, mut driver, mut stream) = configured_driver();
    let surface = driver.session().objects().surface.expect("surface id");
    let toplevel = driver.session().objects().toplevel.expect("toplevel id");

    // Dispatching the initial commit maps and focuses the real server window.
    // The resulting activation configure is produced after the driver's frame
    // completion has drained, just as it is between production outer-loop
    // turns, so its ACK is the only request in the next outbound batch.
    let initial_signals = flush_driver(&mut driver, &mut stream, &mut server, server_client_id);
    assert!(initial_signals.contains(&SessionSignal::FramePresented {
        input_response: false,
    }));
    let activation_events = drain_server(&mut server, server_client_id);
    let activation_signals = driver
        .push_server_bytes(&activation_events)
        .expect("consume activation configure");
    assert!(activation_signals.iter().any(|signal| matches!(
        signal,
        SessionSignal::Configured { width, height, .. }
            if *width == FRAME_WIDTH as i32 && *height == FRAME_HEIGHT as i32
    )));

    let (ack_turn, ack_bytes) =
        write_driver_turn_capture(&mut driver, &mut stream, &mut server, server_client_id);
    assert!(ack_turn.attempted);
    assert!(!ack_turn.pending);
    assert!(ack_turn.signals.is_empty());
    let ack_headers = decode_headers(&ack_bytes);
    assert_eq!(ack_headers.len(), 1);
    assert_eq!(ack_headers[0].object_id, toplevel);
    assert_eq!(ack_headers[0].opcode, OP_XDG_TOPLEVEL_ACK_CONFIGURE);

    assert_eq!(
        server.inject_keyboard_key(0x15, key_state::PRESSED),
        Some((server_client_id, surface))
    );
    let key_events = drain_server(&mut server, server_client_id);
    let key_signals = driver
        .push_server_bytes(&key_events)
        .expect("consume keyboard event after repeated configure");
    assert!(key_signals.contains(&SessionSignal::Key {
        key: 0x15,
        state: key_state::PRESSED,
    }));
    let input_signals = flush_driver(&mut driver, &mut stream, &mut server, server_client_id);
    assert!(input_signals.contains(&SessionSignal::FramePresented {
        input_response: true,
    }));
    assert!(driver.session().input_repainted());

    server
        .client_mut(server_client_id)
        .expect("server client")
        .emit_xdg_toplevel_close(toplevel)
        .expect("emit close");
    let close_events = drain_server(&mut server, server_client_id);
    let close_signals = driver
        .push_server_bytes(&close_events)
        .expect("consume close after repeated configure");
    assert!(close_signals.contains(&SessionSignal::CloseRequested));
    assert!(flush_driver(&mut driver, &mut stream, &mut server, server_client_id).is_empty());
    assert!(driver.shutdown_complete());
    assert!(server.window_z_order().is_empty());
}

#[test]
fn close_during_partial_input_upload_preserves_frame_then_teardown_and_exits_after_final_byte() {
    let (mut server, server_client_id, mut driver, mut stream) = configured_driver();
    let initial_signals = flush_driver(&mut driver, &mut stream, &mut server, server_client_id);
    assert!(initial_signals.contains(&SessionSignal::FramePresented {
        input_response: false,
    }));

    let objects = driver.session().objects().clone();
    let surface = objects.surface.expect("surface id");
    let toplevel = objects.toplevel.expect("toplevel id");
    let pool = objects.pool.expect("pool id");
    let buffers = objects.buffers.map(|buffer| buffer.expect("buffer id"));

    assert_eq!(
        server.inject_keyboard_key(0x15, key_state::PRESSED),
        Some((server_client_id, surface))
    );
    let key_events = drain_server(&mut server, server_client_id);
    let key_signals = driver
        .push_server_bytes(&key_events)
        .expect("consume keyboard event");
    assert!(key_signals.contains(&SessionSignal::Key {
        key: 0x15,
        state: key_state::PRESSED,
    }));

    server
        .client_mut(server_client_id)
        .expect("server client")
        .emit_xdg_toplevel_close(toplevel)
        .expect("emit close");
    let close_events = drain_server(&mut server, server_client_id);
    let close_signals = driver
        .push_server_bytes(&close_events)
        .expect("consume close event");
    assert!(close_signals.contains(&SessionSignal::CloseRequested));
    assert!(driver.session().close_requested());
    assert!(!driver.shutdown_complete());

    let mut accepted_wire = Vec::new();
    let mut input_frame_turn = None;
    let mut shutdown_turn = None;
    let mut last_accepted = 0usize;
    for turn_index in 0..512usize {
        if !driver.wants_write() {
            break;
        }
        let mut accepted = Vec::new();
        let turn = driver
            .write_turn(|bytes| {
                let written = if bytes.len() > 1 {
                    bytes.len().saturating_sub(1).clamp(1, 4093)
                } else {
                    1
                };
                accepted.extend_from_slice(&bytes[..written]);
                WriteAttempt::Written(written)
            })
            .expect("partial production write");
        assert!(accepted.len() <= OUTBOUND_WRITE_MAX_BYTES);
        last_accepted = accepted.len();
        accepted_wire.extend_from_slice(&accepted);
        stream.accept_and_dispatch(&mut server, server_client_id, &accepted);

        if turn.signals.contains(&SessionSignal::FramePresented {
            input_response: true,
        }) {
            input_frame_turn = Some(turn_index);
            assert!(!driver.shutdown_complete());
        }
        if driver.shutdown_complete() {
            shutdown_turn = Some(turn_index);
            break;
        }
    }

    let input_frame_turn = input_frame_turn.expect("input commit completed");
    let shutdown_turn = shutdown_turn.expect("teardown completed");
    assert!(input_frame_turn < shutdown_turn);
    assert_eq!(last_accepted, 1, "final teardown suffix must be observed");
    assert!(!driver.wants_write());
    assert!(driver.session().input_repainted());

    let headers = decode_headers(&accepted_wire);
    let last_write = headers
        .iter()
        .rposition(|header| header.object_id == pool && header.opcode == OP_SHM_POOL_WRITE)
        .expect("input pool writes");
    let attach = headers
        .iter()
        .position(|header| header.object_id == surface && header.opcode == OP_SURFACE_ATTACH)
        .expect("input attach");
    let damage = headers
        .iter()
        .position(|header| header.object_id == surface && header.opcode == OP_SURFACE_DAMAGE)
        .expect("input damage");
    let commit = headers
        .iter()
        .position(|header| header.object_id == surface && header.opcode == OP_SURFACE_COMMIT)
        .expect("input commit");
    let destroy_toplevel = headers
        .iter()
        .position(|header| header.object_id == toplevel && header.opcode == OP_XDG_TOPLEVEL_DESTROY)
        .expect("toplevel destroy");
    let destroy_surface = headers
        .iter()
        .position(|header| header.object_id == surface && header.opcode == OP_SURFACE_DESTROY)
        .expect("surface destroy");
    let destroy_first_buffer = headers
        .iter()
        .position(|header| header.object_id == buffers[0] && header.opcode == OP_BUFFER_DESTROY)
        .expect("first buffer destroy");
    let destroy_second_buffer = headers
        .iter()
        .position(|header| header.object_id == buffers[1] && header.opcode == OP_BUFFER_DESTROY)
        .expect("second buffer destroy");
    let destroy_pool = headers
        .iter()
        .position(|header| header.object_id == pool && header.opcode == OP_SHM_POOL_DESTROY)
        .expect("pool destroy");
    assert!(last_write < attach);
    assert!(attach < damage);
    assert!(damage < commit);
    assert!(commit < destroy_toplevel);
    assert!(destroy_toplevel < destroy_surface);
    assert!(destroy_surface < destroy_first_buffer);
    assert!(destroy_first_buffer < destroy_second_buffer);
    assert!(destroy_second_buffer < destroy_pool);
    assert!(server.window_z_order().is_empty());
    assert_eq!(exact_pixel_count(&server, INPUT_FRAME_RGBA), 0);
}
