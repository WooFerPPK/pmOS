//! IPC isolation tests (T064).
//!
//! Runs via `cargo test -p kernel`. Covers the kernel's pipe
//! and unix-socket-equivalent primitives against the
//! IpcTable front-end.

#![cfg(feature = "native-platform")]

use kernel::ipc::{
    IpcError, IpcTable, PipeReadResult, PipeWriteResult, SocketState, SocketType,
    PIPE_BUF_CAP,
};

// ---- Pipes ---------------------------------------------------------

#[test]
fn pipe_round_trip_single_write_and_read() {
    let mut ipc = IpcTable::new();
    let pid = ipc.create_pipe();

    let pipe = ipc.pipe_mut(pid).unwrap();
    assert_eq!(pipe.try_write(b"hello"), PipeWriteResult::Wrote(5));
    let mut buf = [0u8; 8];
    assert_eq!(pipe.try_read(&mut buf), PipeReadResult::Read(5));
    assert_eq!(&buf[..5], b"hello");
}

#[test]
fn pipe_read_empty_returns_would_block_while_writer_open() {
    let mut ipc = IpcTable::new();
    let pid = ipc.create_pipe();
    let pipe = ipc.pipe_mut(pid).unwrap();
    let mut buf = [0u8; 4];
    assert_eq!(pipe.try_read(&mut buf), PipeReadResult::WouldBlock);
}

#[test]
fn pipe_write_to_closed_reader_is_broken() {
    let mut ipc = IpcTable::new();
    let pid = ipc.create_pipe();
    // Drop the reader reference.
    let wakes = ipc.drop_pipe_reader(pid).unwrap();
    assert!(wakes.is_empty()); // nobody was parked

    let pipe = ipc.pipe_mut(pid).unwrap();
    assert_eq!(pipe.try_write(b"hi"), PipeWriteResult::Broken);
}

#[test]
fn pipe_read_after_writer_closes_returns_eof_after_draining() {
    let mut ipc = IpcTable::new();
    let pid = ipc.create_pipe();

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
    let pid = ipc.create_pipe();
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
    let pid = ipc.create_pipe();
    assert_eq!(ipc.pipe_count(), 1);

    ipc.drop_pipe_writer(pid).unwrap();
    // Still alive — the buffer is empty but reader is still open.
    assert_eq!(ipc.pipe_count(), 1);

    ipc.drop_pipe_reader(pid).unwrap();
    // Both ends closed, buffer empty → reaped.
    assert_eq!(ipc.pipe_count(), 0);
}

#[test]
fn pipe_refcounts_allow_multiple_readers_and_writers() {
    let mut ipc = IpcTable::new();
    let pid = ipc.create_pipe();

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
    let pid = ipc.create_pipe();
    let pipe = ipc.pipe_mut(pid).unwrap();
    pipe.park_reader(42);
    pipe.park_reader(77);

    let wakes = ipc.drop_pipe_writer(pid).unwrap();
    assert_eq!(wakes, vec![42, 77]);
}

#[test]
fn pipe_drop_reader_wakes_parked_writers() {
    let mut ipc = IpcTable::new();
    let pid = ipc.create_pipe();
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
    let id = ipc.create_socket(SocketType::Stream);
    let s = ipc.socket_mut(id).unwrap();
    assert_eq!(s.state, SocketState::Unbound);
    assert_eq!(s.ty, SocketType::Stream);
    assert!(s.bound_path.is_none());
    assert!(s.peer.is_none());
}

#[test]
fn socket_bind_then_listen_transitions_state() {
    let mut ipc = IpcTable::new();
    let id = ipc.create_socket(SocketType::Stream);

    ipc.bind_socket(id, "/run/display").unwrap();
    assert_eq!(ipc.socket_mut(id).unwrap().state, SocketState::Bound);

    ipc.listen_socket(id, 16).unwrap();
    assert_eq!(ipc.socket_mut(id).unwrap().state, SocketState::Listening);
}

#[test]
fn socket_bind_duplicate_path_is_address_in_use() {
    let mut ipc = IpcTable::new();
    let a = ipc.create_socket(SocketType::Stream);
    let b = ipc.create_socket(SocketType::Stream);

    ipc.bind_socket(a, "/run/svc").unwrap();
    let err = ipc.bind_socket(b, "/run/svc").unwrap_err();
    assert_eq!(err, IpcError::AddressInUse);
}

#[test]
fn socket_listen_without_bind_is_invalid_state() {
    let mut ipc = IpcTable::new();
    let id = ipc.create_socket(SocketType::Stream);
    let err = ipc.listen_socket(id, 4).unwrap_err();
    assert_eq!(err, IpcError::InvalidState);
}

#[test]
fn socket_connect_without_listener_refused() {
    let mut ipc = IpcTable::new();
    let client = ipc.create_socket(SocketType::Stream);
    let err = ipc.connect_socket(client, "/run/ghost").unwrap_err();
    assert_eq!(err, IpcError::ConnectionRefused);
}

#[test]
fn socket_connect_accept_pairs_client_and_server() {
    let mut ipc = IpcTable::new();
    let listener = ipc.create_socket(SocketType::Stream);
    ipc.bind_socket(listener, "/run/svc").unwrap();
    ipc.listen_socket(listener, 4).unwrap();

    let client = ipc.create_socket(SocketType::Stream);
    ipc.connect_socket(client, "/run/svc").unwrap();
    assert_eq!(ipc.socket_mut(client).unwrap().state, SocketState::Connecting);

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
    let listener = ipc.create_socket(SocketType::Stream);
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

    let passed = alloc::vec![100u32, 200, 300];
    let n = ipc.send_on_socket(client, b"fd test", passed).unwrap();
    assert_eq!(n, 7);

    let mut buf = [0u8; 16];
    let (n, fds) = ipc.recv_on_socket(server, &mut buf, 4).unwrap();
    assert_eq!(n, 7);
    assert_eq!(&buf[..7], b"fd test");
    assert_eq!(fds, vec![100, 200, 300]);
}

#[test]
fn socket_recv_fd_limit_is_respected() {
    let mut ipc = IpcTable::new();
    let (client, server) = paired(&mut ipc);

    let passed = alloc::vec![1u32, 2, 3, 4, 5];
    ipc.send_on_socket(client, b"x", passed).unwrap();

    // First recv asks for max 2 fds.
    let mut buf = [0u8; 4];
    let (_, fds) = ipc.recv_on_socket(server, &mut buf, 2).unwrap();
    assert_eq!(fds, vec![1, 2]);

    // Second recv asks for max 10 fds → gets the rest.
    let mut buf = [0u8; 4];
    let (_, fds) = ipc.recv_on_socket(server, &mut buf, 10).unwrap();
    assert_eq!(fds, vec![3, 4, 5]);
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
    let listener = ipc.create_socket(SocketType::Stream);
    ipc.bind_socket(listener, "/run/svc").unwrap();
    ipc.listen_socket(listener, 4).unwrap();

    // Second bind to the same path fails while the first is alive.
    let other = ipc.create_socket(SocketType::Stream);
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
    let listener = ipc.create_socket(SocketType::Stream);
    ipc.bind_socket(listener, "/run/svc").unwrap();
    ipc.listen_socket(listener, 4).unwrap();

    let client = ipc.create_socket(SocketType::Stream);
    ipc.connect_socket(client, "/run/svc").unwrap();
    // Pre-drop, the client is Connecting (queued on the backlog).
    assert_eq!(ipc.socket_mut(client).unwrap().state, SocketState::Connecting);

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
    let listener = ipc.create_socket(SocketType::Stream);
    ipc.bind_socket(listener, "/run/svc").unwrap();
    ipc.listen_socket(listener, 8).unwrap();

    let clients: alloc::vec::Vec<_> = (0..3)
        .map(|_| {
            let c = ipc.create_socket(SocketType::Stream);
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
    let listener = ipc.create_socket(SocketType::Stream);
    ipc.bind_socket(listener, "/run/svc").unwrap();
    ipc.listen_socket(listener, 4).unwrap();

    let client = ipc.create_socket(SocketType::Stream);
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
fn socket_send_on_connecting_client_with_live_listener_still_invalid_state() {
    // Invariant: the accept-race case (listener alive, client
    // queued but not accepted yet) keeps returning InvalidState.
    // display-client-demo's EINVAL retry loop depends on this
    // split — only the listener-drop case should surface the new
    // ConnectionRefused.
    let mut ipc = IpcTable::new();
    let listener = ipc.create_socket(SocketType::Stream);
    ipc.bind_socket(listener, "/run/svc").unwrap();
    ipc.listen_socket(listener, 4).unwrap();

    let client = ipc.create_socket(SocketType::Stream);
    ipc.connect_socket(client, "/run/svc").unwrap();
    // No close — the listener is still live.

    let err = ipc
        .send_on_socket(client, b"hi", alloc::vec::Vec::new())
        .unwrap_err();
    assert_eq!(err, IpcError::InvalidState);
}

#[test]
fn socket_close_listener_with_empty_backlog_is_unchanged() {
    let mut ipc = IpcTable::new();
    let listener = ipc.create_socket(SocketType::Stream);
    ipc.bind_socket(listener, "/run/svc").unwrap();
    ipc.listen_socket(listener, 4).unwrap();

    // No pending clients; close should behave exactly as before:
    // binding released, listener flagged closed.
    ipc.close_socket(listener).unwrap();
    let other = ipc.create_socket(SocketType::Stream);
    ipc.bind_socket(other, "/run/svc").unwrap();
}

// ---- Helpers -------------------------------------------------------

/// Build a pair of Connected sockets for use in the
/// send/recv tests. Creates a listener at `/tmp/pair`,
/// connects a client, accepts to produce the server, and
/// returns (client, server).
fn paired(ipc: &mut IpcTable) -> (kernel::ipc::SocketId, kernel::ipc::SocketId) {
    let listener = ipc.create_socket(SocketType::Stream);
    ipc.bind_socket(listener, "/tmp/pair").unwrap();
    ipc.listen_socket(listener, 4).unwrap();

    let client = ipc.create_socket(SocketType::Stream);
    ipc.connect_socket(client, "/tmp/pair").unwrap();
    let server = ipc.accept_socket(listener).unwrap();
    (client, server)
}

// alloc-free tests live in the ipc submodule unit tests; this
// file is the integration-level harness.
extern crate alloc;
