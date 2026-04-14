//! Toolkit client protocol isolation tests.
//!
//! Drives `toolkit::Client` over an in-memory `MemoryConnection`
//! and verifies the state machine in isolation. A companion
//! integration test (`tests/loopback.rs`) pairs a
//! toolkit `Client` with a `display_server::Server` over
//! matched in-memory transports and walks a full
//! bind/create/commit sequence to prove the two sides stay
//! in sync on the identical wire format from `display-proto`.

use toolkit::protocol::{Client, ClientError, MemoryConnection};
use toolkit::{HEADER_SIZE, Interface, MessageHeader, ObjectId};

fn boot() -> Client<MemoryConnection> {
    Client::new(MemoryConnection::new())
}

#[test]
fn new_client_has_display_pre_bound_and_nothing_else() {
    let c = boot();
    assert_eq!(c.object_count(), 1);
    assert_eq!(c.get(ObjectId::DISPLAY), Some(Interface::Display));
    assert_eq!(c.get(ObjectId::new(3)), None);
}

#[test]
fn allocate_id_starts_at_three_and_hands_out_odd_ids() {
    let mut c = boot();
    let a = c.allocate_id().unwrap();
    let b = c.allocate_id().unwrap();
    let d = c.allocate_id().unwrap();
    assert_eq!(a.raw(), 3);
    assert_eq!(b.raw(), 5);
    assert_eq!(d.raw(), 7);
}

#[test]
fn bind_new_allocates_and_binds_in_one_shot() {
    let mut c = boot();
    let id = c.bind_new(Interface::Registry).unwrap();
    assert_eq!(id.raw(), 3);
    assert_eq!(c.get(id), Some(Interface::Registry));
    assert_eq!(c.object_count(), 2);
}

#[test]
fn bind_server_id_refuses_client_partition_ids() {
    let mut c = boot();
    let err = c
        .bind_server_id(ObjectId::new(3), Interface::Compositor)
        .unwrap_err();
    assert_eq!(
        err,
        ClientError::IllegalBindTarget {
            requested: ObjectId::new(3)
        }
    );
}

#[test]
fn bind_server_id_accepts_even_ids() {
    let mut c = boot();
    c.bind_server_id(ObjectId::new(2), Interface::Registry)
        .unwrap();
    assert_eq!(c.get(ObjectId::new(2)), Some(Interface::Registry));
}

#[test]
fn drop_object_removes_binding_and_second_drop_is_false() {
    let mut c = boot();
    let id = c.bind_new(Interface::Registry).unwrap();
    assert!(c.drop_object(id));
    assert_eq!(c.get(id), None);
    assert!(!c.drop_object(id));
}

#[test]
fn send_request_on_unknown_object_is_unknown_object() {
    let mut c = boot();
    let err = c
        .send_request(ObjectId::new(99), 1, &[])
        .unwrap_err();
    assert_eq!(
        err,
        ClientError::UnknownObject {
            id: ObjectId::new(99)
        }
    );
}

#[test]
fn send_request_on_wrong_opcode_is_unknown_opcode() {
    let mut c = boot();
    // pmd_display has requests 1 (sync) and 2 (get_registry).
    let err = c.send_request(ObjectId::DISPLAY, 42, &[]).unwrap_err();
    assert_eq!(
        err,
        ClientError::UnknownOpcode {
            interface: Interface::Display,
            opcode: 42,
        }
    );
}

#[test]
fn send_request_with_event_opcode_is_wrong_direction() {
    let mut c = boot();
    // pmd_display event opcodes are 1 (error) and 2 (delete_id).
    // But opcode 1 on Display is ALSO sync (a request) — no
    // collision there. pmd_registry's events include 2
    // (global_remove), whose opcode isn't a registry request.
    let reg = c.bind_new(Interface::Registry).unwrap();
    let err = c.send_request(reg, 2, &[]).unwrap_err();
    assert_eq!(
        err,
        ClientError::WrongDirection {
            interface: Interface::Registry,
            opcode: 2,
        }
    );
}

#[test]
fn send_request_encodes_a_header_into_the_outbound_queue() {
    let mut c = boot();
    c.send_request(ObjectId::DISPLAY, 2 /* get_registry */, &[1, 2, 3, 4])
        .unwrap();
    let bytes = c.drain_outbound();
    assert_eq!(bytes.len(), HEADER_SIZE + 4);
    let decoded = MessageHeader::decode(&bytes).unwrap();
    assert_eq!(decoded.object_id, ObjectId::DISPLAY);
    assert_eq!(decoded.opcode, 2);
    assert_eq!(decoded.payload_len(), 4);
    assert_eq!(&bytes[HEADER_SIZE..], &[1, 2, 3, 4]);
}

#[test]
fn get_registry_binds_a_fresh_id_and_sends_the_framed_request() {
    let mut c = boot();
    let reg = c.get_registry().unwrap();
    assert_eq!(reg.raw(), 3);
    assert_eq!(c.get(reg), Some(Interface::Registry));

    let bytes = c.drain_outbound();
    // Header (10 bytes) + 4-byte new_id payload.
    assert_eq!(bytes.len(), HEADER_SIZE + 4);
    let decoded = MessageHeader::decode(&bytes).unwrap();
    assert_eq!(decoded.object_id, ObjectId::DISPLAY);
    assert_eq!(decoded.opcode, 2);
    assert_eq!(decoded.payload_len(), 4);
    // Payload carries the little-endian new_id.
    let new_id = u32::from_le_bytes([
        bytes[HEADER_SIZE],
        bytes[HEADER_SIZE + 1],
        bytes[HEADER_SIZE + 2],
        bytes[HEADER_SIZE + 3],
    ]);
    assert_eq!(new_id, reg.raw());
}

// ---- Event parsing ----------------------------------------------

/// Build a framed event on a byte buffer the test can feed
/// back into `Client::push_received`.
fn build_event_bytes(object_id: ObjectId, opcode: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    let h = MessageHeader::try_new(object_id, opcode, payload.len(), 0).unwrap();
    h.encode(&mut out[..HEADER_SIZE]).unwrap();
    out[HEADER_SIZE..].copy_from_slice(payload);
    out
}

#[test]
fn push_received_parses_a_single_event_and_surfaces_it() {
    let mut c = boot();
    // display.error is opcode 1.
    let bytes = build_event_bytes(ObjectId::DISPLAY, 1, &[0, 0, 0, 0]);
    let (events, consumed) = c.push_received(&bytes).unwrap();
    assert_eq!(consumed, bytes.len());
    assert_eq!(events.len(), 1);
    let e = &events[0];
    assert_eq!(e.object_id, ObjectId::DISPLAY);
    assert_eq!(e.interface, Interface::Display);
    assert_eq!(e.opcode, 1);
    assert_eq!(e.opcode_name, "error");
    assert_eq!(e.payload_len, 4);
}

#[test]
fn push_received_parses_multiple_back_to_back_events() {
    let mut c = boot();
    // Bind a registry so its events resolve.
    let _ = c.bind_new(Interface::Registry).unwrap();
    let mut stream = Vec::new();
    stream.extend(build_event_bytes(ObjectId::DISPLAY, 2, &[0; 4])); // delete_id
    stream.extend(build_event_bytes(ObjectId::new(3), 1, &[])); // registry.global
    stream.extend(build_event_bytes(ObjectId::new(3), 2, &[0; 4])); // registry.global_remove

    let (events, consumed) = c.push_received(&stream).unwrap();
    assert_eq!(consumed, stream.len());
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].opcode_name, "delete_id");
    assert_eq!(events[1].opcode_name, "global");
    assert_eq!(events[2].opcode_name, "global_remove");
}

#[test]
fn push_received_stops_at_a_partial_trailing_message_and_reports_consumed() {
    let mut c = boot();
    let mut stream = Vec::new();
    stream.extend(build_event_bytes(ObjectId::DISPLAY, 2, &[]));
    // Partial header for a second message — only 3 bytes.
    stream.extend_from_slice(&[0, 0, 0]);

    let (events, consumed) = c.push_received(&stream).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(consumed, HEADER_SIZE); // first message only
}

#[test]
fn push_received_with_event_targeting_unknown_object_is_unknown_object() {
    let mut c = boot();
    let bytes = build_event_bytes(ObjectId::new(77), 1, &[]);
    let err = c.push_received(&bytes).unwrap_err();
    assert_eq!(
        err,
        ClientError::UnknownObject {
            id: ObjectId::new(77)
        }
    );
}

#[test]
fn push_received_with_request_opcode_is_wrong_direction() {
    let mut c = boot();
    // display.get_registry (opcode 2) is a REQUEST, not an
    // event. But the Display events include opcode 2 (delete_id)
    // so the "wrong direction" distinction doesn't fire here —
    // delete_id is a legal event. Use the registry instead,
    // whose events are 1 (global) and 2 (global_remove) and
    // whose requests are just opcode 1 (bind). Sending opcode
    // 1 as an event works (it IS an event — global). So we
    // need an interface with a request opcode that is NOT
    // also an event opcode. Compositor fits: it has opcode 1
    // (create_surface) as a request and NO events at all, so
    // an incoming event on opcode 1 looks like a "wrong
    // direction" error.
    let comp = c.bind_new(Interface::Compositor).unwrap();
    let bytes = build_event_bytes(comp, 1, &[]);
    let err = c.push_received(&bytes).unwrap_err();
    assert_eq!(
        err,
        ClientError::WrongDirection {
            interface: Interface::Compositor,
            opcode: 1,
        }
    );
}

#[test]
fn re_exports_from_display_proto_match() {
    // Sanity: toolkit::Interface is the same type as
    // display_proto::Interface, not a wrapper. If a future
    // refactor accidentally adds a local enum, this assertion
    // surfaces the drift immediately.
    fn take_display_proto(_: display_proto::Interface) {}
    take_display_proto(Interface::Display);
}

// ---- push_received_with_payload + typed event decoders ---------

#[test]
fn push_received_with_payload_exposes_raw_bytes_for_typed_decoding() {
    use display_proto::DisplayDeleteId;

    let mut c = boot();
    // Build a display.delete_id event: opcode 2, 4-byte new_id.
    let event = DisplayDeleteId {
        id: ObjectId::new(11),
    };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    let bytes = build_event_bytes(ObjectId::DISPLAY, 2, &payload);

    let (events, consumed) = c.push_received_with_payload(&bytes).unwrap();
    assert_eq!(consumed, bytes.len());
    assert_eq!(events.len(), 1);
    let e = &events[0];
    assert_eq!(e.interface, Interface::Display);
    assert_eq!(e.opcode, 2);
    assert_eq!(e.opcode_name, "delete_id");
    assert_eq!(e.payload, payload);

    // Caller runs the typed decoder on the payload.
    let decoded = DisplayDeleteId::decode(&e.payload).unwrap();
    assert_eq!(decoded, event);
}

#[test]
fn typed_decoder_round_trips_registry_global_through_push_received_with_payload() {
    use display_proto::RegistryGlobal;

    let mut c = boot();
    // Register a registry object at id 3 so the incoming
    // event resolves.
    let registry_id = c.bind_new(Interface::Registry).unwrap();

    let event = RegistryGlobal {
        name: 1,
        interface: "pmd_compositor".to_string(),
        version: 1,
    };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    let bytes = build_event_bytes(registry_id, 1 /* global */, &payload);

    let (events, _) = c.push_received_with_payload(&bytes).unwrap();
    assert_eq!(events.len(), 1);
    let e = &events[0];
    assert_eq!(e.interface, Interface::Registry);
    assert_eq!(e.opcode_name, "global");

    let decoded = RegistryGlobal::decode(&e.payload).unwrap();
    assert_eq!(decoded, event);
}

#[test]
fn push_received_with_payload_parses_multiple_back_to_back_events() {
    use display_proto::{DisplayError, RegistryGlobal, RegistryGlobalRemove};

    let mut c = boot();
    let registry_id = c.bind_new(Interface::Registry).unwrap();

    // Three events in one byte buffer.
    let err = DisplayError {
        object_id: ObjectId::new(3),
        code: 7,
        message: "oops".to_string(),
    };
    let global = RegistryGlobal {
        name: 2,
        interface: "pmd_shm".to_string(),
        version: 1,
    };
    let remove = RegistryGlobalRemove { name: 2 };

    let mut err_payload = Vec::new();
    err.encode(&mut err_payload);
    let mut global_payload = Vec::new();
    global.encode(&mut global_payload);
    let mut remove_payload = Vec::new();
    remove.encode(&mut remove_payload);

    let mut stream = Vec::new();
    stream.extend(build_event_bytes(ObjectId::DISPLAY, 1, &err_payload));
    stream.extend(build_event_bytes(registry_id, 1, &global_payload));
    stream.extend(build_event_bytes(registry_id, 2, &remove_payload));

    let (events, consumed) = c.push_received_with_payload(&stream).unwrap();
    assert_eq!(consumed, stream.len());
    assert_eq!(events.len(), 3);

    assert_eq!(events[0].opcode_name, "error");
    assert_eq!(
        DisplayError::decode(&events[0].payload).unwrap(),
        err
    );
    assert_eq!(events[1].opcode_name, "global");
    assert_eq!(
        RegistryGlobal::decode(&events[1].payload).unwrap(),
        global
    );
    assert_eq!(events[2].opcode_name, "global_remove");
    assert_eq!(
        RegistryGlobalRemove::decode(&events[2].payload).unwrap(),
        remove
    );
}

#[test]
fn push_received_with_payload_stops_at_partial_trailing_message() {
    use display_proto::DisplayDeleteId;

    let mut c = boot();
    let event = DisplayDeleteId {
        id: ObjectId::new(99),
    };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    let full = build_event_bytes(ObjectId::DISPLAY, 2, &payload);
    // Append a truncated second header so the parser must
    // stop cleanly.
    let mut stream = full.clone();
    stream.extend_from_slice(&[0, 0, 0]);

    let (events, consumed) = c.push_received_with_payload(&stream).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(consumed, full.len());
}

#[test]
fn client_event_with_payload_copies_defensively() {
    use display_proto::DisplayDeleteId;

    // Mutating the input buffer AFTER parsing must NOT
    // affect the returned ClientEventWithPayload.
    let mut c = boot();
    let event = DisplayDeleteId {
        id: ObjectId::new(7),
    };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    let mut bytes = build_event_bytes(ObjectId::DISPLAY, 2, &payload);
    let (events, _) = c.push_received_with_payload(&bytes).unwrap();
    // Trash the original buffer.
    for b in bytes.iter_mut() {
        *b = 0xff;
    }
    let decoded = DisplayDeleteId::decode(&events[0].payload).unwrap();
    assert_eq!(decoded, event);
}
