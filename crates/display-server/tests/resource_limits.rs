//! Adversarial isolation tests for display-client resource admission.

use abi::cap::{Cap, CapSet};
use display_server::{
    Client, ClientError, ClientId, ClientLimits, ClientResource, DamageRect, Interface,
    MessageHeader, ObjectId, OutboundQueue, Server, ServerError, ServerLimits, Toplevel,
    DEFAULT_HEIGHT, DEFAULT_WIDTH, MAX_CLIENT_OBJECTS, MAX_PENDING_EVENTS, MAX_PENDING_EVENT_BYTES,
    MAX_SERVER_CLIENTS, MAX_SERVER_ORDINARY_CLIENTS, MAX_SERVER_SHELL_CLIENTS,
    MAX_SERVER_TOPLEVELS, MAX_SERVER_TOPLEVEL_METADATA_BYTES, MAX_SERVER_WINDOW_SNAPSHOT_BYTES,
    MAX_TOPLEVEL_METADATA_BYTES, SHELL_FULL_OUTPUT_POOL_BYTES,
    SHELL_METADATA_BYTES_RESERVED_PER_CLIENT, SHELL_TOPLEVELS_RESERVED_PER_CLIENT,
};

fn limits() -> ClientLimits {
    ClientLimits {
        objects: 64,
        pools: 8,
        pool_bytes: 1024,
        buffers: 8,
        surfaces: 8,
        toplevels: 8,
        toplevel_metadata_bytes: 1024,
        damage_rects_per_surface: 8,
        pending_events: 32,
        pending_event_bytes: 4096,
    }
}

fn new_client(limits: ClientLimits) -> Client {
    Client::new_with_caps_and_limits(ClientId(1), CapSet::EMPTY, limits)
}

fn dispatch(
    client: &mut Client,
    object: ObjectId,
    opcode: u16,
    payload: &[u8],
) -> Result<(), ClientError> {
    let header = MessageHeader::try_new(object, opcode, payload.len(), 0).unwrap();
    client.dispatch_request(header, payload)
}

fn new_id_payload(id: ObjectId) -> Vec<u8> {
    id.raw().to_le_bytes().to_vec()
}

fn create_pool_payload(id: ObjectId, size: u32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(8);
    payload.extend_from_slice(&id.raw().to_le_bytes());
    payload.extend_from_slice(&size.to_le_bytes());
    payload
}

fn message(object: ObjectId, opcode: u16, payload: &[u8]) -> Vec<u8> {
    let header = MessageHeader::try_new(object, opcode, payload.len(), 0).unwrap();
    let mut bytes = vec![0; display_server::HEADER_SIZE];
    header.encode(&mut bytes).unwrap();
    bytes.extend_from_slice(payload);
    bytes
}

fn create_buffer_payload(
    id: ObjectId,
    offset: u32,
    width: u32,
    height: u32,
    stride: u32,
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(24);
    for value in [id.raw(), offset, width, height, stride, 0] {
        payload.extend_from_slice(&value.to_le_bytes());
    }
    payload
}

fn create_toplevel_payload(id: ObjectId, surface: ObjectId) -> Vec<u8> {
    let mut payload = Vec::with_capacity(8);
    payload.extend_from_slice(&id.raw().to_le_bytes());
    payload.extend_from_slice(&surface.raw().to_le_bytes());
    payload
}

fn damage_payload(x: i32, y: i32, width: i32, height: i32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(16);
    for value in [x, y, width, height] {
        payload.extend_from_slice(&value.to_le_bytes());
    }
    payload
}

fn attach_payload(buffer: ObjectId) -> Vec<u8> {
    let mut payload = Vec::with_capacity(12);
    payload.extend_from_slice(&buffer.raw().to_le_bytes());
    payload.extend_from_slice(&0i32.to_le_bytes());
    payload.extend_from_slice(&0i32.to_le_bytes());
    payload
}

fn resize_payload(size: u32) -> [u8; 4] {
    size.to_le_bytes()
}

fn string_payload_bytes(bytes: &[u8]) -> Vec<u8> {
    let pad = (4 - (bytes.len() % 4)) % 4;
    let mut payload = Vec::with_capacity(4 + bytes.len() + pad);
    payload.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    payload.extend_from_slice(bytes);
    payload.resize(payload.len() + pad, 0);
    payload
}

fn install_test_toplevel(client: &mut Client, toplevel: ObjectId, surface: ObjectId) {
    client
        .install_client_object(toplevel, Interface::XdgToplevel)
        .unwrap();
    client
        .toplevels
        .insert(toplevel, Toplevel::new(toplevel, surface, 0, 0));
}

fn assert_limit(error: ClientError, resource: ClientResource, attempted: u64, limit: u64) {
    assert_eq!(
        error,
        ClientError::ResourceLimitExceeded {
            resource,
            attempted,
            limit,
        }
    );
}

fn install_full_output_shell_window(server: &mut Server, client_id: ClientId) {
    let shm = ObjectId::new(3);
    let xdg_shell = ObjectId::new(5);
    let surface = ObjectId::new(7);
    let pool = ObjectId::new(9);
    let first_buffer = ObjectId::new(11);
    let second_buffer = ObjectId::new(13);
    let toplevel = ObjectId::new(15);
    let client = server.client_mut(client_id).unwrap();
    for (id, interface) in [
        (shm, Interface::Shm),
        (xdg_shell, Interface::XdgShell),
        (surface, Interface::Surface),
    ] {
        client.install_client_object(id, interface).unwrap();
    }

    server
        .dispatch_request(
            client_id,
            &message(
                shm,
                1,
                &create_pool_payload(pool, SHELL_FULL_OUTPUT_POOL_BYTES as u32),
            ),
        )
        .unwrap();
    let stride = DEFAULT_WIDTH * 4;
    let per_buffer = stride * DEFAULT_HEIGHT;
    for (id, offset) in [(first_buffer, 0), (second_buffer, per_buffer)] {
        server
            .dispatch_request(
                client_id,
                &message(
                    pool,
                    1,
                    &create_buffer_payload(id, offset, DEFAULT_WIDTH, DEFAULT_HEIGHT, stride),
                ),
            )
            .unwrap();
    }
    server
        .dispatch_request(
            client_id,
            &message(xdg_shell, 1, &create_toplevel_payload(toplevel, surface)),
        )
        .unwrap();
    server
        .dispatch_request(
            client_id,
            &message(
                toplevel,
                1,
                &string_payload_bytes(&vec![
                    b's';
                    SHELL_METADATA_BYTES_RESERVED_PER_CLIENT as usize
                ]),
            ),
        )
        .unwrap();
}

#[test]
fn repeated_object_install_is_rejected_before_the_table_changes() {
    let mut client = Client::new(ClientId(1));
    for raw in (3..).step_by(2).take(MAX_CLIENT_OBJECTS - 1) {
        client
            .install_client_object(ObjectId::new(raw as u32), Interface::Registry)
            .unwrap();
    }
    assert_eq!(client.object_count(), MAX_CLIENT_OBJECTS);

    let rejected = ObjectId::new((MAX_CLIENT_OBJECTS as u32) * 2 + 1);
    let error = client
        .install_client_object(rejected, Interface::Registry)
        .unwrap_err();
    assert_limit(
        error,
        ClientResource::Objects,
        (MAX_CLIENT_OBJECTS + 1) as u64,
        MAX_CLIENT_OBJECTS as u64,
    );
    assert_eq!(client.object_count(), MAX_CLIENT_OBJECTS);
    assert_eq!(client.get(rejected), None);
}

#[test]
fn repeated_pools_obey_count_and_aggregate_byte_limits_atomically() {
    let mut configured = limits();
    configured.pools = 2;
    configured.pool_bytes = 10;
    let mut client = new_client(configured);
    let shm = ObjectId::new(3);
    client.install_client_object(shm, Interface::Shm).unwrap();

    let first = ObjectId::new(5);
    dispatch(&mut client, shm, 1, &create_pool_payload(first, 6)).unwrap();
    assert_eq!(client.pool_bytes_len(), 6);

    let too_many_bytes = ObjectId::new(7);
    let error = dispatch(&mut client, shm, 1, &create_pool_payload(too_many_bytes, 5)).unwrap_err();
    assert_limit(error, ClientResource::PoolBytes, 11, 10);
    assert_eq!(client.get(too_many_bytes), None);
    assert!(client.pool(too_many_bytes).is_none());
    assert_eq!(client.pool_bytes_len(), 6);

    let second = ObjectId::new(9);
    dispatch(&mut client, shm, 1, &create_pool_payload(second, 4)).unwrap();
    let too_many_pools = ObjectId::new(11);
    let error = dispatch(&mut client, shm, 1, &create_pool_payload(too_many_pools, 0)).unwrap_err();
    assert_limit(error, ClientResource::Pools, 3, 2);
    assert_eq!(client.get(too_many_pools), None);
    assert_eq!(client.pools.len(), 2);
    assert_eq!(client.pool_bytes_len(), 10);
}

#[test]
fn per_client_toplevel_metadata_limit_is_preparse_exact_and_replacement_atomic() {
    let mut configured = limits();
    configured.toplevel_metadata_bytes = 10;
    let mut client = new_client(configured);
    let toplevel = ObjectId::new(3);
    install_test_toplevel(&mut client, toplevel, ObjectId::new(5));

    dispatch(&mut client, toplevel, 1, &string_payload_bytes(b"123456")).unwrap();
    dispatch(&mut client, toplevel, 2, &string_payload_bytes(b"abcd")).unwrap();
    assert_eq!(client.toplevel_metadata_bytes_len(), 10);

    // The invalid UTF-8 body proves admission uses the declared wire length:
    // N+1 is rejected as a resource error before owned-string parsing.
    let error = dispatch(&mut client, toplevel, 1, &string_payload_bytes(&[0xff; 7])).unwrap_err();
    assert_limit(error, ClientResource::ToplevelMetadataBytes, 11, 10);
    assert_eq!(client.pending_events_len(), 1);
    assert_eq!(client.toplevel_metadata_bytes_len(), 10);
    assert_eq!(client.toplevel(toplevel).unwrap().title, "123456");
    assert_eq!(client.toplevel(toplevel).unwrap().app_id, "abcd");

    dispatch(&mut client, toplevel, 1, &string_payload_bytes(b"xy")).unwrap();
    assert_eq!(client.toplevel_metadata_bytes_len(), 6);
    assert_eq!(client.toplevel(toplevel).unwrap().title, "xy");
}

#[test]
fn server_toplevel_metadata_budget_releases_on_destroy_and_disconnect() {
    let mut server = Server::with_limits(
        8,
        8,
        ServerLimits {
            clients: 2,
            shell_clients: 0,
            pool_bytes: 1024,
            toplevels: 2,
            client_toplevel_metadata_bytes: 10,
            toplevel_metadata_bytes: 10,
        },
    );
    let first = server.try_accept().unwrap();
    let second = server.try_accept().unwrap();
    let first_top = ObjectId::new(3);
    let second_top = ObjectId::new(3);
    install_test_toplevel(
        server.client_mut(first).unwrap(),
        first_top,
        ObjectId::new(5),
    );
    install_test_toplevel(
        server.client_mut(second).unwrap(),
        second_top,
        ObjectId::new(5),
    );

    server
        .dispatch_request(
            first,
            &message(first_top, 1, &string_payload_bytes(b"123456")),
        )
        .unwrap();
    server
        .dispatch_request(
            second,
            &message(second_top, 1, &string_payload_bytes(b"abcd")),
        )
        .unwrap();
    assert_eq!(server.toplevel_metadata_bytes_len(), 10);

    let error = server
        .dispatch_request(
            second,
            &message(second_top, 1, &string_payload_bytes(b"abcde")),
        )
        .unwrap_err();
    assert_eq!(
        error,
        ServerError::Client(ClientError::ResourceLimitExceeded {
            resource: ClientResource::ServerToplevelMetadataBytes,
            attempted: 11,
            limit: 10,
        })
    );
    assert_eq!(server.toplevel_metadata_bytes_len(), 10);
    assert_eq!(
        server
            .client(second)
            .unwrap()
            .toplevel(second_top)
            .unwrap()
            .title,
        "abcd"
    );

    server
        .dispatch_request(first, &message(first_top, 3, &[]))
        .unwrap();
    assert_eq!(server.toplevel_metadata_bytes_len(), 4);
    server
        .dispatch_request(
            second,
            &message(second_top, 1, &string_payload_bytes(b"abcde")),
        )
        .unwrap();
    assert_eq!(server.toplevel_metadata_bytes_len(), 5);

    server.disconnect(second).unwrap();
    assert_eq!(server.toplevel_metadata_bytes_len(), 0);
}

#[test]
fn buffer_surface_and_toplevel_maps_each_have_independent_ceilings() {
    let mut configured = limits();
    configured.buffers = 1;
    configured.surfaces = 2;
    configured.toplevels = 1;
    let mut client = new_client(configured);
    let shm = ObjectId::new(3);
    let compositor = ObjectId::new(5);
    let xdg_shell = ObjectId::new(7);
    for (id, interface) in [
        (shm, Interface::Shm),
        (compositor, Interface::Compositor),
        (xdg_shell, Interface::XdgShell),
    ] {
        client.install_client_object(id, interface).unwrap();
    }

    let pool = ObjectId::new(9);
    dispatch(&mut client, shm, 1, &create_pool_payload(pool, 8)).unwrap();
    let buffer = ObjectId::new(11);
    dispatch(
        &mut client,
        pool,
        1,
        &create_buffer_payload(buffer, 0, 1, 1, 4),
    )
    .unwrap();
    let rejected_buffer = ObjectId::new(13);
    let error = dispatch(
        &mut client,
        pool,
        1,
        &create_buffer_payload(rejected_buffer, 4, 1, 1, 4),
    )
    .unwrap_err();
    assert_limit(error, ClientResource::Buffers, 2, 1);
    assert_eq!(client.get(rejected_buffer), None);

    let surface_a = ObjectId::new(15);
    let surface_b = ObjectId::new(17);
    for surface in [surface_a, surface_b] {
        dispatch(&mut client, compositor, 1, &new_id_payload(surface)).unwrap();
    }
    let rejected_surface = ObjectId::new(19);
    let error = dispatch(
        &mut client,
        compositor,
        1,
        &new_id_payload(rejected_surface),
    )
    .unwrap_err();
    assert_limit(error, ClientResource::Surfaces, 3, 2);
    assert_eq!(client.get(rejected_surface), None);

    let toplevel = ObjectId::new(21);
    dispatch(
        &mut client,
        xdg_shell,
        1,
        &create_toplevel_payload(toplevel, surface_a),
    )
    .unwrap();
    let rejected_toplevel = ObjectId::new(23);
    let error = dispatch(
        &mut client,
        xdg_shell,
        1,
        &create_toplevel_payload(rejected_toplevel, surface_b),
    )
    .unwrap_err();
    assert_limit(error, ClientResource::Toplevels, 2, 1);
    assert_eq!(client.get(rejected_toplevel), None);
    assert_eq!(client.buffers.len(), 1);
    assert_eq!(client.surfaces.len(), 2);
    assert_eq!(client.toplevels.len(), 1);
}

#[test]
fn repeated_damage_is_coalesced_without_exceeding_the_surface_ceiling() {
    let mut configured = limits();
    configured.damage_rects_per_surface = 2;
    let mut client = new_client(configured);
    let compositor = ObjectId::new(3);
    let surface = ObjectId::new(5);
    client
        .install_client_object(compositor, Interface::Compositor)
        .unwrap();
    dispatch(&mut client, compositor, 1, &new_id_payload(surface)).unwrap();

    for rect in [(0, 0, 4, 4), (10, 10, 2, 2), (-2, -3, 1, 1)] {
        dispatch(
            &mut client,
            surface,
            3,
            &damage_payload(rect.0, rect.1, rect.2, rect.3),
        )
        .unwrap();
    }
    assert_eq!(
        client.surface(surface).unwrap().pending_damage,
        vec![DamageRect {
            x: -2,
            y: -3,
            width: 14,
            height: 15,
        }]
    );

    for i in 0..10_000 {
        dispatch(&mut client, surface, 3, &damage_payload(i, i, 1, 1)).unwrap();
        assert!(client.surface(surface).unwrap().pending_damage.len() <= 2);
    }
}

#[test]
fn repeated_events_mark_the_client_connection_fatal_at_the_count_ceiling() {
    let mut server = Server::new();
    let id = server.accept();
    {
        let client = server.client_mut(id).unwrap();
        for _ in 0..MAX_PENDING_EVENTS {
            client.emit_delete_id(ObjectId::new(3)).unwrap();
        }
        let error = client.emit_delete_id(ObjectId::new(3)).unwrap_err();
        assert_limit(
            error,
            ClientResource::PendingEvents,
            (MAX_PENDING_EVENTS + 1) as u64,
            MAX_PENDING_EVENTS as u64,
        );
        assert_eq!(client.pending_events_len(), MAX_PENDING_EVENTS);
    }
    assert!(server.client_event_queue_overflowed(id));
}

#[test]
fn event_byte_overflow_queues_no_partial_event_and_never_resumes() {
    let mut configured = limits();
    configured.pending_events = 16;
    configured.pending_event_bytes = 20;
    let mut client = new_client(configured);
    assert_eq!(client.emit_delete_id(ObjectId::new(3)).unwrap(), 14);
    assert_eq!(client.pending_event_bytes(), 14);

    let error = client.emit_delete_id(ObjectId::new(5)).unwrap_err();
    assert_limit(error, ClientResource::PendingEventBytes, 28, 20);
    assert_eq!(client.pending_events_len(), 1);
    assert_eq!(client.pending_event_bytes(), 14);
    assert!(client.event_queue_overflowed());

    assert_eq!(client.drain_pending_events().len(), 14);
    assert_eq!(client.pending_event_bytes(), 0);
    assert_eq!(
        client.emit_delete_id(ObjectId::new(7)).unwrap_err(),
        ClientError::EventQueueOverflowed
    );
}

#[test]
fn nonreading_transport_queue_is_bounded_and_partial_writes_keep_order() {
    let mut queue = OutboundQueue::with_limit(16);
    queue.append(&[1; 8]).unwrap();
    assert_eq!(queue.remaining_capacity(), 8);
    queue.append(&[2; 8]).unwrap();
    assert_eq!(queue.remaining_capacity(), 0);
    let error = queue.append(&[3]).unwrap_err();
    assert_eq!(error.queued, 16);
    assert_eq!(error.incoming, 1);
    assert_eq!(error.max, 16);
    assert_eq!(queue.len(), 16);
    assert_eq!(
        queue.as_slice(),
        &[1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2]
    );

    queue.consume(6);
    assert_eq!(queue.remaining_capacity(), 6);
    queue.append(&[3; 6]).unwrap();
    assert_eq!(queue.len(), 16);
    assert_eq!(&queue.as_slice()[..2], &[1, 1]);
    assert_eq!(&queue.as_slice()[10..], &[3; 6]);
}

#[test]
fn responsive_partial_write_leaves_complete_event_queued_until_capacity_frees() {
    let mut client = Client::new(ClientId(1));
    client.emit_delete_id(ObjectId::new(3)).unwrap();
    let mut outbound = OutboundQueue::with_limit(16);
    outbound.append(&[9; 8]).unwrap();

    let first = client.drain_pending_events_bounded(outbound.remaining_capacity());
    assert!(first.is_empty());
    assert_eq!(client.pending_events_len(), 1);
    assert_eq!(outbound.as_slice(), &[9; 8]);

    outbound.consume(8);
    let second = client.drain_pending_events_bounded(outbound.remaining_capacity());
    assert_eq!(second.len(), 14);
    outbound.append(&second).unwrap();
    assert_eq!(client.pending_events_len(), 0);
    assert_eq!(outbound.as_slice(), second.as_slice());
}

#[test]
fn default_limits_admit_a_full_output_double_buffer_pool() {
    let mut client = Client::new(ClientId(1));
    let shm = ObjectId::new(3);
    let pool = ObjectId::new(5);
    client.install_client_object(shm, Interface::Shm).unwrap();
    let bytes = 1024 * 736 * 4 * 2;
    dispatch(&mut client, shm, 1, &create_pool_payload(pool, bytes)).unwrap();
    assert_eq!(client.pool_bytes_len(), u64::from(bytes));
    assert_eq!(client.pool(pool).unwrap().storage.len(), bytes as usize);
}

#[test]
fn server_client_limit_rejects_n_plus_one_before_allocating_state() {
    let limits = ServerLimits {
        clients: 2,
        shell_clients: 0,
        pool_bytes: 1024,
        ..ServerLimits::default()
    };
    let mut server = Server::with_limits(8, 8, limits);
    let first = server.try_accept().unwrap();
    let _second = server.try_accept().unwrap();
    assert_eq!(
        server.try_accept().unwrap_err(),
        ServerError::ClientLimitExceeded {
            attempted: 3,
            limit: 2,
        }
    );
    assert_eq!(server.client_count(), 2);

    server.disconnect(first).unwrap();
    assert!(server.try_accept().is_ok());
    assert_eq!(server.client_count(), 2);
}

#[test]
fn authenticated_shell_connection_reserve_is_exact_and_reclaims() {
    let limits = ServerLimits {
        clients: 4,
        shell_clients: 2,
        ..ServerLimits::default()
    };
    let mut server = Server::with_limits(8, 8, limits);
    let shell_caps = CapSet::from_caps(&[Cap::Shell]);
    let first = server.try_accept_with_caps(shell_caps).unwrap();
    let _second = server.try_accept_with_caps(shell_caps).unwrap();
    assert!(server.try_accept().is_ok());
    assert_eq!(
        server.try_accept_with_caps(shell_caps).unwrap_err(),
        ServerError::ShellClientLimitExceeded {
            attempted: 3,
            limit: 2,
        }
    );

    server.disconnect(first).unwrap();
    assert!(server.try_accept_with_caps(shell_caps).is_ok());
}

#[test]
fn ordinary_clients_cannot_consume_the_two_default_shell_slots() {
    let mut server = Server::new();
    for _ in 0..MAX_SERVER_ORDINARY_CLIENTS {
        server.try_accept().unwrap();
    }
    assert_eq!(server.client_count(), MAX_SERVER_ORDINARY_CLIENTS);
    assert_eq!(
        server.try_accept().unwrap_err(),
        ServerError::ClientLimitExceeded {
            attempted: MAX_SERVER_ORDINARY_CLIENTS + 1,
            limit: MAX_SERVER_ORDINARY_CLIENTS,
        }
    );

    let shell_caps = CapSet::from_caps(&[Cap::Shell]);
    for _ in 0..MAX_SERVER_SHELL_CLIENTS {
        server.try_accept_with_caps(shell_caps).unwrap();
    }
    assert_eq!(server.client_count(), MAX_SERVER_CLIENTS);
    assert_eq!(
        server.try_accept_with_caps(shell_caps).unwrap_err(),
        ServerError::ShellClientLimitExceeded {
            attempted: MAX_SERVER_SHELL_CLIENTS + 1,
            limit: MAX_SERVER_SHELL_CLIENTS,
        }
    );
}

#[test]
fn ordinary_resources_cannot_starve_active_and_replacement_shell_windows() {
    let ordinary_pool_bytes = 10u64;
    let ordinary_metadata_bytes = 10u64;
    let limits = ServerLimits {
        clients: 3,
        shell_clients: 2,
        pool_bytes: ordinary_pool_bytes
            + SHELL_FULL_OUTPUT_POOL_BYTES * MAX_SERVER_SHELL_CLIENTS as u64,
        toplevels: 1 + SHELL_TOPLEVELS_RESERVED_PER_CLIENT * MAX_SERVER_SHELL_CLIENTS,
        client_toplevel_metadata_bytes: SHELL_METADATA_BYTES_RESERVED_PER_CLIENT,
        toplevel_metadata_bytes: ordinary_metadata_bytes
            + SHELL_METADATA_BYTES_RESERVED_PER_CLIENT * MAX_SERVER_SHELL_CLIENTS as u64,
    };
    let mut server = Server::with_limits(DEFAULT_WIDTH, DEFAULT_HEIGHT, limits);
    let ordinary = server.try_accept().unwrap();
    let shm = ObjectId::new(3);
    let xdg_shell = ObjectId::new(5);
    let first_surface = ObjectId::new(7);
    let pool = ObjectId::new(9);
    let first_toplevel = ObjectId::new(11);
    let second_surface = ObjectId::new(13);
    let rejected_pool = ObjectId::new(15);
    let rejected_toplevel = ObjectId::new(17);
    let client = server.client_mut(ordinary).unwrap();
    for (id, interface) in [
        (shm, Interface::Shm),
        (xdg_shell, Interface::XdgShell),
        (first_surface, Interface::Surface),
        (second_surface, Interface::Surface),
    ] {
        client.install_client_object(id, interface).unwrap();
    }

    server
        .dispatch_request(
            ordinary,
            &message(
                shm,
                1,
                &create_pool_payload(pool, ordinary_pool_bytes as u32),
            ),
        )
        .unwrap();
    server
        .dispatch_request(
            ordinary,
            &message(
                xdg_shell,
                1,
                &create_toplevel_payload(first_toplevel, first_surface),
            ),
        )
        .unwrap();
    server
        .dispatch_request(
            ordinary,
            &message(
                first_toplevel,
                1,
                &string_payload_bytes(&vec![b'o'; ordinary_metadata_bytes as usize]),
            ),
        )
        .unwrap();

    assert_eq!(
        server
            .dispatch_request(
                ordinary,
                &message(shm, 1, &create_pool_payload(rejected_pool, 1)),
            )
            .unwrap_err(),
        ServerError::Client(ClientError::ResourceLimitExceeded {
            resource: ClientResource::ServerPoolBytes,
            attempted: ordinary_pool_bytes + 1,
            limit: ordinary_pool_bytes,
        })
    );
    assert_eq!(
        server
            .dispatch_request(
                ordinary,
                &message(
                    xdg_shell,
                    1,
                    &create_toplevel_payload(rejected_toplevel, second_surface),
                ),
            )
            .unwrap_err(),
        ServerError::Client(ClientError::ResourceLimitExceeded {
            resource: ClientResource::ServerToplevels,
            attempted: 2,
            limit: 1,
        })
    );
    assert_eq!(
        server
            .dispatch_request(
                ordinary,
                &message(
                    first_toplevel,
                    1,
                    &string_payload_bytes(&vec![b'x'; ordinary_metadata_bytes as usize + 1]),
                ),
            )
            .unwrap_err(),
        ServerError::Client(ClientError::ResourceLimitExceeded {
            resource: ClientResource::ServerToplevelMetadataBytes,
            attempted: ordinary_metadata_bytes + 1,
            limit: ordinary_metadata_bytes,
        })
    );

    let shell_caps = CapSet::from_caps(&[Cap::Shell]);
    let active_shell = server.try_accept_with_caps(shell_caps).unwrap();
    install_full_output_shell_window(&mut server, active_shell);
    let replacement_shell = server.try_accept_with_caps(shell_caps).unwrap();
    install_full_output_shell_window(&mut server, replacement_shell);
    assert_eq!(server.pool_bytes_len(), limits.pool_bytes);
    assert_eq!(
        server.toplevel_metadata_bytes_len(),
        limits.toplevel_metadata_bytes,
    );

    server.disconnect(replacement_shell).unwrap();
    let next_replacement = server.try_accept_with_caps(shell_caps).unwrap();
    install_full_output_shell_window(&mut server, next_replacement);
    assert_eq!(server.pool_bytes_len(), limits.pool_bytes);
    assert_eq!(
        server.toplevel_metadata_bytes_len(),
        limits.toplevel_metadata_bytes,
    );
}

#[test]
fn server_toplevel_limit_rejects_atomically_and_reclaims_on_destroy() {
    let limits = ServerLimits {
        clients: 1,
        shell_clients: 0,
        toplevels: 2,
        ..ServerLimits::default()
    };
    let mut server = Server::with_limits(8, 8, limits);
    let client_id = server.try_accept().unwrap();
    let xdg_shell = ObjectId::new(3);
    let surfaces = [ObjectId::new(5), ObjectId::new(7), ObjectId::new(9)];
    server
        .client_mut(client_id)
        .unwrap()
        .install_client_object(xdg_shell, Interface::XdgShell)
        .unwrap();
    for surface in surfaces {
        server
            .client_mut(client_id)
            .unwrap()
            .install_client_object(surface, Interface::Surface)
            .unwrap();
    }

    let tops = [ObjectId::new(11), ObjectId::new(13), ObjectId::new(15)];
    for (top, surface) in tops[..2].iter().zip(surfaces[..2].iter()) {
        server
            .dispatch_request(
                client_id,
                &message(xdg_shell, 1, &create_toplevel_payload(*top, *surface)),
            )
            .unwrap();
    }
    let error = server
        .dispatch_request(
            client_id,
            &message(xdg_shell, 1, &create_toplevel_payload(tops[2], surfaces[2])),
        )
        .unwrap_err();
    assert_eq!(
        error,
        ServerError::Client(ClientError::ResourceLimitExceeded {
            resource: ClientResource::ServerToplevels,
            attempted: 3,
            limit: 2,
        })
    );
    assert_eq!(server.window_z_order().len(), 2);
    assert!(server
        .client(client_id)
        .unwrap()
        .toplevel(tops[2])
        .is_none());

    server
        .dispatch_request(client_id, &message(tops[0], 3, &[]))
        .unwrap();
    server
        .dispatch_request(
            client_id,
            &message(xdg_shell, 1, &create_toplevel_payload(tops[2], surfaces[2])),
        )
        .unwrap();
    assert_eq!(server.window_z_order().len(), 2);
}

#[test]
fn replacement_shell_snapshot_has_queue_headroom_at_default_caps() {
    assert_eq!(
        MAX_SERVER_WINDOW_SNAPSHOT_BYTES,
        MAX_SERVER_TOPLEVEL_METADATA_BYTES as usize + MAX_SERVER_TOPLEVELS * 80 + 28,
    );
}

#[test]
fn one_window_metadata_is_bounded_at_n_and_rejected_at_n_plus_one() {
    let mut client = Client::new(ClientId(1));
    let toplevel = ObjectId::new(3);
    install_test_toplevel(&mut client, toplevel, ObjectId::new(5));

    let title = vec![b'x'; MAX_TOPLEVEL_METADATA_BYTES as usize];
    dispatch(&mut client, toplevel, 1, &string_payload_bytes(&title)).unwrap();
    assert_eq!(
        client.toplevel(toplevel).unwrap().title.len(),
        MAX_TOPLEVEL_METADATA_BYTES as usize,
    );

    let error = dispatch(&mut client, toplevel, 2, &string_payload_bytes(b"x")).unwrap_err();
    assert_limit(
        error,
        ClientResource::ToplevelMetadataBytesPerWindow,
        MAX_TOPLEVEL_METADATA_BYTES + 1,
        MAX_TOPLEVEL_METADATA_BYTES,
    );
    assert!(client.toplevel(toplevel).unwrap().app_id.is_empty());

    let mut shell = Client::new(ClientId(2));
    let shell_manager = ObjectId::new(7);
    shell
        .install_client_object(shell_manager, Interface::ShellManager)
        .unwrap();
    shell
        .emit_window_created(
            shell_manager,
            1,
            client.toplevel(toplevel).unwrap().title.as_str(),
            "",
        )
        .unwrap();
    let encoded = shell.drain_pending_events();
    assert!(encoded.len() <= u16::MAX as usize);
    assert_eq!(
        MessageHeader::decode(&encoded).unwrap().length as usize,
        encoded.len(),
    );
}

#[test]
fn repeated_maximum_title_updates_coalesce_without_poisoning_shell_queue() {
    let mut shell = Client::new(ClientId(1));
    let shell_manager = ObjectId::new(3);
    shell
        .install_client_object(shell_manager, Interface::ShellManager)
        .unwrap();
    let title = "x".repeat(MAX_TOPLEVEL_METADATA_BYTES as usize);
    for _ in 0..8 {
        shell
            .emit_window_title_changed(shell_manager, 9, &title)
            .unwrap();
    }
    assert_eq!(shell.pending_events_len(), 1);
    assert!(shell.pending_event_bytes() < MAX_PENDING_EVENT_BYTES);
    assert!(!shell.event_queue_overflowed());
}

#[test]
fn one_maximum_unique_title_request_turn_fits_one_shell_socket_write() {
    let mut shell = Client::new(ClientId(1));
    let shell_manager = ObjectId::new(3);
    shell
        .install_client_object(shell_manager, Interface::ShellManager)
        .unwrap();
    let title = "x".repeat(MAX_TOPLEVEL_METADATA_BYTES as usize);
    for window_id in 1..=32 {
        shell
            .emit_window_title_changed(shell_manager, window_id, &title)
            .unwrap();
    }
    assert_eq!(shell.pending_events_len(), 32);
    let queued = shell.pending_event_bytes();
    assert!(queued <= 32 * 1024);
    let turn = shell.drain_pending_events_bounded(32 * 1024);
    assert_eq!(turn.len(), queued);
    assert_eq!(shell.pending_event_bytes(), 0);
    assert_eq!(shell.pending_events_len(), 0);
}

#[test]
fn server_pool_budget_is_shared_atomic_and_released_on_disconnect() {
    let limits = ServerLimits {
        clients: 3,
        shell_clients: 0,
        pool_bytes: 10,
        ..ServerLimits::default()
    };
    let mut server = Server::with_limits(8, 8, limits);
    let first = server.try_accept().unwrap();
    let second = server.try_accept().unwrap();
    let shm = ObjectId::new(3);
    for id in [first, second] {
        server
            .client_mut(id)
            .unwrap()
            .install_client_object(shm, Interface::Shm)
            .unwrap();
    }

    let first_pool = ObjectId::new(5);
    server
        .dispatch_request(first, &message(shm, 1, &create_pool_payload(first_pool, 6)))
        .unwrap();
    let rejected_pool = ObjectId::new(5);
    let error = server
        .dispatch_request(
            second,
            &message(shm, 1, &create_pool_payload(rejected_pool, 5)),
        )
        .unwrap_err();
    assert_eq!(
        error,
        ServerError::Client(ClientError::ResourceLimitExceeded {
            resource: ClientResource::ServerPoolBytes,
            attempted: 11,
            limit: 10,
        })
    );
    assert_eq!(server.pool_bytes_len(), 6);
    assert_eq!(server.client(second).unwrap().get(rejected_pool), None);

    server.disconnect(first).unwrap();
    assert_eq!(server.pool_bytes_len(), 0);
    server
        .dispatch_request(
            second,
            &message(shm, 1, &create_pool_payload(rejected_pool, 5)),
        )
        .unwrap();
    assert_eq!(server.pool_bytes_len(), 5);
}

#[test]
fn destroyed_backing_stays_reference_safe_then_reclaims_after_rebind() {
    let limits = ServerLimits {
        clients: 1,
        shell_clients: 0,
        pool_bytes: 32,
        ..ServerLimits::default()
    };
    let mut server = Server::with_limits(8, 8, limits);
    let client_id = server.try_accept().unwrap();
    let shm = ObjectId::new(3);
    let compositor = ObjectId::new(5);
    for (id, interface) in [(shm, Interface::Shm), (compositor, Interface::Compositor)] {
        server
            .client_mut(client_id)
            .unwrap()
            .install_client_object(id, interface)
            .unwrap();
    }

    let old_pool = ObjectId::new(7);
    let old_buffer = ObjectId::new(9);
    let surface = ObjectId::new(11);
    server
        .dispatch_request(
            client_id,
            &message(shm, 1, &create_pool_payload(old_pool, 8)),
        )
        .unwrap();
    server
        .dispatch_request(
            client_id,
            &message(old_pool, 1, &create_buffer_payload(old_buffer, 0, 1, 1, 4)),
        )
        .unwrap();
    server
        .dispatch_request(client_id, &message(compositor, 1, &new_id_payload(surface)))
        .unwrap();
    server
        .dispatch_request(client_id, &message(surface, 2, &attach_payload(old_buffer)))
        .unwrap();
    server
        .dispatch_request(client_id, &message(surface, 7, &[]))
        .unwrap();

    server
        .dispatch_request(client_id, &message(old_buffer, 1, &[]))
        .unwrap();
    server
        .dispatch_request(client_id, &message(old_pool, 3, &[]))
        .unwrap();
    let client = server.client(client_id).unwrap();
    assert_eq!(client.get(old_buffer), None);
    assert_eq!(client.get(old_pool), None);
    assert!(client.buffer_info(old_buffer).is_some());
    assert!(client.pool(old_pool).is_some());
    assert_eq!(server.pool_bytes_len(), 8);
    assert_eq!(
        server
            .dispatch_request(
                client_id,
                &message(shm, 1, &create_pool_payload(old_pool, 1)),
            )
            .unwrap_err(),
        ServerError::Client(ClientError::DuplicateObject { id: old_pool })
    );

    let new_pool = ObjectId::new(13);
    let new_buffer = ObjectId::new(15);
    server
        .dispatch_request(
            client_id,
            &message(shm, 1, &create_pool_payload(new_pool, 4)),
        )
        .unwrap();
    assert_eq!(
        server
            .dispatch_request(
                client_id,
                &message(new_pool, 1, &create_buffer_payload(old_buffer, 0, 1, 1, 4),),
            )
            .unwrap_err(),
        ServerError::Client(ClientError::DuplicateObject { id: old_buffer })
    );
    server
        .dispatch_request(
            client_id,
            &message(new_pool, 1, &create_buffer_payload(new_buffer, 0, 1, 1, 4)),
        )
        .unwrap();
    server
        .dispatch_request(client_id, &message(surface, 2, &attach_payload(new_buffer)))
        .unwrap();
    server
        .dispatch_request(client_id, &message(surface, 7, &[]))
        .unwrap();

    let client = server.client(client_id).unwrap();
    assert!(client.buffer_info(old_buffer).is_none());
    assert!(client.pool(old_pool).is_none());
    assert_eq!(server.pool_bytes_len(), 4);

    server
        .dispatch_request(
            client_id,
            &message(shm, 1, &create_pool_payload(old_pool, 1)),
        )
        .unwrap();
    assert_eq!(server.pool_bytes_len(), 5);
}

#[test]
fn retained_pool_and_buffer_backing_count_toward_resource_limits() {
    let mut configured = limits();
    configured.pools = 2;
    configured.buffers = 1;
    let mut client = new_client(configured);
    let shm = ObjectId::new(3);
    let compositor = ObjectId::new(5);
    for (id, interface) in [(shm, Interface::Shm), (compositor, Interface::Compositor)] {
        client.install_client_object(id, interface).unwrap();
    }

    let retired_pool = ObjectId::new(7);
    let live_pool = ObjectId::new(9);
    let retired_buffer = ObjectId::new(11);
    let surface = ObjectId::new(13);
    dispatch(&mut client, shm, 1, &create_pool_payload(retired_pool, 4)).unwrap();
    dispatch(&mut client, shm, 1, &create_pool_payload(live_pool, 4)).unwrap();
    dispatch(
        &mut client,
        retired_pool,
        1,
        &create_buffer_payload(retired_buffer, 0, 1, 1, 4),
    )
    .unwrap();
    dispatch(&mut client, compositor, 1, &new_id_payload(surface)).unwrap();
    dispatch(&mut client, surface, 2, &attach_payload(retired_buffer)).unwrap();
    dispatch(&mut client, surface, 7, &[]).unwrap();
    dispatch(&mut client, retired_buffer, 1, &[]).unwrap();
    dispatch(&mut client, retired_pool, 3, &[]).unwrap();

    assert_eq!(client.pools.len(), 2);
    assert_eq!(client.buffers.len(), 1);
    let rejected_pool = ObjectId::new(15);
    let error = dispatch(&mut client, shm, 1, &create_pool_payload(rejected_pool, 0)).unwrap_err();
    assert_limit(error, ClientResource::Pools, 3, 2);
    let rejected_buffer = ObjectId::new(17);
    let error = dispatch(
        &mut client,
        live_pool,
        1,
        &create_buffer_payload(rejected_buffer, 0, 1, 1, 4),
    )
    .unwrap_err();
    assert_limit(error, ClientResource::Buffers, 2, 1);

    dispatch(&mut client, surface, 1, &[]).unwrap();
    assert_eq!(client.pools.len(), 1);
    assert_eq!(client.buffers.len(), 0);
    dispatch(&mut client, shm, 1, &create_pool_payload(rejected_pool, 0)).unwrap();
    dispatch(
        &mut client,
        live_pool,
        1,
        &create_buffer_payload(rejected_buffer, 0, 1, 1, 4),
    )
    .unwrap();
}

#[test]
fn pool_resize_updates_budgets_and_rejects_truncating_live_buffers() {
    let limits = ServerLimits {
        clients: 1,
        shell_clients: 0,
        pool_bytes: 16,
        ..ServerLimits::default()
    };
    let mut server = Server::with_limits(8, 8, limits);
    let client_id = server.try_accept().unwrap();
    let shm = ObjectId::new(3);
    let pool = ObjectId::new(5);
    let buffer = ObjectId::new(7);
    server
        .client_mut(client_id)
        .unwrap()
        .install_client_object(shm, Interface::Shm)
        .unwrap();
    server
        .dispatch_request(client_id, &message(shm, 1, &create_pool_payload(pool, 8)))
        .unwrap();
    server
        .dispatch_request(
            client_id,
            &message(pool, 1, &create_buffer_payload(buffer, 0, 1, 1, 4)),
        )
        .unwrap();

    server
        .dispatch_request(client_id, &message(pool, 2, &resize_payload(12)))
        .unwrap();
    assert_eq!(server.pool_bytes_len(), 12);
    assert_eq!(
        server.client(client_id).unwrap().pool(pool).unwrap().size,
        12
    );

    assert_eq!(
        server
            .dispatch_request(client_id, &message(pool, 2, &resize_payload(3)))
            .unwrap_err(),
        ServerError::Client(ClientError::PoolResizeWouldTruncateBuffer {
            pool_id: pool,
            requested: 3,
            required: 4,
        })
    );
    assert_eq!(server.pool_bytes_len(), 12);
    assert_eq!(
        server.client(client_id).unwrap().pool(pool).unwrap().size,
        12
    );

    server
        .dispatch_request(client_id, &message(pool, 2, &resize_payload(4)))
        .unwrap();
    assert_eq!(server.pool_bytes_len(), 4);
    assert_eq!(
        server.client(client_id).unwrap().pool(pool).unwrap().size,
        4
    );
}

#[test]
fn surface_destroy_reclaims_retired_buffer_and_pool_backing() {
    let mut server = Server::with_limits(
        8,
        8,
        ServerLimits {
            clients: 1,
            shell_clients: 0,
            pool_bytes: 16,
            ..ServerLimits::default()
        },
    );
    let client_id = server.try_accept().unwrap();
    let shm = ObjectId::new(3);
    let compositor = ObjectId::new(5);
    let pool = ObjectId::new(7);
    let buffer = ObjectId::new(9);
    let surface = ObjectId::new(11);
    for (id, interface) in [(shm, Interface::Shm), (compositor, Interface::Compositor)] {
        server
            .client_mut(client_id)
            .unwrap()
            .install_client_object(id, interface)
            .unwrap();
    }
    server
        .dispatch_request(client_id, &message(shm, 1, &create_pool_payload(pool, 4)))
        .unwrap();
    server
        .dispatch_request(
            client_id,
            &message(pool, 1, &create_buffer_payload(buffer, 0, 1, 1, 4)),
        )
        .unwrap();
    server
        .dispatch_request(client_id, &message(compositor, 1, &new_id_payload(surface)))
        .unwrap();
    server
        .dispatch_request(client_id, &message(surface, 2, &attach_payload(buffer)))
        .unwrap();
    server
        .dispatch_request(client_id, &message(surface, 7, &[]))
        .unwrap();
    server
        .dispatch_request(client_id, &message(buffer, 1, &[]))
        .unwrap();
    server
        .dispatch_request(client_id, &message(pool, 3, &[]))
        .unwrap();
    assert_eq!(server.pool_bytes_len(), 4);

    server
        .dispatch_request(client_id, &message(surface, 1, &[]))
        .unwrap();
    let client = server.client(client_id).unwrap();
    assert!(client.surface(surface).is_none());
    assert!(client.buffer_info(buffer).is_none());
    assert!(client.pool(pool).is_none());
    assert_eq!(server.pool_bytes_len(), 0);
}
