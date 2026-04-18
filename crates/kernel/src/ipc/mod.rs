//! Inter-process communication primitives.
//!
//! Two kinds of IPC endpoints:
//!
//! * **Pipes** — unidirectional byte streams with a reader side
//!   and a writer side, created in pairs. Matches POSIX `pipe()`
//!   semantics. Lives in [`pipe`].
//! * **Unix-domain-socket-equivalents** — bidirectional stream
//!   or datagram sockets with VFS-path binding, `connect` /
//!   `accept` handshake, and file-descriptor passing via the
//!   `ipc_send` / `ipc_recv` ancillary-data channel. Lives in
//!   [`socket`].
//!
//! The [`IpcTable`] owns every pipe and socket. The kernel's
//! per-process fd table stores opaque `PipeId` / `SocketId`
//! handles; dereferencing them goes through the `IpcTable`.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use abi::ext::Pid;

pub mod pipe;
pub mod socket;

pub use pipe::{Pipe, PipeId, PipeIdAllocator, PipeReadResult, PipeWriteResult, PIPE_BUF_CAP};
pub use socket::{
    Socket, SocketId, SocketIdAllocator, SocketState, SocketType, SOCKET_BUF_CAP,
};

/// Errors returned by [`IpcTable`] operations.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IpcError {
    /// No pipe with this id.
    NoSuchPipe,
    /// No socket with this id.
    NoSuchSocket,
    /// `ipc_bind`: path is already bound by another socket.
    AddressInUse,
    /// `ipc_connect`: no listener at the given path.
    ConnectionRefused,
    /// `ipc_listen`: socket was not in the correct state.
    InvalidState,
    /// Buffer full on write / empty on read with a non-blocking
    /// fd; the caller should convert to `EAGAIN`.
    WouldBlock,
    /// Write attempted on a pipe / stream whose peer has closed.
    /// The kernel converts this into SIGPIPE / EPIPE at syscall
    /// dispatch time.
    PipeBroken,
    /// Attempted to receive with a buffer smaller than the
    /// next datagram (DGRAM sockets only — v1 has none yet).
    MsgTooLarge,
}

/// Kernel-wide IPC state: owns every pipe and socket, plus the
/// path → socket-id bindings that let `ipc_connect` reach a
/// listener by VFS path.
pub struct IpcTable {
    pipes: BTreeMap<PipeId, Pipe>,
    pipe_ids: PipeIdAllocator,
    sockets: BTreeMap<SocketId, Socket>,
    socket_ids: SocketIdAllocator,
    /// Absolute-path → bound socket. Used by `ipc_connect` to
    /// find a listener. For v1 this is a flat map; path
    /// normalisation happens at the VFS layer before the
    /// socket lookup.
    bindings: BTreeMap<String, SocketId>,
}

impl IpcTable {
    pub const fn new() -> Self {
        IpcTable {
            pipes: BTreeMap::new(),
            pipe_ids: PipeIdAllocator::new(),
            sockets: BTreeMap::new(),
            socket_ids: SocketIdAllocator::new(),
            bindings: BTreeMap::new(),
        }
    }

    // --- Pipes ---------------------------------------------------

    /// Create a fresh pipe. The returned id refers to a
    /// [`Pipe`] with one reader reference and one writer
    /// reference, ready to be installed into the creating
    /// process's fd table as (read fd, write fd).
    pub fn create_pipe(&mut self) -> PipeId {
        let id = self.pipe_ids.allocate();
        self.pipes.insert(id, Pipe::new(id));
        id
    }

    /// Borrow a pipe by id.
    pub fn pipe_mut(&mut self, id: PipeId) -> Result<&mut Pipe, IpcError> {
        self.pipes.get_mut(&id).ok_or(IpcError::NoSuchPipe)
    }

    /// Drop one reader reference. If the pipe is fully dead
    /// afterwards it's removed from the table. Returns any
    /// parked-writer pids that should be woken so they observe
    /// the broken-pipe condition.
    pub fn drop_pipe_reader(&mut self, id: PipeId) -> Result<Vec<Pid>, IpcError> {
        let pipe = self.pipes.get_mut(&id).ok_or(IpcError::NoSuchPipe)?;
        let wakes = pipe.drop_reader();
        if pipe.is_dead() {
            self.pipes.remove(&id);
        }
        Ok(wakes)
    }

    /// Drop one writer reference. Returns any parked-reader
    /// pids that should be woken so they observe EOF.
    pub fn drop_pipe_writer(&mut self, id: PipeId) -> Result<Vec<Pid>, IpcError> {
        let pipe = self.pipes.get_mut(&id).ok_or(IpcError::NoSuchPipe)?;
        let wakes = pipe.drop_writer();
        if pipe.is_dead() {
            self.pipes.remove(&id);
        }
        Ok(wakes)
    }

    /// Number of live pipes. Diagnostic.
    pub fn pipe_count(&self) -> usize {
        self.pipes.len()
    }

    // --- Sockets -------------------------------------------------

    /// Create an unbound socket of the given type. Returns the
    /// fresh id.
    pub fn create_socket(&mut self, ty: SocketType) -> SocketId {
        let id = self.socket_ids.allocate();
        self.sockets.insert(id, Socket::new(id, ty));
        id
    }

    /// Borrow a socket by id.
    pub fn socket_mut(&mut self, id: SocketId) -> Result<&mut Socket, IpcError> {
        self.sockets.get_mut(&id).ok_or(IpcError::NoSuchSocket)
    }

    /// Bind a socket to a VFS path. The path is the
    /// already-normalised absolute form (the caller in the
    /// syscall layer will have run it through
    /// `crate::vfs::path::normalize` before calling here).
    pub fn bind_socket(&mut self, id: SocketId, path: &str) -> Result<(), IpcError> {
        if self.bindings.contains_key(path) {
            return Err(IpcError::AddressInUse);
        }
        let sock = self.sockets.get_mut(&id).ok_or(IpcError::NoSuchSocket)?;
        if sock.state != SocketState::Unbound {
            return Err(IpcError::InvalidState);
        }
        sock.bound_path = Some(path.into());
        sock.state = SocketState::Bound;
        self.bindings.insert(path.into(), id);
        Ok(())
    }

    /// Transition a bound socket to `Listening`.
    pub fn listen_socket(&mut self, id: SocketId, backlog: usize) -> Result<(), IpcError> {
        let sock = self.sockets.get_mut(&id).ok_or(IpcError::NoSuchSocket)?;
        if sock.state != SocketState::Bound {
            return Err(IpcError::InvalidState);
        }
        sock.state = SocketState::Listening;
        sock.backlog_cap = backlog.max(1);
        Ok(())
    }

    /// Look up a bound socket by path. Used by `ipc_connect`.
    pub fn lookup_binding(&self, path: &str) -> Option<SocketId> {
        self.bindings.get(path).copied()
    }

    /// Connect `client_id` (Unbound) to the listening socket at
    /// `path`. Queues a connection request on the listener's
    /// backlog and moves `client_id` to `Connecting`. The
    /// accept step (separate call) matches up pending clients.
    pub fn connect_socket(&mut self, client_id: SocketId, path: &str) -> Result<(), IpcError> {
        let listener_id = self
            .bindings
            .get(path)
            .copied()
            .ok_or(IpcError::ConnectionRefused)?;
        // Verify client state without holding a mut borrow
        // across the listener lookup.
        {
            let client = self.sockets.get_mut(&client_id).ok_or(IpcError::NoSuchSocket)?;
            if client.state != SocketState::Unbound {
                return Err(IpcError::InvalidState);
            }
            client.state = SocketState::Connecting;
        }
        let listener = self.sockets.get_mut(&listener_id).ok_or(IpcError::NoSuchSocket)?;
        if listener.state != SocketState::Listening {
            return Err(IpcError::ConnectionRefused);
        }
        if listener.backlog.len() >= listener.backlog_cap {
            // No room in backlog — reset the client and report
            // ECONNREFUSED. Standard POSIX behaviour.
            let client = self.sockets.get_mut(&client_id).unwrap();
            client.state = SocketState::Unbound;
            return Err(IpcError::ConnectionRefused);
        }
        listener.backlog.push_back(client_id);
        Ok(())
    }

    /// Accept one pending connection from a listening socket,
    /// returning the freshly-connected server-side socket id.
    /// Pairs the newly-created server socket with the waiting
    /// client socket from the backlog.
    pub fn accept_socket(&mut self, listener_id: SocketId) -> Result<SocketId, IpcError> {
        // Pop a client from the backlog under a scoped mut
        // borrow so we can then take a fresh mut borrow on
        // the new server socket below.
        let client_id = {
            let listener = self
                .sockets
                .get_mut(&listener_id)
                .ok_or(IpcError::NoSuchSocket)?;
            if listener.state != SocketState::Listening {
                return Err(IpcError::InvalidState);
            }
            match listener.backlog.pop_front() {
                Some(c) => c,
                None => return Err(IpcError::WouldBlock),
            }
        };

        // Allocate the server-side socket for this connection
        // and pair it with the client.
        let server_id = self.socket_ids.allocate();
        let mut server = Socket::new(server_id, SocketType::Stream);
        server.state = SocketState::Connected;
        server.peer = Some(client_id);
        self.sockets.insert(server_id, server);

        // Promote the client from Connecting → Connected and
        // point its peer at the new server.
        if let Some(client) = self.sockets.get_mut(&client_id) {
            client.state = SocketState::Connected;
            client.peer = Some(server_id);
        }

        Ok(server_id)
    }

    /// Send `data` plus optional passed fds from `sender_id`
    /// to its peer. Returns the number of bytes actually
    /// enqueued on the peer's receive side.
    pub fn send_on_socket(
        &mut self,
        sender_id: SocketId,
        data: &[u8],
        passed_fds: Vec<u32>,
    ) -> Result<usize, IpcError> {
        let peer_id = {
            let sender = self
                .sockets
                .get(&sender_id)
                .ok_or(IpcError::NoSuchSocket)?;
            // `Closed` is the state a pending Connecting client
            // transitions to when its listener drops before
            // `accept` (see `close_socket`'s backlog-drain step).
            // Surface `ConnectionRefused` so the caller's errno
            // layer sees ECONNREFUSED — the semantic cue "the
            // listener you tried to reach is gone", distinct
            // from the accept-race `InvalidState` / EINVAL that
            // fires while the listener is still live.
            if sender.state == SocketState::Closed {
                return Err(IpcError::ConnectionRefused);
            }
            if sender.state != SocketState::Connected {
                return Err(IpcError::InvalidState);
            }
            sender.peer.ok_or(IpcError::InvalidState)?
        };
        let peer = self.sockets.get_mut(&peer_id).ok_or(IpcError::NoSuchSocket)?;
        if peer.closed {
            return Err(IpcError::PipeBroken);
        }
        let free = peer.rx_cap.saturating_sub(peer.rx_buf.len());
        if free == 0 && !data.is_empty() {
            return Err(IpcError::WouldBlock);
        }
        let take = core::cmp::min(free, data.len());
        peer.rx_buf.extend(&data[..take]);
        for fd in passed_fds {
            peer.rx_fds.push_back(fd);
        }
        Ok(take)
    }

    /// Receive bytes + any passed fds from a connected socket.
    /// Returns (bytes_read, fds_received).
    ///
    /// Semantics:
    ///
    /// * If either the rx byte buffer OR the rx fd queue has
    ///   content, return whatever is available up to `buf.len()`
    ///   bytes and `max_fds` fds.
    /// * If both are empty AND the peer is still open, return
    ///   [`IpcError::WouldBlock`] — the caller's syscall
    ///   dispatcher turns that into `EAGAIN` for non-blocking
    ///   fds or parks the process otherwise.
    /// * If both are empty AND the peer is closed, return
    ///   `Ok((0, Vec::new()))` — the stream EOF convention.
    pub fn recv_on_socket(
        &mut self,
        sock_id: SocketId,
        buf: &mut [u8],
        max_fds: usize,
    ) -> Result<(usize, Vec<u32>), IpcError> {
        let peer_closed = {
            let sock = self.sockets.get(&sock_id).ok_or(IpcError::NoSuchSocket)?;
            if sock.state != SocketState::Connected {
                return Err(IpcError::InvalidState);
            }
            // A None peer (e.g. the peer was torn down) counts
            // as closed.
            sock.peer
                .map(|pid| {
                    self.sockets
                        .get(&pid)
                        .map(|p| p.closed)
                        .unwrap_or(true)
                })
                .unwrap_or(true)
        };
        let sock = self.sockets.get_mut(&sock_id).ok_or(IpcError::NoSuchSocket)?;
        let have_data = !sock.rx_buf.is_empty();
        let have_fds = !sock.rx_fds.is_empty();
        if !have_data && !have_fds {
            if peer_closed {
                return Ok((0, Vec::new()));
            }
            return Err(IpcError::WouldBlock);
        }
        let take = core::cmp::min(buf.len(), sock.rx_buf.len());
        for (i, b) in sock.rx_buf.drain(..take).enumerate() {
            buf[i] = b;
        }
        let mut out_fds = Vec::new();
        while out_fds.len() < max_fds {
            if let Some(fd) = sock.rx_fds.pop_front() {
                out_fds.push(fd);
            } else {
                break;
            }
        }
        Ok((take, out_fds))
    }

    /// Close a socket. If it was bound, unbinds the path. The
    /// peer pointer is left intact so subsequent `send` and
    /// `recv` on the peer detect the closure via the
    /// peer-closed check (which looks up the pointed-at
    /// socket's `closed` flag).
    ///
    /// v1 leaves the closed socket in the map so any still-
    /// open peer can observe it. A full implementation would
    /// reap it once the peer has also closed.
    ///
    /// If the closing socket is a Listening one with pending
    /// Connecting clients on its backlog, each of those clients
    /// transitions to [`SocketState::Closed`] (leaving `peer =
    /// None`). A subsequent `send_on_socket` on a drained client
    /// returns [`IpcError::ConnectionRefused`] — the semantic cue
    /// "the listener you tried to reach is gone" — instead of the
    /// pre-drain `InvalidState` / EINVAL that conflated the dead-
    /// listener case with the accept-race retry window the display
    /// client walks during a healthy handshake.
    pub fn close_socket(&mut self, id: SocketId) -> Result<(), IpcError> {
        let (_peer, bound, drained) = {
            let sock = self.sockets.get_mut(&id).ok_or(IpcError::NoSuchSocket)?;
            sock.closed = true;
            // Snapshot the pending backlog *only* if this socket
            // was in the Listening role. A non-listening close
            // leaves the backlog alone (the vec is empty anyway
            // for non-listening sockets, but the explicit guard
            // makes the intent unambiguous).
            let drained: Vec<SocketId> = if sock.state == SocketState::Listening {
                sock.backlog.drain(..).collect()
            } else {
                Vec::new()
            };
            (sock.peer, sock.bound_path.clone(), drained)
        };
        if let Some(path) = bound {
            self.bindings.remove(&path);
        }
        for client_id in drained {
            if let Some(client) = self.sockets.get_mut(&client_id) {
                client.state = SocketState::Closed;
            }
        }
        // DO NOT clear the peer's .peer pointer: the peer
        // needs to reach us via that pointer to observe our
        // `closed` flag during its next send/recv.
        Ok(())
    }

    /// Number of live sockets. Diagnostic.
    pub fn socket_count(&self) -> usize {
        self.sockets.len()
    }
}

impl Default for IpcTable {
    fn default() -> Self {
        IpcTable::new()
    }
}
