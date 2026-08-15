//! Per-client state machine tests.

use display_proto::MAX_SURFACE_PATCH_BYTES;
use display_server::client::{
    BufferAttachment, BufferInfo, Client, ClientError, ClientId, DamageRect, HandledRequest, Pool,
    Surface, Toplevel, AUTO_LAYOUT_STEP, MAX_JOURNAL_ENTRIES, MAX_POOL_SIZE,
};
use display_server::ids::{IdKind, ObjectId};
use display_server::objects::Interface;
use display_server::wire::MessageHeader;
use display_server::HEADER_SIZE;

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
    out.extend(core::iter::repeat_n(0u8, pad));
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
fn handled_request_journal_is_metadata_only_and_strictly_bounded() {
    let mut c = Client::new(ClientId(1));
    for fd_passing in 0..(MAX_JOURNAL_ENTRIES + 17) {
        let payload = ObjectId::new(3).raw().to_le_bytes();
        let header = MessageHeader::try_new(
            ObjectId::DISPLAY,
            1, /* sync */
            payload.len(),
            fd_passing as u8,
        )
        .unwrap();
        c.dispatch_request(header, &payload).unwrap();
        c.drain_pending_events();
    }

    assert_eq!(c.journal_len(), MAX_JOURNAL_ENTRIES);
    let retained = c.drain_journal();
    assert_eq!(retained.len(), MAX_JOURNAL_ENTRIES);
    assert_eq!(retained[0].fd_passing, 17);
    assert_eq!(retained.last().unwrap().fd_passing, 16);
    assert_eq!(c.journal_len(), 0);
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
fn dispatch_display_sync_emits_done_before_delete_and_destroys_callback() {
    let mut c = Client::new(ClientId(1));
    let callback_id = ObjectId::new(3);
    let payload = callback_id.raw().to_le_bytes();
    let header = MessageHeader::try_new(ObjectId::DISPLAY, 1, payload.len(), 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();

    assert_eq!(c.get(callback_id), None);
    let events = c.drain_pending_events();
    let done = MessageHeader::decode(&events).unwrap();
    assert_eq!(done.object_id, callback_id);
    assert_eq!(done.opcode, 1);
    assert_eq!(done.length as usize, display_server::HEADER_SIZE + 4);
    assert_eq!(
        u32::from_le_bytes(
            events[display_server::HEADER_SIZE..display_server::HEADER_SIZE + 4]
                .try_into()
                .unwrap()
        ),
        0
    );

    let delete_offset = done.length as usize;
    let delete = MessageHeader::decode(&events[delete_offset..]).unwrap();
    assert_eq!(delete.object_id, ObjectId::DISPLAY);
    assert_eq!(delete.opcode, 2);
    assert_eq!(
        u32::from_le_bytes(
            events[delete_offset + display_server::HEADER_SIZE
                ..delete_offset + display_server::HEADER_SIZE + 4]
                .try_into()
                .unwrap()
        ),
        callback_id.raw()
    );
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

// ---- Emit path --------------------------------------------------

#[test]
fn emit_error_enqueues_a_display_error_event() {
    use display_proto::{wire::MessageHeader, DisplayError};
    let mut c = Client::new(ClientId(1));
    let n = c.emit_error(ObjectId::new(7), 42, "broken").unwrap();
    assert_eq!(c.pending_events_len(), 1);

    let bytes = c.drain_pending_events();
    assert_eq!(bytes.len(), n);
    let header = MessageHeader::decode(&bytes).unwrap();
    assert_eq!(header.object_id, ObjectId::DISPLAY);
    assert_eq!(header.opcode, 1 /* error */);

    let decoded = DisplayError::decode(&bytes[10..header.length as usize]).unwrap();
    assert_eq!(decoded.object_id, ObjectId::new(7));
    assert_eq!(decoded.code, 42);
    assert_eq!(decoded.message, "broken");
}

#[test]
fn emit_delete_id_enqueues_the_event_with_a_u32_payload() {
    use display_proto::{wire::MessageHeader, DisplayDeleteId};
    let mut c = Client::new(ClientId(1));
    c.emit_delete_id(ObjectId::new(99)).unwrap();

    let bytes = c.drain_pending_events();
    let header = MessageHeader::decode(&bytes).unwrap();
    assert_eq!(header.opcode, 2 /* delete_id */);
    let decoded = DisplayDeleteId::decode(&bytes[10..header.length as usize]).unwrap();
    assert_eq!(decoded.id, ObjectId::new(99));
}

#[test]
fn emit_global_enqueues_a_registry_global_event() {
    use display_proto::{wire::MessageHeader, RegistryGlobal};
    let mut c = Client::new(ClientId(1));
    // The registry object has to exist in the client's
    // table for the server to be allowed to emit events
    // on it. dispatch_request normally installs it on a
    // get_registry from the client; here we install by
    // hand.
    let registry_id = ObjectId::new(3);
    c.install_client_object(registry_id, Interface::Registry)
        .unwrap();
    c.emit_global(registry_id, 1, "pmd_compositor", 1).unwrap();

    let bytes = c.drain_pending_events();
    let header = MessageHeader::decode(&bytes).unwrap();
    assert_eq!(header.object_id, registry_id);
    assert_eq!(header.opcode, 1 /* global */);
    let decoded = RegistryGlobal::decode(&bytes[10..header.length as usize]).unwrap();
    assert_eq!(decoded.name, 1);
    assert_eq!(decoded.interface, "pmd_compositor");
    assert_eq!(decoded.version, 1);
}

#[test]
fn emit_global_remove_enqueues_with_a_u32_name_payload() {
    use display_proto::{wire::MessageHeader, RegistryGlobalRemove};
    let mut c = Client::new(ClientId(1));
    let registry_id = ObjectId::new(3);
    c.install_client_object(registry_id, Interface::Registry)
        .unwrap();
    c.emit_global_remove(registry_id, 7).unwrap();

    let bytes = c.drain_pending_events();
    let header = MessageHeader::decode(&bytes).unwrap();
    assert_eq!(header.object_id, registry_id);
    assert_eq!(header.opcode, 2 /* global_remove */);
    let decoded = RegistryGlobalRemove::decode(&bytes[10..header.length as usize]).unwrap();
    assert_eq!(decoded.name, 7);
}

#[test]
fn emit_raw_rejects_unknown_object_with_unknown_object_error() {
    let mut c = Client::new(ClientId(1));
    let err = c.emit_raw(ObjectId::new(99), 1, &[]).unwrap_err();
    assert_eq!(
        err,
        ClientError::UnknownObject {
            id: ObjectId::new(99)
        }
    );
    assert_eq!(c.pending_events_len(), 0);
}

#[test]
fn emit_raw_rejects_request_opcode_as_wrong_direction() {
    let mut c = Client::new(ClientId(1));
    // display.get_registry (opcode 2) is a REQUEST, not
    // an event. But display.delete_id is ALSO opcode 2 —
    // as an event. So this opcode is valid in both
    // directions and emit accepts it. Use display.sync
    // (opcode 1, request only; the event opcode 1 on
    // display is `error`, which IS an event, so sync 1
    // resolves as a valid event too).
    //
    // Find a real request-only opcode: compositor.create_
    // surface is opcode 1 as a request, and compositor
    // defines NO events. Emit compositor.opcode=1 on an
    // installed compositor → WrongDirection.
    let compositor_id = ObjectId::new(3);
    c.install_client_object(compositor_id, Interface::Compositor)
        .unwrap();
    let err = c.emit_raw(compositor_id, 1, &[0, 0, 0, 0]).unwrap_err();
    assert_eq!(
        err,
        ClientError::WrongDirection {
            interface: Interface::Compositor,
            opcode: 1,
        }
    );
}

#[test]
fn emit_raw_rejects_unknown_opcode_that_is_neither_request_nor_event() {
    let mut c = Client::new(ClientId(1));
    // Display has opcodes 1,2 for both directions.
    // Opcode 99 doesn't exist either way.
    let err = c.emit_raw(ObjectId::DISPLAY, 99, &[]).unwrap_err();
    assert_eq!(
        err,
        ClientError::UnknownOpcode {
            interface: Interface::Display,
            opcode: 99,
        }
    );
}

#[test]
fn drain_pending_events_concatenates_multiple_enqueued_messages() {
    use display_proto::wire::MessageHeader;
    let mut c = Client::new(ClientId(1));
    let registry_id = ObjectId::new(3);
    c.install_client_object(registry_id, Interface::Registry)
        .unwrap();

    c.emit_global(registry_id, 1, "pmd_compositor", 1).unwrap();
    c.emit_global(registry_id, 2, "pmd_shm", 1).unwrap();
    c.emit_delete_id(ObjectId::new(5)).unwrap();
    assert_eq!(c.pending_events_len(), 3);

    let bytes = c.drain_pending_events();
    assert_eq!(c.pending_events_len(), 0);

    // Walk the three messages by header length.
    let mut cursor = 0usize;
    let mut n = 0;
    while cursor < bytes.len() {
        let h = MessageHeader::decode(&bytes[cursor..]).unwrap();
        cursor += h.length as usize;
        n += 1;
    }
    assert_eq!(n, 3);
    assert_eq!(cursor, bytes.len());
}

#[test]
fn drain_pending_events_is_empty_when_nothing_was_emitted() {
    let mut c = Client::new(ClientId(1));
    assert_eq!(c.drain_pending_events(), Vec::<u8>::new());
}

#[test]
fn emit_window_created_enqueues_a_shell_window_created_event() {
    use display_proto::{wire::MessageHeader, ShellWindowCreated};
    let mut c = Client::new(ClientId(1));
    let sm_id = ObjectId::new(5);
    c.install_client_object(sm_id, Interface::ShellManager)
        .unwrap();
    c.emit_window_created(sm_id, 42, "term", "pmos.term")
        .unwrap();

    let bytes = c.drain_pending_events();
    let header = MessageHeader::decode(&bytes).unwrap();
    assert_eq!(header.object_id, sm_id);
    assert_eq!(header.opcode, 1 /* window_created */);
    let decoded = ShellWindowCreated::decode(&bytes[10..header.length as usize]).unwrap();
    assert_eq!(decoded.window_id, 42);
    assert_eq!(decoded.title, "term");
    assert_eq!(decoded.app_id, "pmos.term");
}

#[test]
fn emit_window_destroyed_focused_title_changed_use_distinct_opcodes() {
    use display_proto::wire::MessageHeader;
    let mut c = Client::new(ClientId(1));
    let sm_id = ObjectId::new(5);
    c.install_client_object(sm_id, Interface::ShellManager)
        .unwrap();
    c.emit_window_destroyed(sm_id, 1).unwrap();
    c.emit_window_focused(sm_id, 2).unwrap();
    c.emit_window_title_changed(sm_id, 3, "new title").unwrap();

    let bytes = c.drain_pending_events();
    let mut cursor = 0usize;
    let h1 = MessageHeader::decode(&bytes[cursor..]).unwrap();
    assert_eq!(h1.opcode, 2 /* window_destroyed */);
    cursor += h1.length as usize;
    let h2 = MessageHeader::decode(&bytes[cursor..]).unwrap();
    assert_eq!(h2.opcode, 3 /* window_focused */);
    cursor += h2.length as usize;
    let h3 = MessageHeader::decode(&bytes[cursor..]).unwrap();
    assert_eq!(h3.opcode, 4 /* window_title_changed */);
    cursor += h3.length as usize;
    assert_eq!(cursor, bytes.len());
}

#[test]
fn pending_window_titles_coalesce_by_window_and_retain_fifo_position() {
    use display_proto::ShellWindowTitleChanged;
    let mut client = Client::new(ClientId(1));
    let shell_manager = ObjectId::new(5);
    client
        .install_client_object(shell_manager, Interface::ShellManager)
        .unwrap();

    client
        .emit_window_title_changed(shell_manager, 3, "old")
        .unwrap();
    client
        .emit_window_title_changed(shell_manager, 4, "other")
        .unwrap();
    client
        .emit_window_title_changed(shell_manager, 3, "latest")
        .unwrap();
    assert_eq!(client.pending_events_len(), 2);
    assert!(!client.event_queue_overflowed());

    let bytes = client.drain_pending_events();
    let first_header = MessageHeader::decode(&bytes).unwrap();
    let first =
        ShellWindowTitleChanged::decode(&bytes[HEADER_SIZE..first_header.length as usize]).unwrap();
    assert_eq!((first.window_id, first.new_title.as_str()), (3, "latest"));
    let second_offset = first_header.length as usize;
    let second_header = MessageHeader::decode(&bytes[second_offset..]).unwrap();
    let second = ShellWindowTitleChanged::decode(
        &bytes[second_offset + HEADER_SIZE..second_offset + second_header.length as usize],
    )
    .unwrap();
    assert_eq!((second.window_id, second.new_title.as_str()), (4, "other"));
}

#[test]
fn emit_window_event_on_unknown_object_is_unknown_object() {
    let mut c = Client::new(ClientId(1));
    let err = c
        .emit_window_created(ObjectId::new(99), 1, "x", "y")
        .unwrap_err();
    assert!(matches!(err, ClientError::UnknownObject { .. }));
}

// ---- Capability gate -------------------------------------------

#[test]
fn dispatch_registry_bind_shell_manager_without_cap_shell_is_permission_denied() {
    use abi::cap::CapSet;
    let mut c = Client::new_with_caps(ClientId(1), CapSet::EMPTY);
    let registry_id = ObjectId::new(3);
    c.install_client_object(registry_id, Interface::Registry)
        .unwrap();

    let payload = registry_bind_payload(1, "pmd_shell_manager", 1, ObjectId::new(5));
    let header = MessageHeader::try_new(registry_id, 1, payload.len(), 0).unwrap();
    let err = c.dispatch_request(header, &payload).unwrap_err();
    match err {
        ClientError::PermissionDenied {
            interface,
            required,
            new_id,
        } => {
            assert_eq!(interface, Interface::ShellManager);
            assert_eq!(required, abi::cap::Cap::Shell);
            assert_eq!(new_id, ObjectId::new(5));
        }
        other => panic!("expected PermissionDenied, got {other:?}"),
    }
    // Shell-manager is NOT installed at the requested id.
    assert_eq!(c.get(ObjectId::new(5)), None);
    // The error event was enqueued for the client to
    // observe — pmd_display.error against object 5.
    assert_eq!(c.pending_events_len(), 1);
    let event_bytes = c.drain_pending_events();
    let header = display_proto::wire::MessageHeader::decode(&event_bytes).unwrap();
    assert_eq!(header.object_id, ObjectId::DISPLAY);
    assert_eq!(header.opcode, 1 /* error */);
    let decoded =
        display_proto::DisplayError::decode(&event_bytes[10..header.length as usize]).unwrap();
    assert_eq!(decoded.object_id, ObjectId::new(5));
    assert_eq!(decoded.code, display_proto::error_code::PERMISSION_DENIED);
    assert!(decoded.message.contains("pmd_shell_manager"));
    assert!(decoded.message.contains("Shell"));
}

#[test]
fn dispatch_registry_bind_shell_manager_with_cap_shell_succeeds() {
    use abi::cap::{Cap, CapSet};
    let caps = CapSet::from_caps(&[Cap::Shell]);
    let mut c = Client::new_with_caps(ClientId(1), caps);
    let registry_id = ObjectId::new(3);
    c.install_client_object(registry_id, Interface::Registry)
        .unwrap();

    let payload = registry_bind_payload(1, "pmd_shell_manager", 1, ObjectId::new(5));
    let header = MessageHeader::try_new(registry_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();
    assert_eq!(c.get(ObjectId::new(5)), Some(Interface::ShellManager));
}

#[test]
fn dispatch_registry_bind_compositor_does_not_require_any_cap() {
    use abi::cap::CapSet;
    // No caps at all.
    let mut c = Client::new_with_caps(ClientId(1), CapSet::EMPTY);
    let registry_id = ObjectId::new(3);
    c.install_client_object(registry_id, Interface::Registry)
        .unwrap();
    let payload = registry_bind_payload(1, "pmd_compositor", 1, ObjectId::new(5));
    let header = MessageHeader::try_new(registry_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();
    assert_eq!(c.get(ObjectId::new(5)), Some(Interface::Compositor));
}

#[test]
fn interface_required_cap_is_only_set_for_shell_manager_in_v1() {
    use display_server::interface_required_cap;
    assert_eq!(
        interface_required_cap(Interface::ShellManager),
        Some(abi::cap::Cap::Shell)
    );
    for iface in [
        Interface::Display,
        Interface::Registry,
        Interface::Compositor,
        Interface::Shm,
        Interface::ShmPool,
        Interface::Buffer,
        Interface::Surface,
    ] {
        assert_eq!(
            interface_required_cap(iface),
            None,
            "{iface:?} should be unrestricted"
        );
    }
}

#[test]
fn new_client_default_constructor_has_empty_caps() {
    let c = Client::new(ClientId(1));
    assert!(!c.has_cap(abi::cap::Cap::Shell));
    assert!(!c.has_cap(abi::cap::Cap::DisplayClient));
}

#[test]
fn new_with_caps_constructor_stores_the_cap_set() {
    use abi::cap::{Cap, CapSet};
    let caps = CapSet::from_caps(&[Cap::Shell, Cap::DisplayClient]);
    let c = Client::new_with_caps(ClientId(1), caps);
    assert!(c.has_cap(Cap::Shell));
    assert!(c.has_cap(Cap::DisplayClient));
    assert!(!c.has_cap(Cap::ProcKillAny));
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
    let _ = HandledRequest {
        object_id: ObjectId::NULL,
        interface: Interface::Display,
        opcode: 0,
        opcode_name: "",
        payload_len: 0,
        fd_passing: 0,
    };
}

/// Build the `pmd_shm.create_pool(new_id, size)` payload.
/// Matches the display-proto `ShmCreatePool` decoder.
fn shm_create_pool_payload(new_id: ObjectId, size: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(&new_id.raw().to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out
}

/// Build the `pmd_shm_pool.create_buffer(new_id, offset, w,
/// h, stride, format)` payload.
fn shm_pool_create_buffer_payload(
    new_id: ObjectId,
    offset: u32,
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(24);
    out.extend_from_slice(&new_id.raw().to_le_bytes());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&stride.to_le_bytes());
    out.extend_from_slice(&format.to_le_bytes());
    out
}

fn shm_pool_write_rows_payload(
    offset: u32,
    row_bytes: u32,
    rows: u32,
    stride: u32,
    bytes: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + bytes.len());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(&row_bytes.to_le_bytes());
    out.extend_from_slice(&rows.to_le_bytes());
    out.extend_from_slice(&stride.to_le_bytes());
    out.extend_from_slice(bytes);
    out
}

#[test]
fn dispatch_shm_create_pool_auto_installs_shm_pool_at_new_id() {
    let mut c = Client::new(ClientId(1));
    let shm_id = ObjectId::new(5);
    c.install_client_object(shm_id, Interface::Shm).unwrap();

    let pool_id = ObjectId::new(7);
    let payload = shm_create_pool_payload(pool_id, 64 * 1024);
    // fd_passing=1 signals that an SAB fd is attached
    // out-of-band. The v1 skeleton accepts any value and
    // just journals it — the real host will validate.
    let header = MessageHeader::try_new(shm_id, 1, payload.len(), 1).unwrap();
    c.dispatch_request(header, &payload).unwrap();

    assert_eq!(c.get(pool_id), Some(Interface::ShmPool));
    let journal = c.drain_journal();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].interface, Interface::Shm);
    assert_eq!(journal[0].opcode_name, "create_pool");
    assert_eq!(journal[0].fd_passing, 1);
}

#[test]
fn dispatch_shm_create_pool_with_truncated_payload_is_malformed() {
    let mut c = Client::new(ClientId(1));
    let shm_id = ObjectId::new(5);
    c.install_client_object(shm_id, Interface::Shm).unwrap();
    let header = frame_request(shm_id, 1);
    let err = c.dispatch_request(header, &[0u8; 3]).unwrap_err();
    assert!(matches!(err, ClientError::Malformed { .. }));
    // No pool was installed on the failure path.
    assert_eq!(c.object_count(), 2); // Display + Shm
}

#[test]
fn dispatch_shm_pool_create_buffer_auto_installs_buffer_at_new_id() {
    // create_buffer now validates the buffer region against
    // its parent pool's allocated storage, so we can't just
    // hand-install a `ShmPool` object — the pool map must
    // also carry a real `Pool { size, storage }` entry. The
    // cleanest way to get that is to dispatch a real
    // `shm.create_pool` request first.
    let mut c = Client::new(ClientId(1));
    let shm_id = ObjectId::new(5);
    c.install_client_object(shm_id, Interface::Shm).unwrap();
    let pool_id = ObjectId::new(7);
    let payload = shm_create_pool_payload(pool_id, 320 * 240 * 4);
    let header = MessageHeader::try_new(shm_id, 1, payload.len(), 1).unwrap();
    c.dispatch_request(header, &payload).unwrap();
    c.drain_journal();

    let buffer_id = ObjectId::new(9);
    let payload =
        shm_pool_create_buffer_payload(buffer_id, 0, 320, 240, 320 * 4, 0 /* ARGB8888 */);
    let header = MessageHeader::try_new(pool_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();

    assert_eq!(c.get(buffer_id), Some(Interface::Buffer));
    let journal = c.drain_journal();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].interface, Interface::ShmPool);
    assert_eq!(journal[0].opcode_name, "create_buffer");
}

#[test]
fn dispatch_full_walk_display_to_surface_commit_with_attached_buffer() {
    // Extended version of the compositor-create-surface walk
    // that also threads through shm.create_pool,
    // pool.create_buffer, surface.attach, surface.damage, and
    // surface.commit — the full single-frame present path a
    // real app uses.
    let mut c = Client::new(ClientId(1));

    let registry_id = ObjectId::new(3);
    let compositor_id = ObjectId::new(5);
    let shm_id = ObjectId::new(7);
    let surface_id = ObjectId::new(9);
    let pool_id = ObjectId::new(11);
    let buffer_id = ObjectId::new(13);

    // 1. get_registry → registry auto-installed.
    let header = MessageHeader::try_new(ObjectId::DISPLAY, 2, 4, 0).unwrap();
    c.dispatch_request(header, &new_id_payload(registry_id))
        .unwrap();

    // 2. bind compositor.
    let payload = registry_bind_payload(1, "pmd_compositor", 1, compositor_id);
    let header = MessageHeader::try_new(registry_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();

    // 3. bind shm.
    let payload = registry_bind_payload(2, "pmd_shm", 1, shm_id);
    let header = MessageHeader::try_new(registry_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();

    // 4. compositor.create_surface.
    let payload = new_id_payload(surface_id);
    let header = MessageHeader::try_new(compositor_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();

    // 5. shm.create_pool.
    let payload = shm_create_pool_payload(pool_id, 320 * 240 * 4);
    let header = MessageHeader::try_new(shm_id, 1, payload.len(), 1).unwrap();
    c.dispatch_request(header, &payload).unwrap();

    // 6. pool.create_buffer.
    let payload = shm_pool_create_buffer_payload(buffer_id, 0, 320, 240, 320 * 4, 0);
    let header = MessageHeader::try_new(pool_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();

    // 7. surface.attach(buffer_id, 0, 0). Payload is
    //    (u32 buffer_id, i32 x, i32 y) = 12 bytes.
    let mut payload = Vec::with_capacity(12);
    payload.extend_from_slice(&buffer_id.raw().to_le_bytes());
    payload.extend_from_slice(&0i32.to_le_bytes());
    payload.extend_from_slice(&0i32.to_le_bytes());
    let header = MessageHeader::try_new(surface_id, 2, 12, 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();

    // 8. surface.damage(0, 0, 320, 240) — four i32s.
    let mut payload = Vec::with_capacity(16);
    for v in [0i32, 0, 320, 240] {
        payload.extend_from_slice(&v.to_le_bytes());
    }
    let header = MessageHeader::try_new(surface_id, 3, 16, 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();

    // 9. surface.commit.
    let header = frame_request(surface_id, 7);
    c.dispatch_request(header, &[]).unwrap();

    // Object table has every allocated binding.
    assert_eq!(c.get(registry_id), Some(Interface::Registry));
    assert_eq!(c.get(compositor_id), Some(Interface::Compositor));
    assert_eq!(c.get(shm_id), Some(Interface::Shm));
    assert_eq!(c.get(surface_id), Some(Interface::Surface));
    assert_eq!(c.get(pool_id), Some(Interface::ShmPool));
    assert_eq!(c.get(buffer_id), Some(Interface::Buffer));

    // Journal contains every dispatched request in order.
    let journal = c.drain_journal();
    let names: Vec<&str> = journal.iter().map(|r| r.opcode_name).collect();
    assert_eq!(
        names,
        vec![
            "get_registry",
            "bind",
            "bind",
            "create_surface",
            "create_pool",
            "create_buffer",
            "attach",
            "damage",
            "commit",
        ]
    );
}

// ---- Pool memory backing + buffer region validation ------------

/// Convenience: run `shm.create_pool(pool_id, size)` on a
/// client that already has a shm_id installed. Panics on
/// any dispatch error.
fn dispatch_create_pool(c: &mut Client, shm_id: ObjectId, pool_id: ObjectId, size: u32) {
    let payload = shm_create_pool_payload(pool_id, size);
    let header = MessageHeader::try_new(shm_id, 1, payload.len(), 1).unwrap();
    c.dispatch_request(header, &payload).unwrap();
    c.drain_journal();
}

#[test]
fn create_pool_allocates_zero_filled_storage_at_the_requested_size() {
    let mut c = Client::new(ClientId(1));
    let shm_id = ObjectId::new(5);
    c.install_client_object(shm_id, Interface::Shm).unwrap();
    let pool_id = ObjectId::new(7);
    dispatch_create_pool(&mut c, shm_id, pool_id, 1024);

    let pool: &Pool = c.pool(pool_id).expect("pool installed");
    assert_eq!(pool.size, 1024);
    assert_eq!(pool.storage.len(), 1024);
    assert!(pool.storage.iter().all(|b| *b == 0));

    let bytes = c.pool_bytes(pool_id).unwrap();
    assert_eq!(bytes.len(), 1024);
}

#[test]
fn create_pool_of_size_zero_is_accepted_as_an_empty_pool() {
    // Not useful on its own, but we don't want to reject
    // it — a client that later resizes the pool starts here.
    let mut c = Client::new(ClientId(1));
    let shm_id = ObjectId::new(5);
    c.install_client_object(shm_id, Interface::Shm).unwrap();
    dispatch_create_pool(&mut c, shm_id, ObjectId::new(7), 0);
    let pool = c.pool(ObjectId::new(7)).unwrap();
    assert_eq!(pool.size, 0);
    assert_eq!(pool.storage.len(), 0);
}

#[test]
fn create_pool_above_max_size_is_rejected_with_pool_too_large() {
    let mut c = Client::new(ClientId(1));
    let shm_id = ObjectId::new(5);
    c.install_client_object(shm_id, Interface::Shm).unwrap();
    let pool_id = ObjectId::new(7);
    // MAX_POOL_SIZE + 1 is one byte over the limit.
    let payload = shm_create_pool_payload(pool_id, MAX_POOL_SIZE + 1);
    let header = MessageHeader::try_new(shm_id, 1, payload.len(), 1).unwrap();
    let err = c.dispatch_request(header, &payload).unwrap_err();
    match err {
        ClientError::PoolTooLarge { requested, max } => {
            assert_eq!(requested, MAX_POOL_SIZE + 1);
            assert_eq!(max, MAX_POOL_SIZE);
        }
        other => panic!("expected PoolTooLarge, got {other:?}"),
    }
    // Neither the object nor the storage was installed.
    assert_eq!(c.get(pool_id), None);
    assert!(c.pool(pool_id).is_none());
}

#[test]
fn pool_bytes_mut_lets_a_test_simulate_a_client_sab_write() {
    let mut c = Client::new(ClientId(1));
    let shm_id = ObjectId::new(5);
    c.install_client_object(shm_id, Interface::Shm).unwrap();
    let pool_id = ObjectId::new(7);
    dispatch_create_pool(&mut c, shm_id, pool_id, 16);

    let bytes = c.pool_bytes_mut(pool_id).expect("pool exists");
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = i as u8;
    }
    // Read-back path.
    let read = c.pool_bytes(pool_id).unwrap();
    assert_eq!(
        read,
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
    );
}

#[test]
fn two_pools_have_independent_storage() {
    let mut c = Client::new(ClientId(1));
    let shm_id = ObjectId::new(5);
    c.install_client_object(shm_id, Interface::Shm).unwrap();

    let pool_a = ObjectId::new(7);
    let pool_b = ObjectId::new(9);
    dispatch_create_pool(&mut c, shm_id, pool_a, 4);
    dispatch_create_pool(&mut c, shm_id, pool_b, 8);

    for (i, b) in c.pool_bytes_mut(pool_a).unwrap().iter_mut().enumerate() {
        *b = 0xA0 + i as u8;
    }
    for (i, b) in c.pool_bytes_mut(pool_b).unwrap().iter_mut().enumerate() {
        *b = 0xB0 + i as u8;
    }

    assert_eq!(c.pool_bytes(pool_a).unwrap(), &[0xA0, 0xA1, 0xA2, 0xA3]);
    assert_eq!(
        c.pool_bytes(pool_b).unwrap(),
        &[0xB0, 0xB1, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7]
    );
}

#[test]
fn create_buffer_records_buffer_info_in_the_per_client_map() {
    let mut c = Client::new(ClientId(1));
    let shm_id = ObjectId::new(5);
    c.install_client_object(shm_id, Interface::Shm).unwrap();
    let pool_id = ObjectId::new(7);
    dispatch_create_pool(&mut c, shm_id, pool_id, 8 * 8 * 4);

    let buffer_id = ObjectId::new(9);
    let payload = shm_pool_create_buffer_payload(buffer_id, 0, 8, 8, 8 * 4, 0 /* ARGB8888 */);
    let header = MessageHeader::try_new(pool_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();

    let info: &BufferInfo = c.buffer_info(buffer_id).expect("buffer installed");
    assert_eq!(info.pool_id, pool_id);
    assert_eq!(info.offset, 0);
    assert_eq!(info.width, 8);
    assert_eq!(info.height, 8);
    assert_eq!(info.stride, 32);
    assert_eq!(info.format, 0);
    assert_eq!(info.byte_end(), 8 * 32);
}

#[test]
fn create_buffer_outside_pool_bounds_is_rejected_with_buffer_out_of_pool() {
    let mut c = Client::new(ClientId(1));
    let shm_id = ObjectId::new(5);
    c.install_client_object(shm_id, Interface::Shm).unwrap();
    let pool_id = ObjectId::new(7);
    dispatch_create_pool(&mut c, shm_id, pool_id, 16);

    // 4x4 ARGB8888 buffer would need 4 * 16 = 64 bytes,
    // way past the 16-byte pool.
    let buffer_id = ObjectId::new(9);
    let payload = shm_pool_create_buffer_payload(buffer_id, 0, 4, 4, 16, 0);
    let header = MessageHeader::try_new(pool_id, 1, payload.len(), 0).unwrap();
    let err = c.dispatch_request(header, &payload).unwrap_err();
    match err {
        ClientError::BufferOutOfPool {
            pool_id: pid,
            pool_size,
            byte_end,
        } => {
            assert_eq!(pid, pool_id);
            assert_eq!(pool_size, 16);
            assert_eq!(byte_end, 64);
        }
        other => panic!("expected BufferOutOfPool, got {other:?}"),
    }
    // Buffer object was NOT installed on the error path.
    assert_eq!(c.get(buffer_id), None);
    assert!(c.buffer_info(buffer_id).is_none());
}

#[test]
fn create_buffer_with_offset_pushing_past_end_is_rejected() {
    let mut c = Client::new(ClientId(1));
    let shm_id = ObjectId::new(5);
    c.install_client_object(shm_id, Interface::Shm).unwrap();
    let pool_id = ObjectId::new(7);
    dispatch_create_pool(&mut c, shm_id, pool_id, 128);

    // 2x2 ARGB8888 = 16 bytes, but offset = 120 means the
    // buffer would span [120, 136) — 8 bytes past the end.
    let buffer_id = ObjectId::new(9);
    let payload = shm_pool_create_buffer_payload(buffer_id, 120, 2, 2, 8, 0);
    let header = MessageHeader::try_new(pool_id, 1, payload.len(), 0).unwrap();
    let err = c.dispatch_request(header, &payload).unwrap_err();
    assert!(matches!(err, ClientError::BufferOutOfPool { .. }));
}

#[test]
fn buffer_bytes_returns_the_sub_slice_of_the_parent_pool() {
    // Pool is 32 bytes, buffer is 2x2 ARGB8888 starting at
    // offset 16 with stride 8 → byte range [16, 32).
    let mut c = Client::new(ClientId(1));
    let shm_id = ObjectId::new(5);
    c.install_client_object(shm_id, Interface::Shm).unwrap();
    let pool_id = ObjectId::new(7);
    dispatch_create_pool(&mut c, shm_id, pool_id, 32);

    // Write a distinctive pattern into the pool so we can
    // verify the sub-slice lands on the right bytes.
    for (i, b) in c.pool_bytes_mut(pool_id).unwrap().iter_mut().enumerate() {
        *b = i as u8;
    }

    let buffer_id = ObjectId::new(9);
    let payload = shm_pool_create_buffer_payload(buffer_id, 16, 2, 2, 8, 0);
    let header = MessageHeader::try_new(pool_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();

    let bytes = c.buffer_bytes(buffer_id).expect("buffer bytes");
    assert_eq!(
        bytes,
        &[16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31]
    );
}

#[test]
fn buffer_bytes_sees_subsequent_pool_writes_live() {
    // After create_buffer, mutating the pool's storage
    // directly is visible through buffer_bytes — the
    // buffer holds no copy, just a view.
    let mut c = Client::new(ClientId(1));
    let shm_id = ObjectId::new(5);
    c.install_client_object(shm_id, Interface::Shm).unwrap();
    let pool_id = ObjectId::new(7);
    dispatch_create_pool(&mut c, shm_id, pool_id, 16);

    let buffer_id = ObjectId::new(9);
    let payload = shm_pool_create_buffer_payload(buffer_id, 0, 2, 2, 8, 0);
    let header = MessageHeader::try_new(pool_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();

    // Buffer starts zero-filled.
    assert!(c.buffer_bytes(buffer_id).unwrap().iter().all(|&b| b == 0));

    // Simulate a client SAB write.
    for (i, b) in c.pool_bytes_mut(pool_id).unwrap().iter_mut().enumerate() {
        *b = 0xFF - i as u8;
    }

    let bytes = c.buffer_bytes(buffer_id).unwrap();
    assert_eq!(
        bytes,
        &[
            0xFF, 0xFE, 0xFD, 0xFC, 0xFB, 0xFA, 0xF9, 0xF8, 0xF7, 0xF6, 0xF5, 0xF4, 0xF3, 0xF2,
            0xF1, 0xF0
        ]
    );
}

#[test]
fn buffer_info_none_for_unknown_object_id() {
    let c = Client::new(ClientId(1));
    assert!(c.buffer_info(ObjectId::new(999)).is_none());
    assert!(c.buffer_bytes(ObjectId::new(999)).is_none());
}

#[test]
fn pool_none_for_unknown_object_id() {
    let c = Client::new(ClientId(1));
    assert!(c.pool(ObjectId::new(999)).is_none());
    assert!(c.pool_bytes(ObjectId::new(999)).is_none());
}

#[test]
fn packed_pool_rows_update_only_the_validated_strided_region() {
    let mut c = Client::new(ClientId(1));
    let shm_id = ObjectId::new(5);
    let pool_id = ObjectId::new(7);
    c.install_client_object(shm_id, Interface::Shm).unwrap();
    dispatch_create_pool(&mut c, shm_id, pool_id, 16);

    let payload = shm_pool_write_rows_payload(1, 3, 2, 5, &[1, 2, 3, 4, 5, 6]);
    let header = MessageHeader::try_new(pool_id, 5, payload.len(), 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();

    assert_eq!(
        c.pool_bytes(pool_id).unwrap(),
        &[0, 1, 2, 3, 0, 0, 4, 5, 6, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(c.drain_journal().last().unwrap().opcode_name, "write_rows");
}

#[test]
fn packed_pool_rows_reject_malformed_geometry_and_extent_atomically() {
    let mut c = Client::new(ClientId(1));
    let shm_id = ObjectId::new(5);
    let pool_id = ObjectId::new(7);
    c.install_client_object(shm_id, Interface::Shm).unwrap();
    dispatch_create_pool(&mut c, shm_id, pool_id, 16);
    c.pool_bytes_mut(pool_id).unwrap().fill(0x55);
    let before = c.pool_bytes(pool_id).unwrap().to_vec();

    let truncated = shm_pool_write_rows_payload(1, 3, 2, 5, &[1, 2, 3, 4, 5]);
    let header = MessageHeader::try_new(pool_id, 5, truncated.len(), 0).unwrap();
    assert!(matches!(
        c.dispatch_request(header, &truncated).unwrap_err(),
        ClientError::Malformed { .. }
    ));
    assert_eq!(c.pool_bytes(pool_id).unwrap(), before);

    let overlapping = shm_pool_write_rows_payload(1, 3, 2, 2, &[1, 2, 3, 4, 5, 6]);
    let header = MessageHeader::try_new(pool_id, 5, overlapping.len(), 0).unwrap();
    assert!(matches!(
        c.dispatch_request(header, &overlapping).unwrap_err(),
        ClientError::InvalidPoolWriteRows {
            row_bytes: 3,
            rows: 2,
            stride: 2,
        }
    ));
    assert_eq!(c.pool_bytes(pool_id).unwrap(), before);

    let outside = shm_pool_write_rows_payload(14, 2, 2, u32::MAX, &[1, 2, 3, 4]);
    let header = MessageHeader::try_new(pool_id, 5, outside.len(), 0).unwrap();
    assert!(matches!(
        c.dispatch_request(header, &outside).unwrap_err(),
        ClientError::BufferOutOfPool { .. }
    ));
    assert_eq!(c.pool_bytes(pool_id).unwrap(), before);
}

// ---- Surface state machine (attach / damage / commit) ----------

/// Build a `Client` that already has a compositor, shm,
/// a pool, a surface, and one buffer carved out of the
/// pool. Returns the client plus every allocated id.
fn boot_client_with_surface_and_buffer() -> (Client, ObjectId, ObjectId, ObjectId) {
    let mut c = Client::new(ClientId(1));
    let registry_id = ObjectId::new(3);
    let compositor_id = ObjectId::new(5);
    let shm_id = ObjectId::new(7);
    let surface_id = ObjectId::new(9);
    let pool_id = ObjectId::new(11);
    let buffer_id = ObjectId::new(13);

    // get_registry → registry auto-installed.
    let header = MessageHeader::try_new(ObjectId::DISPLAY, 2, 4, 0).unwrap();
    c.dispatch_request(header, &new_id_payload(registry_id))
        .unwrap();

    // bind compositor + shm.
    for (name, iface_name, bound) in [
        (1u32, "pmd_compositor", compositor_id),
        (2, "pmd_shm", shm_id),
    ] {
        let payload = registry_bind_payload(name, iface_name, 1, bound);
        let header = MessageHeader::try_new(registry_id, 1, payload.len(), 0).unwrap();
        c.dispatch_request(header, &payload).unwrap();
    }

    // create_surface.
    let payload = new_id_payload(surface_id);
    let header = MessageHeader::try_new(compositor_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();

    // create_pool (64 bytes = 4x4 ARGB8888).
    let payload = shm_create_pool_payload(pool_id, 64);
    let header = MessageHeader::try_new(shm_id, 1, payload.len(), 1).unwrap();
    c.dispatch_request(header, &payload).unwrap();

    // create_buffer (4x4 at offset 0).
    let payload = shm_pool_create_buffer_payload(buffer_id, 0, 4, 4, 16, 0);
    let header = MessageHeader::try_new(pool_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();
    c.drain_journal();

    (c, surface_id, pool_id, buffer_id)
}

/// Build a `pmd_surface.attach(buffer_id, x, y)` payload
/// (12 bytes): `u32 buffer_id, i32 x, i32 y`.
fn surface_attach_payload(buffer_id: ObjectId, x: i32, y: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    out.extend_from_slice(&buffer_id.raw().to_le_bytes());
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    out
}

/// Build a `pmd_surface.damage(x, y, w, h)` payload
/// (16 bytes): four i32s.
fn surface_damage_payload(x: i32, y: i32, w: i32, h: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    for v in [x, y, w, h] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn surface_patch_payload(x: i32, y: i32, width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + pixels.len());
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(pixels);
    out
}

fn shm_pool_write_payload(offset: u32, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + bytes.len());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(bytes);
    out
}

fn dispatch_attach(c: &mut Client, surface_id: ObjectId, buffer_id: ObjectId, x: i32, y: i32) {
    let payload = surface_attach_payload(buffer_id, x, y);
    let header = MessageHeader::try_new(surface_id, 2, payload.len(), 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();
}

fn dispatch_damage(c: &mut Client, surface_id: ObjectId, x: i32, y: i32, w: i32, h: i32) {
    let payload = surface_damage_payload(x, y, w, h);
    let header = MessageHeader::try_new(surface_id, 3, payload.len(), 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();
}

fn dispatch_commit(c: &mut Client, surface_id: ObjectId) {
    let header = MessageHeader::try_new(surface_id, 7, 0, 0).unwrap();
    c.dispatch_request(header, &[]).unwrap();
}

fn dispatch_patch(
    c: &mut Client,
    surface_id: ObjectId,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    pixels: &[u8],
) -> Result<(), ClientError> {
    let payload = surface_patch_payload(x, y, width, height, pixels);
    let header = MessageHeader::try_new(surface_id, 8, payload.len(), 0).unwrap();
    c.dispatch_request(header, &payload)
}

fn create_test_surface(c: &mut Client, surface_id: ObjectId) {
    let payload = new_id_payload(surface_id);
    let header = MessageHeader::try_new(ObjectId::new(5), 1, payload.len(), 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();
}

fn create_test_buffer(c: &mut Client, buffer_id: ObjectId, info: BufferInfo) {
    let payload = shm_pool_create_buffer_payload(
        buffer_id,
        info.offset,
        info.width,
        info.height,
        info.stride,
        info.format,
    );
    let header = MessageHeader::try_new(info.pool_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(header, &payload).unwrap();
}

fn drain_event_descriptors(c: &mut Client) -> Vec<(ObjectId, u16)> {
    let bytes = c.drain_pending_events();
    let mut offset = 0;
    let mut events = Vec::new();
    while offset < bytes.len() {
        let header = MessageHeader::decode(&bytes[offset..]).unwrap();
        events.push((header.object_id, header.opcode));
        offset += header.length as usize;
    }
    events
}

#[test]
fn create_surface_initializes_empty_surface_state() {
    let (c, surface_id, _, _) = boot_client_with_surface_and_buffer();
    let surface: &Surface = c.surface(surface_id).expect("surface installed");
    assert_eq!(surface.id, surface_id);
    assert!(surface.pending_buffer.is_none());
    assert!(surface.current_buffer.is_none());
    assert!(surface.pending_damage.is_empty());
    assert_eq!(surface.commit_count, 0);
}

#[test]
fn attach_records_pending_attachment_without_promoting_current() {
    let (mut c, surface_id, _, buffer_id) = boot_client_with_surface_and_buffer();
    dispatch_attach(&mut c, surface_id, buffer_id, 3, 5);
    let surface = c.surface(surface_id).unwrap();
    assert_eq!(
        surface.pending_buffer,
        Some(BufferAttachment {
            buffer_id,
            x: 3,
            y: 5,
        })
    );
    // Current is still empty until commit.
    assert!(surface.current_buffer.is_none());
    assert_eq!(surface.commit_count, 0);
}

#[test]
fn attach_with_unknown_buffer_id_rejects_with_unknown_buffer() {
    let (mut c, surface_id, _, _) = boot_client_with_surface_and_buffer();
    let stray = ObjectId::new(9999);
    let payload = surface_attach_payload(stray, 0, 0);
    let header = MessageHeader::try_new(surface_id, 2, payload.len(), 0).unwrap();
    let err = c.dispatch_request(header, &payload).unwrap_err();
    match err {
        ClientError::UnknownBuffer { buffer_id } => assert_eq!(buffer_id, stray),
        other => panic!("expected UnknownBuffer, got {other:?}"),
    }
    // Pending state is untouched.
    assert!(c.surface(surface_id).unwrap().pending_buffer.is_none());
}

#[test]
fn attach_with_null_buffer_id_clears_pending_for_detach() {
    let (mut c, surface_id, _, buffer_id) = boot_client_with_surface_and_buffer();
    dispatch_attach(&mut c, surface_id, buffer_id, 0, 0);
    assert!(c.surface(surface_id).unwrap().pending_buffer.is_some());

    // attach(NULL) — wire payload is (0, 0, 0).
    dispatch_attach(&mut c, surface_id, ObjectId::NULL, 0, 0);
    let surface = c.surface(surface_id).unwrap();
    assert!(surface.pending_buffer.is_none());
}

#[test]
fn damage_appends_to_pending_damage_list() {
    let (mut c, surface_id, _, _) = boot_client_with_surface_and_buffer();
    dispatch_damage(&mut c, surface_id, 0, 0, 10, 20);
    dispatch_damage(&mut c, surface_id, 5, 5, 1, 1);
    let surface = c.surface(surface_id).unwrap();
    assert_eq!(
        surface.pending_damage,
        vec![
            DamageRect {
                x: 0,
                y: 0,
                width: 10,
                height: 20,
            },
            DamageRect {
                x: 5,
                y: 5,
                width: 1,
                height: 1,
            },
        ]
    );
}

#[test]
fn commit_promotes_pending_buffer_to_current_and_clears_damage() {
    let (mut c, surface_id, _, buffer_id) = boot_client_with_surface_and_buffer();
    dispatch_attach(&mut c, surface_id, buffer_id, 10, 20);
    dispatch_damage(&mut c, surface_id, 0, 0, 4, 4);
    dispatch_commit(&mut c, surface_id);

    let surface = c.surface(surface_id).unwrap();
    // Pending is cleared.
    assert!(surface.pending_buffer.is_none());
    assert!(surface.pending_damage.is_empty());
    // Current is now the attached buffer.
    assert_eq!(
        surface.current_buffer,
        Some(BufferAttachment {
            buffer_id,
            x: 10,
            y: 20,
        })
    );
    assert_eq!(surface.commit_count, 1);
}

#[test]
fn commit_without_prior_attach_increments_counter_but_leaves_buffers_alone() {
    let (mut c, surface_id, _, _) = boot_client_with_surface_and_buffer();
    dispatch_commit(&mut c, surface_id);
    let surface = c.surface(surface_id).unwrap();
    assert!(surface.pending_buffer.is_none());
    assert!(surface.current_buffer.is_none());
    assert_eq!(surface.commit_count, 1);
}

#[test]
fn commit_with_no_new_attach_keeps_previously_current_buffer() {
    // Wayland semantics: a commit without an intervening
    // attach leaves the current buffer alone — it does NOT
    // drop it to None. This is how a client can submit a
    // damage-only update to the same pixels.
    let (mut c, surface_id, _, buffer_id) = boot_client_with_surface_and_buffer();
    dispatch_attach(&mut c, surface_id, buffer_id, 0, 0);
    dispatch_commit(&mut c, surface_id);
    let first = c.surface(surface_id).unwrap().current_buffer;
    assert!(first.is_some());

    // Second commit without a new attach.
    dispatch_damage(&mut c, surface_id, 1, 1, 2, 2);
    dispatch_commit(&mut c, surface_id);

    let surface = c.surface(surface_id).unwrap();
    assert_eq!(surface.current_buffer, first, "current buffer must persist");
    assert_eq!(surface.commit_count, 2);
}

#[test]
fn detach_via_attach_null_then_commit_clears_current_buffer() {
    let (mut c, surface_id, _, buffer_id) = boot_client_with_surface_and_buffer();
    dispatch_attach(&mut c, surface_id, buffer_id, 0, 0);
    dispatch_commit(&mut c, surface_id);
    assert!(c.surface(surface_id).unwrap().current_buffer.is_some());

    // Now attach(null) + commit → detach.
    dispatch_attach(&mut c, surface_id, ObjectId::NULL, 0, 0);
    dispatch_commit(&mut c, surface_id);
    assert!(c.surface(surface_id).unwrap().current_buffer.is_none());
    assert!(!c.surface(surface_id).unwrap().pending_attach);
}

#[test]
fn two_attaches_before_a_commit_keep_only_the_last_pending() {
    let (mut c, surface_id, _, buffer_id) = boot_client_with_surface_and_buffer();
    dispatch_attach(&mut c, surface_id, buffer_id, 1, 1);
    dispatch_attach(&mut c, surface_id, buffer_id, 2, 2);
    let surface = c.surface(surface_id).unwrap();
    assert_eq!(
        surface.pending_buffer,
        Some(BufferAttachment {
            buffer_id,
            x: 2,
            y: 2,
        })
    );
}

#[test]
fn patch_current_updates_only_the_requested_current_rows_without_release() {
    let (mut c, surface_id, pool_id, buffer_id) = boot_client_with_surface_and_buffer();
    dispatch_attach(&mut c, surface_id, buffer_id, 0, 0);
    dispatch_commit(&mut c, surface_id);
    assert!(drain_event_descriptors(&mut c).is_empty());

    let attachment = c.surface(surface_id).unwrap().current_buffer;
    let pixels = [1, 2, 3, 4, 5, 6, 7, 8];
    dispatch_patch(&mut c, surface_id, 1, 2, 2, 1, &pixels).unwrap();

    let pool = c.pool_bytes(pool_id).unwrap();
    assert_eq!(&pool[36..44], &pixels);
    assert!(pool[..36].iter().all(|byte| *byte == 0));
    assert!(pool[44..].iter().all(|byte| *byte == 0));
    let surface = c.surface(surface_id).unwrap();
    assert_eq!(surface.current_buffer, attachment);
    assert!(!surface.pending_attach);
    assert!(surface.pending_damage.is_empty());
    assert_eq!(surface.commit_count, 2);
    assert!(drain_event_descriptors(&mut c).is_empty());
    assert_eq!(
        c.drain_journal().last().unwrap().opcode_name,
        "patch_current"
    );
}

#[test]
fn patch_current_missing_malformed_and_pending_rejections_are_atomic() {
    let (mut c, surface_id, pool_id, buffer_id) = boot_client_with_surface_and_buffer();
    let pool_before = c.pool_bytes(pool_id).unwrap().to_vec();
    let surface_before = c.surface(surface_id).unwrap().clone();
    assert_eq!(
        dispatch_patch(&mut c, surface_id, 0, 0, 1, 1, &[0; 4]).unwrap_err(),
        ClientError::SurfacePatchNoCurrentBuffer { surface_id }
    );
    assert_eq!(c.pool_bytes(pool_id).unwrap(), pool_before);
    assert_eq!(c.surface(surface_id).unwrap(), &surface_before);

    dispatch_attach(&mut c, surface_id, buffer_id, 0, 0);
    dispatch_commit(&mut c, surface_id);
    c.drain_pending_events();
    let malformed = surface_patch_payload(0, 0, 1, 1, &[1, 2, 3]);
    let header = MessageHeader::try_new(surface_id, 8, malformed.len(), 0).unwrap();
    let pool_before = c.pool_bytes(pool_id).unwrap().to_vec();
    let surface_before = c.surface(surface_id).unwrap().clone();
    assert!(matches!(
        c.dispatch_request(header, &malformed).unwrap_err(),
        ClientError::Malformed {
            interface: Interface::Surface,
            opcode: 8,
            error: display_proto::DecodeError::PayloadLengthMismatch { .. },
        }
    ));
    assert_eq!(c.pool_bytes(pool_id).unwrap(), pool_before);
    assert_eq!(c.surface(surface_id).unwrap(), &surface_before);

    dispatch_attach(&mut c, surface_id, buffer_id, 3, 3);
    let pending_before = c.surface(surface_id).unwrap().clone();
    assert_eq!(
        dispatch_patch(&mut c, surface_id, 0, 0, 1, 1, &[9; 4]).unwrap_err(),
        ClientError::SurfacePatchHasPendingState { surface_id }
    );
    assert_eq!(c.surface(surface_id).unwrap(), &pending_before);
    assert_eq!(c.pool_bytes(pool_id).unwrap(), pool_before);

    dispatch_commit(&mut c, surface_id);
    c.drain_pending_events();
    dispatch_damage(&mut c, surface_id, 0, 0, 1, 1);
    let pending_before = c.surface(surface_id).unwrap().clone();
    assert_eq!(
        dispatch_patch(&mut c, surface_id, 0, 0, 1, 1, &[9; 4]).unwrap_err(),
        ClientError::SurfacePatchHasPendingState { surface_id }
    );
    assert_eq!(c.surface(surface_id).unwrap(), &pending_before);
    assert_eq!(c.pool_bytes(pool_id).unwrap(), pool_before);
}

#[test]
fn patch_current_accepts_exact_cap_and_rejects_the_next_pixel_atomically() {
    let (mut c, surface_id, pool_id, buffer_id) = boot_client_with_surface_and_buffer();
    let next_pixel_bytes = MAX_SURFACE_PATCH_BYTES + 4;
    {
        let pool = c.pools.get_mut(&pool_id).unwrap();
        pool.size = next_pixel_bytes as u32;
        pool.storage.resize(next_pixel_bytes, 0);
        let buffer = c.buffers.get_mut(&buffer_id).unwrap();
        buffer.width = (next_pixel_bytes / 4) as u32;
        buffer.height = 1;
        buffer.stride = next_pixel_bytes as u32;
    }
    dispatch_attach(&mut c, surface_id, buffer_id, 0, 0);
    dispatch_commit(&mut c, surface_id);
    c.drain_pending_events();

    let at_cap = vec![0x5a; MAX_SURFACE_PATCH_BYTES];
    dispatch_patch(
        &mut c,
        surface_id,
        0,
        0,
        (MAX_SURFACE_PATCH_BYTES / 4) as u32,
        1,
        &at_cap,
    )
    .unwrap();
    assert_eq!(
        &c.pool_bytes(pool_id).unwrap()[..MAX_SURFACE_PATCH_BYTES],
        at_cap
    );

    let pool_before = c.pool_bytes(pool_id).unwrap().to_vec();
    let surface_before = c.surface(surface_id).unwrap().clone();
    let over_cap = vec![0xa5; next_pixel_bytes];
    assert_eq!(
        dispatch_patch(
            &mut c,
            surface_id,
            0,
            0,
            (next_pixel_bytes / 4) as u32,
            1,
            &over_cap,
        )
        .unwrap_err(),
        ClientError::SurfacePatchTooLarge {
            bytes: next_pixel_bytes as u64,
            max: MAX_SURFACE_PATCH_BYTES,
        }
    );
    assert_eq!(c.pool_bytes(pool_id).unwrap(), pool_before);
    assert_eq!(c.surface(surface_id).unwrap(), &surface_before);
}

#[test]
fn patch_current_format_geometry_and_backing_failures_do_not_mutate() {
    let (mut c, surface_id, pool_id, buffer_id) = boot_client_with_surface_and_buffer();
    dispatch_attach(&mut c, surface_id, buffer_id, 0, 0);
    dispatch_commit(&mut c, surface_id);
    c.drain_pending_events();
    let pixels = [7; 4];

    c.buffers.get_mut(&buffer_id).unwrap().format = 99;
    let pool_before = c.pool_bytes(pool_id).unwrap().to_vec();
    let surface_before = c.surface(surface_id).unwrap().clone();
    assert_eq!(
        dispatch_patch(&mut c, surface_id, 0, 0, 1, 1, &pixels).unwrap_err(),
        ClientError::SurfacePatchUnsupportedFormat {
            buffer_id,
            format: 99,
        }
    );
    assert_eq!(c.pool_bytes(pool_id).unwrap(), pool_before);
    assert_eq!(c.surface(surface_id).unwrap(), &surface_before);

    c.buffers.get_mut(&buffer_id).unwrap().format = 0;
    assert!(matches!(
        dispatch_patch(&mut c, surface_id, -1, 0, 1, 1, &pixels).unwrap_err(),
        ClientError::SurfacePatchInvalidGeometry { .. }
    ));
    assert_eq!(c.pool_bytes(pool_id).unwrap(), pool_before);
    assert_eq!(c.surface(surface_id).unwrap(), &surface_before);

    c.buffers.get_mut(&buffer_id).unwrap().stride = 1;
    assert_eq!(
        dispatch_patch(&mut c, surface_id, 0, 0, 1, 1, &pixels).unwrap_err(),
        ClientError::SurfacePatchInvalidBacking { buffer_id, pool_id }
    );
    assert_eq!(c.pool_bytes(pool_id).unwrap(), pool_before);
    assert_eq!(c.surface(surface_id).unwrap(), &surface_before);
}

#[test]
fn patch_current_fails_closed_for_aliased_or_incomplete_other_current_state() {
    let (mut c, surface_id, pool_id, buffer_id) = boot_client_with_surface_and_buffer();
    dispatch_attach(&mut c, surface_id, buffer_id, 0, 0);
    dispatch_commit(&mut c, surface_id);
    c.drain_pending_events();
    let other_surface = ObjectId::new(15);
    create_test_surface(&mut c, other_surface);
    c.surfaces.get_mut(&other_surface).unwrap().current_buffer = Some(BufferAttachment {
        buffer_id,
        x: 0,
        y: 0,
    });

    let pool_before = c.pool_bytes(pool_id).unwrap().to_vec();
    let target_before = c.surface(surface_id).unwrap().clone();
    assert_eq!(
        dispatch_patch(&mut c, surface_id, 0, 0, 1, 1, &[1; 4]).unwrap_err(),
        ClientError::SurfacePatchAliasedCurrentBuffer {
            surface_id,
            other_surface_id: other_surface,
        }
    );
    assert_eq!(c.pool_bytes(pool_id).unwrap(), pool_before);
    assert_eq!(c.surface(surface_id).unwrap(), &target_before);

    let missing_buffer = ObjectId::new(17);
    c.surfaces.get_mut(&other_surface).unwrap().current_buffer = Some(BufferAttachment {
        buffer_id: missing_buffer,
        x: 0,
        y: 0,
    });
    assert_eq!(
        dispatch_patch(&mut c, surface_id, 0, 0, 1, 1, &[1; 4]).unwrap_err(),
        ClientError::SurfacePatchInvalidAliasBacking {
            other_surface_id: other_surface,
            buffer_id: missing_buffer,
        }
    );
    assert_eq!(c.pool_bytes(pool_id).unwrap(), pool_before);
    assert_eq!(c.surface(surface_id).unwrap(), &target_before);
}

#[test]
fn pool_write_cannot_change_current_bytes_before_a_patch_recomposition() {
    let (mut c, surface_id, pool_id, buffer_id) = boot_client_with_surface_and_buffer();
    {
        let buffer = c.buffers.get_mut(&buffer_id).unwrap();
        buffer.width = 2;
        buffer.height = 1;
        buffer.stride = 8;
    }
    let red = [0, 0, 0xff, 0xff, 0, 0, 0xff, 0xff];
    let payload = shm_pool_write_payload(0, &red);
    c.dispatch_request(
        MessageHeader::try_new(pool_id, 4, payload.len(), 0).unwrap(),
        &payload,
    )
    .unwrap();
    dispatch_attach(&mut c, surface_id, buffer_id, 0, 0);
    dispatch_commit(&mut c, surface_id);
    c.drain_pending_events();

    let green = [0, 0xff, 0, 0xff];
    let payload = shm_pool_write_payload(0, &green);
    let surface_before = c.surface(surface_id).unwrap().clone();
    assert_eq!(
        c.dispatch_request(
            MessageHeader::try_new(pool_id, 4, payload.len(), 0).unwrap(),
            &payload,
        )
        .unwrap_err(),
        ClientError::PoolWriteIntersectsCurrentBuffer {
            pool_id,
            surface_id,
            buffer_id,
        }
    );
    assert_eq!(&c.pool_bytes(pool_id).unwrap()[..8], &red);
    assert_eq!(c.surface(surface_id).unwrap(), &surface_before);

    let blue = [0xff, 0, 0, 0xff];
    dispatch_patch(&mut c, surface_id, 1, 0, 1, 1, &blue).unwrap();
    assert_eq!(&c.pool_bytes(pool_id).unwrap()[..4], &red[..4]);
    assert_eq!(&c.pool_bytes(pool_id).unwrap()[4..8], &blue);
}

#[test]
fn write_rows_checks_actual_rows_not_stride_gaps_and_rejects_overlap_atomically() {
    let (mut c, surface_id, pool_id, buffer_id) = boot_client_with_surface_and_buffer();
    {
        let buffer = c.buffers.get_mut(&buffer_id).unwrap();
        buffer.offset = 4;
        buffer.width = 1;
        buffer.height = 1;
        buffer.stride = 4;
    }
    dispatch_attach(&mut c, surface_id, buffer_id, 0, 0);
    dispatch_commit(&mut c, surface_id);
    c.drain_pending_events();

    let gap_write = shm_pool_write_rows_payload(0, 4, 2, 8, &[1, 1, 1, 1, 2, 2, 2, 2]);
    c.dispatch_request(
        MessageHeader::try_new(pool_id, 5, gap_write.len(), 0).unwrap(),
        &gap_write,
    )
    .unwrap();
    assert_eq!(&c.pool_bytes(pool_id).unwrap()[0..4], &[1; 4]);
    assert_eq!(&c.pool_bytes(pool_id).unwrap()[4..8], &[0; 4]);
    assert_eq!(&c.pool_bytes(pool_id).unwrap()[8..12], &[2; 4]);

    let overlap = shm_pool_write_rows_payload(4, 4, 1, 4, &[9; 4]);
    let before = c.pool_bytes(pool_id).unwrap().to_vec();
    let surface_before = c.surface(surface_id).unwrap().clone();
    assert!(matches!(
        c.dispatch_request(
            MessageHeader::try_new(pool_id, 5, overlap.len(), 0).unwrap(),
            &overlap,
        )
        .unwrap_err(),
        ClientError::PoolWriteIntersectsCurrentBuffer { .. }
    ));
    assert_eq!(c.pool_bytes(pool_id).unwrap(), before);
    assert_eq!(c.surface(surface_id).unwrap(), &surface_before);
}

#[test]
fn commit_rejects_cross_surface_same_and_overlapping_current_backing() {
    let (mut c, first_surface, pool_id, first_buffer) = boot_client_with_surface_and_buffer();
    dispatch_attach(&mut c, first_surface, first_buffer, 0, 0);
    dispatch_commit(&mut c, first_surface);
    c.drain_pending_events();

    let second_surface = ObjectId::new(15);
    create_test_surface(&mut c, second_surface);
    dispatch_attach(&mut c, second_surface, first_buffer, 0, 0);
    let before = c.surface(second_surface).unwrap().clone();
    let error = c
        .dispatch_request(frame_request(second_surface, 7), &[])
        .unwrap_err();
    assert_eq!(
        error,
        ClientError::SurfaceCommitAliasedBuffer {
            surface_id: second_surface,
            buffer_id: first_buffer,
            conflicting_surface_id: first_surface,
            conflicting_buffer_id: first_buffer,
        }
    );
    assert_eq!(c.surface(second_surface).unwrap(), &before);

    let overlapping_buffer = ObjectId::new(17);
    create_test_buffer(
        &mut c,
        overlapping_buffer,
        BufferInfo {
            pool_id,
            offset: 32,
            width: 2,
            height: 4,
            stride: 8,
            format: 0,
        },
    );
    dispatch_attach(&mut c, second_surface, overlapping_buffer, 0, 0);
    let before = c.surface(second_surface).unwrap().clone();
    assert!(matches!(
        c.dispatch_request(frame_request(second_surface, 7), &[])
            .unwrap_err(),
        ClientError::SurfaceCommitAliasedBuffer {
            conflicting_surface_id,
            ..
        } if conflicting_surface_id == first_surface
    ));
    assert_eq!(c.surface(second_surface).unwrap(), &before);
    assert!(drain_event_descriptors(&mut c).is_empty());
}

#[test]
fn commit_rejects_same_surface_overlapping_replacement_without_state_change() {
    let (mut c, surface_id, pool_id, first_buffer) = boot_client_with_surface_and_buffer();
    dispatch_attach(&mut c, surface_id, first_buffer, 0, 0);
    dispatch_commit(&mut c, surface_id);
    c.drain_pending_events();
    let overlapping_buffer = ObjectId::new(15);
    create_test_buffer(
        &mut c,
        overlapping_buffer,
        BufferInfo {
            pool_id,
            offset: 32,
            width: 2,
            height: 4,
            stride: 8,
            format: 0,
        },
    );
    dispatch_attach(&mut c, surface_id, overlapping_buffer, 0, 0);
    dispatch_damage(&mut c, surface_id, 1, 1, 1, 1);
    let surface_before = c.surface(surface_id).unwrap().clone();
    let pool_before = c.pool_bytes(pool_id).unwrap().to_vec();
    assert_eq!(
        c.dispatch_request(frame_request(surface_id, 7), &[])
            .unwrap_err(),
        ClientError::SurfaceCommitAliasedBuffer {
            surface_id,
            buffer_id: overlapping_buffer,
            conflicting_surface_id: surface_id,
            conflicting_buffer_id: first_buffer,
        }
    );
    assert_eq!(c.surface(surface_id).unwrap(), &surface_before);
    assert_eq!(c.pool_bytes(pool_id).unwrap(), pool_before);
    assert!(drain_event_descriptors(&mut c).is_empty());
}

#[test]
fn releases_only_live_buffers_that_actually_cease_to_be_current() {
    let (mut c, surface_id, pool_id, first_buffer) = boot_client_with_surface_and_buffer();
    let resize = 128u32.to_le_bytes();
    c.dispatch_request(
        MessageHeader::try_new(pool_id, 2, resize.len(), 0).unwrap(),
        &resize,
    )
    .unwrap();
    let second_buffer = ObjectId::new(15);
    create_test_buffer(
        &mut c,
        second_buffer,
        BufferInfo {
            pool_id,
            offset: 64,
            width: 4,
            height: 4,
            stride: 16,
            format: 0,
        },
    );

    dispatch_attach(&mut c, surface_id, first_buffer, 0, 0);
    dispatch_commit(&mut c, surface_id);
    assert!(drain_event_descriptors(&mut c).is_empty(), "initial commit");

    dispatch_damage(&mut c, surface_id, 0, 0, 1, 1);
    dispatch_commit(&mut c, surface_id);
    assert!(
        drain_event_descriptors(&mut c).is_empty(),
        "damage-only commit"
    );

    dispatch_attach(&mut c, surface_id, first_buffer, 1, 1);
    dispatch_commit(&mut c, surface_id);
    assert!(
        drain_event_descriptors(&mut c).is_empty(),
        "same-buffer reattach"
    );

    dispatch_attach(&mut c, surface_id, second_buffer, 0, 0);
    dispatch_commit(&mut c, surface_id);
    assert_eq!(drain_event_descriptors(&mut c), [(first_buffer, 1)]);

    dispatch_attach(&mut c, surface_id, ObjectId::NULL, 0, 0);
    dispatch_commit(&mut c, surface_id);
    assert_eq!(drain_event_descriptors(&mut c), [(second_buffer, 1)]);
}

#[test]
fn destroying_a_live_roleless_surface_releases_current_before_deferred_surface_delete() {
    let (mut c, surface_id, _, buffer_id) = boot_client_with_surface_and_buffer();
    dispatch_attach(&mut c, surface_id, buffer_id, 0, 0);
    dispatch_commit(&mut c, surface_id);
    c.drain_pending_events();

    c.dispatch_request(frame_request(surface_id, 1), &[])
        .unwrap();
    assert_eq!(drain_event_descriptors(&mut c), [(buffer_id, 1)]);
    assert_eq!(c.cancelled_frame_callback_lifecycle_count(), 1);
}

#[test]
fn replacing_a_client_destroyed_current_buffer_emits_delete_without_release() {
    let (mut c, surface_id, pool_id, first_buffer) = boot_client_with_surface_and_buffer();
    let resize = 128u32.to_le_bytes();
    c.dispatch_request(
        MessageHeader::try_new(pool_id, 2, resize.len(), 0).unwrap(),
        &resize,
    )
    .unwrap();
    let second_buffer = ObjectId::new(15);
    create_test_buffer(
        &mut c,
        second_buffer,
        BufferInfo {
            pool_id,
            offset: 64,
            width: 4,
            height: 4,
            stride: 16,
            format: 0,
        },
    );
    dispatch_attach(&mut c, surface_id, first_buffer, 0, 0);
    dispatch_commit(&mut c, surface_id);
    c.drain_pending_events();

    c.dispatch_request(frame_request(first_buffer, 1), &[])
        .unwrap();
    assert!(c.drain_pending_events().is_empty());
    dispatch_attach(&mut c, surface_id, second_buffer, 0, 0);
    dispatch_commit(&mut c, surface_id);

    let events = c.drain_pending_events();
    let header = MessageHeader::decode(&events).unwrap();
    assert_eq!((header.object_id, header.opcode), (ObjectId::DISPLAY, 2));
    assert_eq!(events.len(), header.length as usize);
    assert_eq!(
        u32::from_le_bytes(events[HEADER_SIZE..HEADER_SIZE + 4].try_into().unwrap()),
        first_buffer.raw()
    );
}

#[test]
fn patch_current_uses_destroyed_but_retained_current_backing() {
    let (mut c, surface_id, pool_id, buffer_id) = boot_client_with_surface_and_buffer();
    dispatch_attach(&mut c, surface_id, buffer_id, 0, 0);
    dispatch_commit(&mut c, surface_id);
    c.drain_pending_events();
    c.dispatch_request(frame_request(buffer_id, 1), &[])
        .unwrap();
    c.dispatch_request(frame_request(pool_id, 3), &[]).unwrap();
    assert!(c.drain_pending_events().is_empty());

    dispatch_patch(&mut c, surface_id, 0, 0, 1, 1, &[4, 3, 2, 1]).unwrap();
    assert_eq!(&c.pool_bytes(pool_id).unwrap()[..4], &[4, 3, 2, 1]);
    assert!(drain_event_descriptors(&mut c).is_empty());
}

#[test]
fn surface_none_for_unknown_object_id() {
    let c = Client::new(ClientId(1));
    assert!(c.surface(ObjectId::new(999)).is_none());
}

// ---- pmd_xdg_shell + pmd_xdg_toplevel --------------------------

/// Build an `xdg_shell.get_toplevel(new_id, surface_id)`
/// payload: two little-endian u32 object ids back-to-back.
fn xdg_get_toplevel_payload(new_id: ObjectId, surface_id: ObjectId) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(&new_id.raw().to_le_bytes());
    out.extend_from_slice(&surface_id.raw().to_le_bytes());
    out
}

/// Build an `xdg_toplevel.set_title(string)` payload:
/// u32 length + bytes + NUL padding to 4-byte boundary.
fn xdg_set_string_payload(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let pad = (4 - (bytes.len() % 4)) % 4;
    let mut out = Vec::with_capacity(4 + bytes.len() + pad);
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
    out.extend(core::iter::repeat_n(0u8, pad));
    out
}

/// Bring a fresh client to the point where it has a bound
/// xdg_shell global + one surface, ready to exercise the
/// toplevel dispatch. Returns the allocated surface + shell
/// ids.
fn boot_client_with_xdg_shell() -> (Client, ObjectId, ObjectId) {
    let mut c = Client::new(ClientId(1));
    let registry_id = ObjectId::new(3);
    let compositor_id = ObjectId::new(5);
    let xdg_shell_id = ObjectId::new(7);
    let surface_id = ObjectId::new(9);

    // get_registry.
    let h = MessageHeader::try_new(ObjectId::DISPLAY, 2, 4, 0).unwrap();
    c.dispatch_request(h, &new_id_payload(registry_id)).unwrap();

    // bind compositor + xdg_shell.
    for (name, iface_name, bound) in [
        (1u32, "pmd_compositor", compositor_id),
        (2, "pmd_xdg_shell", xdg_shell_id),
    ] {
        let payload = registry_bind_payload(name, iface_name, 1, bound);
        let h = MessageHeader::try_new(registry_id, 1, payload.len(), 0).unwrap();
        c.dispatch_request(h, &payload).unwrap();
    }

    // create_surface.
    let payload = new_id_payload(surface_id);
    let h = MessageHeader::try_new(compositor_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(h, &payload).unwrap();
    c.drain_journal();

    (c, surface_id, xdg_shell_id)
}

#[test]
fn dispatch_xdg_get_toplevel_installs_toplevel_with_auto_layout_origin() {
    let (mut c, surface_id, xdg_shell_id) = boot_client_with_xdg_shell();
    let toplevel_id = ObjectId::new(11);
    let payload = xdg_get_toplevel_payload(toplevel_id, surface_id);
    let h = MessageHeader::try_new(xdg_shell_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(h, &payload).unwrap();

    assert_eq!(c.get(toplevel_id), Some(Interface::XdgToplevel));
    let top: &Toplevel = c.toplevel(toplevel_id).expect("toplevel exists");
    assert_eq!(top.id, toplevel_id);
    assert_eq!(top.surface_id, surface_id);
    assert_eq!(top.title, "");
    assert_eq!(top.app_id, "");
    // First toplevel lands at (0, 0).
    assert_eq!(top.x, 0);
    assert_eq!(top.y, 0);
}

#[test]
fn a_second_toplevel_is_placed_at_the_auto_layout_step() {
    let (mut c, surface_a, xdg_shell_id) = boot_client_with_xdg_shell();

    // Create one more surface so the second toplevel has
    // something to wrap.
    let surface_b = ObjectId::new(15);
    let compositor_id = c
        .objects
        .iter()
        .find_map(|(id, iface)| {
            if *iface == Interface::Compositor {
                Some(*id)
            } else {
                None
            }
        })
        .unwrap();
    let payload = new_id_payload(surface_b);
    let h = MessageHeader::try_new(compositor_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(h, &payload).unwrap();

    let top_a = ObjectId::new(11);
    let top_b = ObjectId::new(13);
    let payload = xdg_get_toplevel_payload(top_a, surface_a);
    let h = MessageHeader::try_new(xdg_shell_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(h, &payload).unwrap();
    let payload = xdg_get_toplevel_payload(top_b, surface_b);
    let h = MessageHeader::try_new(xdg_shell_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(h, &payload).unwrap();

    let a = c.toplevel(top_a).unwrap();
    let b = c.toplevel(top_b).unwrap();
    assert_eq!((a.x, a.y), (0, 0));
    assert_eq!((b.x, b.y), (AUTO_LAYOUT_STEP, AUTO_LAYOUT_STEP));
}

#[test]
fn xdg_get_toplevel_rejects_a_non_existent_surface_id() {
    let (mut c, _surface, xdg_shell_id) = boot_client_with_xdg_shell();
    let stray = ObjectId::new(9999);
    let top = ObjectId::new(11);
    let payload = xdg_get_toplevel_payload(top, stray);
    let h = MessageHeader::try_new(xdg_shell_id, 1, payload.len(), 0).unwrap();
    let err = c.dispatch_request(h, &payload).unwrap_err();
    match err {
        ClientError::ToplevelSurfaceNotFound { surface_id } => {
            assert_eq!(surface_id, stray);
        }
        other => panic!("expected ToplevelSurfaceNotFound, got {other:?}"),
    }
    // No toplevel installed.
    assert_eq!(c.get(top), None);
}

#[test]
fn xdg_get_toplevel_rejects_a_surface_that_already_has_one() {
    let (mut c, surface_id, xdg_shell_id) = boot_client_with_xdg_shell();
    let top_a = ObjectId::new(11);
    let top_b = ObjectId::new(13);
    let payload = xdg_get_toplevel_payload(top_a, surface_id);
    let h = MessageHeader::try_new(xdg_shell_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(h, &payload).unwrap();

    // Second toplevel on the same surface → error.
    let payload = xdg_get_toplevel_payload(top_b, surface_id);
    let h = MessageHeader::try_new(xdg_shell_id, 1, payload.len(), 0).unwrap();
    let err = c.dispatch_request(h, &payload).unwrap_err();
    match err {
        ClientError::SurfaceAlreadyHasToplevel {
            surface_id: s,
            existing_toplevel,
        } => {
            assert_eq!(s, surface_id);
            assert_eq!(existing_toplevel, top_a);
        }
        other => panic!("expected SurfaceAlreadyHasToplevel, got {other:?}"),
    }
    assert_eq!(c.get(top_b), None);
}

#[test]
fn xdg_set_title_updates_toplevel_state() {
    let (mut c, surface_id, xdg_shell_id) = boot_client_with_xdg_shell();
    let top = ObjectId::new(11);
    let payload = xdg_get_toplevel_payload(top, surface_id);
    let h = MessageHeader::try_new(xdg_shell_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(h, &payload).unwrap();

    let payload = xdg_set_string_payload("term — interactive");
    let h = MessageHeader::try_new(top, 1, payload.len(), 0).unwrap();
    c.dispatch_request(h, &payload).unwrap();

    assert_eq!(c.toplevel(top).unwrap().title, "term — interactive");
}

#[test]
fn xdg_set_app_id_updates_toplevel_state() {
    let (mut c, surface_id, xdg_shell_id) = boot_client_with_xdg_shell();
    let top = ObjectId::new(11);
    let payload = xdg_get_toplevel_payload(top, surface_id);
    let h = MessageHeader::try_new(xdg_shell_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(h, &payload).unwrap();

    let payload = xdg_set_string_payload("pmos.term");
    let h = MessageHeader::try_new(top, 2, payload.len(), 0).unwrap();
    c.dispatch_request(h, &payload).unwrap();

    assert_eq!(c.toplevel(top).unwrap().app_id, "pmos.term");
}

#[test]
fn toplevel_for_surface_round_trips() {
    let (mut c, surface_id, xdg_shell_id) = boot_client_with_xdg_shell();
    let top = ObjectId::new(11);
    let payload = xdg_get_toplevel_payload(top, surface_id);
    let h = MessageHeader::try_new(xdg_shell_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(h, &payload).unwrap();

    let looked_up = c.toplevel_for_surface(surface_id).unwrap();
    assert_eq!(looked_up.id, top);
}

#[test]
fn xdg_toplevel_set_title_without_install_returns_unknown_toplevel() {
    let mut c = Client::new(ClientId(1));
    // Hand-install an xdg_toplevel object WITHOUT going
    // through `get_toplevel`, so `toplevels` is empty but
    // the object table has it. `set_title` should fail.
    c.install_client_object(ObjectId::new(5), Interface::XdgToplevel)
        .unwrap();
    let payload = xdg_set_string_payload("hi");
    let h = MessageHeader::try_new(ObjectId::new(5), 1, payload.len(), 0).unwrap();
    let err = c.dispatch_request(h, &payload).unwrap_err();
    match err {
        ClientError::UnknownToplevel { toplevel_id } => {
            assert_eq!(toplevel_id, ObjectId::new(5));
        }
        other => panic!("expected UnknownToplevel, got {other:?}"),
    }
}

#[test]
fn xdg_set_maximized_flips_toplevel_maximized_flag() {
    let (mut c, surface_id, xdg_shell_id) = boot_client_with_xdg_shell();
    let top = ObjectId::new(11);
    let payload = xdg_get_toplevel_payload(top, surface_id);
    let h = MessageHeader::try_new(xdg_shell_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(h, &payload).unwrap();
    assert!(!c.toplevel(top).unwrap().maximized);

    // set_maximized has an empty payload.
    let h = MessageHeader::try_new(top, 5 /* set_maximized */, 0, 0).unwrap();
    c.dispatch_request(h, &[]).unwrap();
    assert!(c.toplevel(top).unwrap().maximized);

    // unset_maximized clears it.
    let h = MessageHeader::try_new(top, 6 /* unset_maximized */, 0, 0).unwrap();
    c.dispatch_request(h, &[]).unwrap();
    assert!(!c.toplevel(top).unwrap().maximized);
}

#[test]
fn xdg_set_maximized_without_install_returns_unknown_toplevel() {
    let mut c = Client::new(ClientId(1));
    // Hand-install xdg_toplevel WITHOUT going through
    // get_toplevel, so toplevels is empty but the object
    // table has it. set_maximized should fail.
    c.install_client_object(ObjectId::new(5), Interface::XdgToplevel)
        .unwrap();
    let h = MessageHeader::try_new(ObjectId::new(5), 5, 0, 0).unwrap();
    let err = c.dispatch_request(h, &[]).unwrap_err();
    match err {
        ClientError::UnknownToplevel { toplevel_id } => {
            assert_eq!(toplevel_id, ObjectId::new(5));
        }
        other => panic!("expected UnknownToplevel, got {other:?}"),
    }
}

#[test]
fn toplevel_default_minimized_is_false() {
    let (mut c, surface_id, xdg_shell_id) = boot_client_with_xdg_shell();
    let top = ObjectId::new(11);
    let payload = xdg_get_toplevel_payload(top, surface_id);
    let h = MessageHeader::try_new(xdg_shell_id, 1, payload.len(), 0).unwrap();
    c.dispatch_request(h, &payload).unwrap();
    assert!(!c.toplevel(top).unwrap().minimized);
    assert!(!c.toplevel(top).unwrap().maximized);
}
