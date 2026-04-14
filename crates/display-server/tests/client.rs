//! Per-client state machine tests.

use display_server::client::{Client, ClientError, ClientId, HandledRequest};
use display_server::ids::{IdKind, ObjectId};
use display_server::objects::Interface;
use display_server::wire::MessageHeader;

fn frame_request(object_id: ObjectId, opcode: u16) -> MessageHeader {
    MessageHeader::new(object_id, opcode, 0, 0)
}

#[test]
fn new_client_has_only_the_display_object() {
    let c = Client::new(ClientId(1));
    assert_eq!(c.object_count(), 1);
    assert_eq!(c.get(ObjectId::DISPLAY), Some(Interface::Display));
    assert_eq!(c.get(ObjectId::new(3)), None);
}

#[test]
fn dispatch_display_get_registry_succeeds_and_journals() {
    let mut c = Client::new(ClientId(1));
    let header = frame_request(ObjectId::DISPLAY, 2 /* get_registry */);
    c.dispatch_request(header, &[]).unwrap();
    let journal = c.drain_journal();
    assert_eq!(journal.len(), 1);
    let r = &journal[0];
    assert_eq!(r.object_id, ObjectId::DISPLAY);
    assert_eq!(r.interface, Interface::Display);
    assert_eq!(r.opcode, 2);
    assert_eq!(r.opcode_name, "get_registry");
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
fn dispatch_full_walk_display_to_compositor_to_surface() {
    // This test walks the same sequence a real toolkit would
    // use to get from a fresh connection to a committed
    // surface, pinning the object-table shape at each step.
    let mut c = Client::new(ClientId(1));

    // Client pretends to allocate IDs 3 (registry), 5
    // (compositor), 7 (surface). Since client IDs are owned
    // by the client in production, we just install them
    // directly — the server's dispatcher trusts the client's
    // new_id arguments, subject to the partition check.
    c.install_client_object(ObjectId::new(3), Interface::Registry)
        .unwrap();
    c.install_client_object(ObjectId::new(5), Interface::Compositor)
        .unwrap();
    c.install_client_object(ObjectId::new(7), Interface::Surface)
        .unwrap();

    // display.get_registry (opcode 2) — journals against display.
    c.dispatch_request(frame_request(ObjectId::DISPLAY, 2), &[])
        .unwrap();
    // registry.bind (opcode 1) — journals against registry.
    c.dispatch_request(frame_request(ObjectId::new(3), 1), &[])
        .unwrap();
    // compositor.create_surface (opcode 1) — journals against compositor.
    c.dispatch_request(frame_request(ObjectId::new(5), 1), &[])
        .unwrap();
    // surface.commit (opcode 7) — journals against surface.
    c.dispatch_request(frame_request(ObjectId::new(7), 7), &[])
        .unwrap();

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
