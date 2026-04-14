//! Top-level `Server` tests.

use display_server::client::ClientError;
use display_server::ids::ObjectId;
use display_server::objects::Interface;
use display_server::server::{Server, ServerError};
use display_server::wire::{MessageHeader, WireError, HEADER_SIZE};

fn encode_request_bytes(object_id: ObjectId, opcode: u16) -> Vec<u8> {
    let mut buf = vec![0u8; HEADER_SIZE];
    let h = MessageHeader::new(object_id, opcode, 0, 0);
    h.encode(&mut buf).unwrap();
    buf
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
    // display.get_registry (opcode 2).
    let bytes = encode_request_bytes(ObjectId::DISPLAY, 2);
    s.dispatch_request(c, &bytes).unwrap();

    let client = s.client_mut(c).unwrap();
    let journal = client.drain_journal();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].opcode_name, "get_registry");
}

#[test]
fn dispatch_on_unknown_client_is_no_such_client() {
    let mut s = Server::new();
    let bytes = encode_request_bytes(ObjectId::DISPLAY, 1);
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
    // Object 99 isn't bound, so dispatch returns UnknownObject.
    let bytes = encode_request_bytes(ObjectId::new(99), 1);
    let err = s.dispatch_request(c, &bytes).unwrap_err();
    assert_eq!(
        err,
        ServerError::Client(ClientError::UnknownObject {
            id: ObjectId::new(99)
        })
    );
}

#[test]
fn multiple_clients_have_independent_object_tables() {
    let mut s = Server::new();
    let a = s.accept();
    let b = s.accept();

    // Install a registry object at ID 3 in client a only.
    s.client_mut(a)
        .unwrap()
        .install_client_object(ObjectId::new(3), Interface::Registry)
        .unwrap();

    // Client a can dispatch registry.bind.
    s.dispatch_request(a, &encode_request_bytes(ObjectId::new(3), 1))
        .unwrap();

    // Client b does NOT have object 3 — same bytes fail.
    let err = s
        .dispatch_request(b, &encode_request_bytes(ObjectId::new(3), 1))
        .unwrap_err();
    assert_eq!(
        err,
        ServerError::Client(ClientError::UnknownObject {
            id: ObjectId::new(3)
        })
    );
}
