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
use display_proto::ids::ObjectId;
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

/// Target of a hit-test: which client + surface is at a
/// given screen-space point. Returned by
/// [`Server::hit_test`] and used by the input-routing
/// path to decide which event object should receive an
/// injected event.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HitResult {
    pub client_id: ClientId,
    pub surface_id: ObjectId,
    /// The point converted to surface-local coordinates
    /// (i.e. `(screen_x - toplevel.x, screen_y -
    /// toplevel.y)`).
    pub local_x: i32,
    pub local_y: i32,
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
    /// Current pointer position in screen space. Updated
    /// by [`Server::inject_pointer_motion`]; consulted by
    /// [`Server::inject_pointer_button`] when the click
    /// needs to be routed to whatever surface the pointer
    /// is currently over.
    pointer_x: i32,
    pointer_y: i32,
    /// Currently-focused client + surface for keyboard
    /// input. Set by
    /// [`Server::inject_pointer_button`] on a press
    /// (click-to-focus); cleared when the focused window
    /// is destroyed.
    keyboard_focus: Option<(ClientId, ObjectId)>,
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
            pointer_x: 0,
            pointer_y: 0,
            keyboard_focus: None,
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

    /// Current pointer position in screen space.
    pub fn pointer_position(&self) -> (i32, i32) {
        (self.pointer_x, self.pointer_y)
    }

    /// Currently-focused client + surface for keyboard
    /// input, if any.
    pub fn keyboard_focus(&self) -> Option<(ClientId, ObjectId)> {
        self.keyboard_focus
    }

    /// Hit-test a screen-space point against the toplevels
    /// in every connected client. Returns the topmost
    /// surface whose rectangle contains `(x, y)`, or
    /// `None` if the point doesn't land on any window.
    ///
    /// Z-order is "newer wins" in v1: toplevels created
    /// later sit on top of older ones. The `BTreeMap`
    /// iteration is by ascending `ObjectId`, and object
    /// ids are monotonic, so walking the map in reverse
    /// yields the most-recently-created toplevel first.
    ///
    /// The hit rectangle is `(top.x, top.y, top.x + w,
    /// top.y + h)` where `(w, h)` comes from the
    /// surface's current buffer geometry. A surface that
    /// hasn't committed a buffer yet is invisible to the
    /// hit-test (no rectangle → no hit).
    pub fn hit_test(&self, x: i32, y: i32) -> Option<HitResult> {
        for (&client_id, client) in self.clients.iter().rev() {
            for (&toplevel_id, toplevel) in client.toplevels.iter().rev() {
                let _ = toplevel_id;
                let Some(surface) = client.surfaces.get(&toplevel.surface_id)
                else {
                    continue;
                };
                let Some(attachment) = surface.current_buffer else {
                    continue;
                };
                let Some(info) = client.buffers.get(&attachment.buffer_id)
                else {
                    continue;
                };
                let rect_x = toplevel.x.saturating_add(attachment.x);
                let rect_y = toplevel.y.saturating_add(attachment.y);
                let rect_w = info.width as i32;
                let rect_h = info.height as i32;
                if x >= rect_x
                    && x < rect_x.saturating_add(rect_w)
                    && y >= rect_y
                    && y < rect_y.saturating_add(rect_h)
                {
                    return Some(HitResult {
                        client_id,
                        surface_id: toplevel.surface_id,
                        local_x: x - rect_x,
                        local_y: y - rect_y,
                    });
                }
            }
        }
        None
    }

    /// Inject a pointer motion event in screen space.
    /// Updates the server's pointer position, hit-tests
    /// against the toplevels, and (if the hit client has
    /// a bound `pmd_pointer`) emits a `motion` event on
    /// its pointer object carrying surface-local
    /// coordinates. Returns the hit result, or `None`
    /// if the pointer didn't land on any window.
    pub fn inject_pointer_motion(
        &mut self,
        x: i32,
        y: i32,
    ) -> Option<HitResult> {
        self.pointer_x = x;
        self.pointer_y = y;
        let hit = self.hit_test(x, y)?;
        let client = self.clients.get_mut(&hit.client_id)?;
        if client.pointer_id.is_some() {
            let _ =
                client.emit_pointer_motion(hit.surface_id, hit.local_x, hit.local_y);
        }
        Some(hit)
    }

    /// Inject a pointer button event at the current
    /// pointer position. Emits a `button` event on the
    /// target client's pointer object (if any), and on a
    /// press, sets keyboard focus to the hit surface.
    /// Returns the hit result, or `None` if the click
    /// didn't land on any window.
    pub fn inject_pointer_button(
        &mut self,
        button: u32,
        state: u32,
    ) -> Option<HitResult> {
        let hit = self.hit_test(self.pointer_x, self.pointer_y)?;
        if state == display_proto::events::pointer_button_state::PRESSED {
            self.keyboard_focus = Some((hit.client_id, hit.surface_id));
        }
        let client = self.clients.get_mut(&hit.client_id)?;
        if client.pointer_id.is_some() {
            let _ = client.emit_pointer_button(
                hit.surface_id,
                hit.local_x,
                hit.local_y,
                button,
                state,
            );
        }
        Some(hit)
    }

    /// Inject a keyboard key event. Routes to the
    /// currently-focused client + surface (if any). The
    /// target client must have a bound `pmd_keyboard`
    /// object; otherwise the event is silently dropped.
    /// Returns the (client_id, surface_id) the event was
    /// routed to, or `None` if no window has keyboard
    /// focus.
    pub fn inject_keyboard_key(
        &mut self,
        key: u32,
        state: u32,
    ) -> Option<(ClientId, ObjectId)> {
        let (client_id, surface_id) = self.keyboard_focus?;
        let client = self.clients.get_mut(&client_id)?;
        if client.keyboard_id.is_some() {
            let _ = client.emit_keyboard_key(surface_id, key, state);
        }
        Some((client_id, surface_id))
    }

    /// Explicitly set keyboard focus. Used by tests and
    /// by the desktop shell's click-to-focus path.
    pub fn set_keyboard_focus(
        &mut self,
        focus: Option<(ClientId, ObjectId)>,
    ) {
        self.keyboard_focus = focus;
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
    ///
    /// If the surface has an associated
    /// [`crate::client::Toplevel`], the blit lands at that
    /// toplevel's server-assigned origin (plus any offset
    /// the attach request supplied). Otherwise the blit
    /// uses only the attach offset, which is what ordinary
    /// non-toplevel surfaces want.
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
        let (origin_x, origin_y) =
            if let Some(toplevel) = client.toplevel_for_surface(surface_id) {
                (
                    toplevel.x.saturating_add(attachment.x),
                    toplevel.y.saturating_add(attachment.y),
                )
            } else {
                (attachment.x, attachment.y)
            };
        framebuffer.blit_buffer(info, src_bytes, origin_x, origin_y);
    }
}

impl Default for Server {
    fn default() -> Self {
        Server::new()
    }
}
