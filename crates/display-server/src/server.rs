//! Top-level display server state.
//!
//! [`Server`] owns a map of connected clients keyed by
//! [`ClientId`]. It is the only place in the library that
//! knows about the ensemble of connections; per-connection
//! state lives in [`crate::client::Client`].
//!
//! The server is transport-agnostic: it takes byte buffers on
//! one side and yields byte buffers on the other. The
//! production binary in `src/main.rs` is the integration
//! point that opens `/run/display`, runs
//! `ipc_recv` / `ipc_send` loops, and feeds the byte streams
//! into `Server::dispatch_request` / consumes
//! `Server::take_pending_events`.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::client::{Client, ClientError, ClientId};
use crate::compositor::{Framebuffer, DEFAULT_HEIGHT, DEFAULT_WIDTH};
use display_proto::objects::Interface;
use display_proto::wire::{MessageHeader, WireError, HEADER_SIZE};

/// Errors surfaced by [`Server`] operations. Most of them are
/// thin wrappers over [`ClientError`] or [`WireError`] so the
/// caller has a single `?`-friendly error type.
///
/// Not `Copy` because [`ClientError::UnknownInterfaceName`]
/// carries an owned `String`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerError {
    /// Client ID is not in the server's table.
    NoSuchClient { id: ClientId },
    /// Wire-format error from [`MessageHeader::decode`].
    Wire(WireError),
    /// Client-state error from request dispatch.
    Client(ClientError),
}

impl From<WireError> for ServerError {
    fn from(e: WireError) -> Self {
        ServerError::Wire(e)
    }
}
impl From<ClientError> for ServerError {
    fn from(e: ClientError) -> Self {
        ServerError::Client(e)
    }
}

/// The display server.
pub struct Server {
    next_client_id: u32,
    clients: BTreeMap<ClientId, Client>,
    /// The composed output framebuffer that every committed
    /// surface blits into. v1 is a single global output at
    /// [`DEFAULT_WIDTH`] × [`DEFAULT_HEIGHT`]; real
    /// multi-output support lands with the kernel-side
    /// `Fb` driver.
    framebuffer: Framebuffer,
}

impl Server {
    pub fn new() -> Self {
        Server::with_framebuffer_size(DEFAULT_WIDTH, DEFAULT_HEIGHT)
    }

    /// Build a server whose composed framebuffer is
    /// `width × height`. Tests use this to keep the
    /// allocated pixel buffer small and to produce
    /// deterministic clipping boundaries.
    pub fn with_framebuffer_size(width: u32, height: u32) -> Self {
        Server {
            next_client_id: 1,
            clients: BTreeMap::new(),
            framebuffer: Framebuffer::new(width, height),
        }
    }

    /// Borrow the composed output framebuffer. Every
    /// committed surface's current buffer has been blitted
    /// into this; a future `Fb` driver bridge will read
    /// the pixels here on each frame tick.
    pub fn framebuffer(&self) -> &Framebuffer {
        &self.framebuffer
    }

    /// Mutable access to the framebuffer. Useful in tests
    /// that want to pre-fill with a distinctive background
    /// colour before asserting a blit landed in the
    /// expected rectangle.
    pub fn framebuffer_mut(&mut self) -> &mut Framebuffer {
        &mut self.framebuffer
    }

    /// Accept a new client connection. Returns the
    /// allocated [`ClientId`]. The client starts with
    /// `pmd_display` bound at `ObjectId::DISPLAY` and
    /// **no capabilities** — privileged interfaces like
    /// `pmd_shell_manager` will refuse to bind for this
    /// client. Use [`Server::accept_with_caps`] when the
    /// connecting process holds capabilities the bind
    /// dispatcher should honour.
    pub fn accept(&mut self) -> ClientId {
        self.accept_with_caps(abi::cap::CapSet::EMPTY)
    }

    /// Accept a new client connection with a given
    /// capability set. The caps are stored on the
    /// per-client `Client::capabilities` field and
    /// consulted by the `pmd_registry.bind` auto-install
    /// path: binding `pmd_shell_manager` requires
    /// `Cap::Shell`, future privileged interfaces add
    /// their own entries to
    /// [`crate::client::interface_required_cap`].
    pub fn accept_with_caps(&mut self, caps: abi::cap::CapSet) -> ClientId {
        let id = ClientId(self.next_client_id);
        self.next_client_id = self.next_client_id.checked_add(1).unwrap_or(u32::MAX);
        self.clients.insert(id, Client::new_with_caps(id, caps));
        id
    }

    /// Drop a client, e.g. on connection close. Returns the
    /// removed client so the caller can inspect its final
    /// state (mostly for tests).
    pub fn disconnect(&mut self, id: ClientId) -> Option<Client> {
        self.clients.remove(&id)
    }

    /// Number of currently-connected clients.
    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// Immutably borrow a client.
    pub fn client(&self, id: ClientId) -> Option<&Client> {
        self.clients.get(&id)
    }

    /// Mutably borrow a client.
    pub fn client_mut(&mut self, id: ClientId) -> Option<&mut Client> {
        self.clients.get_mut(&id)
    }

    /// Drain any events the server has enqueued for a
    /// client. Returns a single flat `Vec<u8>` containing
    /// every pending event back-to-back, or `None` if the
    /// client id is unknown.
    ///
    /// The caller typically ships the returned bytes over
    /// the transport the client is connected on; see
    /// `integration-tests/tests/shell_over_display_server.rs`
    /// for a full loopback example.
    pub fn drain_client_events(&mut self, client_id: ClientId) -> Option<Vec<u8>> {
        let client = self.clients.get_mut(&client_id)?;
        Some(client.drain_pending_events())
    }

    /// Feed one wire-format request (header + payload) through
    /// the dispatcher. The input buffer must contain a full
    /// message starting at offset 0; callers should frame
    /// their byte stream with [`MessageHeader::decode`] first
    /// to know how many bytes to pass.
    ///
    /// If the dispatched request is a `pmd_surface.commit`,
    /// the server's minimal compositor runs after the
    /// client-side state transition: the newly-current
    /// buffer's pixels are blitted into [`Self::framebuffer`]
    /// at the attachment's origin. This is how a
    /// client-side pool write becomes a "pixel on the
    /// screen" in the v1 skeleton.
    pub fn dispatch_request(
        &mut self,
        client_id: ClientId,
        bytes: &[u8],
    ) -> Result<(), ServerError> {
        let header = MessageHeader::decode(bytes)?;
        let payload_end = header.length as usize;
        let payload = &bytes[HEADER_SIZE..payload_end];

        // Peek at the target object's interface BEFORE
        // dispatch so we can tell whether this request is
        // a surface commit. After dispatch the client's
        // state has already been mutated.
        let pre_interface = self
            .clients
            .get(&client_id)
            .and_then(|c| c.get(header.object_id));

        let client = self
            .clients
            .get_mut(&client_id)
            .ok_or(ServerError::NoSuchClient { id: client_id })?;
        client.dispatch_request(header, payload)?;

        if pre_interface == Some(Interface::Surface)
            && header.opcode == 7 /* commit */
        {
            self.composite_surface_commit(client_id, header.object_id);
        }

        Ok(())
    }

    /// Blit a surface's current buffer into the server's
    /// framebuffer. Called by [`Server::dispatch_request`]
    /// after a `pmd_surface.commit` has promoted the
    /// pending buffer to current on the client side. Silently
    /// no-ops if any of the required state is missing —
    /// the client may have legitimately committed a surface
    /// with nothing attached.
    fn composite_surface_commit(
        &mut self,
        client_id: ClientId,
        surface_id: display_proto::ids::ObjectId,
    ) {
        // Destructure `self` so the immutable borrow on
        // `clients` and the mutable borrow on `framebuffer`
        // are disjoint as far as the borrow checker is
        // concerned.
        let Server {
            clients,
            framebuffer,
            ..
        } = self;
        let Some(client) = clients.get(&client_id) else {
            return;
        };
        let Some(surface) = client.surfaces.get(&surface_id) else {
            return;
        };
        let Some(attachment) = surface.current_buffer else {
            return;
        };
        let Some(info) = client.buffers.get(&attachment.buffer_id) else {
            return;
        };
        let Some(pool) = client.pools.get(&info.pool_id) else {
            return;
        };
        let start = info.offset as usize;
        let end = info.byte_end() as usize;
        let Some(src_bytes) = pool.storage.get(start..end) else {
            return;
        };
        framebuffer.blit_buffer(info, src_bytes, attachment.x, attachment.y);
    }
}

impl Default for Server {
    fn default() -> Self {
        Server::new()
    }
}
