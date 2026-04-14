//! `shell::Session` paired with a real `display_server::Server`
//! over in-memory transports.
//!
//! Half of the flow is real: the shell's requests
//! (get_registry + registry.bind) reach the display
//! server's dispatcher and are journaled, the server's
//! object table gets auto-populated from the bind
//! payloads, and the shell's outbound queue drains
//! correctly.
//!
//! The other half — server-to-client events — is
//! simulated by hand-building event bytes with the
//! `display_proto::events` encoders, because the v1
//! display-server library does not yet have an emit
//! path. When it does (a follow-up slice adds
//! `Client::emit_event` on the server side), this test
//! will grow a genuine bidirectional loopback.

use display_proto::{
    wire::{MessageHeader, HEADER_SIZE},
    Interface, ObjectId, RegistryGlobal,
};
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

fn global_event_bytes(
    registry_id: ObjectId,
    name: u32,
    interface_name: &str,
    version: u32,
) -> Vec<u8> {
    let event = RegistryGlobal {
        name,
        interface: interface_name.to_string(),
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

    // Ship the initial get_registry and drain the
    // journal so the subsequent assertion only sees the
    // bind we care about.
    pump_requests_into_server(
        &mut server,
        server_client_id,
        session.drain_outbound(),
    );
    server
        .client_mut(server_client_id)
        .unwrap()
        .drain_journal();

    // Now hand-build a registry.global(compositor) event
    // for the shell to pump.
    let registry_id = session.registry_id().unwrap();
    let event_bytes = global_event_bytes(registry_id, 1, "pmd_compositor", 1);
    let (step, consumed) = session.pump(&event_bytes).unwrap();
    assert_eq!(consumed, event_bytes.len());
    assert_eq!(step.bound, vec![Interface::Compositor]);

    // The auto-bind queued a registry.bind request on the
    // session's connection. Ship that to the server.
    let bind_bytes = session.drain_outbound();
    assert!(!bind_bytes.is_empty());
    pump_requests_into_server(&mut server, server_client_id, bind_bytes);

    // Server's journal now has exactly one new entry —
    // the bind we just shipped.
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

    // Advertise compositor first.
    let compositor_bytes = global_event_bytes(registry_id, 1, "pmd_compositor", 1);
    session.pump(&compositor_bytes).unwrap();
    pump_requests_into_server(
        &mut server,
        server_client_id,
        session.drain_outbound(),
    );
    assert!(!session.is_ready(), "shm still missing");

    // Then shm.
    let shm_bytes = global_event_bytes(registry_id, 2, "pmd_shm", 1);
    session.pump(&shm_bytes).unwrap();
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
    let event_bytes = global_event_bytes(registry_id, 7, "pmd_net", 1);
    let (step, _) = session.pump(&event_bytes).unwrap();
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
