//! IPC isolation tests (T064).
//!
//! Runs via `cargo test -p kernel`. Covers the kernel's pipe
//! and unix-socket-equivalent primitives against the
//! IpcTable front-end.

#![cfg(feature = "native-platform")]

use kernel::fd::FdObject;
use kernel::ipc::{
    IpcError, IpcLimits, IpcTable, PipeReadResult, PipeWriteResult, SocketState, SocketType,
    PIPE_BUF_CAP, SOCKET_FD_QUEUE_CAP,
};

// ---- Pipes ---------------------------------------------------------

#[test]
fn pipe_round_trip_single_write_and_read() {
    let mut ipc = IpcTable::new();
    let pid = ipc.create_pipe().unwrap();

    let pipe = ipc.pipe_mut(pid).unwrap();
    assert_eq!(pipe.try_write(b"hello"), PipeWriteResult::Wrote(5));
    let mut buf = [0u8; 8];
    assert_eq!(pipe.try_read(&mut buf), PipeReadResult::Read(5));
    assert_eq!(&buf[..5], b"hello");
}

#[test]
fn pipe_read_empty_returns_would_block_while_writer_open() {
    let mut ipc = IpcTable::new();
    let pid = ipc.create_pipe().unwrap();
    let pipe = ipc.pipe_mut(pid).unwrap();
    let mut buf = [0u8; 4];
    assert_eq!(pipe.try_read(&mut buf), PipeReadResult::WouldBlock);
}

#[test]
fn pipe_write_to_closed_reader_is_broken() {
    let mut ipc = IpcTable::new();
    let pid = ipc.create_pipe().unwrap();
    // Drop the reader reference.
    let wakes = ipc.drop_pipe_reader(pid).unwrap();
    assert!(wakes.is_empty()); // nobody was parked

    let pipe = ipc.pipe_mut(pid).unwrap();
    assert_eq!(pipe.try_write(b"hi"), PipeWriteResult::Broken);
}

#[test]
fn pipe_read_after_writer_closes_returns_eof_after_draining() {
    let mut ipc = IpcTable::new();
    let pid = ipc.create_pipe().unwrap();

    // Write some data first, then close the writer.
    let pipe = ipc.pipe_mut(pid).unwrap();
    pipe.try_write(b"data");
    ipc.drop_pipe_writer(pid).unwrap();

    // Read the buffered data — still returns Read, not Eof.
    let pipe = ipc.pipe_mut(pid).unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(pipe.try_read(&mut buf), PipeReadResult::Read(4));
    assert_eq!(&buf[..4], b"data");

    // Next read on an empty, writer-closed pipe returns Eof.
    assert_eq!(pipe.try_read(&mut buf), PipeReadResult::Eof);
}

#[test]
fn pipe_partial_write_when_buffer_near_full() {
    let mut ipc = IpcTable::new();
    let pid = ipc.create_pipe().unwrap();
    let pipe = ipc.pipe_mut(pid).unwrap();

    // Fill to within 100 bytes of full.
    let bulk = vec![0x55u8; PIPE_BUF_CAP - 100];
    assert_eq!(
        pipe.try_write(&bulk),
        PipeWriteResult::Wrote(PIPE_BUF_CAP - 100)
    );

    // Try to write 300 bytes — should write exactly 100.
    let more = vec![0xAAu8; 300];
    assert_eq!(pipe.try_write(&more), PipeWriteResult::Wrote(100));

    // Buffer is now full.
    assert_eq!(pipe.len(), PIPE_BUF_CAP);
    assert_eq!(pipe.try_write(b"x"), PipeWriteResult::WouldBlock);
}

#[test]
fn pipe_is_dead_and_reaped_after_both_ends_close() {
    let mut ipc = IpcTable::new();
    let pid = ipc.create_pipe().unwrap();
    assert_eq!(ipc.pipe_count(), 1);

    ipc.drop_pipe_writer(pid).unwrap();
    // Still alive — the buffer is empty but reader is still open.
    assert_eq!(ipc.pipe_count(), 1);

    ipc.drop_pipe_reader(pid).unwrap();
    // Both ends closed, buffer empty → reaped.
    assert_eq!(ipc.pipe_count(), 0);
}

#[test]
fn filled_pipe_is_reaped_when_both_ends_close_without_a_read() {
    let mut ipc = IpcTable::new();
    for _ in 0..32 {
        let id = ipc.create_pipe().unwrap();
        assert_eq!(
            ipc.pipe_mut(id)
                .unwrap()
                .try_write(&vec![0x5a; PIPE_BUF_CAP]),
            PipeWriteResult::Wrote(PIPE_BUF_CAP)
        );
        ipc.drop_pipe_writer(id).unwrap();
        ipc.drop_pipe_reader(id).unwrap();
        assert_eq!(ipc.pipe_count(), 0);
    }
}

#[test]
fn pipe_refcounts_allow_multiple_readers_and_writers() {
    let mut ipc = IpcTable::new();
    let pid = ipc.create_pipe().unwrap();

    // Dup both ends once.
    let pipe = ipc.pipe_mut(pid).unwrap();
    pipe.dup_reader();
    pipe.dup_writer();
    assert_eq!(pipe.reader_count(), 2);
    assert_eq!(pipe.writer_count(), 2);

    // Drop one reader and one writer; the pipe stays alive.
    ipc.drop_pipe_reader(pid).unwrap();
    ipc.drop_pipe_writer(pid).unwrap();
    let pipe = ipc.pipe_mut(pid).unwrap();
    assert_eq!(pipe.reader_count(), 1);
    assert_eq!(pipe.writer_count(), 1);
    assert!(!pipe.reader_closed());
    assert!(!pipe.writer_closed());
}

#[test]
fn pipe_drop_writer_wakes_parked_readers() {
    let mut ipc = IpcTable::new();
    let pid = ipc.create_pipe().unwrap();
    let pipe = ipc.pipe_mut(pid).unwrap();
    pipe.park_reader(42);
    pipe.park_reader(77);

    let wakes = ipc.drop_pipe_writer(pid).unwrap();
    assert_eq!(wakes, vec![42, 77]);
}

#[test]
fn pipe_drop_reader_wakes_parked_writers() {
    let mut ipc = IpcTable::new();
    let pid = ipc.create_pipe().unwrap();
    let pipe = ipc.pipe_mut(pid).unwrap();
    pipe.park_writer(10);

    let wakes = ipc.drop_pipe_reader(pid).unwrap();
    assert_eq!(wakes, vec![10]);
}

#[test]
fn pipe_operations_on_unknown_id_fail() {
    let mut ipc = IpcTable::new();
    use kernel::ipc::PipeId;
    let unknown = PipeId(999);
    assert!(matches!(ipc.pipe_mut(unknown), Err(IpcError::NoSuchPipe)));
    assert!(matches!(
        ipc.drop_pipe_reader(unknown),
        Err(IpcError::NoSuchPipe)
    ));
}

// ---- Sockets -------------------------------------------------------

#[test]
fn socket_create_starts_unbound() {
    let mut ipc = IpcTable::new();
    let id = ipc.create_socket(SocketType::Stream).unwrap();
    let s = ipc.socket_mut(id).unwrap();
    assert_eq!(s.state, SocketState::Unbound);
    assert_eq!(s.ty, SocketType::Stream);
    assert!(s.bound_path.is_none());
    assert!(s.peer.is_none());
}

#[test]
fn datagram_creation_is_rejected_without_consuming_an_id_or_object() {
    let mut ipc = IpcTable::new();

    assert_eq!(
        ipc.create_socket(SocketType::Dgram),
        Err(IpcError::UnsupportedSocketType)
    );
    assert_eq!(ipc.socket_count(), 0);

    let stream = ipc.create_socket(SocketType::Stream).unwrap();
    assert_eq!(stream, kernel::ipc::SocketId(1));
    assert_eq!(ipc.socket_count(), 1);
}

#[test]
fn socket_bind_then_listen_transitions_state() {
    let mut ipc = IpcTable::new();
    let id = ipc.create_socket(SocketType::Stream).unwrap();

    ipc.bind_socket(id, "/run/display").unwrap();
    assert_eq!(ipc.socket_mut(id).unwrap().state, SocketState::Bound);

    ipc.listen_socket(id, 16).unwrap();
    assert_eq!(ipc.socket_mut(id).unwrap().state, SocketState::Listening);
}

#[test]
fn socket_bind_duplicate_path_is_address_in_use() {
    let mut ipc = IpcTable::new();
    let a = ipc.create_socket(SocketType::Stream).unwrap();
    let b = ipc.create_socket(SocketType::Stream).unwrap();

    ipc.bind_socket(a, "/run/svc").unwrap();
    let err = ipc.bind_socket(b, "/run/svc").unwrap_err();
    assert_eq!(err, IpcError::AddressInUse);
}

#[test]
fn socket_listen_without_bind_is_invalid_state() {
    let mut ipc = IpcTable::new();
    let id = ipc.create_socket(SocketType::Stream).unwrap();
    let err = ipc.listen_socket(id, 4).unwrap_err();
    assert_eq!(err, IpcError::InvalidState);
}

#[test]
fn socket_connect_without_listener_refused() {
    let mut ipc = IpcTable::new();
    let client = ipc.create_socket(SocketType::Stream).unwrap();
    let err = ipc.connect_socket(client, "/run/ghost").unwrap_err();
    assert_eq!(err, IpcError::ConnectionRefused);
}

#[test]
fn socket_connect_before_listen_is_retryable_on_the_same_client() {
    let mut ipc = IpcTable::new();
    let listener = ipc.create_socket(SocketType::Stream).unwrap();
    ipc.bind_socket(listener, "/run/svc").unwrap();
    let client = ipc.create_socket(SocketType::Stream).unwrap();

    assert_eq!(
        ipc.connect_socket(client, "/run/svc").unwrap_err(),
        IpcError::ConnectionRefused,
    );
    assert_eq!(ipc.socket_mut(client).unwrap().state, SocketState::Unbound,);

    ipc.listen_socket(listener, 4).unwrap();
    ipc.connect_socket(client, "/run/svc").unwrap();
    assert_eq!(
        ipc.socket_mut(client).unwrap().state,
        SocketState::Connecting,
    );
    assert!(ipc.accept_socket(listener).is_ok());
}

#[test]
fn socket_connect_accept_pairs_client_and_server() {
    let mut ipc = IpcTable::new();
    let listener = ipc.create_socket(SocketType::Stream).unwrap();
    ipc.bind_socket(listener, "/run/svc").unwrap();
    ipc.listen_socket(listener, 4).unwrap();

    let client = ipc.create_socket(SocketType::Stream).unwrap();
    ipc.connect_socket(client, "/run/svc").unwrap();
    assert_eq!(
        ipc.socket_mut(client).unwrap().state,
        SocketState::Connecting
    );

    let server = ipc.accept_socket(listener).unwrap();
    assert_ne!(server, listener);
    assert_ne!(server, client);

    // Server and client are now paired and connected.
    let s = ipc.socket_mut(server).unwrap();
    assert_eq!(s.state, SocketState::Connected);
    assert_eq!(s.peer, Some(client));

    let c = ipc.socket_mut(client).unwrap();
    assert_eq!(c.state, SocketState::Connected);
    assert_eq!(c.peer, Some(server));
}

#[test]
fn socket_accept_without_pending_would_block() {
    let mut ipc = IpcTable::new();
    let listener = ipc.create_socket(SocketType::Stream).unwrap();
    ipc.bind_socket(listener, "/run/svc").unwrap();
    ipc.listen_socket(listener, 4).unwrap();

    let err = ipc.accept_socket(listener).unwrap_err();
    assert_eq!(err, IpcError::WouldBlock);
}

#[test]
fn socket_send_recv_round_trip() {
    let mut ipc = IpcTable::new();
    let (client, server) = paired(&mut ipc);

    // client → server
    let n = ipc
        .send_on_socket(client, b"hello", alloc::vec::Vec::new())
        .unwrap();
    assert_eq!(n, 5);

    let mut buf = [0u8; 16];
    let (n, fds) = ipc.recv_on_socket(server, &mut buf, 4).unwrap();
    assert_eq!(n, 5);
    assert_eq!(&buf[..5], b"hello");
    assert!(fds.is_empty());

    // server → client
    let n = ipc
        .send_on_socket(server, b"world", alloc::vec::Vec::new())
        .unwrap();
    assert_eq!(n, 5);
    let (n, _) = ipc.recv_on_socket(client, &mut buf, 0).unwrap();
    assert_eq!(n, 5);
    assert_eq!(&buf[..5], b"world");
}

#[test]
fn socket_send_with_passed_fds_round_trip() {
    let mut ipc = IpcTable::new();
    let (client, server) = paired(&mut ipc);

    let passed = alloc::vec![
        FdObject::CharDevice(100),
        FdObject::CharDevice(200),
        FdObject::CharDevice(300),
    ];
    let n = ipc.send_on_socket(client, b"fd test", passed).unwrap();
    assert_eq!(n, 7);

    let mut buf = [0u8; 16];
    let (n, fds) = ipc.recv_on_socket(server, &mut buf, 4).unwrap();
    assert_eq!(n, 7);
    assert_eq!(&buf[..7], b"fd test");
    assert_eq!(
        fds,
        vec![
            FdObject::CharDevice(100),
            FdObject::CharDevice(200),
            FdObject::CharDevice(300)
        ]
    );
}

#[test]
fn zero_byte_fd_passing_is_bounded_and_close_reclaims_the_queue() {
    let mut ipc = IpcTable::new();
    let (client, server) = paired(&mut ipc);

    for fd in 0..SOCKET_FD_QUEUE_CAP as u32 {
        assert_eq!(
            ipc.send_on_socket(client, b"", vec![FdObject::CharDevice(fd)])
                .unwrap(),
            0
        );
    }
    assert_eq!(
        ipc.sockets_get(server).unwrap().rx_fd_count(),
        SOCKET_FD_QUEUE_CAP
    );
    assert_eq!(
        ipc.send_on_socket(client, b"", vec![FdObject::CharDevice(999)])
            .unwrap_err(),
        IpcError::ResourceLimit
    );

    ipc.close_socket(server).unwrap();
    ipc.close_socket(client).unwrap();
    assert_eq!(ipc.socket_count(), 0);
}

#[test]
fn filled_socket_pair_is_removed_even_when_neither_side_reads() {
    let mut ipc = IpcTable::new();
    for _ in 0..32 {
        let (client, server) = paired(&mut ipc);
        assert_eq!(
            ipc.send_on_socket(client, &vec![0x33; kernel::ipc::SOCKET_BUF_CAP], Vec::new())
                .unwrap(),
            kernel::ipc::SOCKET_BUF_CAP
        );
        ipc.close_socket(server).unwrap();
        ipc.close_socket(client).unwrap();
        assert_eq!(ipc.socket_count(), 0);
    }
}

#[test]
fn socket_recv_fd_limit_is_respected() {
    let mut ipc = IpcTable::new();
    let (client, server) = paired(&mut ipc);

    let passed = alloc::vec![
        FdObject::CharDevice(1),
        FdObject::CharDevice(2),
        FdObject::CharDevice(3),
        FdObject::CharDevice(4),
        FdObject::CharDevice(5),
    ];
    ipc.send_on_socket(client, b"x", passed).unwrap();

    // First recv asks for max 2 fds.
    let mut buf = [0u8; 4];
    let (_, fds) = ipc.recv_on_socket(server, &mut buf, 2).unwrap();
    assert_eq!(fds, vec![FdObject::CharDevice(1), FdObject::CharDevice(2)]);

    // Second recv asks for max 10 fds → gets the rest.
    let mut buf = [0u8; 4];
    let (_, fds) = ipc.recv_on_socket(server, &mut buf, 10).unwrap();
    assert_eq!(
        fds,
        vec![
            FdObject::CharDevice(3),
            FdObject::CharDevice(4),
            FdObject::CharDevice(5)
        ]
    );
}

#[test]
fn socket_recv_empty_returns_would_block_while_peer_open() {
    let mut ipc = IpcTable::new();
    let (_client, server) = paired(&mut ipc);
    let mut buf = [0u8; 4];
    let err = ipc.recv_on_socket(server, &mut buf, 0).unwrap_err();
    assert_eq!(err, IpcError::WouldBlock);
}

#[test]
fn socket_recv_empty_after_peer_close_returns_eof() {
    let mut ipc = IpcTable::new();
    let (client, server) = paired(&mut ipc);
    ipc.close_socket(client).unwrap();

    let mut buf = [0u8; 4];
    // EOF on a stream is encoded as Ok((0, Vec::new())).
    let (n, fds) = ipc.recv_on_socket(server, &mut buf, 0).unwrap();
    assert_eq!(n, 0);
    assert!(fds.is_empty());
}

#[test]
fn socket_recv_drains_buffered_bytes_after_peer_close_before_eof() {
    let mut ipc = IpcTable::new();
    let (client, server) = paired(&mut ipc);
    ipc.send_on_socket(client, b"pixels", Vec::new()).unwrap();
    ipc.close_socket(client).unwrap();

    let mut buf = [0u8; 8];
    let (n, fds) = ipc.recv_on_socket(server, &mut buf, 0).unwrap();
    assert_eq!(n, 6);
    assert_eq!(&buf[..n], b"pixels");
    assert!(fds.is_empty());

    let (n, fds) = ipc.recv_on_socket(server, &mut buf, 0).unwrap();
    assert_eq!(n, 0, "EOF is observable only after buffered bytes drain");
    assert!(fds.is_empty());
}

#[test]
fn socket_send_after_peer_close_is_broken_pipe() {
    let mut ipc = IpcTable::new();
    let (client, server) = paired(&mut ipc);
    ipc.close_socket(server).unwrap();

    let err = ipc
        .send_on_socket(client, b"hi", alloc::vec::Vec::new())
        .unwrap_err();
    assert_eq!(err, IpcError::PipeBroken);
}

#[test]
fn socket_close_unbinds_the_path() {
    let mut ipc = IpcTable::new();
    let listener = ipc.create_socket(SocketType::Stream).unwrap();
    ipc.bind_socket(listener, "/run/svc").unwrap();
    ipc.listen_socket(listener, 4).unwrap();

    // Second bind to the same path fails while the first is alive.
    let other = ipc.create_socket(SocketType::Stream).unwrap();
    let err = ipc.bind_socket(other, "/run/svc").unwrap_err();
    assert_eq!(err, IpcError::AddressInUse);

    // After close, the binding is released and rebind succeeds.
    ipc.close_socket(listener).unwrap();
    ipc.bind_socket(other, "/run/svc").unwrap();
}

// ---- Listener drop with pending Connecting clients ----------------
//
// When a listener socket is dropped while pending Connecting clients
// are queued on its backlog, those clients stay paired with a peer
// that will never accept them. Before the backlog-drain step these
// clients observed `InvalidState` (→ EINVAL) on subsequent
// `fd_write`, which is indistinguishable from the "server is alive
// but hasn't accepted yet" EINVAL that display-client-demo's retry
// loop papers over. After the drain, drained clients observe
// `ConnectionRefused` (→ ECONNREFUSED) instead — the same errno they
// would have seen if the listener had never been reachable at all,
// and the cue display-client-demo would use to break out of the
// retry loop.
//
// The drain transitions each pending client to `SocketState::Closed`
// and leaves `peer = None`. `send_on_socket` gains one branch that
// maps the Closed-state case to `ConnectionRefused`. The existing
// Connecting-state case keeps returning `InvalidState`, so the
// accept-race EINVAL retry window in display-client-demo is not
// disturbed.

#[test]
fn socket_close_listener_drains_pending_connecting_client_to_closed_state() {
    let mut ipc = IpcTable::new();
    let listener = ipc.create_socket(SocketType::Stream).unwrap();
    ipc.bind_socket(listener, "/run/svc").unwrap();
    ipc.listen_socket(listener, 4).unwrap();

    let client = ipc.create_socket(SocketType::Stream).unwrap();
    ipc.connect_socket(client, "/run/svc").unwrap();
    // Pre-drop, the client is Connecting (queued on the backlog).
    assert_eq!(
        ipc.socket_mut(client).unwrap().state,
        SocketState::Connecting
    );

    ipc.close_socket(listener).unwrap();

    // Post-drop, the drained client transitions to Closed — the
    // signal subsequent send/recv branches detect.
    let c = ipc.socket_mut(client).unwrap();
    assert_eq!(c.state, SocketState::Closed);
    // The drained client never had a peer; the listener minted the
    // server side only at accept time.
    assert_eq!(c.peer, None);
}

#[test]
fn socket_close_listener_drains_all_pending_connecting_clients() {
    let mut ipc = IpcTable::new();
    let listener = ipc.create_socket(SocketType::Stream).unwrap();
    ipc.bind_socket(listener, "/run/svc").unwrap();
    ipc.listen_socket(listener, 8).unwrap();

    let clients: alloc::vec::Vec<_> = (0..3)
        .map(|_| {
            let c = ipc.create_socket(SocketType::Stream).unwrap();
            ipc.connect_socket(c, "/run/svc").unwrap();
            c
        })
        .collect();

    ipc.close_socket(listener).unwrap();

    for c in clients {
        assert_eq!(ipc.socket_mut(c).unwrap().state, SocketState::Closed);
    }
}

#[test]
fn socket_send_on_drained_client_returns_connection_refused() {
    let mut ipc = IpcTable::new();
    let listener = ipc.create_socket(SocketType::Stream).unwrap();
    ipc.bind_socket(listener, "/run/svc").unwrap();
    ipc.listen_socket(listener, 4).unwrap();

    let client = ipc.create_socket(SocketType::Stream).unwrap();
    ipc.connect_socket(client, "/run/svc").unwrap();
    ipc.close_socket(listener).unwrap();

    // fd_write on the drained client observes a ConnectionRefused
    // error that maps to ECONNREFUSED at the syscall layer — the
    // semantic cue "the server you tried to reach is gone".
    let err = ipc
        .send_on_socket(client, b"hi", alloc::vec::Vec::new())
        .unwrap_err();
    assert_eq!(err, IpcError::ConnectionRefused);
}

#[test]
fn socket_send_on_connecting_client_with_live_listener_would_block() {
    // A successful asynchronous connect is pending rather than malformed.
    // The caller parks on FD_WRITE and retries after accept.
    let mut ipc = IpcTable::new();
    let listener = ipc.create_socket(SocketType::Stream).unwrap();
    ipc.bind_socket(listener, "/run/svc").unwrap();
    ipc.listen_socket(listener, 4).unwrap();

    let client = ipc.create_socket(SocketType::Stream).unwrap();
    ipc.connect_socket(client, "/run/svc").unwrap();
    // No close — the listener is still live.

    let err = ipc
        .send_on_socket(client, b"hi", alloc::vec::Vec::new())
        .unwrap_err();
    assert_eq!(err, IpcError::WouldBlock);
}

#[test]
fn socket_close_listener_with_empty_backlog_is_unchanged() {
    let mut ipc = IpcTable::new();
    let listener = ipc.create_socket(SocketType::Stream).unwrap();
    ipc.bind_socket(listener, "/run/svc").unwrap();
    ipc.listen_socket(listener, 4).unwrap();

    // No pending clients; close should behave exactly as before:
    // binding released, listener flagged closed.
    ipc.close_socket(listener).unwrap();
    let other = ipc.create_socket(SocketType::Stream).unwrap();
    ipc.bind_socket(other, "/run/svc").unwrap();
}

// ---- shutdown_socket (half-close) ---------------------------------
//
// Post-slice, IpcTable tracks per-direction shutdown flags in
// addition to the existing .closed flag. shutdown_socket(id, read,
// write) sets them monotonically (an OR — repeated calls never
// un-shut a direction). Subsequent send / recv honour the flags via
// the additional short-circuits introduced alongside the
// SOCK_SHUTDOWN handler rewrite.

#[test]
fn ipc_shutdown_socket_sets_read_and_write_flags() {
    let mut ipc = IpcTable::new();
    let (a, _b) = paired(&mut ipc);

    ipc.shutdown_socket(a, true, false).unwrap();
    {
        let sa = ipc.socket_mut(a).unwrap();
        assert!(sa.shutdown_read);
        assert!(!sa.shutdown_write);
        assert!(!sa.closed, "shutdown is not close");
    }

    ipc.shutdown_socket(a, false, true).unwrap();
    {
        let sa = ipc.socket_mut(a).unwrap();
        assert!(sa.shutdown_read, "read side stays shut (monotonic OR)");
        assert!(sa.shutdown_write);
    }
}

#[test]
fn ipc_shutdown_socket_on_unknown_id_returns_no_such_socket() {
    let mut ipc = IpcTable::new();
    let err = ipc
        .shutdown_socket(kernel::ipc::SocketId(42), true, true)
        .unwrap_err();
    assert_eq!(err, IpcError::NoSuchSocket);
}

#[test]
fn ipc_shutdown_socket_read_short_circuits_recv_to_eof() {
    // With bytes in rx_buf, a read-shut socket still gets EOF —
    // discards pending data, matching POSIX shutdown(SHUT_RD).
    let mut ipc = IpcTable::new();
    let (a, b) = paired(&mut ipc);

    // Populate a's rx_buf via b sending.
    ipc.send_on_socket(b, b"stale", Vec::new()).unwrap();
    assert_eq!(ipc.socket_mut(a).unwrap().rx_len(), 5);

    ipc.shutdown_socket(a, true, false).unwrap();
    let mut buf = [0u8; 16];
    let (n, _) = ipc.recv_on_socket(a, &mut buf, 0).unwrap();
    assert_eq!(n, 0, "read-shut recv returns EOF regardless of rx contents");
}

#[test]
fn ipc_shutdown_socket_read_makes_peer_send_return_pipe_broken() {
    let mut ipc = IpcTable::new();
    let (a, b) = paired(&mut ipc);
    ipc.shutdown_socket(a, true, false).unwrap();
    let err = ipc.send_on_socket(b, b"data", Vec::new()).unwrap_err();
    assert_eq!(err, IpcError::PipeBroken);
}

#[test]
fn ipc_shutdown_socket_write_makes_own_send_return_pipe_broken() {
    let mut ipc = IpcTable::new();
    let (a, _b) = paired(&mut ipc);
    ipc.shutdown_socket(a, false, true).unwrap();
    let err = ipc.send_on_socket(a, b"data", Vec::new()).unwrap_err();
    assert_eq!(err, IpcError::PipeBroken);
}

#[test]
fn ipc_shutdown_socket_write_makes_peer_recv_eof_after_drain() {
    let mut ipc = IpcTable::new();
    let (a, b) = paired(&mut ipc);
    ipc.send_on_socket(a, b"ab", Vec::new()).unwrap();
    ipc.shutdown_socket(a, false, true).unwrap();

    let mut buf = [0u8; 4];
    let (n1, _) = ipc.recv_on_socket(b, &mut buf, 0).unwrap();
    assert_eq!(n1, 2, "existing rx buffer bytes still readable");
    assert_eq!(&buf[..2], b"ab");

    let (n2, _) = ipc.recv_on_socket(b, &mut buf, 0).unwrap();
    assert_eq!(n2, 0, "EOF after drain");
}

#[test]
fn ipc_shutdown_socket_rdwr_does_not_set_closed_flag() {
    // Full-direction shutdown is still semantically distinct from
    // close — the fd stays open, the socket entry stays live in
    // the IpcTable map, and close_socket is the only path that
    // unbinds a listener or transitions pending Connecting clients.
    let mut ipc = IpcTable::new();
    let (a, _b) = paired(&mut ipc);
    ipc.shutdown_socket(a, true, true).unwrap();
    let sa = ipc.socket_mut(a).unwrap();
    assert!(sa.shutdown_read);
    assert!(sa.shutdown_write);
    assert!(!sa.closed);
}

#[test]
fn ipc_live_object_limits_reject_before_allocation_and_reclaim() {
    let mut ipc = IpcTable::with_limits(IpcLimits {
        live_pipes: 1,
        live_sockets: 3,
        buffered_bytes: 64,
        queued_ancillary_refs: 4,
    });

    let pipe = ipc.create_pipe().unwrap();
    assert_eq!(ipc.create_pipe(), Err(IpcError::ResourceLimit));
    assert_eq!(ipc.pipe_count(), 1);
    ipc.drop_pipe_reader(pipe).unwrap();
    ipc.drop_pipe_writer(pipe).unwrap();
    assert_eq!(ipc.pipe_count(), 0);
    assert!(ipc.create_pipe().is_ok());

    let listener = ipc.create_socket(SocketType::Stream).unwrap();
    ipc.bind_socket(listener, "/tmp/admission").unwrap();
    ipc.listen_socket(listener, 1).unwrap();
    let client = ipc.create_socket(SocketType::Stream).unwrap();
    ipc.connect_socket(client, "/tmp/admission").unwrap();
    let unrelated = ipc.create_socket(SocketType::Stream).unwrap();
    assert_eq!(ipc.accept_socket(listener), Err(IpcError::ResourceLimit));
    assert_eq!(ipc.sockets_get(listener).unwrap().backlog.len(), 1);
    ipc.close_socket(unrelated).unwrap();
    assert!(ipc.accept_socket(listener).is_ok());
}

#[test]
fn ipc_aggregate_byte_budget_rejects_atomically_and_reclaims() {
    let mut ipc = IpcTable::with_limits(IpcLimits {
        live_pipes: 2,
        live_sockets: 4,
        buffered_bytes: 4,
        queued_ancillary_refs: 4,
    });
    let first = ipc.create_pipe().unwrap();
    let second = ipc.create_pipe().unwrap();

    assert_eq!(
        ipc.write_pipe(first, b"abcd").unwrap(),
        PipeWriteResult::Wrote(4)
    );
    assert_eq!(ipc.buffered_byte_count(), 4);
    assert_eq!(ipc.write_pipe(second, b"x"), Err(IpcError::WouldBlock));
    assert_eq!(ipc.pipe_mut(second).unwrap().len(), 0);

    let mut read = [0u8; 2];
    assert_eq!(
        ipc.read_pipe(first, &mut read).unwrap(),
        PipeReadResult::Read(2)
    );
    assert_eq!(ipc.buffered_byte_count(), 2);
    assert_eq!(
        ipc.write_pipe(second, b"xy").unwrap(),
        PipeWriteResult::Wrote(2)
    );
    assert_eq!(ipc.buffered_byte_count(), 4);

    ipc.drop_pipe_reader(first).unwrap();
    ipc.drop_pipe_reader(second).unwrap();
    assert_eq!(ipc.buffered_byte_count(), 0);
}

#[test]
fn pipe_write_short_writes_to_global_remaining_budget() {
    let mut ipc = IpcTable::with_limits(IpcLimits {
        live_pipes: 2,
        live_sockets: 1,
        buffered_bytes: 3,
        queued_ancillary_refs: 1,
    });
    let occupied = ipc.create_pipe().unwrap();
    let writer = ipc.create_pipe().unwrap();
    assert_eq!(
        ipc.write_pipe(occupied, b"xx").unwrap(),
        PipeWriteResult::Wrote(2)
    );
    assert_eq!(ipc.buffered_byte_capacity_remaining(), 1);

    assert_eq!(
        ipc.write_pipe(writer, b"yz").unwrap(),
        PipeWriteResult::Wrote(1)
    );
    assert_eq!(ipc.pipe_mut(writer).unwrap().len(), 1);
    assert_eq!(ipc.buffered_byte_count(), 3);
}

#[test]
fn ipc_socket_byte_and_ancillary_budgets_are_atomic_and_reclaimed() {
    let mut ipc = IpcTable::with_limits(IpcLimits {
        live_pipes: 1,
        live_sockets: 3,
        buffered_bytes: 4,
        queued_ancillary_refs: 1,
    });
    let (client, server) = paired(&mut ipc);

    assert_eq!(
        ipc.send_on_socket(client, b"abcd", vec![FdObject::CharDevice(7)])
            .unwrap(),
        4
    );
    assert_eq!(ipc.buffered_byte_count(), 4);
    assert_eq!(ipc.queued_ancillary_count(), 1);
    assert_eq!(
        ipc.send_on_socket(client, b"z", Vec::new()),
        Err(IpcError::WouldBlock)
    );
    assert_eq!(
        ipc.send_on_socket(client, b"", vec![FdObject::CharDevice(8)]),
        Err(IpcError::ResourceLimit)
    );
    assert_eq!(ipc.sockets_get(server).unwrap().rx_len(), 4);
    assert_eq!(ipc.sockets_get(server).unwrap().rx_fd_count(), 1);

    let mut buf = [0u8; 2];
    let (n, fds) = ipc.recv_on_socket(server, &mut buf, 1).unwrap();
    assert_eq!(n, 2);
    assert_eq!(fds, vec![FdObject::CharDevice(7)]);
    assert_eq!(ipc.buffered_byte_count(), 2);
    assert_eq!(ipc.queued_ancillary_count(), 0);

    assert_eq!(
        ipc.send_on_socket(server, b"xy", vec![FdObject::CharDevice(9)])
            .unwrap(),
        2
    );
    assert_eq!(ipc.buffered_byte_count(), 4);
    assert_eq!(ipc.queued_ancillary_count(), 1);
    let cleanup = ipc.close_socket(client).unwrap();
    assert_eq!(cleanup.ancillary, vec![FdObject::CharDevice(9)]);
    assert_eq!(ipc.buffered_byte_count(), 2);
    assert_eq!(ipc.queued_ancillary_count(), 0);
    ipc.shutdown_socket(server, true, false).unwrap();
    assert_eq!(ipc.buffered_byte_count(), 0);
}

#[test]
fn socket_send_short_writes_to_global_remaining_budget() {
    let mut ipc = IpcTable::with_limits(IpcLimits {
        live_pipes: 1,
        live_sockets: 8,
        buffered_bytes: 3,
        queued_ancillary_refs: 1,
    });
    let (first_tx, _first_rx) = paired(&mut ipc);
    let (second_tx, second_rx) = paired(&mut ipc);
    assert_eq!(ipc.send_on_socket(first_tx, b"xx", Vec::new()).unwrap(), 2);
    assert_eq!(ipc.buffered_byte_capacity_remaining(), 1);

    assert_eq!(ipc.send_on_socket(second_tx, b"yz", Vec::new()).unwrap(), 1);
    assert_eq!(ipc.sockets_get(second_rx).unwrap().rx_len(), 1);
    assert_eq!(ipc.buffered_byte_count(), 3);
}

// ---- Helpers -------------------------------------------------------

/// Build a pair of Connected sockets for use in the
/// send/recv tests. Creates a listener at `/tmp/pair`,
/// connects a client, accepts to produce the server, and
/// returns (client, server).
fn paired(ipc: &mut IpcTable) -> (kernel::ipc::SocketId, kernel::ipc::SocketId) {
    let listener = ipc.create_socket(SocketType::Stream).unwrap();
    ipc.bind_socket(listener, "/tmp/pair").unwrap();
    ipc.listen_socket(listener, 4).unwrap();

    let client = ipc.create_socket(SocketType::Stream).unwrap();
    ipc.connect_socket(client, "/tmp/pair").unwrap();
    let server = ipc.accept_socket(listener).unwrap();
    ipc.close_socket(listener).unwrap();
    (client, server)
}

// alloc-free tests live in the ipc submodule unit tests; this
// file is the integration-level harness.
extern crate alloc;
