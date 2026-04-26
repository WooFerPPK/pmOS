//! Top-level window facade wrapping `pmd_xdg_toplevel`.
//!
//! [`Window`] is a thin layer on top of the [`crate::App`]
//! facade that owns the compositor surface +
//! `pmd_xdg_toplevel` object pair, tracks the
//! `configure` / `ack_configure` handshake, and exposes an
//! app-friendly title / app-id / configured-size / close
//! request API.
//!
//! Higher layers layering on top of `Window`:
//!
//! * widgets + layout (T116 / T117) drive painting through
//!   the window's surface;
//! * the SAB buffer + commit loop (T119) attaches a
//!   `pmd_buffer` to the surface and commits it on each
//!   frame callback.
//!
//! ## Collapsed `pmd_xdg_surface` deviation
//!
//! `contracts/display-protocol.md` §11 defines
//! `pmd_xdg_surface` as a distinct intermediate between a
//! plain [`Interface::Surface`] and a
//! [`Interface::XdgToplevel`]. The v1 server collapses the
//! two into a single `xdg_shell.get_toplevel(new_id,
//! surface)` step (see `display-server/src/client.rs`), and
//! the toolkit's [`crate::protocol::Client`] mirrors this
//! shape. The `Window` struct keeps an `xdg_surface` field
//! for spec-alignment, but in v1 it aliases the surface
//! id — no separate `get_xdg_surface` request is sent on
//! the wire. The configure/ack_configure handshake is
//! similarly folded into `pmd_xdg_toplevel` (see the
//! comment on the `XDG_TOPLEVEL_*` opcode tables in
//! `display-proto/src/objects.rs`).

use display_proto::events::{XdgToplevelClose, XdgToplevelConfigure};
use display_proto::ids::ObjectId;
use display_proto::objects::Interface;
use display_proto::wire::WireError;

use crate::app::App;
use crate::protocol::{ClientError, ClientEventWithPayload, Connection};

/// A top-level application window.
///
/// Thin facade over a compositor surface +
/// `pmd_xdg_toplevel` pair. Owns the object ids, tracks
/// the `configure` / `ack_configure` handshake, and
/// exposes a small app-friendly API (title, app id,
/// configured size, close-requested flag).
///
/// **Lifecycle.** Construct via [`Window::new`] — this
/// allocates the surface + xdg_toplevel object ids and
/// sends the `compositor.create_surface` +
/// `xdg_shell.get_toplevel` request sequence. Follow up
/// with [`Window::set_title`] / [`Window::set_app_id`] to
/// populate metadata, then call [`Window::commit`] to
/// trigger the server's first
/// `xdg_toplevel::configure`. Call [`Window::dispatch`] in
/// a loop to handle incoming events; `dispatch` internally
/// acks any `configure` event (updating
/// [`Window::configured_size`] and flipping
/// [`Window::is_configured`] to `true`) and records
/// `close` events ([`Window::close_requested`]). Buffer
/// attachment and frame-callback-driven redraws land in
/// T119.
pub struct Window<'a, C: Connection> {
    app: &'a mut App<C>,
    surface: ObjectId,
    /// Held for spec-alignment; in the v1 collapsed
    /// protocol this is an alias for `surface` (no
    /// separate `get_xdg_surface` request is on the wire).
    xdg_surface: ObjectId,
    xdg_toplevel: ObjectId,
    title: String,
    app_id: String,
    /// Configured size from the server; `(0, 0)` until
    /// the first `configure` event lands or when the
    /// server explicitly defers to the client's
    /// preferred size.
    configured_size: (u32, u32),
    /// `true` iff at least one `xdg_toplevel::configure`
    /// has been received.
    configured: bool,
    /// Most-recent state bitfield from `xdg_toplevel::
    /// configure(... states)`. Decoded against
    /// [`display_proto::xdg_toplevel_state`] bits.
    states: u32,
    /// `true` iff the server has emitted
    /// `xdg_toplevel::close`.
    close_requested: bool,
}

impl<'a, C: Connection> Window<'a, C> {
    /// Create a new top-level window.
    ///
    /// Allocates the surface + xdg_toplevel object ids
    /// through the borrowed [`App`] and sends the
    /// `compositor.create_surface` +
    /// `xdg_shell.get_toplevel` request sequence. Does
    /// NOT commit — the caller should follow up with
    /// [`Window::set_title`] / [`Window::set_app_id`]
    /// and then [`Window::commit`] to trigger the
    /// server's initial configure event.
    ///
    /// The returned [`Window`] has `is_configured ==
    /// false` and an empty title / app id. In the v1
    /// collapsed protocol, `xdg_surface` is aliased to
    /// `surface` — the spec's `get_xdg_surface` step is
    /// folded into `xdg_shell.get_toplevel`. See the
    /// module-level doc comment.
    pub fn new(app: &'a mut App<C>) -> Result<Self, ClientError> {
        let compositor = app.compositor();
        let xdg_shell = app.xdg_shell();
        let client = app.client_mut();
        let surface = client.compositor_create_surface(compositor)?;
        let xdg_toplevel = client.xdg_shell_get_toplevel(xdg_shell, surface)?;
        Ok(Window {
            app,
            surface,
            xdg_surface: surface,
            xdg_toplevel,
            title: String::new(),
            app_id: String::new(),
            configured_size: (0, 0),
            configured: false,
            states: 0,
            close_requested: false,
        })
    }

    /// Set the window title. Sends
    /// `pmd_xdg_toplevel.set_title(title)` and caches
    /// the string locally so callers can read it back.
    pub fn set_title(&mut self, title: &str) -> Result<(), ClientError> {
        self.app
            .client_mut()
            .xdg_toplevel_set_title(self.xdg_toplevel, title)?;
        self.title = title.into();
        Ok(())
    }

    /// Set the application id. Sends
    /// `pmd_xdg_toplevel.set_app_id(app_id)` and caches
    /// the string locally.
    pub fn set_app_id(&mut self, app_id: &str) -> Result<(), ClientError> {
        self.app
            .client_mut()
            .xdg_toplevel_set_app_id(self.xdg_toplevel, app_id)?;
        self.app_id = app_id.into();
        Ok(())
    }

    /// Commit pending surface state, triggering the
    /// server's first `configure` event when this is the
    /// initial commit. Sends `pmd_surface.commit()`.
    pub fn commit(&mut self) -> Result<(), ClientError> {
        self.app.client_mut().surface_commit(self.surface)
    }

    /// Run one event-dispatch cycle.
    ///
    /// Delegates to [`App::dispatch`] to drain whatever
    /// the server has queued, then post-processes each
    /// parsed event:
    ///
    /// * `xdg_toplevel::configure(serial, width, height)`
    ///   — update [`Window::configured_size`], flip
    ///   [`Window::is_configured`] to `true`, and reply
    ///   with `ack_configure(serial)`. Not returned to
    ///   the caller.
    /// * `xdg_toplevel::close` — set
    ///   [`Window::close_requested`]. Not returned to
    ///   the caller.
    /// * everything else — returned in the output vec so
    ///   the caller can drive app-level logic from frame
    ///   callbacks + input events (future slices).
    pub fn dispatch(&mut self) -> Result<Vec<ClientEventWithPayload>, ClientError> {
        let events = self.app.dispatch()?;
        let mut passthrough = Vec::with_capacity(events.len());
        for event in events {
            if event.object_id == self.xdg_toplevel
                && event.interface == Interface::XdgToplevel
            {
                match event.opcode {
                    1 /* configure */ => {
                        let decoded = XdgToplevelConfigure::decode(&event.payload)
                            .map_err(|_| ClientError::Wire(WireError::InvalidLength))?;
                        let width = decoded.width.max(0) as u32;
                        let height = decoded.height.max(0) as u32;
                        self.configured_size = (width, height);
                        self.configured = true;
                        self.states = decoded.states;
                        self.app
                            .client_mut()
                            .xdg_toplevel_ack_configure(self.xdg_toplevel, decoded.serial)?;
                        continue;
                    }
                    2 /* close */ => {
                        let _ = XdgToplevelClose::decode(&event.payload)
                            .map_err(|_| ClientError::Wire(WireError::InvalidLength))?;
                        self.close_requested = true;
                        continue;
                    }
                    _ => {}
                }
            }
            passthrough.push(event);
        }
        Ok(passthrough)
    }

    /// True iff the server has sent at least one
    /// `configure` event.
    pub fn is_configured(&self) -> bool {
        self.configured
    }

    /// The server-configured size. `(0, 0)` until the
    /// first `configure` event lands, and also `(0, 0)`
    /// if the server has deferred to the client's
    /// preferred size. Updated by [`Window::dispatch`].
    pub fn configured_size(&self) -> (u32, u32) {
        self.configured_size
    }

    /// True iff the server has requested the window
    /// close (via `xdg_toplevel::close`). The caller
    /// observes this and performs whatever cleanup the
    /// app wants before destroying the `Window`.
    pub fn close_requested(&self) -> bool {
        self.close_requested
    }

    /// The most-recent state bitfield from
    /// `xdg_toplevel::configure(... states)`. Decoded
    /// against [`display_proto::xdg_toplevel_state`] bits.
    /// Returns `0` until the first configure event lands.
    pub fn states(&self) -> u32 {
        self.states
    }

    /// True iff the server's most-recent configure event
    /// included the `MAXIMIZED` state bit.
    pub fn is_maximized(&self) -> bool {
        (self.states & display_proto::xdg_toplevel_state::MAXIMIZED) != 0
    }

    /// True iff the server's most-recent configure event
    /// included the `FULLSCREEN` state bit.
    pub fn is_fullscreen(&self) -> bool {
        (self.states & display_proto::xdg_toplevel_state::FULLSCREEN) != 0
    }

    /// True iff the server's most-recent configure event
    /// included the `ACTIVATED` state bit (keyboard focus).
    pub fn is_activated(&self) -> bool {
        (self.states & display_proto::xdg_toplevel_state::ACTIVATED) != 0
    }

    /// Send `pmd_xdg_toplevel.set_maximized()` — ask the
    /// server to maximize this window. The server replies
    /// (eventually) with a `configure` carrying the
    /// `MAXIMIZED` state bit + new size; the toolkit picks
    /// it up via [`Window::dispatch`].
    pub fn set_maximized(&mut self) -> Result<(), ClientError> {
        self.app
            .client_mut()
            .xdg_toplevel_set_maximized(self.xdg_toplevel)
    }

    /// Send `pmd_xdg_toplevel.unset_maximized()` — ask the
    /// server to restore this window from a previously-set
    /// maximized state. The server replies with a
    /// `configure` carrying the previous (non-maximized)
    /// size + state.
    pub fn unset_maximized(&mut self) -> Result<(), ClientError> {
        self.app
            .client_mut()
            .xdg_toplevel_unset_maximized(self.xdg_toplevel)
    }

    /// Ask the server to initiate an interactive move drag.
    /// Sends `pmd_xdg_toplevel.move(serial)`. The caller
    /// passes the serial from the pointer-button event that
    /// started the drag — typically the most recent
    /// `pmd_pointer.button` event the app handled. The
    /// server takes over pointer events for the drag and
    /// sends `configure` events as the toplevel moves.
    pub fn request_move(&mut self, serial: u32) -> Result<(), ClientError> {
        self.app
            .client_mut()
            .xdg_toplevel_move(self.xdg_toplevel, serial)
    }

    /// Ask the server to initiate an interactive resize
    /// drag. Sends `pmd_xdg_toplevel.resize(serial, edges)`.
    /// `edges` is a bitfield of
    /// [`display_proto::xdg_toplevel_resize_edge`] bits —
    /// either a single edge (`TOP` / `BOTTOM` / `LEFT` /
    /// `RIGHT`) or one of the four corner combinations
    /// (`TOP_LEFT` / `TOP_RIGHT` / `BOTTOM_LEFT` /
    /// `BOTTOM_RIGHT`).
    pub fn request_resize(
        &mut self,
        serial: u32,
        edges: u32,
    ) -> Result<(), ClientError> {
        self.app
            .client_mut()
            .xdg_toplevel_resize(self.xdg_toplevel, serial, edges)
    }

    /// The surface object id. Escape hatch for downstream
    /// buffer attachment (T119) and direct protocol use.
    pub fn surface(&self) -> ObjectId {
        self.surface
    }

    /// The `pmd_xdg_surface` object id. In the v1
    /// collapsed protocol this aliases [`Window::surface`];
    /// see the module-level doc comment.
    pub fn xdg_surface(&self) -> ObjectId {
        self.xdg_surface
    }

    /// The `pmd_xdg_toplevel` object id. Escape hatch for
    /// callers needing direct protocol access.
    pub fn xdg_toplevel(&self) -> ObjectId {
        self.xdg_toplevel
    }

    /// Currently-cached title (last value passed to
    /// [`Window::set_title`], or `""` if unset).
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Currently-cached app id (last value passed to
    /// [`Window::set_app_id`], or `""` if unset).
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    /// Borrow the underlying [`App`]. Escape hatch for
    /// callers that need to send other protocol requests
    /// alongside the window (buffer attachment in T119,
    /// multiple-window fixtures in tests).
    pub fn app(&self) -> &App<C> {
        self.app
    }

    /// Mutably borrow the underlying [`App`]. Mutable
    /// counterpart to [`Window::app`].
    pub fn app_mut(&mut self) -> &mut App<C> {
        self.app
    }
}
