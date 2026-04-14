//! Per-client state machine tests.

use display_server::client::{Client, ClientError, ClientId, HandledRequest};
use display_server::ids::{IdKind, ObjectId};
use display_server::objects::Interface;
use display_server::wire::MessageHeader;

fn frame_request(object_id: ObjectId, opcode: u16) -> MessageHeader {
    MessageHeader::new(object_id, opcode, 0, 0)
}

/// Build a 4-byte `new_id` payload, the shape
/// `display.get_registry` and `compositor.create_surface`
/// expect.
fn new_id_payload(id: ObjectId) -> Vec<u8> {
    id.raw().to_le_bytes().to_vec()
}

/// Build the `registry.bind(name, interface, version, new_id)`
/// payload per spec §4. The interface name is padded to a
/// 4-byte boundary after the length prefix.
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
fn new_client_has_only_the_display_object() {
    let c = Client::new(ClientId(1));
    assert_eq!(c.object_count(), 1);
    assert_eq!(c.get(ObjectId::DISPLAY), Some(Interface::Display));
    assert_eq!(c.get(ObjectId::new(3)), None);
}

#[test]
fn dispatch_display_get_registry_succeeds_and_auto_installs_registry() {
    let mut c = Client::new(ClientId(1));
    let registry_id = ObjectId::new(3);
    // payload_len == 4 so the header's `length` matches.
    let header = MessageHeader::try_new(ObjectId::DISPLAY, 2, 4, 0).unwrap();
    let payload = new_id_payload(registry_id);
    c.dispatch_request(header, &payload).unwrap();
    let journal = c.drain_journal();
    assert_eq!(journal.len(), 1);
    let r = &journal[0];
    assert_eq!(r.interface, Interface::Display);
    assert_eq!(r.opcode_name, "get_registry");
    assert_eq!(r.payload_len, 4);
    // Registry was auto-installed — no hand installation
    // by the test.
    assert_eq!(c.get(registry_id), Some(Interface::Registry));
}

#[test]
fn dispatch_get_registry_with_empty_payload_is_malformed() {
    let mut c = Client::new(ClientId(1));
    // Bare header, no payload → decoder reports Truncated.
    let header = frame_request(ObjectId::DISPLAY, 2);
    let err = c.dispatch_request(header, &[]).unwrap_err();
    match err {
        ClientError::Malformed {
            interface,
            opcode,
            error: _,
        } => {
            assert_eq!(interface, Interface::Display);
            assert_eq!(opcode, 2);
        }
        other => panic!("expected Malformed, got {other:?}"),
    }
    // Object table is untouched on a decode failure.
    assert_eq!(c.object_count(), 1);
}

#[test]
fn dispatch_registry_bind_auto_installs_by_interface_name() {
    let mut c = Client::new(ClientId(1));
    // First, install a registry object manually — the only
    // way the client would get one in the server's table is
    // via the get_registry auto-install we tested above.
    // Here we shortcut by calling install_client_object.
    let registry_id = ObjectId::new(3);
    c.install_client_object(registry_id, Interface::Registry)
        .unwrap();

    let compositor_id = ObjectId::new(5);
    let payload = registry_bind_payload(1, "pmd_compositor", 1, compositor_id);
    let header = MessageHeader::try_new(registry_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();

    assert_eq!(c.get(compositor_id), Some(Interface::Compositor));
}

#[test]
fn dispatch_registry_bind_with_unknown_interface_name_is_an_error() {
    let mut c = Client::new(ClientId(1));
    let registry_id = ObjectId::new(3);
    c.install_client_object(registry_id, Interface::Registry)
        .unwrap();
    // "pmd_nope" isn't an interface the server knows.
    let payload = registry_bind_payload(1, "pmd_nope", 1, ObjectId::new(5));
    let header = MessageHeader::try_new(registry_id, 1, payload.len(), 0).unwrap();
    let err = c.dispatch_request(header, &payload).unwrap_err();
    match err {
        ClientError::UnknownInterfaceName { name } => {
            assert_eq!(name, "pmd_nope");
        }
        other => panic!("expected UnknownInterfaceName, got {other:?}"),
    }
    // Object 5 was NOT installed.
    assert_eq!(c.get(ObjectId::new(5)), None);
}

#[test]
fn dispatch_on_unknown_object_returns_unknown_object() {
    let mut c = Client::new(ClientId(1));
    let header = frame_request(ObjectId::new(99), 1);
    let err = c.dispatch_request(header, &[]).unwrap_err();
    assert_eq!(
        err,
        ClientError::UnknownObject {
            id: ObjectId::new(99)
        }
    );
}

#[test]
fn dispatch_with_wrong_opcode_returns_unknown_opcode() {
    let mut c = Client::new(ClientId(1));
    // pmd_display has opcodes 1 and 2. Opcode 7 is not defined.
    let header = frame_request(ObjectId::DISPLAY, 7);
    let err = c.dispatch_request(header, &[]).unwrap_err();
    assert_eq!(
        err,
        ClientError::UnknownOpcode {
            interface: Interface::Display,
            opcode: 7,
        }
    );
}

#[test]
fn dispatch_with_event_opcode_returns_wrong_direction() {
    let mut c = Client::new(ClientId(1));
    // pmd_registry opcodes: request 1 (bind), events 1 (global)
    // and 2 (global_remove). We first need a registry object in
    // the client's table.
    c.install_client_object(ObjectId::new(3), Interface::Registry)
        .unwrap();
    // Sending "global_remove" (opcode 2, an event) as a request
    // is a direction mismatch.
    let header = frame_request(ObjectId::new(3), 2);
    let err = c.dispatch_request(header, &[]).unwrap_err();
    assert_eq!(
        err,
        ClientError::WrongDirection {
            interface: Interface::Registry,
            opcode: 2,
        }
    );
}

#[test]
fn install_client_object_refuses_server_partition_ids() {
    let mut c = Client::new(ClientId(1));
    // ID 4 is on the server side; a new_id from the client
    // should never land here.
    let err = c
        .install_client_object(ObjectId::new(4), Interface::Registry)
        .unwrap_err();
    assert_eq!(
        err,
        ClientError::IllegalBindTarget {
            requested: ObjectId::new(4)
        }
    );
    // The object table was not modified.
    assert_eq!(c.object_count(), 1);
}

#[test]
fn install_client_object_refuses_duplicates() {
    let mut c = Client::new(ClientId(1));
    c.install_client_object(ObjectId::new(3), Interface::Registry)
        .unwrap();
    let err = c
        .install_client_object(ObjectId::new(3), Interface::Compositor)
        .unwrap_err();
    assert_eq!(
        err,
        ClientError::DuplicateObject {
            id: ObjectId::new(3)
        }
    );
    // Original binding is intact.
    assert_eq!(c.get(ObjectId::new(3)), Some(Interface::Registry));
}

#[test]
fn install_server_object_hands_out_a_server_partition_id() {
    let mut c = Client::new(ClientId(1));
    let id = c.install_server_object(Interface::ShmPool).unwrap();
    assert_eq!(id.kind(), IdKind::Server);
    assert_eq!(c.get(id), Some(Interface::ShmPool));
}

#[test]
fn drop_object_removes_the_binding() {
    let mut c = Client::new(ClientId(1));
    c.install_client_object(ObjectId::new(3), Interface::Registry)
        .unwrap();
    assert_eq!(c.object_count(), 2); // display + registry

    let dropped = c.drop_object(ObjectId::new(3));
    assert!(dropped);
    assert_eq!(c.object_count(), 1);
    assert_eq!(c.get(ObjectId::new(3)), None);

    // Second drop is a no-op.
    assert!(!c.drop_object(ObjectId::new(3)));
}

#[test]
fn dispatch_full_walk_display_to_compositor_to_surface_via_auto_install() {
    // Walks the same sequence a real client uses from a
    // fresh connection all the way to `surface.commit`, WITH
    // NO HAND-INSTALLED OBJECTS. Every new_id binding is
    // driven by the server's payload decoders.
    let mut c = Client::new(ClientId(1));

    let registry_id = ObjectId::new(3);
    let compositor_id = ObjectId::new(5);
    let surface_id = ObjectId::new(7);

    // 1. display.get_registry(registry_id) — auto-installs
    //    registry_id as Interface::Registry.
    let payload = new_id_payload(registry_id);
    let header = MessageHeader::try_new(ObjectId::DISPLAY, 2, payload.len(), 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();
    assert_eq!(c.get(registry_id), Some(Interface::Registry));

    // 2. registry.bind(name=1, "pmd_compositor", 1, compositor_id).
    let payload = registry_bind_payload(1, "pmd_compositor", 1, compositor_id);
    let header = MessageHeader::try_new(registry_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();
    assert_eq!(c.get(compositor_id), Some(Interface::Compositor));

    // 3. compositor.create_surface(surface_id).
    let payload = new_id_payload(surface_id);
    let header = MessageHeader::try_new(compositor_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();
    assert_eq!(c.get(surface_id), Some(Interface::Surface));

    // 4. surface.commit — no payload, no new_id.
    let header = frame_request(surface_id, 7);
    c.dispatch_request(header, &[]).unwrap();

    let journal = c.drain_journal();
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
    let _ = HandledRequest {
        object_id: ObjectId::NULL,
        interface: Interface::Display,
        opcode: 0,
        opcode_name: "",
        payload_len: 0,
        fd_passing: 0,
    };
}
