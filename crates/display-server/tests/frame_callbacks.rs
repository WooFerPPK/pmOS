//! `pmd_surface.frame` lifecycle and backpressure isolation tests.

use display_proto::{CallbackDone, DisplayDeleteId};
use display_server::{
    ClientError, ClientResource, Interface, MessageHeader, ObjectId, Server, ServerError,
    HEADER_SIZE, MAX_CLIENT_OBJECTS, MAX_FRAME_CALLBACK_COMPLETIONS_PER_TURN, MAX_PENDING_EVENTS,
    MAX_PENDING_EVENT_BYTES,
};

fn message(object_id: ObjectId, opcode: u16, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0; HEADER_SIZE + payload.len()];
    MessageHeader::try_new(object_id, opcode, payload.len(), 0)
        .unwrap()
        .encode(&mut bytes[..HEADER_SIZE])
        .unwrap();
    bytes[HEADER_SIZE..].copy_from_slice(payload);
    bytes
}

fn boot() -> (Server, display_server::ClientId, ObjectId) {
    let mut server = Server::new();
    let client_id = server.accept();
    let compositor_id = ObjectId::new(3);
    server
        .client_mut(client_id)
        .unwrap()
        .install_client_object(compositor_id, Interface::Compositor)
        .unwrap();
    (server, client_id, compositor_id)
}

fn create_surface(
    server: &mut Server,
    client_id: display_server::ClientId,
    compositor_id: ObjectId,
    surface_id: ObjectId,
) -> Result<(), ServerError> {
    server.dispatch_request(
        client_id,
        &message(compositor_id, 1, &surface_id.raw().to_le_bytes()),
    )
}

fn request_frame(
    server: &mut Server,
    client_id: display_server::ClientId,
    surface_id: ObjectId,
    callback_id: ObjectId,
) {
    server
        .dispatch_request(
            client_id,
            &message(surface_id, 4, &callback_id.raw().to_le_bytes()),
        )
        .unwrap();
}

fn commit(server: &mut Server, client_id: display_server::ClientId, surface_id: ObjectId) {
    server
        .dispatch_request(client_id, &message(surface_id, 7, &[]))
        .unwrap();
}

#[derive(Debug, PartialEq, Eq)]
struct Event {
    object_id: ObjectId,
    opcode: u16,
    payload: Vec<u8>,
}

fn decode_events(bytes: &[u8]) -> Vec<Event> {
    let mut events = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let header = MessageHeader::decode(&bytes[offset..]).unwrap();
        let end = offset + usize::from(header.length);
        events.push(Event {
            object_id: header.object_id,
            opcode: header.opcode,
            payload: bytes[offset + HEADER_SIZE..end].to_vec(),
        });
        offset = end;
    }
    events
}

fn done(callback_id: ObjectId, callback_data: u32) -> Event {
    Event {
        object_id: callback_id,
        opcode: 1,
        payload: callback_data.to_le_bytes().to_vec(),
    }
}

fn deleted(id: ObjectId) -> Event {
    Event {
        object_id: ObjectId::DISPLAY,
        opcode: 2,
        payload: id.raw().to_le_bytes().to_vec(),
    }
}

#[test]
fn frame_binds_callback_without_event_or_scene_work_until_commit_is_presented() {
    let (mut server, client_id, compositor_id) = boot();
    let surface_id = ObjectId::new(5);
    let callback_id = ObjectId::new(7);
    create_surface(&mut server, client_id, compositor_id, surface_id).unwrap();
    let serial = server.recomposition_serial();

    request_frame(&mut server, client_id, surface_id, callback_id);

    let client = server.client(client_id).unwrap();
    assert_eq!(client.get(callback_id), Some(Interface::Callback));
    assert_eq!(client.pending_frame_callback_count(), 1);
    assert_eq!(client.awaiting_present_frame_callback_count(), 0);
    assert_eq!(server.recomposition_serial(), serial);
    assert!(server.drain_client_events(client_id).unwrap().is_empty());

    commit(&mut server, client_id, surface_id);
    let client = server.client(client_id).unwrap();
    assert_eq!(client.pending_frame_callback_count(), 0);
    assert_eq!(client.awaiting_present_frame_callback_count(), 1);
    assert!(server.drain_client_events(client_id).unwrap().is_empty());

    let mut budget = MAX_FRAME_CALLBACK_COMPLETIONS_PER_TURN;
    assert_eq!(server.complete_presented_frame_callbacks(&mut budget), 0);
    assert!(server.drain_client_events(client_id).unwrap().is_empty());

    server.mark_frame_callbacks_presented(0x1234_5678);
    assert_eq!(server.complete_presented_frame_callbacks(&mut budget), 1);
    assert_eq!(
        decode_events(&server.drain_client_events(client_id).unwrap()),
        vec![done(callback_id, 0x1234_5678), deleted(callback_id)]
    );
    assert_eq!(server.client(client_id).unwrap().get(callback_id), None);
    assert_eq!(server.complete_presented_frame_callbacks(&mut budget), 0);
    assert!(server.drain_client_events(client_id).unwrap().is_empty());
}

#[test]
fn malformed_frame_payloads_install_nothing_and_queue_no_work_or_event() {
    let (mut server, client_id, compositor_id) = boot();
    let surface_id = ObjectId::new(5);
    create_surface(&mut server, client_id, compositor_id, surface_id).unwrap();
    let object_count = server.client(client_id).unwrap().object_count();
    let serial = server.recomposition_serial();

    for payload in [vec![7, 0, 0], vec![7, 0, 0, 0, 9]] {
        assert!(matches!(
            server.dispatch_request(client_id, &message(surface_id, 4, &payload)),
            Err(ServerError::Client(ClientError::Malformed {
                interface: Interface::Surface,
                opcode: 4,
                error: display_proto::DecodeError::PayloadLengthMismatch { .. },
            }))
        ));
        let client = server.client(client_id).unwrap();
        assert_eq!(client.object_count(), object_count);
        assert_eq!(client.pending_frame_callback_count(), 0);
        assert_eq!(client.awaiting_present_frame_callback_count(), 0);
        assert_eq!(server.recomposition_serial(), serial);
        assert!(server.drain_client_events(client_id).unwrap().is_empty());
    }
}

#[test]
fn callback_fifo_follows_commit_order_and_post_commit_requests_wait() {
    let (mut server, client_id, compositor_id) = boot();
    let surface_a = ObjectId::new(5);
    let surface_b = ObjectId::new(7);
    let callback_a = ObjectId::new(9);
    let callback_b1 = ObjectId::new(11);
    let callback_b2 = ObjectId::new(13);
    let callback_after = ObjectId::new(15);
    create_surface(&mut server, client_id, compositor_id, surface_a).unwrap();
    create_surface(&mut server, client_id, compositor_id, surface_b).unwrap();

    request_frame(&mut server, client_id, surface_a, callback_a);
    request_frame(&mut server, client_id, surface_b, callback_b1);
    request_frame(&mut server, client_id, surface_b, callback_b2);
    commit(&mut server, client_id, surface_b);
    commit(&mut server, client_id, surface_a);
    request_frame(&mut server, client_id, surface_a, callback_after);

    server.mark_frame_callbacks_presented(41);
    let mut budget = MAX_FRAME_CALLBACK_COMPLETIONS_PER_TURN;
    assert_eq!(server.complete_presented_frame_callbacks(&mut budget), 3);
    assert_eq!(
        decode_events(&server.drain_client_events(client_id).unwrap()),
        vec![
            done(callback_b1, 41),
            deleted(callback_b1),
            done(callback_b2, 41),
            deleted(callback_b2),
            done(callback_a, 41),
            deleted(callback_a),
        ]
    );
    assert_eq!(
        server.client(client_id).unwrap().get(callback_after),
        Some(Interface::Callback)
    );

    server.mark_frame_callbacks_presented(42);
    assert_eq!(server.complete_presented_frame_callbacks(&mut budget), 0);
    commit(&mut server, client_id, surface_a);
    server.mark_frame_callbacks_presented(43);
    assert_eq!(server.complete_presented_frame_callbacks(&mut budget), 1);
    assert_eq!(
        decode_events(&server.drain_client_events(client_id).unwrap()),
        vec![done(callback_after, 43), deleted(callback_after)]
    );
}

#[test]
fn surface_destroy_cancels_pending_and_awaiting_callbacks_without_done() {
    let (mut server, client_id, compositor_id) = boot();
    let surface_id = ObjectId::new(5);
    let awaiting = ObjectId::new(7);
    let pending = ObjectId::new(9);
    create_surface(&mut server, client_id, compositor_id, surface_id).unwrap();
    request_frame(&mut server, client_id, surface_id, awaiting);
    commit(&mut server, client_id, surface_id);
    request_frame(&mut server, client_id, surface_id, pending);

    server
        .dispatch_request(client_id, &message(surface_id, 1, &[]))
        .unwrap();
    assert!(server.drain_client_events(client_id).unwrap().is_empty());
    assert_eq!(server.cancelled_frame_callback_lifecycle_count(), 3);

    let mut budget = MAX_FRAME_CALLBACK_COMPLETIONS_PER_TURN;
    assert_eq!(server.drain_ready_frame_callback_lifecycle(&mut budget), 3);
    assert_eq!(
        decode_events(&server.drain_client_events(client_id).unwrap()),
        vec![deleted(awaiting), deleted(pending), deleted(surface_id)]
    );
    let client = server.client(client_id).unwrap();
    assert_eq!(client.get(awaiting), None);
    assert_eq!(client.get(pending), None);
    assert_eq!(client.get(surface_id), None);
}

#[test]
fn presentation_qualified_callback_is_not_downgraded_by_surface_destroy() {
    let (mut server, client_id, compositor_id) = boot();
    let surface_id = ObjectId::new(5);
    let presented = ObjectId::new(7);
    let cancelled = ObjectId::new(9);
    create_surface(&mut server, client_id, compositor_id, surface_id).unwrap();
    request_frame(&mut server, client_id, surface_id, presented);
    commit(&mut server, client_id, surface_id);
    server.mark_frame_callbacks_presented(77);
    request_frame(&mut server, client_id, surface_id, cancelled);

    server
        .dispatch_request(client_id, &message(surface_id, 1, &[]))
        .unwrap();
    let mut budget = MAX_FRAME_CALLBACK_COMPLETIONS_PER_TURN;
    assert_eq!(server.drain_ready_frame_callback_lifecycle(&mut budget), 3);
    assert_eq!(
        decode_events(&server.drain_client_events(client_id).unwrap()),
        vec![
            done(presented, 77),
            deleted(presented),
            deleted(cancelled),
            deleted(surface_id),
        ]
    );
}

#[test]
fn callback_pair_waits_for_exact_event_capacity_and_retries_without_poisoning() {
    let (mut server, client_id, compositor_id) = boot();
    let surface_id = ObjectId::new(5);
    let callback_id = ObjectId::new(7);
    create_surface(&mut server, client_id, compositor_id, surface_id).unwrap();
    request_frame(&mut server, client_id, surface_id, callback_id);
    commit(&mut server, client_id, surface_id);
    server.mark_frame_callbacks_presented(91);

    let client = server.client_mut(client_id).unwrap();
    for _ in 0..MAX_PENDING_EVENTS - 1 {
        client.emit_error(ObjectId::DISPLAY, 1, "").unwrap();
    }
    assert_eq!(client.pending_events_len(), MAX_PENDING_EVENTS - 1);

    let mut budget = 1;
    assert_eq!(server.drain_ready_frame_callback_lifecycle(&mut budget), 0);
    assert_eq!(budget, 1);
    assert_eq!(server.presented_frame_callback_count(), 1);
    assert_eq!(
        server.client(client_id).unwrap().get(callback_id),
        Some(Interface::Callback)
    );
    assert!(!server.client_event_queue_overflowed(client_id));

    let _ = server.drain_client_events(client_id).unwrap();
    assert_eq!(server.drain_ready_frame_callback_lifecycle(&mut budget), 1);
    assert_eq!(
        decode_events(&server.drain_client_events(client_id).unwrap()),
        vec![done(callback_id, 91), deleted(callback_id)]
    );
}

#[test]
fn callback_pair_waits_for_exact_byte_capacity_and_retries_atomically() {
    let (mut server, client_id, compositor_id) = boot();
    let surface_id = ObjectId::new(5);
    let callback_id = ObjectId::new(7);
    create_surface(&mut server, client_id, compositor_id, surface_id).unwrap();
    request_frame(&mut server, client_id, surface_id, callback_id);
    commit(&mut server, client_id, surface_id);
    server.mark_frame_callbacks_presented(92);

    // An error event occupies 22 bytes plus its message padded to four bytes.
    // These five events leave 26 bytes: enough for either 14-byte lifecycle
    // event alone, but not the required atomic 28-byte done/delete pair.
    let client = server.client_mut(client_id).unwrap();
    for message_len in [60_000, 60_000, 60_000, 60_000, 22_008] {
        client
            .emit_error(ObjectId::DISPLAY, 1, &"x".repeat(message_len))
            .unwrap();
    }
    assert_eq!(client.pending_event_bytes(), MAX_PENDING_EVENT_BYTES - 26);

    let mut budget = 1;
    assert_eq!(server.drain_ready_frame_callback_lifecycle(&mut budget), 0);
    assert_eq!(server.presented_frame_callback_count(), 1);
    assert_eq!(
        server.client(client_id).unwrap().get(callback_id),
        Some(Interface::Callback)
    );
    assert!(!server.client_event_queue_overflowed(client_id));

    let _ = server.drain_client_events(client_id).unwrap();
    assert_eq!(server.drain_ready_frame_callback_lifecycle(&mut budget), 1);
    assert_eq!(
        decode_events(&server.drain_client_events(client_id).unwrap()),
        vec![done(callback_id, 92), deleted(callback_id)]
    );
}

#[test]
fn blocked_client_does_not_prevent_ready_peer_completion() {
    let mut server = Server::new();
    let low = server.accept();
    let high = server.accept();
    let compositor = ObjectId::new(3);
    let surface = ObjectId::new(5);
    let callback = ObjectId::new(7);
    for client_id in [low, high] {
        server
            .client_mut(client_id)
            .unwrap()
            .install_client_object(compositor, Interface::Compositor)
            .unwrap();
        create_surface(&mut server, client_id, compositor, surface).unwrap();
        request_frame(&mut server, client_id, surface, callback);
        commit(&mut server, client_id, surface);
    }
    server.mark_frame_callbacks_presented(101);
    for _ in 0..MAX_PENDING_EVENTS - 1 {
        server
            .client_mut(low)
            .unwrap()
            .emit_error(ObjectId::DISPLAY, 1, "")
            .unwrap();
    }

    let mut budget = 2;
    assert_eq!(server.drain_ready_frame_callback_lifecycle(&mut budget), 1);
    assert_eq!(
        server.client(low).unwrap().get(callback),
        Some(Interface::Callback)
    );
    assert_eq!(server.client(high).unwrap().get(callback), None);
    assert_eq!(
        decode_events(&server.drain_client_events(high).unwrap()),
        vec![done(callback, 101), deleted(callback)]
    );
    assert!(!server.client_event_queue_overflowed(low));
}

#[test]
fn callback_quantum_is_fair_across_clients_and_rotates_next_turn_start() {
    let mut server = Server::new();
    let low = server.accept();
    let high = server.accept();
    let compositor = ObjectId::new(3);
    let surface = ObjectId::new(5);
    for client_id in [low, high] {
        server
            .client_mut(client_id)
            .unwrap()
            .install_client_object(compositor, Interface::Compositor)
            .unwrap();
        create_surface(&mut server, client_id, compositor, surface).unwrap();
    }
    for index in 0..65 {
        request_frame(&mut server, low, surface, ObjectId::new(7 + index * 2));
    }
    let high_first = ObjectId::new(7);
    request_frame(&mut server, high, surface, high_first);
    commit(&mut server, low, surface);
    commit(&mut server, high, surface);
    server.mark_frame_callbacks_presented(110);

    let mut first_budget = MAX_FRAME_CALLBACK_COMPLETIONS_PER_TURN;
    assert_eq!(
        server.drain_ready_frame_callback_lifecycle(&mut first_budget),
        MAX_FRAME_CALLBACK_COMPLETIONS_PER_TURN
    );
    assert_eq!(first_budget, 0);
    assert_eq!(server.client(high).unwrap().get(high_first), None);
    assert_eq!(
        server.client(low).unwrap().presented_frame_callback_count(),
        2
    );
    let _ = server.drain_client_events(low).unwrap();
    let _ = server.drain_client_events(high).unwrap();

    // The first turn ended on the low-id client, so the rotating cursor starts
    // this one at the high-id client. With a one-item budget, its new callback
    // must complete ahead of the low client's older backlog.
    let low_new = ObjectId::new(137);
    let high_new = ObjectId::new(9);
    request_frame(&mut server, low, surface, low_new);
    request_frame(&mut server, high, surface, high_new);
    commit(&mut server, low, surface);
    commit(&mut server, high, surface);
    server.mark_frame_callbacks_presented(111);
    let mut second_budget = 1;
    assert_eq!(
        server.drain_ready_frame_callback_lifecycle(&mut second_budget),
        1
    );
    assert_eq!(server.client(high).unwrap().get(high_new), None);
    assert_eq!(
        server.client(low).unwrap().get(low_new),
        Some(Interface::Callback)
    );
    assert_eq!(
        decode_events(&server.drain_client_events(high).unwrap()),
        vec![done(high_new, 111), deleted(high_new)]
    );
}

#[test]
fn blocked_destroy_keeps_surface_tombstoned_until_cancel_and_delete_admit() {
    let (mut server, client_id, compositor_id) = boot();
    let surface_id = ObjectId::new(5);
    let callback_id = ObjectId::new(7);
    create_surface(&mut server, client_id, compositor_id, surface_id).unwrap();
    request_frame(&mut server, client_id, surface_id, callback_id);
    let client = server.client_mut(client_id).unwrap();
    for _ in 0..MAX_PENDING_EVENTS {
        client.emit_error(ObjectId::DISPLAY, 1, "").unwrap();
    }

    server
        .dispatch_request(client_id, &message(surface_id, 1, &[]))
        .unwrap();
    let charged_count = server.client(client_id).unwrap().object_count();
    let mut budget = MAX_FRAME_CALLBACK_COMPLETIONS_PER_TURN;
    assert_eq!(server.drain_ready_frame_callback_lifecycle(&mut budget), 0);
    assert_eq!(
        server.client(client_id).unwrap().object_count(),
        charged_count
    );
    assert_eq!(server.cancelled_frame_callback_lifecycle_count(), 2);
    assert!(!server.client_event_queue_overflowed(client_id));

    assert_eq!(
        create_surface(&mut server, client_id, compositor_id, surface_id),
        Err(ServerError::Client(ClientError::DuplicateObject {
            id: surface_id
        }))
    );

    let _ = server.drain_client_events(client_id).unwrap();
    assert_eq!(server.drain_ready_frame_callback_lifecycle(&mut budget), 2);
    assert_eq!(
        decode_events(&server.drain_client_events(client_id).unwrap()),
        vec![deleted(callback_id), deleted(surface_id)]
    );
    create_surface(&mut server, client_id, compositor_id, surface_id).unwrap();
    assert_eq!(
        server.client(client_id).unwrap().get(surface_id),
        Some(Interface::Surface)
    );
}

#[test]
fn deferred_surface_delete_churn_remains_object_cap_charged() {
    let (mut server, client_id, compositor_id) = boot();
    let client = server.client_mut(client_id).unwrap();
    for _ in 0..MAX_PENDING_EVENTS {
        client.emit_error(ObjectId::DISPLAY, 1, "").unwrap();
    }

    let tombstone_capacity = MAX_CLIENT_OBJECTS - 2;
    for index in 0..tombstone_capacity {
        let id = ObjectId::new(5 + (index as u32) * 2);
        create_surface(&mut server, client_id, compositor_id, id).unwrap();
        server
            .dispatch_request(client_id, &message(id, 1, &[]))
            .unwrap();
    }
    assert_eq!(
        server.client(client_id).unwrap().object_count(),
        MAX_CLIENT_OBJECTS
    );
    assert_eq!(
        server.cancelled_frame_callback_lifecycle_count(),
        tombstone_capacity
    );
    assert!(!server.client_event_queue_overflowed(client_id));

    let rejected = ObjectId::new(5 + (tombstone_capacity as u32) * 2);
    assert_eq!(
        create_surface(&mut server, client_id, compositor_id, rejected),
        Err(ServerError::Client(ClientError::ResourceLimitExceeded {
            resource: ClientResource::Objects,
            attempted: (MAX_CLIENT_OBJECTS + 1) as u64,
            limit: MAX_CLIENT_OBJECTS as u64,
        }))
    );
}

#[test]
fn completion_quantum_caps_one_turn_and_disconnect_drops_remaining_state() {
    let (mut server, client_id, compositor_id) = boot();
    let surface_id = ObjectId::new(5);
    create_surface(&mut server, client_id, compositor_id, surface_id).unwrap();
    for index in 0..65 {
        request_frame(
            &mut server,
            client_id,
            surface_id,
            ObjectId::new(7 + index * 2),
        );
    }
    commit(&mut server, client_id, surface_id);
    server.mark_frame_callbacks_presented(5);

    let mut budget = MAX_FRAME_CALLBACK_COMPLETIONS_PER_TURN;
    assert_eq!(server.complete_presented_frame_callbacks(&mut budget), 64);
    assert_eq!(budget, 0);
    assert_eq!(server.drain_ready_frame_callback_lifecycle(&mut budget), 0);
    assert_eq!(server.presented_frame_callback_count(), 1);
    assert!(server.has_ready_frame_callback_lifecycle());
    assert!(server.ready_frame_callback_lifecycle_can_progress());

    assert!(server.disconnect(client_id).is_some());
    assert_eq!(server.client_count(), 0);
    assert_eq!(server.presented_frame_callback_count(), 0);
    assert!(!server.has_ready_frame_callback_lifecycle());
}

#[test]
fn event_payload_decoders_match_exact_done_then_delete_wire() {
    let callback_id = ObjectId::new(7);
    let events = [done(callback_id, 11), deleted(callback_id)];
    assert_eq!(
        CallbackDone::decode(&events[0].payload)
            .unwrap()
            .callback_data,
        11
    );
    assert_eq!(
        DisplayDeleteId::decode(&events[1].payload).unwrap().id,
        callback_id
    );
}
