//! Inter-process communication primitives.
//!
//! Two kinds of IPC endpoints:
//!
//! * **Pipes** — unidirectional byte streams with a reader side
//!   and a writer side, created in pairs. Matches POSIX `pipe()`
//!   semantics. Lives in [`pipe`].
//! * **Unix-domain-socket-equivalents** — bidirectional stream
//!   sockets with VFS-path binding, `connect` /
//!   `accept` handshake, and file-descriptor passing via the
//!   `ipc_send` / `ipc_recv` ancillary-data channel. The datagram
//!   type discriminant is ABI-reserved and rejected in v1. Lives in [`socket`].
//!
//! The [`IpcTable`] owns every pipe and socket. The kernel's
//! per-process fd table stores opaque `PipeId` / `SocketId`
//! handles; dereferencing them goes through the `IpcTable`.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use abi::ext::Pid;

use crate::fd::FdObject;

pub mod pipe;
pub mod socket;

pub use pipe::{Pipe, PipeId, PipeIdAllocator, PipeReadResult, PipeWriteResult, PIPE_BUF_CAP};
pub use socket::{
    Socket, SocketCredentials, SocketId, SocketIdAllocator, SocketState, SocketType,
    SOCKET_BUF_CAP, SOCKET_FD_QUEUE_CAP,
};

/// Default kernel-wide admission limit for live pipe objects.
pub const MAX_LIVE_PIPES: usize = 1_024;
/// Default kernel-wide admission limit for live socket objects.
pub const MAX_LIVE_SOCKETS: usize = 2_048;
/// Default aggregate byte budget shared by pipe and socket receive buffers.
pub const MAX_BUFFERED_BYTES: usize = 32 * 1024 * 1024;
/// Default aggregate budget for queued ancillary descriptor references.
pub const MAX_QUEUED_ANCILLARY_REFS: usize = 1_024;

/// Kernel-wide IPC admission limits. Production uses [`Default`]; the
/// constructor accepting explicit values exists for deterministic boundary
/// and reclamation tests.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct IpcLimits {
    pub live_pipes: usize,
    pub live_sockets: usize,
    pub buffered_bytes: usize,
    pub queued_ancillary_refs: usize,
}

impl Default for IpcLimits {
    fn default() -> Self {
        Self {
            live_pipes: MAX_LIVE_PIPES,
            live_sockets: MAX_LIVE_SOCKETS,
            buffered_bytes: MAX_BUFFERED_BYTES,
            queued_ancillary_refs: MAX_QUEUED_ANCILLARY_REFS,
        }
    }
}

/// Resources removed from a socket receive queue during close. The kernel
/// releases each descriptor snapshot after the table borrow ends.
#[derive(Debug, PartialEq, Eq)]
pub struct SocketClose {
    pub parked_acceptor: Option<(Pid, u32)>,
    pub ancillary: Vec<FdObject>,
}

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
    /// The requested socket type has a reserved wire discriminant but no
    /// truthful kernel implementation in v1.
    UnsupportedSocketType,
    /// A kernel-wide live-object or queued-data admission budget is full.
    ResourceLimit,
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
    limits: IpcLimits,
    buffered_bytes: usize,
    queued_ancillary_refs: usize,
}

impl IpcTable {
    pub fn new() -> Self {
        Self::with_limits(IpcLimits::default())
    }

    pub const fn with_limits(limits: IpcLimits) -> Self {
        IpcTable {
            pipes: BTreeMap::new(),
            pipe_ids: PipeIdAllocator::new(),
            sockets: BTreeMap::new(),
            socket_ids: SocketIdAllocator::new(),
            bindings: BTreeMap::new(),
            limits,
            buffered_bytes: 0,
            queued_ancillary_refs: 0,
        }
    }

    // --- Pipes ---------------------------------------------------

    /// Create a fresh pipe. The returned id refers to a
    /// [`Pipe`] with one reader reference and one writer
    /// reference, ready to be installed into the creating
    /// process's fd table as (read fd, write fd).
    pub fn create_pipe(&mut self) -> Result<PipeId, IpcError> {
        if self.pipes.len() >= self.limits.live_pipes {
            return Err(IpcError::ResourceLimit);
        }
        let id = self.pipe_ids.allocate();
        self.pipes.insert(id, Pipe::new(id));
        Ok(id)
    }

    /// Borrow a pipe by id.
    pub fn pipe_mut(&mut self, id: PipeId) -> Result<&mut Pipe, IpcError> {
        self.pipes.get_mut(&id).ok_or(IpcError::NoSuchPipe)
    }

    /// Read through a pipe while maintaining the aggregate byte budget.
    pub fn read_pipe(&mut self, id: PipeId, buf: &mut [u8]) -> Result<PipeReadResult, IpcError> {
        let pipe = self.pipes.get_mut(&id).ok_or(IpcError::NoSuchPipe)?;
        let before = pipe.len();
        let result = pipe.try_read(buf);
        let reclaimed = before.saturating_sub(pipe.len());
        self.buffered_bytes = self.buffered_bytes.saturating_sub(reclaimed);
        Ok(result)
    }

    /// Write through a pipe with atomic admission against the aggregate byte
    /// budget. A rejected write changes neither the pipe nor the counter.
    pub fn write_pipe(&mut self, id: PipeId, buf: &[u8]) -> Result<PipeWriteResult, IpcError> {
        let pipe = self.pipes.get(&id).ok_or(IpcError::NoSuchPipe)?;
        if pipe.reader_closed() {
            return Ok(PipeWriteResult::Broken);
        }
        if buf.is_empty() {
            return Ok(PipeWriteResult::Wrote(0));
        }
        let local_take = core::cmp::min(pipe.capacity().saturating_sub(pipe.len()), buf.len());
        if local_take == 0 {
            return Ok(PipeWriteResult::WouldBlock);
        }
        let global_free = self
            .limits
            .buffered_bytes
            .saturating_sub(self.buffered_bytes);
        let take = core::cmp::min(local_take, global_free);
        if take == 0 {
            return Err(IpcError::WouldBlock);
        }
        let result = self
            .pipes
            .get_mut(&id)
            .ok_or(IpcError::NoSuchPipe)?
            .try_write(&buf[..take]);
        if let PipeWriteResult::Wrote(written) = result {
            self.buffered_bytes = self.buffered_bytes.saturating_add(written);
        }
        Ok(result)
    }

    /// Drop one reader reference. If the pipe is fully dead
    /// afterwards it's removed from the table. Returns any
    /// parked-writer pids that should be woken so they observe
    /// the broken-pipe condition.
    pub fn drop_pipe_reader(&mut self, id: PipeId) -> Result<Vec<Pid>, IpcError> {
        let pipe = self.pipes.get_mut(&id).ok_or(IpcError::NoSuchPipe)?;
        let before = pipe.len();
        let wakes = pipe.drop_reader();
        let reclaimed = before.saturating_sub(pipe.len());
        let dead = pipe.is_dead();
        self.buffered_bytes = self.buffered_bytes.saturating_sub(reclaimed);
        if dead {
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
    pub fn create_socket(&mut self, ty: SocketType) -> Result<SocketId, IpcError> {
        if ty != SocketType::Stream {
            return Err(IpcError::UnsupportedSocketType);
        }
        if self.sockets.len() >= self.limits.live_sockets {
            return Err(IpcError::ResourceLimit);
        }
        let id = self.socket_ids.allocate();
        self.sockets.insert(id, Socket::new(id, ty));
        Ok(id)
    }

    /// Borrow a socket by id.
    pub fn socket_mut(&mut self, id: SocketId) -> Result<&mut Socket, IpcError> {
        self.sockets.get_mut(&id).ok_or(IpcError::NoSuchSocket)
    }

    /// Attach the kernel-derived identity snapshot for a socket
    /// endpoint. Kernel syscall glue calls this at connect or accept;
    /// userland has no path to set credentials directly.
    pub fn set_socket_credentials(
        &mut self,
        id: SocketId,
        credentials: SocketCredentials,
    ) -> Result<(), IpcError> {
        let socket = self.sockets.get_mut(&id).ok_or(IpcError::NoSuchSocket)?;
        socket.credentials = Some(credentials);
        Ok(())
    }

    /// Return the authenticated identity snapshot attached to the
    /// opposite endpoint of a connected socket.
    pub fn peer_credentials(&self, id: SocketId) -> Result<SocketCredentials, IpcError> {
        let socket = self.sockets.get(&id).ok_or(IpcError::NoSuchSocket)?;
        if socket.state != SocketState::Connected {
            return Err(IpcError::InvalidState);
        }
        let peer_id = socket.peer.ok_or(IpcError::InvalidState)?;
        let peer = self.sockets.get(&peer_id).ok_or(IpcError::NoSuchSocket)?;
        peer.credentials.ok_or(IpcError::InvalidState)
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
    ///
    /// If the listener has a parked acceptor at the time of
    /// connect, the parker tuple `(pid, request_id, listener_id)`
    /// is drained out of the socket and returned so the caller
    /// (Kernel) can complete the parked accept inline and queue
    /// a wake response. Otherwise returns `Ok(None)`.
    pub fn connect_socket(
        &mut self,
        client_id: SocketId,
        path: &str,
    ) -> Result<Option<(Pid, u32, SocketId)>, IpcError> {
        let listener_id = self
            .bindings
            .get(path)
            .copied()
            .ok_or(IpcError::ConnectionRefused)?;
        let client = self.sockets.get(&client_id).ok_or(IpcError::NoSuchSocket)?;
        if client.state != SocketState::Unbound {
            return Err(IpcError::InvalidState);
        }
        let listener = self
            .sockets
            .get(&listener_id)
            .ok_or(IpcError::NoSuchSocket)?;
        if listener.state != SocketState::Listening
            || listener.backlog.len() >= listener.backlog_cap
        {
            return Err(IpcError::ConnectionRefused);
        }
        self.sockets
            .get_mut(&client_id)
            .ok_or(IpcError::NoSuchSocket)?
            .state = SocketState::Connecting;
        let listener = self
            .sockets
            .get_mut(&listener_id)
            .ok_or(IpcError::NoSuchSocket)?;
        listener.backlog.push_back(client_id);
        let parker = listener.parked_acceptor.take();
        Ok(parker.map(|(pid, req_id)| (pid, req_id, listener_id)))
    }

    /// Accept one pending connection from a listening socket,
    /// returning the freshly-connected server-side socket id.
    /// Pairs the newly-created server socket with the waiting
    /// client socket from the backlog.
    pub fn accept_socket(&mut self, listener_id: SocketId) -> Result<SocketId, IpcError> {
        // Validate state and pending work before applying the allocation
        // budget. A full global table must not turn an empty-backlog EAGAIN or
        // an invalid-listener EINVAL into a resource-limit error.
        {
            let listener = self
                .sockets
                .get(&listener_id)
                .ok_or(IpcError::NoSuchSocket)?;
            if listener.state != SocketState::Listening {
                return Err(IpcError::InvalidState);
            }
            if listener.backlog.is_empty() {
                return Err(IpcError::WouldBlock);
            }
        }
        // Check before removing a client from the backlog so admission failure
        // leaves the pending connection intact and retryable.
        if self.sockets.len() >= self.limits.live_sockets {
            return Err(IpcError::ResourceLimit);
        }
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
            listener.backlog.pop_front().ok_or(IpcError::WouldBlock)?
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
        passed_fds: Vec<FdObject>,
    ) -> Result<usize, IpcError> {
        let peer_id = {
            let sender = self.sockets.get(&sender_id).ok_or(IpcError::NoSuchSocket)?;
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
            if sender.state == SocketState::Connecting {
                return Err(IpcError::WouldBlock);
            }
            if sender.state != SocketState::Connected {
                return Err(IpcError::InvalidState);
            }
            // Write-side half-close: the sender has promised not to
            // write anymore. Treat subsequent send as a broken
            // stream — callers should have stopped on the first
            // shutdown.
            if sender.shutdown_write {
                return Err(IpcError::PipeBroken);
            }
            sender.peer.ok_or(IpcError::PipeBroken)?
        };
        let peer = self.sockets.get(&peer_id).ok_or(IpcError::NoSuchSocket)?;
        if peer.closed {
            return Err(IpcError::PipeBroken);
        }
        // Read-side half-close on the peer: the peer has refused
        // further reads, so sending is pointless — EPIPE is the
        // POSIX-correct signal.
        if peer.shutdown_read {
            return Err(IpcError::PipeBroken);
        }
        if passed_fds.len() > SOCKET_FD_QUEUE_CAP.saturating_sub(peer.rx_fds.len()) {
            return Err(IpcError::ResourceLimit);
        }
        let free = peer.rx_cap.saturating_sub(peer.rx_buf.len());
        if free == 0 && !data.is_empty() {
            return Err(IpcError::WouldBlock);
        }
        let local_take = core::cmp::min(free, data.len());
        let global_free = self
            .limits
            .buffered_bytes
            .saturating_sub(self.buffered_bytes);
        let take = core::cmp::min(local_take, global_free);
        if !data.is_empty() && take == 0 {
            return Err(IpcError::WouldBlock);
        }
        let fd_count = passed_fds.len();
        if fd_count
            > self
                .limits
                .queued_ancillary_refs
                .saturating_sub(self.queued_ancillary_refs)
        {
            return Err(IpcError::ResourceLimit);
        }
        let peer = self
            .sockets
            .get_mut(&peer_id)
            .ok_or(IpcError::NoSuchSocket)?;
        peer.rx_buf.extend(&data[..take]);
        for fd in passed_fds {
            peer.rx_fds.push_back(fd);
        }
        self.buffered_bytes += take;
        self.queued_ancillary_refs += fd_count;
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
    ) -> Result<(usize, Vec<FdObject>), IpcError> {
        // Read-side half-close: the caller has promised not to read
        // anymore on this side; future recvs unconditionally EOF
        // (matches POSIX shutdown(SHUT_RD) — peek state is
        // discarded). This short-circuit fires independent of peer
        // state; even a fully-populated rx_buf yields EOF.
        {
            let sock = self.sockets.get(&sock_id).ok_or(IpcError::NoSuchSocket)?;
            if sock.state == SocketState::Closed {
                return Err(IpcError::ConnectionRefused);
            }
            if sock.state == SocketState::Connecting {
                return Err(IpcError::WouldBlock);
            }
            if sock.state != SocketState::Connected {
                return Err(IpcError::InvalidState);
            }
            if sock.shutdown_read {
                return Ok((0, Vec::new()));
            }
        }
        let peer_closed = {
            let sock = self.sockets.get(&sock_id).ok_or(IpcError::NoSuchSocket)?;
            // A None peer (e.g. the peer was torn down) counts
            // as closed. A peer with `shutdown_write = true` has
            // promised no more bytes will arrive, so it counts as
            // closed for recv's EOF check even though its fd is
            // still open.
            sock.peer
                .map(|pid| {
                    self.sockets
                        .get(&pid)
                        .map(|p| p.closed || p.shutdown_write)
                        .unwrap_or(true)
                })
                .unwrap_or(true)
        };
        let sock = self.sockets.get(&sock_id).ok_or(IpcError::NoSuchSocket)?;
        let have_data = !sock.rx_buf.is_empty();
        let have_fds = !sock.rx_fds.is_empty();
        if !have_data && !have_fds {
            if peer_closed {
                return Ok((0, Vec::new()));
            }
            return Err(IpcError::WouldBlock);
        }
        let (take, out_fds) = {
            let sock = self
                .sockets
                .get_mut(&sock_id)
                .ok_or(IpcError::NoSuchSocket)?;
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
            (take, out_fds)
        };
        self.buffered_bytes = self.buffered_bytes.saturating_sub(take);
        self.queued_ancillary_refs = self.queued_ancillary_refs.saturating_sub(out_fds.len());
        Ok((take, out_fds))
    }

    /// Half-close one or both directions of a connected socket. The
    /// fd stays open (the caller can still call [`close_socket`] to
    /// fully tear down); only the per-direction shutdown flags
    /// change.
    ///
    /// * `read = true` sets `shutdown_read`, so subsequent
    ///   `recv_on_socket` short-circuits to `(0, Vec::new())` EOF
    ///   regardless of rx buffer state, and the peer's
    ///   `send_on_socket` returns `PipeBroken` since there's no
    ///   reader left.
    /// * `write = true` sets `shutdown_write`, so subsequent
    ///   `send_on_socket` returns `PipeBroken`, and the peer's
    ///   `recv_on_socket` observes EOF once its rx buffer drains.
    ///
    /// Unconditionally succeeds on a valid socket (idempotent — the
    /// flags set is a monotonic OR, so repeated calls keep the same
    /// endpoints shut). Returns `NoSuchSocket` on an unknown id.
    pub fn shutdown_socket(
        &mut self,
        id: SocketId,
        read: bool,
        write: bool,
    ) -> Result<Vec<FdObject>, IpcError> {
        let sock = self.sockets.get_mut(&id).ok_or(IpcError::NoSuchSocket)?;
        let mut ancillary = Vec::new();
        if read {
            sock.shutdown_read = true;
            self.buffered_bytes = self.buffered_bytes.saturating_sub(sock.rx_buf.len());
            self.queued_ancillary_refs =
                self.queued_ancillary_refs.saturating_sub(sock.rx_fds.len());
            sock.rx_buf.clear();
            ancillary.extend(sock.rx_fds.drain(..));
        }
        if write {
            sock.shutdown_write = true;
        }
        Ok(ancillary)
    }

    /// Close and immediately remove a socket object, dropping its unread byte
    /// and ancillary queues. A connected peer keeps its own already-received
    /// bytes and gets `peer = None`, which means EOF after that buffer drains
    /// and `PipeBroken` on later sends. Listener close also unbinds its path
    /// and marks queued connecting clients `Closed`/`ConnectionRefused`.
    pub fn close_socket(&mut self, id: SocketId) -> Result<SocketClose, IpcError> {
        let mut sock = self.sockets.remove(&id).ok_or(IpcError::NoSuchSocket)?;
        self.buffered_bytes = self.buffered_bytes.saturating_sub(sock.rx_buf.len());
        self.queued_ancillary_refs = self.queued_ancillary_refs.saturating_sub(sock.rx_fds.len());
        let ancillary = sock.rx_fds.drain(..).collect();
        sock.closed = true;
        let peer = sock.peer;
        let bound = sock.bound_path.take();
        let drained: Vec<SocketId> = if sock.state == SocketState::Listening {
            sock.backlog.drain(..).collect()
        } else {
            Vec::new()
        };
        let parker = sock.parked_acceptor.take();
        if let Some(path) = bound {
            self.bindings.remove(&path);
        }
        if let Some(peer_id) = peer {
            if let Some(peer) = self.sockets.get_mut(&peer_id) {
                // A missing peer denotes a closed stream endpoint. Buffered
                // bytes already queued on this surviving side remain readable
                // and are followed by EOF; sends return PipeBroken.
                peer.peer = None;
            }
        }
        // A connecting client can close before the listener accepts it. Remove
        // the stale id from every bounded backlog so accept cannot mint an
        // orphan server endpoint for a socket that no longer exists.
        for candidate in self.sockets.values_mut() {
            if candidate.state == SocketState::Listening {
                candidate.backlog.retain(|client_id| *client_id != id);
            }
        }
        for client_id in drained {
            if let Some(client) = self.sockets.get_mut(&client_id) {
                client.state = SocketState::Closed;
            }
        }
        Ok(SocketClose {
            parked_acceptor: parker,
            ancillary,
        })
    }

    /// Number of live sockets. Diagnostic.
    pub fn socket_count(&self) -> usize {
        self.sockets.len()
    }

    /// Whether `additional` socket objects can be admitted without crossing
    /// the live-object ceiling. Kernel-owned synchronous endpoints use this
    /// to preflight a multi-step connect/accept transaction before either
    /// endpoint's state changes.
    pub fn has_socket_capacity(&self, additional: usize) -> bool {
        additional <= self.limits.live_sockets.saturating_sub(self.sockets.len())
    }

    /// Aggregate queued pipe and socket bytes.
    pub fn buffered_byte_count(&self) -> usize {
        self.buffered_bytes
    }

    /// Remaining bytes in the aggregate pipe/socket receive-buffer budget.
    /// Poll readiness uses the same admission limit as the eventual write so
    /// FD_WRITE never promises progress that `write_pipe`/`send_on_socket`
    /// must reject globally.
    pub fn buffered_byte_capacity_remaining(&self) -> usize {
        self.limits
            .buffered_bytes
            .saturating_sub(self.buffered_bytes)
    }

    /// Aggregate queued ancillary descriptor references.
    pub fn queued_ancillary_count(&self) -> usize {
        self.queued_ancillary_refs
    }

    /// Whether this socket currently has an ancillary descriptor queued.
    pub fn socket_has_ancillary(&self, id: SocketId) -> Result<bool, IpcError> {
        Ok(!self
            .sockets
            .get(&id)
            .ok_or(IpcError::NoSuchSocket)?
            .rx_fds
            .is_empty())
    }

    /// Look up a `Socket` by id. Used by tests that need to inspect
    /// per-socket state (e.g. `parked_acceptor`).
    #[doc(hidden)]
    pub fn sockets_get(&self, id: SocketId) -> Option<&crate::ipc::socket::Socket> {
        self.sockets.get(&id)
    }

    /// Mutable variant of [`sockets_get`]. Used by `Kernel::park_on_
    /// accept` and close-time wake drain to flip the `parked_acceptor`
    /// slot without borrowing the rest of the IpcTable.
    #[doc(hidden)]
    pub fn sockets_get_mut(&mut self, id: SocketId) -> Option<&mut crate::ipc::socket::Socket> {
        self.sockets.get_mut(&id)
    }

    /// Walk every live socket and clear any `parked_acceptor` slot
    /// whose pid matches. Called from `Kernel::proc_exit` when a
    /// parked process dies.
    pub fn clear_parked_acceptor_for_pid(&mut self, pid: Pid) {
        for sock in self.sockets.values_mut() {
            if let Some((parker_pid, _)) = sock.parked_acceptor {
                if parker_pid == pid {
                    sock.parked_acceptor = None;
                }
            }
        }
    }

    /// Walk every live socket, find one whose `parked_acceptor` pid
    /// matches, clear the slot, and return the parker's `req_id`.
    /// Returns `None` if no parked acceptor on any listener matches
    /// `pid`. Mirrors [`clear_parked_acceptor_for_pid`] but
    /// surfaces the `req_id` so the caller can build the EINTR
    /// wake Response.
    ///
    /// v1's one-parker-per-listener invariant means there is at
    /// most one match across the socket table — the walk stops on
    /// the first hit. Called from `Kernel::interrupt_parked_accept`
    /// during `proc_kill(SIGTERM)` handling against a BlockedOnIpc
    /// target.
    pub fn take_parked_acceptor_for_pid(&mut self, pid: Pid) -> Option<u32> {
        for sock in self.sockets.values_mut() {
            if let Some((parker_pid, req_id)) = sock.parked_acceptor {
                if parker_pid == pid {
                    sock.parked_acceptor = None;
                    return Some(req_id);
                }
            }
        }
        None
    }

    /// Iterate all live socket ids. Used by test helpers that need
    /// to reconcile the IpcTable against an externally-observed
    /// fd-table state (notably `Kernel::drain_closed_object_side_
    /// effects`).
    #[doc(hidden)]
    pub fn sockets_iter_ids(&self) -> impl Iterator<Item = SocketId> + '_ {
        self.sockets.keys().copied()
    }
}

impl Default for IpcTable {
    fn default() -> Self {
        IpcTable::new()
    }
}
