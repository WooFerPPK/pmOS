//! Unix-domain-socket-equivalents.
//!
//! PMos's userland processes communicate via sockets bound to
//! VFS paths (typically under `/run/…`). The protocol is
//! stream-oriented in v1 — datagrams are reserved for a later
//! pass. The socket state machine is:
//!
//! ```text
//! Unbound   --bind(path)--> Bound
//! Bound     --listen(n)---> Listening
//! Listening --accept()----> Listening  (and a new Connected socket
//!                                       is minted for the accepted
//!                                       connection)
//! Unbound   --connect(p)--> Connecting (the server accept() then
//!                                       transitions this to
//!                                       Connected)
//! Connected --close()-----> Closed
//! ```
//!
//! The [`IpcTable`](super::IpcTable) in the parent module owns
//! every [`Socket`] and runs the state machine. This file just
//! defines the data structures.

use alloc::collections::VecDeque;
use alloc::string::String;

/// Per-socket identifier handed out by the kernel's IPC table.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SocketId(pub u32);

/// Default per-socket receive buffer capacity in bytes.
pub const SOCKET_BUF_CAP: usize = 64 * 1024;

/// Socket type (corresponds to the `type` argument of POSIX
/// `socket()`).
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SocketType {
    /// Bidirectional, reliable, byte-stream. v1 only supports
    /// this variant.
    Stream = 0,
    /// Datagram. Reserved for v2.
    Dgram = 1,
}

/// Socket lifecycle state.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SocketState {
    Unbound,
    Bound,
    Listening,
    Connecting,
    Connected,
    Closed,
}

/// A unix-domain-socket-equivalent.
pub struct Socket {
    pub id: SocketId,
    pub ty: SocketType,
    pub state: SocketState,
    /// The VFS path this socket is bound to, if any.
    pub bound_path: Option<String>,
    /// Maximum backlog length for a listening socket.
    pub backlog_cap: usize,
    /// For listening sockets: pending connect requests, each
    /// identified by the client socket's id. `accept()` pops
    /// from the front.
    pub backlog: VecDeque<SocketId>,
    /// Peer socket id, set once this socket is paired.
    pub peer: Option<SocketId>,
    /// Received bytes waiting for the process to `recv`.
    pub rx_buf: VecDeque<u8>,
    pub rx_cap: usize,
    /// Received file descriptors waiting to be consumed by
    /// `ipc_recv`.
    pub rx_fds: VecDeque<u32>,
    /// True once [`IpcTable::close_socket`](super::IpcTable::close_socket)
    /// has been called on this socket. A closed socket's peer
    /// observes EOF on the next `recv`.
    pub closed: bool,
    /// True once [`IpcTable::shutdown_socket`](super::IpcTable::shutdown_socket)
    /// has been called with `read = true`. A read-shut socket's own
    /// `recv` short-circuits to `(0, Vec::new())` EOF (independent
    /// of peer state), and the peer's `send` short-circuits to
    /// `PipeBroken` since there's no reader left to accept the bytes.
    pub shutdown_read: bool,
    /// True once [`IpcTable::shutdown_socket`](super::IpcTable::shutdown_socket)
    /// has been called with `write = true`. A write-shut socket's
    /// own `send` short-circuits to `PipeBroken`, and the peer's
    /// `recv` observes EOF once its rx buffer drains — the same
    /// semantic as the peer having been fully closed, but without
    /// tearing down the fd (the caller can still call `fd_close`).
    pub shutdown_write: bool,
}

impl Socket {
    pub fn new(id: SocketId, ty: SocketType) -> Self {
        Socket {
            id,
            ty,
            state: SocketState::Unbound,
            bound_path: None,
            backlog_cap: 1,
            backlog: VecDeque::new(),
            peer: None,
            rx_buf: VecDeque::new(),
            rx_cap: SOCKET_BUF_CAP,
            rx_fds: VecDeque::new(),
            closed: false,
            shutdown_read: false,
            shutdown_write: false,
        }
    }

    /// Bytes waiting to be read.
    pub fn rx_len(&self) -> usize {
        self.rx_buf.len()
    }

    /// Number of fds waiting to be received.
    pub fn rx_fd_count(&self) -> usize {
        self.rx_fds.len()
    }
}

// --- Socket id allocator --------------------------------------------

#[derive(Debug)]
pub struct SocketIdAllocator {
    next: u32,
}

impl SocketIdAllocator {
    pub const fn new() -> Self {
        SocketIdAllocator { next: 1 }
    }

    pub fn allocate(&mut self) -> SocketId {
        let id = SocketId(self.next);
        self.next = self.next.checked_add(1).expect("socket id overflow");
        id
    }
}

impl Default for SocketIdAllocator {
    fn default() -> Self {
        SocketIdAllocator::new()
    }
}
