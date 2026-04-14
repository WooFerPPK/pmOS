//! `shell::Session` paired with a real `display_server::Server`
//! over a **fully bidirectional** in-memory loopback.
//!
//! Every byte of every event and every request traverses
//! the real library code paths on both sides:
//!
//!   * Shell → server: `session.drain_outbound()` bytes
//!     are fed straight into `server.dispatch_request`.
//!   * Server → shell: `server.client_mut().emit_*`
//!     helpers build framed events into the client's
//!     pending queue, `server.drain_client_events`
//!     yields a flat byte buffer, and `session.pump`
//!     parses it.
//!
//! Nothing is hand-built. If any layer drifts — wire
//! format, interface tables, typed decoders on either
//! side — one of these tests fails loudly.

use display_proto::{wire::MessageHeader, Interface, ObjectId};
use display_server::Server as DisplayServerState;
use shell::Session;
use toolkit::protocol::MemoryConnection;

fn split_first_message(bytes: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let header = MessageHeader::decode(bytes).ok()?;
    let msg_len = header.length as usize;
    if bytes.len() < msg_len {
        return None;
    }
    Some((bytes[..msg_len].to_vec(), bytes[msg_len..].to_vec()))
}

fn pump_requests_into_server(
    server: &mut DisplayServerState,
    server_client_id: display_server::ClientId,
    mut bytes: Vec<u8>,
) -> usize {
    let mut n = 0;
    while let Some((msg, rest)) = split_first_message(&bytes) {
        server
            .dispatch_request(server_client_id, &msg)
            .unwrap_or_else(|e| panic!("dispatch {n}: {e:?}"));
        n += 1;
        bytes = rest;
    }
    assert!(bytes.is_empty(), "leftover bytes: {bytes:?}");
    n
}

/// Ship whatever the server has enqueued for `client_id`
/// into the paired shell session's pump, returning the
/// session's step summary. Exists as a helper so tests
/// read top-down.
fn pump_events_into_shell(
    server: &mut DisplayServerState,
    server_client_id: display_server::ClientId,
    session: &mut Session<MemoryConnection>,
) -> shell::SessionStep {
    let bytes = server
        .drain_client_events(server_client_id)
        .expect("client exists");
    if bytes.is_empty() {
        return shell::SessionStep::default();
    }
    let (step, consumed) = session.pump(&bytes).expect("pump ok");
    assert_eq!(consumed, bytes.len(), "session consumed the full stream");
    step
}

#[test]
fn shell_start_reaches_display_server_as_get_registry() {
    let mut session = Session::new(MemoryConnection::new());
    session.start().unwrap();

    let mut server = DisplayServerState::new();
    let server_client_id = server.accept();
    let bytes = session.drain_outbound();
    let dispatched = pump_requests_into_server(&mut server, server_client_id, bytes);
    assert_eq!(dispatched, 1);

    // Server's journal: one get_registry against display.
    let journal = server
        .client_mut(server_client_id)
        .unwrap()
        .drain_journal();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].interface, Interface::Display);
    assert_eq!(journal[0].opcode_name, "get_registry");
    // Server auto-installed the registry from the
    // payload.
    let registry_id = session.registry_id().unwrap();
    assert_eq!(
        server
            .client(server_client_id)
            .unwrap()
            .get(registry_id),
        Some(Interface::Registry)
    );
}

#[test]
fn shell_auto_bind_on_registry_global_reaches_the_server_as_registry_bind() {
    let mut session = Session::new(MemoryConnection::new());
    session.start().unwrap();

    let mut server = DisplayServerState::new();
    let server_client_id = server.accept();

    // 1. Ship the shell's get_registry. The server's
    //    dispatcher auto-installs the registry object on
    //    the new_id the shell allocated.
    pump_requests_into_server(
        &mut server,
        server_client_id,
        session.drain_outbound(),
    );
    server
        .client_mut(server_client_id)
        .unwrap()
        .drain_journal();

    // 2. Server advertises a compositor global via the
    //    new emit path. NO hand-built bytes: the server
    //    builds the event itself.
    let registry_id = session.registry_id().unwrap();
    server
        .client_mut(server_client_id)
        .unwrap()
        .emit_global(registry_id, 1, "pmd_compositor", 1)
        .unwrap();

    // 3. Shell pumps whatever the server enqueued. Its
    //    auto-bind fires, recording the compositor in
    //    known_globals and queueing a registry.bind
    //    request on its own outbound.
    let step = pump_events_into_shell(&mut server, server_client_id, &mut session);
    assert_eq!(step.bound, vec![Interface::Compositor]);

    // 4. Ship the shell's registry.bind back to the server.
    let bind_bytes = session.drain_outbound();
    assert!(!bind_bytes.is_empty());
    pump_requests_into_server(&mut server, server_client_id, bind_bytes);

    // 5. Server's journal picks up exactly one new entry
    //    (the bind). The object table has compositor at
    //    the id the shell chose.
    let journal = server
        .client_mut(server_client_id)
        .unwrap()
        .drain_journal();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].interface, Interface::Registry);
    assert_eq!(journal[0].opcode_name, "bind");

    let compositor_id = session.bound(Interface::Compositor).unwrap();
    assert_eq!(
        server
            .client(server_client_id)
            .unwrap()
            .get(compositor_id),
        Some(Interface::Compositor)
    );
}

#[test]
fn shell_becomes_ready_after_both_compositor_and_shm_advertised() {
    let mut session = Session::new(MemoryConnection::new());
    session.start().unwrap();

    let mut server = DisplayServerState::new();
    let server_client_id = server.accept();
    pump_requests_into_server(
        &mut server,
        server_client_id,
        session.drain_outbound(),
    );

    let registry_id = session.registry_id().unwrap();

    // Server advertises compositor first via the emit
    // path, shell auto-binds.
    server
        .client_mut(server_client_id)
        .unwrap()
        .emit_global(registry_id, 1, "pmd_compositor", 1)
        .unwrap();
    pump_events_into_shell(&mut server, server_client_id, &mut session);
    pump_requests_into_server(
        &mut server,
        server_client_id,
        session.drain_outbound(),
    );
    assert!(!session.is_ready(), "shm still missing");

    // Then shm. Same dance.
    server
        .client_mut(server_client_id)
        .unwrap()
        .emit_global(registry_id, 2, "pmd_shm", 1)
        .unwrap();
    pump_events_into_shell(&mut server, server_client_id, &mut session);
    pump_requests_into_server(
        &mut server,
        server_client_id,
        session.drain_outbound(),
    );
    assert!(session.is_ready());

    // Server's object table has both bindings at the ids
    // the shell chose.
    let compositor_id = session.bound(Interface::Compositor).unwrap();
    let shm_id = session.bound(Interface::Shm).unwrap();
    let view = server.client(server_client_id).unwrap();
    assert_eq!(view.get(compositor_id), Some(Interface::Compositor));
    assert_eq!(view.get(shm_id), Some(Interface::Shm));
}

#[test]
fn shell_ignores_uninteresting_globals_but_still_records_them() {
    let mut session = Session::new(MemoryConnection::new());
    session.start().unwrap();

    let mut server = DisplayServerState::new();
    let server_client_id = server.accept();
    pump_requests_into_server(
        &mut server,
        server_client_id,
        session.drain_outbound(),
    );

    let registry_id = session.registry_id().unwrap();
    // pmd_net is a global the v1 shell doesn't auto-bind.
    server
        .client_mut(server_client_id)
        .unwrap()
        .emit_global(registry_id, 7, "pmd_net", 1)
        .unwrap();
    let step = pump_events_into_shell(&mut server, server_client_id, &mut session);
    assert_eq!(step.discovered, vec![7]);
    assert!(step.bound.is_empty());
    // It's in known_globals with interface == None (v1
    // Interface enum doesn't include pmd_net).
    let entry = session.known_globals().get(&7).unwrap();
    assert_eq!(entry.interface_name, "pmd_net");
    assert_eq!(entry.interface, None);
    // No new outbound bytes.
    assert!(session.drain_outbound().is_empty());
    assert!(!session.is_ready());
}

#[test]
fn shell_receives_multiple_globals_in_one_drain_and_binds_in_order() {
    // Server queues several globals back-to-back via
    // emit_global and then calls drain_client_events
    // once. The shell's push_received_with_payload parses
    // the whole batch in one pump and binds every
    // interesting interface.
    let mut session = Session::new(MemoryConnection::new());
    session.start().unwrap();

    let mut server = DisplayServerState::new();
    let server_client_id = server.accept();
    pump_requests_into_server(
        &mut server,
        server_client_id,
        session.drain_outbound(),
    );

    let registry_id = session.registry_id().unwrap();
    {
        let server_client = server.client_mut(server_client_id).unwrap();
        server_client.emit_global(registry_id, 1, "pmd_compositor", 1).unwrap();
        server_client.emit_global(registry_id, 2, "pmd_shm", 1).unwrap();
        server_client.emit_global(registry_id, 3, "pmd_net", 1).unwrap();
    }

    let step = pump_events_into_shell(&mut server, server_client_id, &mut session);
    assert_eq!(step.discovered, vec![1, 2, 3]);
    assert!(step.bound.contains(&Interface::Compositor));
    assert!(step.bound.contains(&Interface::Shm));
    assert_eq!(step.bound.len(), 2);

    // Shell fired two registry.bind requests — one for
    // compositor, one for shm. Ship them.
    pump_requests_into_server(
        &mut server,
        server_client_id,
        session.drain_outbound(),
    );
    assert!(session.is_ready());
}

#[test]
fn server_emit_error_reaches_the_shell_as_protocol_error_notice() {
    let mut session = Session::new(MemoryConnection::new());
    session.start().unwrap();

    let mut server = DisplayServerState::new();
    let server_client_id = server.accept();
    pump_requests_into_server(
        &mut server,
        server_client_id,
        session.drain_outbound(),
    );

    // Server emits pmd_display.error on the display
    // object. The shell's pump surfaces it as a
    // ProtocolErrorNotice in the step.
    server
        .client_mut(server_client_id)
        .unwrap()
        .emit_error(ObjectId::new(3), 42, "malformed request")
        .unwrap();
    let step = pump_events_into_shell(&mut server, server_client_id, &mut session);
    assert_eq!(step.errors.len(), 1);
    assert_eq!(step.errors[0].object_id, ObjectId::new(3));
    assert_eq!(step.errors[0].code, 42);
    assert_eq!(step.errors[0].message, "malformed request");
}

#[test]
fn server_emit_global_remove_flips_known_global_live_to_false() {
    let mut session = Session::new(MemoryConnection::new());
    session.start().unwrap();

    let mut server = DisplayServerState::new();
    let server_client_id = server.accept();
    pump_requests_into_server(
        &mut server,
        server_client_id,
        session.drain_outbound(),
    );

    let registry_id = session.registry_id().unwrap();
    {
        let c = server.client_mut(server_client_id).unwrap();
        c.emit_global(registry_id, 5, "pmd_net", 1).unwrap();
        c.emit_global_remove(registry_id, 5).unwrap();
    }
    let step = pump_events_into_shell(&mut server, server_client_id, &mut session);
    assert_eq!(step.discovered, vec![5]);
    assert_eq!(step.removed, vec![5]);

    let entry = session.known_globals().get(&5).unwrap();
    assert!(!entry.live, "remove should flip live to false");
    assert_eq!(entry.interface_name, "pmd_net");
}

// Silence the `MessageHeader` import — only used in the
// `use` line that shipped before this slice repurposed it.
#[allow(dead_code)]
fn _keep_imports_honest(_h: MessageHeader) {}
