//! Toolkit ↔ display-server loopback integration test.
//!
//! Pairs a `toolkit::Client` with a `display_server::Server`
//! over matched in-memory byte buffers and walks a sequence
//! of real protocol operations to prove the two sides stay in
//! sync on the identical wire format from `display_proto`.
//!
//! This test is the isolation-layer version of Principle VII's
//! conformance gate: any divergence between the client-side
//! and server-side state machines fails here loudly, well
//! before the `toolkit-free-client` fixture runs in browser
//! integration tests (T109+). Bytes from the toolkit's outbound
//! queue are fed straight into the server's `dispatch_request`
//! and both sides agree on every object ID and opcode name.

use display_server::Server;
use display_server::client::HandledRequest;
use toolkit::protocol::{Client as ToolkitClient, MemoryConnection};
use toolkit::{Interface, MessageHeader, ObjectId, HEADER_SIZE};

/// Pop one framed message off the front of `bytes` and return
/// it alongside the rest of the buffer. Returns `None` if
/// `bytes` doesn't contain a full message yet.
fn split_first_message(bytes: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if bytes.len() < HEADER_SIZE {
        return None;
    }
    let header = MessageHeader::decode(bytes).ok()?;
    let msg_len = header.length as usize;
    if bytes.len() < msg_len {
        return None;
    }
    Some((bytes[..msg_len].to_vec(), bytes[msg_len..].to_vec()))
}

#[test]
fn toolkit_get_registry_reaches_the_server_dispatcher_verbatim() {
    let mut server = Server::new();
    let server_client_id = server.accept();

    let mut client = ToolkitClient::new(MemoryConnection::new());
    let registry_id = client.get_registry().unwrap();
    assert_eq!(registry_id.raw(), 3);

    let wire_bytes = client.drain_outbound();
    server
        .dispatch_request(server_client_id, &wire_bytes)
        .unwrap();

    // Drain the server's journal and assert on what it saw.
    let server_view = server.client_mut(server_client_id).unwrap();
    let journal = server_view.drain_journal();
    assert_eq!(journal.len(), 1);
    let rec = &journal[0];
    assert_eq!(rec.object_id, ObjectId::DISPLAY);
    assert_eq!(rec.interface, Interface::Display);
    assert_eq!(rec.opcode_name, "get_registry");
    // The 4-byte new_id payload was delivered.
    assert_eq!(rec.payload_len, 4);
}

#[test]
fn full_display_to_surface_walk_stays_in_sync_across_sides() {
    // After the typed-payload decoder slice, NO HAND-
    // INSTALLED OBJECTS are needed on either side. The
    // toolkit client carries new_ids in its request
    // payloads and the server's auto-install path installs
    // them verbatim. The server's journal exactly matches
    // the client's request sequence.
    let mut server = Server::new();
    let server_client_id = server.accept();

    let mut client = ToolkitClient::new(MemoryConnection::new());

    // 1. display.get_registry → registry_id allocated.
    let registry_id = client.get_registry().unwrap();

    // 2. registry.bind(compositor) → compositor_id allocated.
    //    The toolkit helper builds a full spec-compliant
    //    payload (u32 name + wire string + u32 version +
    //    u32 new_id).
    let compositor_id = client
        .registry_bind(registry_id, 1, Interface::Compositor, 1)
        .unwrap();

    // 3. compositor.create_surface → surface_id allocated.
    let surface_id = client.compositor_create_surface(compositor_id).unwrap();

    // 4. surface.commit — no payload, no new_id.
    client.surface_commit(surface_id).unwrap();

    // Feed the byte stream through the server one framed
    // message at a time — no hand-install calls.
    let mut remaining = client.drain_outbound();
    let mut dispatched = 0usize;
    while let Some((msg, rest)) = split_first_message(&remaining) {
        server
            .dispatch_request(server_client_id, &msg)
            .unwrap_or_else(|e| panic!("server dispatch failed on msg {dispatched}: {e:?}"));
        dispatched += 1;
        remaining = rest;
    }
    assert_eq!(dispatched, 4);
    assert!(remaining.is_empty());

    // Server journal matches the client's request sequence.
    let server_client = server.client_mut(server_client_id).unwrap();
    let journal: Vec<HandledRequest> = server_client.drain_journal();
    assert_eq!(journal.len(), 4);
    let names: Vec<(Interface, &str)> =
        journal.iter().map(|r| (r.interface, r.opcode_name)).collect();
    assert_eq!(
        names,
        vec![
            (Interface::Display, "get_registry"),
            (Interface::Registry, "bind"),
            (Interface::Compositor, "create_surface"),
            (Interface::Surface, "commit"),
        ]
    );

    // The auto-install path also populated the server's
    // object table: registry, compositor, and surface are
    // bound at the exact ids the client chose.
    assert_eq!(server_client.get(registry_id), Some(Interface::Registry));
    assert_eq!(server_client.get(compositor_id), Some(Interface::Compositor));
    assert_eq!(server_client.get(surface_id), Some(Interface::Surface));
}

#[test]
fn malformed_client_request_surfaces_as_a_server_error() {
    let mut server = Server::new();
    let server_client_id = server.accept();

    let mut client = ToolkitClient::new(MemoryConnection::new());
    // Toolkit won't *send* a request with a non-existent
    // opcode (its own `send_request` rejects it), so we have
    // to craft the bytes directly. A garbage 50-byte packet
    // with a valid header pointing at display.opcode=99.
    let header = MessageHeader::try_new(ObjectId::DISPLAY, 99, 0, 0).unwrap();
    let mut bytes = vec![0u8; HEADER_SIZE];
    header.encode(&mut bytes).unwrap();

    // Server rejects the malformed wire.
    let err = server
        .dispatch_request(server_client_id, &bytes)
        .unwrap_err();
    // Unknown opcode on the Display interface.
    match err {
        display_server::ServerError::Client(
            display_server::ClientError::UnknownOpcode { interface, opcode },
        ) => {
            assert_eq!(interface, Interface::Display);
            assert_eq!(opcode, 99);
        }
        other => panic!("unexpected error: {other:?}"),
    }

    // Drain client's queue too so we don't leave state behind.
    let _ = client.drain_outbound();
}
