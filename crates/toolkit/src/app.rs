//! Application-level facade over the protocol-layer
//! [`crate::protocol::Client`].
//!
//! [`App`] owns a [`Client`] plus the three globals every
//! v1 app needs (compositor, shm, xdg_shell). Its constructor
//! drives the `display.get_registry → registry.global* →
//! registry.bind` handshake in one call so every app in T121+
//! can skip the boilerplate and start creating surfaces.
//!
//! Higher layers (window wrapper T115, widgets T116,
//! layout T117, draw-buffer loop T119) will layer on top of
//! `App`; this slice stops at the bootstrap + dispatch cycle.

use display_proto::events::RegistryGlobal;
use display_proto::ids::ObjectId;
use display_proto::objects::Interface;
use display_proto::wire::WireError;

use crate::protocol::{Client, ClientError, ClientEventWithPayload, Connection};

/// Application-level handle to a connected display server.
///
/// Thin facade over the protocol-layer [`Client`]. Owns the
/// connection, allocates object ids, and drives the
/// `display.get_registry → registry.global* → registry.bind`
/// bootstrap sequence on construction. After
/// [`App::connect`] returns, bound global ids are queryable
/// via accessor methods; the client's event loop is driven
/// by calls to [`App::dispatch`].
pub struct App<C: Connection> {
    client: Client<C>,
    compositor: ObjectId,
    shm: ObjectId,
    xdg_shell: ObjectId,
    /// Re-assembly buffer for inbound bytes that don't yet
    /// form a complete message. The kernel's `fd_read`
    /// returns whatever the rx_buf has — and that boundary
    /// is independent of the protocol's framing — so a
    /// chunked event (e.g. a 2 KiB shell-manager broadcast)
    /// can cross multiple recv calls. Each `App::dispatch`
    /// appends the new chunk to this buffer, parses every
    /// complete message out of it, and leaves any partial
    /// trailing message for the next call.
    inbound: Vec<u8>,
    /// Optional `pmd_seat` binding. Populated by
    /// [`App::connect_with_shell`] when the server advertises
    /// the global; stays `None` otherwise. The desktop shell
    /// uses this to allocate a pointer object so it can
    /// observe taskbar / launcher clicks.
    seat: Option<ObjectId>,
    /// Optional `pmd_pointer` binding allocated via
    /// `pmd_seat.get_pointer`. Populated by
    /// [`App::connect_with_shell`].
    pointer: Option<ObjectId>,
    /// Optional `pmd_shell_manager` binding. Populated by
    /// [`App::connect_with_shell`] iff the connecting client
    /// holds `Cap::Shell`. The desktop shell uses this to
    /// subscribe to window-list events.
    shell_manager: Option<ObjectId>,
    /// Optional `pmd_keyboard` binding allocated via
    /// `pmd_seat.get_keyboard`. Populated by
    /// [`App::connect_with_shell`] when the server advertised
    /// the seat global. Apps that need text input (the term,
    /// the editor's eventual edit mode, …) read keypress events
    /// off the dispatch stream tagged with this object id.
    keyboard: Option<ObjectId>,
}

impl<C: Connection> App<C> {
    /// Connect to a display server via `connection`, bind the
    /// three globals required by every v1 app
    /// (`pmd_compositor`, `pmd_shm`, `pmd_xdg_shell`), and
    /// return an `App` ready to create surfaces.
    ///
    /// The bootstrap sequence:
    ///
    /// 1. Construct a [`Client`] around `connection`.
    /// 2. Send `display.get_registry(new_id)` to allocate a
    ///    registry object.
    /// 3. Drain the connection's inbound side until no more
    ///    bytes arrive, parsing every event and recording
    ///    each `registry.global(name, interface, version)`
    ///    advertisement.
    /// 4. For each of `pmd_compositor`, `pmd_shm`,
    ///    `pmd_xdg_shell`, send `registry.bind(name,
    ///    interface, version, new_id)` — erroring out with
    ///    [`ClientError::MissingGlobal`] if any of the three
    ///    weren't advertised.
    ///
    /// Returns [`ClientError`] variants on wire-format,
    /// object-table, or handshake failures. Use
    /// [`App::dispatch`] afterwards to drive subsequent
    /// events.
    pub fn connect(connection: C) -> Result<Self, ClientError> {
        let mut client = Client::new(connection);
        let registry_id = client.get_registry()?;

        // Drain every byte the server has queued; the bind
        // sequence assumes the server has advertised its
        // globals synchronously. Bidirectional test
        // connections return the full advertisement in one
        // `recv` call; the default-empty `recv` path leaves
        // the globals map empty and falls through to the
        // missing-global check.
        let mut globals: Vec<RegistryGlobal> = Vec::new();
        let mut leftover: Vec<u8> = Vec::new();
        loop {
            let chunk = client.connection_mut().recv();
            if chunk.is_empty() && leftover.is_empty() {
                break;
            }
            leftover.extend_from_slice(&chunk);
            let (events, consumed) =
                client.push_received_with_payload(&leftover)?;
            for event in events {
                if event.interface == Interface::Registry
                    && event.object_id == registry_id
                    && event.opcode == 1
                /* global */
                {
                    let parsed = RegistryGlobal::decode(&event.payload)
                        .map_err(|_| ClientError::Wire(WireError::InvalidLength))?;
                    globals.push(parsed);
                }
            }
            leftover.drain(..consumed);
            if chunk.is_empty() && !leftover.is_empty() {
                // We have a partial trailing message and no
                // more bytes are coming — bail rather than
                // spin. The caller's recv contract is "empty
                // when nothing is ready".
                break;
            }
            if chunk.is_empty() {
                break;
            }
        }

        let compositor = Self::bind_required(
            &mut client,
            registry_id,
            &globals,
            Interface::Compositor,
        )?;
        let shm =
            Self::bind_required(&mut client, registry_id, &globals, Interface::Shm)?;
        let xdg_shell = Self::bind_required(
            &mut client,
            registry_id,
            &globals,
            Interface::XdgShell,
        )?;

        Ok(App {
            client,
            compositor,
            shm,
            xdg_shell,
            inbound: Vec::with_capacity(64 * 1024),
            seat: None,
            pointer: None,
            shell_manager: None,
            keyboard: None,
        })
    }

    /// Like [`App::connect`] but additionally binds the
    /// optional `pmd_seat` (allocating a pointer object via
    /// `pmd_seat.get_pointer`) and `pmd_shell_manager` globals
    /// when the server advertises them. Used by the desktop
    /// shell, which needs both: pointer for taskbar / launcher
    /// click routing, shell_manager for the cross-client
    /// window list.
    ///
    /// Missing seat / shell_manager are non-fatal — the shell
    /// degrades to "no input" + "no window list" rather than
    /// failing the connect.
    pub fn connect_with_shell(connection: C) -> Result<Self, ClientError> {
        let mut client = Client::new(connection);
        let registry_id = client.get_registry()?;

        let mut globals: Vec<RegistryGlobal> = Vec::new();
        let mut leftover: Vec<u8> = Vec::new();
        loop {
            let chunk = client.connection_mut().recv();
            if chunk.is_empty() && leftover.is_empty() {
                break;
            }
            leftover.extend_from_slice(&chunk);
            let (events, consumed) =
                client.push_received_with_payload(&leftover)?;
            for event in events {
                if event.interface == Interface::Registry
                    && event.object_id == registry_id
                    && event.opcode == 1
                {
                    let parsed = RegistryGlobal::decode(&event.payload)
                        .map_err(|_| ClientError::Wire(WireError::InvalidLength))?;
                    globals.push(parsed);
                }
            }
            leftover.drain(..consumed);
            if chunk.is_empty() && !leftover.is_empty() {
                break;
            }
            if chunk.is_empty() {
                break;
            }
        }

        let compositor = Self::bind_required(
            &mut client,
            registry_id,
            &globals,
            Interface::Compositor,
        )?;
        let shm =
            Self::bind_required(&mut client, registry_id, &globals, Interface::Shm)?;
        let xdg_shell = Self::bind_required(
            &mut client,
            registry_id,
            &globals,
            Interface::XdgShell,
        )?;

        let seat =
            Self::bind_optional(&mut client, registry_id, &globals, Interface::Seat)?;
        let pointer = if let Some(seat_id) = seat {
            // pmd_seat.get_pointer(new_id) — opcode 1 on Seat.
            let new_id = client.bind_new(Interface::Pointer)?;
            client.send_request(
                seat_id,
                1, /* get_pointer */
                &new_id.raw().to_le_bytes(),
            )?;
            Some(new_id)
        } else {
            None
        };
        let keyboard = if let Some(seat_id) = seat {
            // pmd_seat.get_keyboard(new_id) — opcode 2 on Seat.
            let new_id = client.bind_new(Interface::Keyboard)?;
            client.send_request(
                seat_id,
                2, /* get_keyboard */
                &new_id.raw().to_le_bytes(),
            )?;
            Some(new_id)
        } else {
            None
        };
        let shell_manager = Self::bind_optional(
            &mut client,
            registry_id,
            &globals,
            Interface::ShellManager,
        )?;

        Ok(App {
            client,
            compositor,
            shm,
            xdg_shell,
            inbound: Vec::with_capacity(64 * 1024),
            seat,
            pointer,
            shell_manager,
            keyboard,
        })
    }

    /// Optional version of [`Self::bind_required`]: returns
    /// `Ok(None)` when the global wasn't advertised, instead
    /// of erroring out.
    fn bind_optional(
        client: &mut Client<C>,
        registry_id: ObjectId,
        globals: &[RegistryGlobal],
        target: Interface,
    ) -> Result<Option<ObjectId>, ClientError> {
        let name = target.name();
        let Some(global) = globals.iter().find(|g| g.interface == name) else {
            return Ok(None);
        };
        let id = client.registry_bind(registry_id, global.name, target, global.version)?;
        Ok(Some(id))
    }

    /// Look up a required global by interface name in the
    /// advertisements the registry handed us, bind it
    /// through `registry.bind`, and return the newly
    /// allocated client-side id. Errors with
    /// [`ClientError::MissingGlobal`] if the advertisement
    /// list has no entry for this interface.
    fn bind_required(
        client: &mut Client<C>,
        registry_id: ObjectId,
        globals: &[RegistryGlobal],
        target: Interface,
    ) -> Result<ObjectId, ClientError> {
        let name = target.name();
        let global = globals
            .iter()
            .find(|g| g.interface == name)
            .ok_or(ClientError::MissingGlobal(name))?;
        client.registry_bind(registry_id, global.name, target, global.version)
    }

    /// Accessor for the bound `pmd_compositor` global's
    /// object id.
    pub fn compositor(&self) -> ObjectId {
        self.compositor
    }

    /// Accessor for the bound `pmd_shm` global's object id.
    pub fn shm(&self) -> ObjectId {
        self.shm
    }

    /// Accessor for the bound `pmd_xdg_shell` global's
    /// object id. (Spec §10 calls the interface
    /// `pmd_xdg_wm_base`; the codebase renamed it to
    /// `pmd_xdg_shell` — see [`Interface::XdgShell`].)
    pub fn xdg_shell(&self) -> ObjectId {
        self.xdg_shell
    }

    /// Accessor for the bound `pmd_seat` global, if any.
    /// `None` when the connect path didn't bind a seat
    /// (i.e. the server didn't advertise it, or the caller
    /// used [`App::connect`] instead of
    /// [`App::connect_with_shell`]).
    pub fn seat(&self) -> Option<ObjectId> {
        self.seat
    }

    /// Accessor for the bound `pmd_pointer` global, if any.
    /// Allocated by `pmd_seat.get_pointer` during
    /// [`App::connect_with_shell`]; pointer events arrive
    /// via [`App::dispatch`] tagged with this object id.
    pub fn pointer(&self) -> Option<ObjectId> {
        self.pointer
    }

    /// Accessor for the bound `pmd_shell_manager` global, if
    /// any. `None` if the server didn't advertise it (the
    /// connecting client lacks `Cap::Shell`) or
    /// [`App::connect`] was used.
    pub fn shell_manager(&self) -> Option<ObjectId> {
        self.shell_manager
    }

    /// Accessor for the bound `pmd_keyboard` object, if any.
    /// Allocated by `pmd_seat.get_keyboard` during
    /// [`App::connect_with_shell`]; keyboard events arrive
    /// via [`App::dispatch`] tagged with this object id.
    /// `None` when the server didn't advertise a seat or
    /// [`App::connect`] was used.
    pub fn keyboard(&self) -> Option<ObjectId> {
        self.keyboard
    }

    /// Send `pmd_shell_manager.subscribe_windows()`. After
    /// this call the server starts emitting `window_*`
    /// events on the shell-manager object — and emits a
    /// catch-up snapshot of every currently-open window
    /// inline. `Err(ClientError::MissingGlobal)` if no
    /// shell_manager was bound.
    pub fn shell_manager_subscribe_windows(&mut self) -> Result<(), ClientError> {
        let id = self
            .shell_manager
            .ok_or(ClientError::MissingGlobal("pmd_shell_manager"))?;
        self.client.send_request(id, 1 /* subscribe_windows */, &[])
    }

    /// Send `pmd_shell_manager.focus_window(window_id)`.
    /// Errors with `MissingGlobal` if no shell_manager.
    pub fn shell_manager_focus_window(&mut self, window_id: u32) -> Result<(), ClientError> {
        let id = self
            .shell_manager
            .ok_or(ClientError::MissingGlobal("pmd_shell_manager"))?;
        self.client
            .send_request(id, 2 /* focus_window */, &window_id.to_le_bytes())
    }

    /// Send `pmd_shell_manager.close_window(window_id)`.
    pub fn shell_manager_close_window(&mut self, window_id: u32) -> Result<(), ClientError> {
        let id = self
            .shell_manager
            .ok_or(ClientError::MissingGlobal("pmd_shell_manager"))?;
        self.client
            .send_request(id, 3 /* close_window */, &window_id.to_le_bytes())
    }

    /// Borrow the wrapped [`Client`] for callers that need
    /// direct protocol access (window wrapper, widgets,
    /// drawing). The mutable-borrow counterpart is
    /// [`App::client_mut`].
    pub fn client(&self) -> &Client<C> {
        &self.client
    }

    /// Mutably borrow the wrapped [`Client`].
    pub fn client_mut(&mut self) -> &mut Client<C> {
        &mut self.client
    }

    /// Run one event-dispatch cycle: drain whatever the
    /// server has queued, parse the byte stream into
    /// [`ClientEventWithPayload`]s, and return them.
    ///
    /// Pulls one chunk via `recv()` and appends to the
    /// re-assembly buffer; parses every complete message
    /// out of the buffer (a multi-message chunk yields all
    /// events in one call); leaves any partial trailing
    /// message for the next dispatch. Without this
    /// re-assembly the toolkit dropped split events, which
    /// surfaced as "the desktop stops responding after ~30s
    /// of activity" — pointer event traffic saturates the
    /// kernel's 64 KiB rx_buf, fragmentation rises, and
    /// every dropped event was a missed click.
    pub fn dispatch(
        &mut self,
    ) -> Result<Vec<ClientEventWithPayload>, ClientError> {
        let chunk = self.client.connection_mut().recv();
        if chunk.is_empty() && self.inbound.is_empty() {
            return Ok(Vec::new());
        }
        if !chunk.is_empty() {
            self.inbound.extend_from_slice(&chunk);
        }
        let (events, consumed) =
            self.client.push_received_with_payload(&self.inbound)?;
        if consumed > 0 {
            self.inbound.drain(..consumed);
        }
        Ok(events)
    }

    /// Consume the `App`, returning the underlying
    /// [`Client`]. Escape hatch for callers that need direct
    /// protocol access across an API boundary the facade
    /// would otherwise bottleneck.
    pub fn into_client(self) -> Client<C> {
        self.client
    }
}
