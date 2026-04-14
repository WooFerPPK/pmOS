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
