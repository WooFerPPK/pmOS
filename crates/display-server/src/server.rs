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

/// What kind of interactive drag is in progress, when one
/// is. `Move` is "follow the pointer with the toplevel's
/// origin"; `Resize { edges }` is "follow the pointer by
/// expanding/contracting along the named edges".
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DragKind {
    Move,
    Resize { edges: u32 },
}

/// State captured when a `pmd_xdg_toplevel.move` /
/// `.resize` request is dispatched. The server consults
/// this on every subsequent pointer-motion event to update
/// the toplevel; the drag ends on the next pointer-button
/// release event.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DragState {
    pub client_id: ClientId,
    pub toplevel_id: ObjectId,
    pub kind: DragKind,
    /// Pointer position when the drag started, in screen
    /// space. Used to compute the pointer delta on each
    /// motion event so the toplevel tracks the cursor 1:1.
    pub start_pointer: (i32, i32),
    /// Toplevel origin when the drag started, in screen
    /// space. Move-drag updates `Toplevel.x/y` to
    /// `start_origin + (current_pointer - start_pointer)`.
    pub start_origin: (i32, i32),
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
    /// Pixels reserved at the bottom of the framebuffer for
    /// the desktop shell's taskbar. `set_maximized`
    /// configures use the framebuffer height MINUS this
    /// value as the work-area height. Defaults to 0;
    /// updated by [`Server::set_taskbar_height_px`].
    taskbar_height_px: u32,
    /// Active interactive drag, if any. Set by
    /// `pmd_xdg_toplevel.move` / `.resize` request dispatch;
    /// consulted by `inject_pointer_motion`; cleared by
    /// `inject_pointer_button` on release.
    active_drag: Option<DragState>,
    /// Cross-client auto-layout counter. Each new toplevel
    /// across ANY client is positioned at
    /// (counter, counter) and the counter advances by
    /// AUTO_LAYOUT_STEP. Without this, every client's first
    /// toplevel would land at (0, 0) and stack invisibly
    /// — so spawning two `hello-toplevel` instances looks
    /// like nothing happened on the second click.
    next_toplevel_offset: i32,
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
            taskbar_height_px: 0,
            active_drag: None,
            next_toplevel_offset: 0,
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
        // T133 server: if a drag is in progress, update the
        // toplevel + emit a configure for resize-drags. Move
        // drags just translate the origin — no configure
        // needed since the size doesn't change.
        if let Some(drag) = self.active_drag {
            self.advance_drag(drag, x, y);
            // During a drag we don't route motion events to
            // app surfaces; the server is exclusively
            // tracking the cursor for the drag.
            return None;
        }
        let hit = self.hit_test(x, y)?;
        let client = self.clients.get_mut(&hit.client_id)?;
        if client.pointer_id.is_some() {
            let _ =
                client.emit_pointer_motion(hit.surface_id, hit.local_x, hit.local_y);
        }
        Some(hit)
    }

    /// Advance an in-progress drag by translating /
    /// resizing the toplevel by the pointer delta. Called
    /// from `inject_pointer_motion`.
    fn advance_drag(&mut self, drag: DragState, pointer_x: i32, pointer_y: i32) {
        use display_proto::xdg_toplevel_resize_edge as edge;
        use display_proto::xdg_toplevel_state;
        let dx = pointer_x - drag.start_pointer.0;
        let dy = pointer_y - drag.start_pointer.1;
        let Some(client) = self.clients.get_mut(&drag.client_id) else {
            return;
        };
        match drag.kind {
            DragKind::Move => {
                if let Some(toplevel) = client.toplevels.get_mut(&drag.toplevel_id) {
                    toplevel.x = drag.start_origin.0.saturating_add(dx);
                    toplevel.y = drag.start_origin.1.saturating_add(dy);
                }
            }
            DragKind::Resize { edges } => {
                // V1 resize semantics: the surface buffer's
                // width/height come from the client side
                // (client decides the buffer geometry on
                // attach). The server emits a configure
                // proposing the new size; the client
                // respects it on its next attach.
                let surface_id = match client.toplevels.get(&drag.toplevel_id) {
                    Some(t) => t.surface_id,
                    None => return,
                };
                let (start_w, start_h) = client
                    .surfaces
                    .get(&surface_id)
                    .and_then(|s| s.current_buffer)
                    .and_then(|attachment| client.buffers.get(&attachment.buffer_id))
                    .map(|info| (info.width as i32, info.height as i32))
                    .unwrap_or((320, 240));
                let mut new_w = start_w;
                let mut new_h = start_h;
                if edges & edge::RIGHT != 0 {
                    new_w = (start_w + dx).max(1);
                } else if edges & edge::LEFT != 0 {
                    new_w = (start_w - dx).max(1);
                }
                if edges & edge::BOTTOM != 0 {
                    new_h = (start_h + dy).max(1);
                } else if edges & edge::TOP != 0 {
                    new_h = (start_h - dy).max(1);
                }
                let serial = client.next_configure_serial();
                let _ = client.emit_xdg_toplevel_configure(
                    drag.toplevel_id,
                    serial,
                    new_w,
                    new_h,
                    xdg_toplevel_state::RESIZING,
                );
            }
        }
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
        // T133 server: a release during a drag terminates
        // the drag and emits a final configure (resize) with
        // the RESIZING bit cleared. Move drags don't need a
        // final configure since the size never changed.
        if state == display_proto::events::pointer_button_state::RELEASED {
            if let Some(drag) = self.active_drag.take() {
                if let DragKind::Resize { .. } = drag.kind {
                    self.emit_resize_final_configure(drag);
                }
                return None;
            }
        }
        let hit = self.hit_test(self.pointer_x, self.pointer_y)?;
        if state == display_proto::events::pointer_button_state::PRESSED {
            self.keyboard_focus = Some((hit.client_id, hit.surface_id));
            // Find the toplevel id that owns the hit surface
            // and broadcast a window_focused event so every
            // subscribed shell repaints its taskbar.
            let mut focused_window_id: Option<u32> = None;
            if let Some(client) = self.clients.get(&hit.client_id) {
                for (toplevel_id, toplevel) in client.toplevels.iter() {
                    if toplevel.surface_id == hit.surface_id {
                        focused_window_id = Some(toplevel_id.0);
                        break;
                    }
                }
            }
            if let Some(wid) = focused_window_id {
                self.broadcast_window_focused(wid);
            }
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

    /// Emit the final `configure` event after a resize drag
    /// ends, with the `RESIZING` state bit cleared. The
    /// proposed size carries the surface's current buffer
    /// dimensions — the client may have mid-drag committed
    /// a buffer matching one of the in-flight resizing
    /// configures, so this is the size the server "settles
    /// on".
    fn emit_resize_final_configure(&mut self, drag: DragState) {
        let Some(client) = self.clients.get_mut(&drag.client_id) else {
            return;
        };
        let surface_id = match client.toplevels.get(&drag.toplevel_id) {
            Some(t) => t.surface_id,
            None => return,
        };
        let (w, h) = client
            .surfaces
            .get(&surface_id)
            .and_then(|s| s.current_buffer)
            .and_then(|attachment| client.buffers.get(&attachment.buffer_id))
            .map(|info| (info.width as i32, info.height as i32))
            .unwrap_or((0, 0));
        let serial = client.next_configure_serial();
        let _ = client.emit_xdg_toplevel_configure(drag.toplevel_id, serial, w, h, 0);
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
    ///
    /// Side-effect: every toplevel the dropped client owned
    /// triggers a `pmd_shell_manager.window_destroyed`
    /// broadcast so subscribed shells can prune the
    /// associated taskbar entries before the connection's
    /// last events are flushed.
    pub fn disconnect(&mut self, id: ClientId) -> Option<Client> {
        let dying_window_ids: alloc::vec::Vec<u32> = self
            .clients
            .get(&id)
            .map(|c| c.toplevels.keys().map(|tid| tid.0).collect())
            .unwrap_or_default();
        let removed = self.clients.remove(&id);
        for wid in dying_window_ids {
            self.broadcast_window_destroyed(wid);
        }
        removed
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
            self.maybe_emit_initial_configure(client_id, header.object_id);
        }

        // T131: cross-client shell_manager.minimize_window
        // takes effect server-wide. The payload is a window_id
        // owned by some OTHER client; we walk every client's
        // toplevel table to find the match and flip the flag.
        if pre_interface == Some(Interface::ShellManager)
            && header.opcode == 4 /* minimize_window */
        {
            if let Ok(req) = display_proto::requests::ShellManagerMinimizeWindow::decode(payload) {
                let target = display_proto::ids::ObjectId::new(req.window_id);
                self.set_toplevel_minimized_across_clients(target, true);
            }
        }

        // T132 server emit: set_maximized / unset_maximized
        // immediately get a configure response back so the
        // toolkit's is_maximized accessor reflects the new
        // state without an external poke. set_maximized
        // sizes to the work area (framebuffer minus taskbar);
        // unset_maximized echoes the server's idea of the
        // pre-max size (cached at set_maximized time).
        if pre_interface == Some(Interface::XdgToplevel) {
            match header.opcode {
                5 /* set_maximized */ => {
                    self.emit_configure_for_max_state(client_id, header.object_id, true)?;
                }
                6 /* unset_maximized */ => {
                    self.emit_configure_for_max_state(client_id, header.object_id, false)?;
                }
                7 /* move */ | 8 /* resize */ => {
                    self.start_drag_from_request(client_id, header.object_id, header.opcode, payload);
                }
                _ => {}
            }
        }

        // pmd_shell_manager hooks. Subscribe is a one-shot —
        // once a client has called subscribe_windows, any
        // subsequent toplevel mutation triggers a broadcast.
        // The catch-up snapshot fires inline so the client's
        // taskbar populates with everything that's already
        // open before the subscription landed.
        if pre_interface == Some(Interface::ShellManager) {
            match header.opcode {
                1 /* subscribe_windows */ => {
                    self.subscribe_windows_for(client_id);
                }
                2 /* focus_window */ => {
                    if let Ok(req) =
                        display_proto::requests::ShellManagerFocusWindow::decode(payload)
                    {
                        self.focus_window_by_id(req.window_id);
                    }
                }
                3 /* close_window */ => {
                    if let Ok(req) =
                        display_proto::requests::ShellManagerCloseWindow::decode(payload)
                    {
                        self.close_window_by_id(req.window_id);
                    }
                }
                _ => {}
            }
        }

        // Broadcast hooks for toplevel lifecycle events.
        // `set_title` / `set_app_id` were applied to the
        // client's per-toplevel state by the underlying
        // `apply_toplevel_state` call inside dispatch_request;
        // we now have to fan out a window_title_changed event
        // to every subscribed client.
        if pre_interface == Some(Interface::XdgToplevel) {
            match header.opcode {
                1 /* set_title */ => {
                    self.broadcast_window_title_changed(client_id, header.object_id);
                }
                2 /* set_app_id */ => {
                    // app_id changes don't have a dedicated
                    // event; the title-changed event covers the
                    // taskbar's repaint trigger since both
                    // affect the visible label.
                    self.broadcast_window_title_changed(client_id, header.object_id);
                }
                _ => {}
            }
        }

        // get_toplevel installed a new toplevel; reposition
        // it using the server-global staircase so two
        // separate clients' first toplevels don't both land
        // at (0, 0). Then broadcast window_created.
        if pre_interface == Some(Interface::XdgShell) && header.opcode == 1 /* get_toplevel */ {
            if let Ok(req) =
                display_proto::requests::XdgShellGetToplevel::decode(payload)
            {
                self.position_new_toplevel(client_id, req.new_id);
                self.broadcast_window_created(client_id, req.new_id);
            }
        }

        // surface.commit on a surface that has a toplevel
        // means the window has produced its first paintable
        // frame; many shells repaint the taskbar after this
        // because the window's title may have just been set.
        // We treat the FIRST commit as the canonical
        // window_created broadcast trigger if the window was
        // never broadcast before — get_toplevel fires before
        // the title is set, so a delayed-broadcast pattern
        // would wait for the first commit. For v1 we keep the
        // simpler get_toplevel-time broadcast above and do
        // nothing extra on commit.

        Ok(())
    }

    /// Override a just-installed toplevel's auto-layout
    /// position with the server-global counter so each new
    /// window across ANY client lands at a different spot.
    /// The per-client `Client::next_toplevel_offset` already
    /// stepped, but its counter starts at 0 for every fresh
    /// client; without this two separate processes would
    /// have their first toplevel both land at (0, 0) and
    /// stack invisibly.
    fn position_new_toplevel(
        &mut self,
        client_id: ClientId,
        toplevel_id: display_proto::ids::ObjectId,
    ) {
        use crate::client::AUTO_LAYOUT_STEP;
        let pos = self.next_toplevel_offset;
        self.next_toplevel_offset = self.next_toplevel_offset.saturating_add(AUTO_LAYOUT_STEP);
        if let Some(client) = self.clients.get_mut(&client_id) {
            if let Some(toplevel) = client.toplevels.get_mut(&toplevel_id) {
                toplevel.x = pos;
                toplevel.y = pos;
            }
        }
    }

    /// Mark `client_id` as subscribed to shell_manager
    /// window-list events and emit a catch-up snapshot of
    /// every currently-open toplevel through the client's
    /// shell_manager object. No-op if the client has no
    /// bound shell_manager.
    fn subscribe_windows_for(&mut self, client_id: ClientId) {
        let Some(shell_manager_id) = self
            .clients
            .get(&client_id)
            .and_then(|c| c.shell_manager_id)
        else {
            return;
        };
        // Snapshot every (window_id, title, app_id, focused)
        // tuple BEFORE mutating the subscriber so the iteration
        // borrow is independent of the mutation.
        let snapshot: alloc::vec::Vec<(u32, alloc::string::String, alloc::string::String, bool)> = self
            .clients
            .values()
            .flat_map(|c| {
                let pinned_focus = self.keyboard_focus;
                c.toplevels
                    .iter()
                    .map(move |(_, t)| {
                        let focused = pinned_focus
                            .map(|(_, surf)| surf == t.surface_id)
                            .unwrap_or(false);
                        (t.id.0, t.title.clone(), t.app_id.clone(), focused)
                    })
                    .collect::<alloc::vec::Vec<_>>()
            })
            .collect();
        let Some(client) = self.clients.get_mut(&client_id) else {
            return;
        };
        client.shell_manager_subscribed = true;
        for (window_id, title, app_id, focused) in &snapshot {
            let _ = client.emit_window_created(shell_manager_id, *window_id, title, app_id);
            if *focused {
                let _ = client.emit_window_focused(shell_manager_id, *window_id);
            }
        }
    }

    /// Broadcast `window_created` to every subscribed client.
    /// Called from the post-dispatch hook for `get_toplevel`;
    /// at that point the title is empty (set_title comes
    /// later) but the taskbar already wants to show an entry.
    /// A subsequent `set_title` triggers a `window_title_changed`
    /// broadcast that updates the existing entry in place.
    fn broadcast_window_created(
        &mut self,
        owning_client_id: ClientId,
        toplevel_id: display_proto::ids::ObjectId,
    ) {
        let Some(owning) = self.clients.get(&owning_client_id) else {
            return;
        };
        let Some(toplevel) = owning.toplevels.get(&toplevel_id) else {
            return;
        };
        let title = toplevel.title.clone();
        let app_id = toplevel.app_id.clone();
        let window_id = toplevel.id.0;
        for client in self.clients.values_mut() {
            if !client.shell_manager_subscribed {
                continue;
            }
            let Some(sm_id) = client.shell_manager_id else {
                continue;
            };
            let _ = client.emit_window_created(sm_id, window_id, &title, &app_id);
        }
    }

    /// Broadcast `window_destroyed` to every subscribed
    /// client. Called when a toplevel is removed from a
    /// client's table (e.g. on disconnect or via
    /// `xdg_toplevel.destroy`).
    pub fn broadcast_window_destroyed(&mut self, window_id: u32) {
        for client in self.clients.values_mut() {
            if !client.shell_manager_subscribed {
                continue;
            }
            let Some(sm_id) = client.shell_manager_id else {
                continue;
            };
            let _ = client.emit_window_destroyed(sm_id, window_id);
        }
    }

    /// Broadcast `window_title_changed` to every subscribed
    /// client. Called from the post-dispatch hooks for
    /// `set_title` / `set_app_id`.
    fn broadcast_window_title_changed(
        &mut self,
        owning_client_id: ClientId,
        toplevel_id: display_proto::ids::ObjectId,
    ) {
        let Some(owning) = self.clients.get(&owning_client_id) else {
            return;
        };
        let Some(toplevel) = owning.toplevels.get(&toplevel_id) else {
            return;
        };
        let new_title = toplevel.title.clone();
        let window_id = toplevel.id.0;
        for client in self.clients.values_mut() {
            if !client.shell_manager_subscribed {
                continue;
            }
            let Some(sm_id) = client.shell_manager_id else {
                continue;
            };
            let _ = client.emit_window_title_changed(sm_id, window_id, &new_title);
        }
    }

    /// Broadcast `window_focused(window_id)` to every
    /// subscribed client. Called from the click-to-focus
    /// path inside `inject_pointer_button` and from the
    /// shell_manager.focus_window dispatch hook.
    fn broadcast_window_focused(&mut self, window_id: u32) {
        for client in self.clients.values_mut() {
            if !client.shell_manager_subscribed {
                continue;
            }
            let Some(sm_id) = client.shell_manager_id else {
                continue;
            };
            let _ = client.emit_window_focused(sm_id, window_id);
        }
    }

    /// Implementation of `pmd_shell_manager.focus_window`.
    /// Walk every client; the toplevel whose ObjectId matches
    /// `window_id` becomes the keyboard focus target. The
    /// shell uses this from the taskbar click-to-focus path.
    fn focus_window_by_id(&mut self, window_id: u32) {
        let target = display_proto::ids::ObjectId::new(window_id);
        let mut hit: Option<(ClientId, display_proto::ids::ObjectId)> = None;
        for (cid, client) in self.clients.iter() {
            if let Some(t) = client.toplevels.get(&target) {
                hit = Some((*cid, t.surface_id));
                break;
            }
        }
        if let Some((cid, surface_id)) = hit {
            self.keyboard_focus = Some((cid, surface_id));
            // Restoring a minimized toplevel is part of the
            // "click to focus" UX — the shell expects a
            // single focus_window call to bring the window
            // back regardless of prior minimized state.
            self.set_toplevel_minimized_across_clients(target, false);
            self.broadcast_window_focused(window_id);
        }
    }

    /// Implementation of `pmd_shell_manager.close_window`.
    /// Sends `xdg_toplevel.close` to the owning client so it
    /// can tear down its surface; the client's subsequent
    /// drop of the toplevel triggers a window_destroyed
    /// broadcast in the disconnect path.
    fn close_window_by_id(&mut self, window_id: u32) {
        let target = display_proto::ids::ObjectId::new(window_id);
        let mut owning: Option<ClientId> = None;
        for (cid, client) in self.clients.iter() {
            if client.toplevels.contains_key(&target) {
                owning = Some(*cid);
                break;
            }
        }
        if let Some(cid) = owning {
            if let Some(client) = self.clients.get_mut(&cid) {
                let _ = client.emit_xdg_toplevel_close(target);
            }
        }
    }

    /// Server-driven configure emission for the
    /// `set_maximized` / `unset_maximized` request handlers.
    /// On `set_maximized = true`, snapshots the current
    /// pre-max size onto the toplevel + emits a configure
    /// with the framebuffer dimensions + the MAXIMIZED bit.
    /// On `set_maximized = false`, emits a configure with
    /// the cached pre-max size + cleared MAXIMIZED bit.
    fn emit_configure_for_max_state(
        &mut self,
        client_id: ClientId,
        toplevel_id: display_proto::ids::ObjectId,
        maximize: bool,
    ) -> Result<(), ServerError> {
        use display_proto::xdg_toplevel_state;
        let work_area_w = self.work_area_width();
        let work_area_h = self.work_area_height();
        let client = self
            .clients
            .get_mut(&client_id)
            .ok_or(ServerError::NoSuchClient { id: client_id })?;
        // Read pre-max snapshot before mutating; the
        // toplevel's state was already updated by the
        // dispatch arm above, so `maximized` already
        // reflects the new value here.
        let _toplevel = client
            .toplevels
            .get(&toplevel_id)
            .ok_or(ServerError::NoSuchClient { id: client_id })?; // misuse if absent
        let serial = client.next_configure_serial();
        let (w, h, states) = if maximize {
            (work_area_w as i32, work_area_h as i32, xdg_toplevel_state::MAXIMIZED)
        } else {
            // V1: defer to the client's preferred size when
            // unmaximizing — the client's toolkit caches its
            // own preferred size, so 0/0 means "you pick".
            (0, 0, 0)
        };
        let _ = client.emit_xdg_toplevel_configure(toplevel_id, serial, w, h, states);
        Ok(())
    }

    /// Width of the v1 "work area" — the screen region apps
    /// can fill when maximized. For now this equals the full
    /// framebuffer width; a future T130 slice that lands the
    /// taskbar will subtract the taskbar's width / height.
    pub fn work_area_width(&self) -> u32 {
        let h = self.framebuffer.height();
        if h > self.taskbar_height_px {
            self.framebuffer.width()
        } else {
            self.framebuffer.width()
        }
    }

    /// Height of the v1 "work area" — framebuffer height
    /// minus the taskbar's reserved strip. `taskbar_height_px`
    /// defaults to 0 and is set by the shell via
    /// [`Server::set_taskbar_height_px`] when it claims the
    /// bottom strip.
    pub fn work_area_height(&self) -> u32 {
        self.framebuffer
            .height()
            .saturating_sub(self.taskbar_height_px)
    }

    /// Tell the server how tall the taskbar is, in pixels.
    /// The shell calls this after it lays out its taskbar
    /// surface so subsequent `set_maximized` configures use
    /// a work-area height that excludes the taskbar strip.
    /// Pass `0` to clear the reservation.
    pub fn set_taskbar_height_px(&mut self, px: u32) {
        self.taskbar_height_px = px;
    }

    /// Begin an interactive drag from a `pmd_xdg_toplevel.move`
    /// (opcode 7) or `.resize` (opcode 8) request. Captures
    /// the current pointer position + toplevel origin/size
    /// into [`Server::active_drag`]; subsequent
    /// `inject_pointer_motion` calls update the toplevel
    /// origin (move) or emit a resize configure (resize).
    /// The drag ends on the next `inject_pointer_button(release)`.
    fn start_drag_from_request(
        &mut self,
        client_id: ClientId,
        toplevel_id: display_proto::ids::ObjectId,
        opcode: u16,
        payload: &[u8],
    ) {
        let kind = if opcode == 7 {
            DragKind::Move
        } else {
            // resize payload: u32 serial + u32 edges
            if payload.len() < 8 {
                return;
            }
            let edges = u32::from_le_bytes(payload[4..8].try_into().unwrap());
            DragKind::Resize { edges }
        };
        let (origin_x, origin_y) = match self
            .clients
            .get(&client_id)
            .and_then(|c| c.toplevels.get(&toplevel_id))
        {
            Some(t) => (t.x, t.y),
            None => return,
        };
        self.active_drag = Some(DragState {
            client_id,
            toplevel_id,
            kind,
            start_pointer: self.pointer_position(),
            start_origin: (origin_x, origin_y),
        });
    }

    /// True if an interactive move/resize drag is in
    /// progress (one of the toolkit's clients sent
    /// `xdg_toplevel.move` / `.resize` and the pointer
    /// hasn't been released yet). Exposed for tests + the
    /// shell's "is the user dragging a window?" cursor
    /// logic.
    pub fn is_dragging(&self) -> bool {
        self.active_drag.is_some()
    }

    /// End any in-progress drag, returning the drag state
    /// for tests that want to inspect what was happening.
    /// Called automatically by `inject_pointer_button` on
    /// release; exposed for tests + restore paths.
    pub fn end_drag(&mut self) -> Option<DragState> {
        self.active_drag.take()
    }

    /// If `surface_id`'s toplevel has not yet received an
    /// `xdg_toplevel.configure`, emit one now sized to the
    /// work area and flag the toplevel as configured. The
    /// initial configure is the Wayland-style handshake
    /// that tells the client "you may now paint real
    /// frames"; the toolkit's `Window::dispatch` blocks
    /// the paint loop on `is_configured = true`. Called
    /// from the surface-commit dispatch hook so the very
    /// first commit synthesises the configure (clients
    /// commit an empty buffer or a placeholder buffer
    /// before they know the size).
    pub fn maybe_emit_initial_configure(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
    ) {
        let work_area_w = self.work_area_width() as i32;
        let work_area_h = self.work_area_height() as i32;
        let Some(client) = self.clients.get_mut(&client_id) else {
            return;
        };
        // Find the toplevel that owns this surface.
        let toplevel_id = match client.toplevel_by_surface.get(&surface_id) {
            Some(&id) => id,
            None => return,
        };
        let already_sent = client
            .toplevels
            .get(&toplevel_id)
            .map(|t| t.initial_configure_sent)
            .unwrap_or(true);
        if already_sent {
            return;
        }
        let serial = client.next_configure_serial();
        let _ = client.emit_xdg_toplevel_configure(
            toplevel_id,
            serial,
            work_area_w,
            work_area_h,
            0,
        );
        if let Some(toplevel) = client.toplevels.get_mut(&toplevel_id) {
            toplevel.initial_configure_sent = true;
        }
    }

    /// Emit the v1 globals catalog onto `registry_id` for
    /// `client_id`. Called by the post-dispatch hook in
    /// [`Server::dispatch_request`] after a successful
    /// `pmd_display.get_registry`. Advertises the four
    /// universal globals (compositor, shm, xdg_shell, seat)
    /// plus pmd_shell_manager — gated on the client holding
    /// `Cap::Shell` since binding shell_manager requires
    /// that cap (see [`crate::client::interface_required_cap`]).
    /// The numeric `name` values are the registry handles
    /// the client echoes back through `registry.bind`.
    pub fn advertise_globals_to(
        &mut self,
        client_id: ClientId,
        registry_id: ObjectId,
    ) {
        use abi::cap::Cap;
        let Some(client) = self.clients.get_mut(&client_id) else {
            return;
        };
        let _ = client.emit_global(registry_id, 1, "pmd_compositor", 1);
        let _ = client.emit_global(registry_id, 2, "pmd_shm", 1);
        let _ = client.emit_global(registry_id, 3, "pmd_xdg_shell", 1);
        let _ = client.emit_global(registry_id, 4, "pmd_seat", 1);
        if client.capabilities.contains(Cap::Shell) {
            let _ = client.emit_global(registry_id, 5, "pmd_shell_manager", 1);
        }
    }

    /// Walk every client's toplevel table and flip the
    /// `minimized` flag on whichever toplevel matches
    /// `target_id` (if any). Used by T131's cross-client
    /// `pmd_shell_manager.minimize_window` dispatch and by
    /// the server-driven restore path. No-op if no toplevel
    /// matches; in v1 toplevel ids are unique across clients.
    pub fn set_toplevel_minimized_across_clients(
        &mut self,
        target_id: display_proto::ids::ObjectId,
        minimized: bool,
    ) -> bool {
        for client in self.clients.values_mut() {
            if let Some(toplevel) = client.toplevels.get_mut(&target_id) {
                toplevel.minimized = minimized;
                return true;
            }
        }
        false
    }

    /// Server-driven restore for a previously-minimized
    /// toplevel. Counterpart to the cross-client minimize
    /// path — there's no `pmd_shell_manager.restore_window`
    /// request today (the spec lets the shell drive restore
    /// via `focus_window`, which in v1 is a separate slice),
    /// so this method exists for callers wiring restore
    /// through other channels (test harnesses, future
    /// taskbar click routing).
    pub fn restore_toplevel(
        &mut self,
        target_id: display_proto::ids::ObjectId,
    ) -> bool {
        self.set_toplevel_minimized_across_clients(target_id, false)
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
                // T131: minimized toplevels are unmapped —
                // skip the blit but keep the surface state
                // intact so a restore can re-map without a
                // round-trip back through the client.
                if toplevel.minimized {
                    return;
                }
                (
                    toplevel.x.saturating_add(attachment.x),
                    toplevel.y.saturating_add(attachment.y),
                )
            } else {
                (attachment.x, attachment.y)
            };
        framebuffer.blit_buffer(info, src_bytes, origin_x, origin_y);
        // Buffer release: server's pool storage already
        // holds a copy of the painted bytes, so the client
        // can recycle this buffer immediately.
        let buffer_id = attachment.buffer_id;
        if let Some(client_mut) = clients.get_mut(&client_id) {
            let _ = client_mut.emit_buffer_release(buffer_id);
        }
    }
}

impl Default for Server {
    fn default() -> Self {
        Server::new()
    }
}
