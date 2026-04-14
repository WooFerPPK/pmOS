//! Desktop-shell session isolation tests.
//!
//! Drives `shell::Session` against a `toolkit::MemoryConnection`
//! and hand-built event bytes. The tests exercise:
//!
//!   * `start()` sends display.get_registry and assigns a
//!     registry id
//!   * pump() before start fails with NotStarted
//!   * pump() on a registry.global event records the entry
//!     in `known_globals` and auto-binds the
//!     INTERESTING_INTERFACES subset
//!   * multiple globals in one pump bind every matching
//!     interface
//!   * a second registry.global for an already-bound
//!     interface is idempotent (no second bind)
//!   * global_remove flips the live flag but keeps the
//!     entry
//!   * display.error events surface as ProtocolErrorNotice
//!   * malformed event payloads are reported as
//!     MalformedEvent
//!
//! Events are crafted by the tests themselves using the
//! encoders from `display_proto::events` so the assertions
//! are self-contained: no server library required.

use display_proto::{
    wire::{MessageHeader, HEADER_SIZE},
    DisplayError, Interface, ObjectId, RegistryGlobal, RegistryGlobalRemove,
};
use shell::{Session, SessionError, INTERESTING_INTERFACES};
use toolkit::protocol::MemoryConnection;

/// Build one framed event message.
fn frame_event(object_id: ObjectId, opcode: u16, payload: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; HEADER_SIZE + payload.len()];
    let h = MessageHeader::try_new(object_id, opcode, payload.len(), 0).unwrap();
    h.encode(&mut buf[..HEADER_SIZE]).unwrap();
    buf[HEADER_SIZE..].copy_from_slice(payload);
    buf
}

fn encode<T, F: FnOnce(&T, &mut Vec<u8>)>(value: &T, encode_fn: F) -> Vec<u8> {
    let mut out = Vec::new();
    encode_fn(value, &mut out);
    out
}

/// Build a registry.global event for the given registry id.
fn global_event(registry_id: ObjectId, global: &RegistryGlobal) -> Vec<u8> {
    let payload = encode(global, RegistryGlobal::encode);
    frame_event(registry_id, 1 /* global */, &payload)
}

/// Build a registry.global_remove event.
fn global_remove_event(registry_id: ObjectId, remove: &RegistryGlobalRemove) -> Vec<u8> {
    let payload = encode(remove, RegistryGlobalRemove::encode);
    frame_event(registry_id, 2 /* global_remove */, &payload)
}

/// Build a display.error event against object 1.
fn display_error_event(err: &DisplayError) -> Vec<u8> {
    let payload = encode(err, DisplayError::encode);
    frame_event(ObjectId::DISPLAY, 1 /* error */, &payload)
}

fn boot_session() -> Session<MemoryConnection> {
    Session::new(MemoryConnection::new())
}

fn boot_started_session() -> (Session<MemoryConnection>, ObjectId) {
    let mut session = boot_session();
    session.start().unwrap();
    // Drain the display.get_registry request bytes so they
    // don't clutter subsequent assertions.
    let _ = session.drain_outbound();
    let registry_id = session.registry_id().unwrap();
    (session, registry_id)
}

#[test]
fn new_session_has_no_registry_and_no_globals() {
    let s = boot_session();
    assert!(s.registry_id().is_none());
    assert!(s.known_globals().is_empty());
    assert!(!s.is_ready());
}

#[test]
fn start_sends_display_get_registry_and_assigns_a_registry_id() {
    let mut s = boot_session();
    s.start().unwrap();
    let registry_id = s.registry_id().unwrap();
    // The toolkit's id allocator starts at 3; the first
    // thing we bind is the registry, so it should be 3.
    assert_eq!(registry_id.raw(), 3);

    let bytes = s.drain_outbound();
    assert!(!bytes.is_empty());
    // First 10 bytes are the message header — the object_id
    // field is 0..4 == 1 (display).
    let header = MessageHeader::decode(&bytes).unwrap();
    assert_eq!(header.object_id, ObjectId::DISPLAY);
    assert_eq!(header.opcode, 2 /* get_registry */);
}

#[test]
fn start_twice_is_an_error() {
    let mut s = boot_session();
    s.start().unwrap();
    let err = s.start().unwrap_err();
    assert_eq!(err, SessionError::AlreadyStarted);
}

#[test]
fn pump_before_start_returns_not_started() {
    let mut s = boot_session();
    let err = s.pump(&[]).unwrap_err();
    assert_eq!(err, SessionError::NotStarted);
}

#[test]
fn pump_with_empty_input_returns_an_empty_step() {
    let (mut s, _) = boot_started_session();
    let (step, consumed) = s.pump(&[]).unwrap();
    assert_eq!(consumed, 0);
    assert!(step.is_empty());
}

#[test]
fn pump_records_a_discovered_global_in_known_globals() {
    let (mut s, registry_id) = boot_started_session();
    let event = RegistryGlobal {
        name: 1,
        interface: "pmd_net".to_string(),
        version: 1,
    };
    let bytes = global_event(registry_id, &event);
    let (step, consumed) = s.pump(&bytes).unwrap();
    assert_eq!(consumed, bytes.len());
    assert_eq!(step.discovered, vec![1]);
    // pmd_net isn't an INTERESTING_INTERFACE so no bind is
    // attempted.
    assert!(step.bound.is_empty());
    let entry = s.known_globals().get(&1).unwrap();
    assert_eq!(entry.interface_name, "pmd_net");
    assert_eq!(entry.interface, None);
    assert_eq!(entry.version, 1);
    assert!(entry.live);
    assert_eq!(entry.bound_id, None);
}

#[test]
fn pump_auto_binds_compositor_when_discovered() {
    let (mut s, registry_id) = boot_started_session();
    let event = RegistryGlobal {
        name: 2,
        interface: "pmd_compositor".to_string(),
        version: 1,
    };
    let bytes = global_event(registry_id, &event);
    let (step, _) = s.pump(&bytes).unwrap();
    assert_eq!(step.bound, vec![Interface::Compositor]);
    assert!(s.bound(Interface::Compositor).is_some());
    let entry = s.known_globals().get(&2).unwrap();
    assert_eq!(entry.bound_id, s.bound(Interface::Compositor));
    // After binding compositor, a `registry.bind` request
    // was enqueued on the connection.
    let outbound = s.drain_outbound();
    assert!(!outbound.is_empty());
    // The bound id is the next odd id after registry
    // (which was 3), so 5.
    assert_eq!(s.bound(Interface::Compositor).unwrap().raw(), 5);
}

#[test]
fn pump_is_ready_only_after_every_interesting_interface_is_bound() {
    let (mut s, registry_id) = boot_started_session();

    // Compositor first.
    let compositor_event = RegistryGlobal {
        name: 1,
        interface: "pmd_compositor".to_string(),
        version: 1,
    };
    s.pump(&global_event(registry_id, &compositor_event)).unwrap();
    assert!(!s.is_ready(), "shm not yet bound");

    // Shm next.
    let shm_event = RegistryGlobal {
        name: 2,
        interface: "pmd_shm".to_string(),
        version: 1,
    };
    s.pump(&global_event(registry_id, &shm_event)).unwrap();
    assert!(s.is_ready());
    assert!(s.bound(Interface::Compositor).is_some());
    assert!(s.bound(Interface::Shm).is_some());
}

#[test]
fn pump_handles_multiple_globals_in_one_byte_stream() {
    let (mut s, registry_id) = boot_started_session();
    let mut stream = Vec::new();
    stream.extend(global_event(
        registry_id,
        &RegistryGlobal {
            name: 1,
            interface: "pmd_compositor".to_string(),
            version: 1,
        },
    ));
    stream.extend(global_event(
        registry_id,
        &RegistryGlobal {
            name: 2,
            interface: "pmd_shm".to_string(),
            version: 1,
        },
    ));
    stream.extend(global_event(
        registry_id,
        &RegistryGlobal {
            name: 3,
            interface: "pmd_net".to_string(),
            version: 2,
        },
    ));

    let (step, consumed) = s.pump(&stream).unwrap();
    assert_eq!(consumed, stream.len());
    assert_eq!(step.discovered, vec![1, 2, 3]);
    assert!(step.bound.contains(&Interface::Compositor));
    assert!(step.bound.contains(&Interface::Shm));
    assert_eq!(step.bound.len(), 2);
    assert!(s.is_ready());
}

#[test]
fn pump_ignores_a_duplicate_global_for_an_already_bound_interface() {
    let (mut s, registry_id) = boot_started_session();
    let first = RegistryGlobal {
        name: 1,
        interface: "pmd_compositor".to_string(),
        version: 1,
    };
    s.pump(&global_event(registry_id, &first)).unwrap();
    let original_compositor = s.bound(Interface::Compositor).unwrap();
    let _ = s.drain_outbound();

    // Second advertisement for pmd_compositor under a
    // different name. We record it in known_globals but do
    // NOT rebind.
    let second = RegistryGlobal {
        name: 2,
        interface: "pmd_compositor".to_string(),
        version: 1,
    };
    let (step, _) = s.pump(&global_event(registry_id, &second)).unwrap();
    assert_eq!(step.discovered, vec![2]);
    assert!(step.bound.is_empty(), "must not rebind compositor");
    // The existing binding is unchanged.
    assert_eq!(s.bound(Interface::Compositor), Some(original_compositor));
    // No new request bytes went out.
    assert!(s.drain_outbound().is_empty());
}

#[test]
fn pump_records_global_remove_but_keeps_the_entry() {
    let (mut s, registry_id) = boot_started_session();
    let advert = RegistryGlobal {
        name: 1,
        interface: "pmd_net".to_string(),
        version: 1,
    };
    s.pump(&global_event(registry_id, &advert)).unwrap();
    let remove = RegistryGlobalRemove { name: 1 };
    let (step, _) = s.pump(&global_remove_event(registry_id, &remove)).unwrap();
    assert_eq!(step.removed, vec![1]);
    let entry = s.known_globals().get(&1).unwrap();
    assert!(!entry.live);
    assert_eq!(entry.interface_name, "pmd_net");
}

#[test]
fn pump_reports_display_error_events_as_protocol_error_notices() {
    let (mut s, _registry_id) = boot_started_session();
    let err = DisplayError {
        object_id: ObjectId::new(3),
        code: 42,
        message: "broken".to_string(),
    };
    let (step, _) = s.pump(&display_error_event(&err)).unwrap();
    assert_eq!(step.errors.len(), 1);
    assert_eq!(step.errors[0].object_id, ObjectId::new(3));
    assert_eq!(step.errors[0].code, 42);
    assert_eq!(step.errors[0].message, "broken");
}

#[test]
fn pump_surfaces_a_client_error_for_unknown_object_id() {
    let (mut s, _) = boot_started_session();
    // Build an event targeting object id 99 which the
    // session's client has never bound.
    let stray = frame_event(
        ObjectId::new(99),
        1, /* any event opcode */
        &[],
    );
    let err = s.pump(&stray).unwrap_err();
    match err {
        SessionError::Client(_) => {}
        other => panic!("expected Client error, got {other:?}"),
    }
}

#[test]
fn pump_reports_malformed_event_payload_as_malformed_event() {
    let (mut s, registry_id) = boot_started_session();
    // Truncated registry.global — only 2 bytes, needs 4
    // for the name u32.
    let bytes = frame_event(registry_id, 1 /* global */, &[0, 0]);
    let err = s.pump(&bytes).unwrap_err();
    match err {
        SessionError::MalformedEvent(_) => {}
        other => panic!("expected MalformedEvent, got {other:?}"),
    }
}

#[test]
fn session_exposes_drain_outbound_matching_the_client() {
    let mut s = boot_session();
    s.start().unwrap();
    let bytes = s.drain_outbound();
    assert!(!bytes.is_empty());
    // Second drain is empty.
    let bytes2 = s.drain_outbound();
    assert!(bytes2.is_empty());
}
