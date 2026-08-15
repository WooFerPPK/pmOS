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

use display_proto::events::{CallbackDone, DisplayDeleteId, RegistryGlobal};
use display_proto::ids::ObjectId;
use display_proto::objects::Interface;
use display_proto::wire::WireError;
use std::time::Duration;

use crate::protocol::{Client, ClientError, ClientEventWithPayload, Connection, WaitFd};

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
    /// the global; stays `None` otherwise. Seat input is
    /// available to ordinary display clients as well as the
    /// desktop shell.
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
    /// 3. Send `display.sync(callback)`, parsing every event and recording
    ///    each `registry.global(name, interface, version)`
    ///    advertisement until the ordered `callback.done` marker arrives.
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
        let (globals, inbound) = Self::collect_registry_catalog(&mut client, registry_id)?;

        let compositor =
            Self::bind_required(&mut client, registry_id, &globals, Interface::Compositor)?;
        let shm = Self::bind_required(&mut client, registry_id, &globals, Interface::Shm)?;
        let xdg_shell =
            Self::bind_required(&mut client, registry_id, &globals, Interface::XdgShell)?;

        Ok(App {
            client,
            compositor,
            shm,
            xdg_shell,
            inbound,
            seat: None,
            pointer: None,
            shell_manager: None,
            keyboard: None,
        })
    }

    /// Like [`App::connect`] but additionally binds the
    /// optional `pmd_seat` (allocating pointer and keyboard
    /// objects) and `pmd_shell_manager` globals when the server
    /// advertises them. Input-enabled ordinary apps use this
    /// path for the seat; only a client whose kernel-authenticated
    /// capability set includes `Cap::Shell` is advertised and can
    /// bind shell-manager control.
    ///
    /// Missing seat / shell_manager are non-fatal: callers degrade
    /// to no input and/or no cross-client window list rather than
    /// failing the connect.
    pub fn connect_with_shell(connection: C) -> Result<Self, ClientError> {
        let mut client = Client::new(connection);
        let registry_id = client.get_registry()?;
        let (globals, inbound) = Self::collect_registry_catalog(&mut client, registry_id)?;

        let compositor =
            Self::bind_required(&mut client, registry_id, &globals, Interface::Compositor)?;
        let shm = Self::bind_required(&mut client, registry_id, &globals, Interface::Shm)?;
        let xdg_shell =
            Self::bind_required(&mut client, registry_id, &globals, Interface::XdgShell)?;

        let seat = Self::bind_optional(&mut client, registry_id, &globals, Interface::Seat)?;
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
        let shell_manager =
            Self::bind_optional(&mut client, registry_id, &globals, Interface::ShellManager)?;

        Ok(App {
            client,
            compositor,
            shm,
            xdg_shell,
            inbound,
            seat,
            pointer,
            shell_manager,
            keyboard,
        })
    }

    fn collect_registry_catalog(
        client: &mut Client<C>,
        registry_id: ObjectId,
    ) -> Result<(Vec<RegistryGlobal>, Vec<u8>), ClientError> {
        let sync_id = client.sync()?;
        let mut globals = Vec::new();
        let mut buffered = Vec::new();
        loop {
            // get_registry + sync are ordinary queued protocol writes. Drive
            // one bounded transport quantum per handshake iteration, then
            // park on FD_WRITE while a suffix remains. Only after both
            // requests are fully delivered can callback.done be reachable.
            if client.outbound_pending() {
                client.flush_outbound()?;
                if client.outbound_pending() {
                    client.wait(None)?;
                    continue;
                }
            }
            let chunk = client.connection_mut().recv();
            if !chunk.is_empty() {
                buffered.extend_from_slice(&chunk);
            }
            let mut consumed = 0usize;
            while buffered.len().saturating_sub(consumed) >= display_proto::wire::HEADER_SIZE {
                let remaining = &buffered[consumed..];
                let message_len = u16::from_le_bytes([remaining[6], remaining[7]]) as usize;
                if message_len < display_proto::wire::HEADER_SIZE {
                    return Err(ClientError::Wire(WireError::InvalidLength));
                }
                if remaining.len() < message_len {
                    break;
                }
                // Parse exactly one frame. Parsing the whole available batch
                // would consume ordinary events following callback.done and
                // leave the constructor with no way to hand them to the first
                // application dispatch.
                let (mut events, frame_consumed) =
                    client.push_received_with_payload(&remaining[..message_len])?;
                debug_assert_eq!(frame_consumed, message_len);
                let event = events
                    .pop()
                    .ok_or(ClientError::Wire(WireError::InvalidLength))?;
                consumed += message_len;
                if event.interface == Interface::Registry
                    && event.object_id == registry_id
                    && event.opcode == 1
                {
                    globals.push(
                        RegistryGlobal::decode(&event.payload)
                            .map_err(|_| ClientError::Wire(WireError::InvalidLength))?,
                    );
                } else if event.interface == Interface::Callback
                    && event.object_id == sync_id
                    && event.opcode == 1
                {
                    if event.payload.len() != 4 {
                        return Err(ClientError::Wire(WireError::InvalidLength));
                    }
                    CallbackDone::decode(&event.payload)
                        .map_err(|_| ClientError::Wire(WireError::InvalidLength))?;
                    client.retire_object(sync_id);
                    buffered.drain(..consumed);
                    // Preserve every byte following the matching marker,
                    // including complete events already present in the same
                    // recv and any partial trailing event. Ordinary dispatch
                    // must observe them in wire order.
                    return Ok((globals, buffered));
                }
            }
            buffered.drain(..consumed);
            // recv is uniformly nonblocking. A partial frame or temporarily
            // empty socket therefore parks on FD_READ; only callback.done,
            // never chunk boundaries or known-global counts, completes the
            // catalog.
            if chunk.is_empty() {
                client.wait(None)?;
            }
        }
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

    /// Send `pmd_shell_manager.minimize_window(window_id)`.
    pub fn shell_manager_minimize_window(&mut self, window_id: u32) -> Result<(), ClientError> {
        let id = self
            .shell_manager
            .ok_or(ClientError::MissingGlobal("pmd_shell_manager"))?;
        self.client
            .send_request(id, 4 /* minimize_window */, &window_id.to_le_bytes())
    }

    /// Send `pmd_shell_manager.unminimize_window(window_id)`.
    pub fn shell_manager_unminimize_window(&mut self, window_id: u32) -> Result<(), ClientError> {
        let id = self
            .shell_manager
            .ok_or(ClientError::MissingGlobal("pmd_shell_manager"))?;
        self.client
            .send_request(id, 5 /* unminimize_window */, &window_id.to_le_bytes())
    }

    /// Reserve a bottom output strip for desktop-shell chrome. Only a client
    /// that successfully bound the capability-gated shell manager can send
    /// this request; ordinary applications never control the work area.
    pub fn shell_manager_set_work_area_bottom(
        &mut self,
        height_px: u32,
    ) -> Result<(), ClientError> {
        let id = self
            .shell_manager
            .ok_or(ClientError::MissingGlobal("pmd_shell_manager"))?;
        self.client.send_request(
            id,
            6, /* set_work_area_bottom */
            &height_px.to_le_bytes(),
        )
    }

    /// Toggle the exact server-global window between normal and maximized
    /// geometry. The capability-gated display server remains authoritative for
    /// resolving the opaque ID and for sizing against the shell work area.
    pub fn shell_manager_toggle_maximized_window(
        &mut self,
        window_id: u32,
    ) -> Result<(), ClientError> {
        let id = self
            .shell_manager
            .ok_or(ClientError::MissingGlobal("pmd_shell_manager"))?;
        self.client.send_request(
            id,
            7, /* toggle_maximized_window */
            &window_id.to_le_bytes(),
        )
    }

    /// Publish a capability-authenticated presentation fence for the desktop.
    /// The display server emits the corresponding host fence only after every
    /// earlier scene mutation has been presented or proven pixel-identical.
    pub fn shell_manager_desktop_ready(&mut self) -> Result<(), ClientError> {
        let id = self
            .shell_manager
            .ok_or(ClientError::MissingGlobal("pmd_shell_manager"))?;
        self.client.send_request(id, 8 /* desktop_ready */, &[])
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
    /// events in one call); applies every `display.delete_id` acknowledgement
    /// to the client's object table while preserving the event in the returned
    /// stream; and leaves any partial trailing message for the next dispatch.
    /// Retired interface metadata is kept until this point so an event queued
    /// before destroy can still be decoded. Without this
    /// re-assembly the toolkit dropped split events, which
    /// surfaced as "the desktop stops responding after ~30s
    /// of activity" — pointer event traffic saturates the
    /// kernel's 64 KiB rx_buf, fragmentation rises, and
    /// every dropped event was a missed click.
    pub fn dispatch(&mut self) -> Result<Vec<ClientEventWithPayload>, ClientError> {
        let chunk = self.client.connection_mut().recv();
        if chunk.is_empty() && self.inbound.is_empty() {
            return Ok(Vec::new());
        }
        if !chunk.is_empty() {
            self.inbound.extend_from_slice(&chunk);
        }
        let (events, consumed) = self.client.push_received_with_payload(&self.inbound)?;
        for event in &events {
            if event.interface == Interface::Callback && event.opcode == 1 {
                if event.payload.len() != 4 {
                    return Err(ClientError::Wire(WireError::InvalidLength));
                }
                CallbackDone::decode(&event.payload)
                    .map_err(|_| ClientError::Wire(WireError::InvalidLength))?;
                self.client.retire_object(event.object_id);
            } else if event.object_id == ObjectId::DISPLAY
                && event.interface == Interface::Display
                && event.opcode == 2
            {
                let deleted = DisplayDeleteId::decode(&event.payload)
                    .map_err(|_| ClientError::Wire(WireError::InvalidLength))?;
                self.client.acknowledge_delete_id(deleted.id);
            }
        }
        if consumed > 0 {
            self.inbound.drain(..consumed);
        }
        Ok(events)
    }

    pub fn flush_outbound(&mut self) -> Result<(), ClientError> {
        self.client.flush_outbound()
    }

    pub fn outbound_pending(&self) -> bool {
        self.client.outbound_pending()
    }

    /// Block for display-socket readability or an optional application-owned
    /// timer. Event loops call this only after dispatching queued input,
    /// completing paint, and flushing protocol requests.
    pub fn wait(&mut self, timeout: Option<Duration>) -> Result<(), ClientError> {
        self.client.wait(timeout)
    }

    /// Park on the display socket plus application-owned descriptors and an
    /// optional real-duty timer in one bounded poll set.
    pub fn wait_with(
        &mut self,
        additional: &[WaitFd],
        timeout: Option<Duration>,
    ) -> Result<(), ClientError> {
        self.client.wait_with(additional, timeout)
    }

    /// Consume the `App`, returning the underlying
    /// [`Client`]. Escape hatch for callers that need direct
    /// protocol access across an API boundary the facade
    /// would otherwise bottleneck.
    pub fn into_client(self) -> Client<C> {
        self.client
    }
}
